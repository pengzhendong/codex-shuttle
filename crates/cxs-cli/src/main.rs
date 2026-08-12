use std::fs::{self, OpenOptions};
use std::io::Write;
use std::os::unix::fs::{FileTypeExt, OpenOptionsExt};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::{Command, Stdio};
use std::task::{Context as TaskContext, Poll};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use cxs_core::{
    AppPaths, OperationLockMode, Profile, ProfileStatus, ProfileStore, desktop_codex_path,
    validate_profile_name,
};
use cxs_install::{InstallOptions, RemoteInstall};
use cxs_probe::{codex_version, local_codex_checks};
use cxs_ssh::{
    SshSnapshot, ensure_managed_include, query_remote, render_host, rewrite_managed_config,
    test_connection,
};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio_tungstenite::tungstenite::Message;

mod session_commands;

#[derive(Debug, Parser)]
#[command(
    name = "cxs",
    about = "Local-control, remote-execution bridge for Codex",
    version
)]
struct Arguments {
    #[command(subcommand)]
    command: Commands,
}

struct InstallArtifacts {
    runtime_package: Option<PathBuf>,
    local_download: bool,
    remote_codex: Option<String>,
    shim: Option<PathBuf>,
}

#[derive(Debug, Subcommand)]
enum Commands {
    /// Prepare a profile from an existing SSH host alias.
    #[command(alias = "setup")]
    Add {
        host: String,
        #[arg(long)]
        name: Option<String>,
        /// Resolve SSH configuration without opening a network connection.
        #[arg(long)]
        offline: bool,
    },
    /// List profiles.
    #[command(alias = "ls")]
    List,
    /// Show one profile's state.
    Status { profile: String },
    /// Check local Codex, SSH, remote Linux, and adapter readiness.
    Doctor {
        profile: String,
        #[arg(long)]
        offline: bool,
        /// Print a machine-readable diagnostic report.
        #[arg(long)]
        json: bool,
    },
    /// Install matching Codex executor and the Shuttle shim on the remote host.
    Install {
        profile: String,
        /// Use a locally built cxs-runtime package.
        #[arg(long, hide = true, conflicts_with_all = ["remote_codex", "local_download"])]
        runtime_package: Option<PathBuf>,
        /// Download the runtime on this Mac, then upload it over SSH.
        #[arg(long, conflicts_with_all = ["remote_codex", "runtime_package"])]
        local_download: bool,
        /// Reuse this existing Codex executable on the remote host.
        #[arg(long, hide = true, conflicts_with_all = ["runtime_package", "local_download"])]
        remote_codex: Option<String>,
        /// Use a local Linux cxs-shim binary instead of downloading it.
        #[arg(long, hide = true)]
        shim: Option<PathBuf>,
    },
    /// Install artifacts matching the desktop-bundled Codex version.
    Update {
        profile: String,
        #[arg(long, hide = true, conflicts_with_all = ["remote_codex", "local_download"])]
        runtime_package: Option<PathBuf>,
        /// Download the runtime on this Mac, then upload it over SSH.
        #[arg(long, conflicts_with_all = ["remote_codex", "runtime_package"])]
        local_download: bool,
        /// Reuse this existing Codex executable on the remote host.
        #[arg(long, hide = true, conflicts_with_all = ["runtime_package", "local_download"])]
        remote_codex: Option<String>,
        #[arg(long, hide = true)]
        shim: Option<PathBuf>,
    },
    /// Switch to the previous release and verify it end to end.
    Rollback { profile: String },
    /// Start the local bridge in the background.
    Up { profile: String },
    /// Stop the profile's local bridge.
    Down { profile: String },
    /// Print the generated SSH host block.
    Config { profile: String },
    /// Pull missing sessions from a remote host into this Mac.
    Sync {
        profile: String,
        /// Remote Codex home. Defaults to ~/.codex on the SSH host.
        #[arg(long)]
        remote_home: Option<String>,
        /// Override the destination provider. Defaults to local config.toml.
        #[arg(long)]
        provider: Option<String>,
    },
    /// Repair local session provider metadata and matching state-database rows.
    Repair {
        /// Override the provider. Defaults to local config.toml.
        #[arg(long)]
        provider: Option<String>,
    },
    /// Run the local relay for a prepared profile.
    Bridge { profile: String },
    /// Remove local state and the generated SSH host block.
    #[command(alias = "rm")]
    Remove {
        profile: String,
        /// Also remove the profile's remote shim configuration and token.
        #[arg(long)]
        remote: bool,
        /// With --remote, also purge cached remote Codex releases.
        #[arg(long, requires = "remote")]
        purge: bool,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let arguments = Arguments::parse();
    let paths = AppPaths::discover()?;
    let store = ProfileStore::new(paths);
    match arguments.command {
        Commands::Add {
            host,
            name,
            offline,
        } => {
            let profile_name = name.as_deref().unwrap_or(&host);
            let _lock = store.lock_profile(profile_name, OperationLockMode::Exclusive)?;
            add(
                &store,
                &host,
                name.as_deref(),
                desktop_codex_path(),
                offline,
            )
        }
        Commands::List => list(&store),
        Commands::Status { profile } => {
            let _lock = store.lock_profile(&profile, OperationLockMode::Shared)?;
            status(&store, &profile)
        }
        Commands::Doctor {
            profile,
            offline,
            json,
        } => {
            let _lock = store.lock_profile(&profile, OperationLockMode::Shared)?;
            doctor(&store, &profile, desktop_codex_path(), offline, json).await
        }
        Commands::Install {
            profile,
            runtime_package,
            local_download,
            remote_codex,
            shim,
        }
        | Commands::Update {
            profile,
            runtime_package,
            local_download,
            remote_codex,
            shim,
        } => {
            let _lock = store.lock_profile(&profile, OperationLockMode::Exclusive)?;
            install(
                &store,
                &profile,
                desktop_codex_path(),
                InstallArtifacts {
                    runtime_package,
                    local_download,
                    remote_codex,
                    shim,
                },
            )
            .await
        }
        Commands::Rollback { profile } => {
            let _lock = store.lock_profile(&profile, OperationLockMode::Exclusive)?;
            rollback(&store, &profile, desktop_codex_path()).await
        }
        Commands::Up { profile } => {
            let _lock = store.lock_profile(&profile, OperationLockMode::Exclusive)?;
            up(&store, &profile, desktop_codex_path())
        }
        Commands::Down { profile } => {
            let _lock = store.lock_profile(&profile, OperationLockMode::Exclusive)?;
            down(&store, &profile)
        }
        Commands::Config { profile } => {
            let _lock = store.lock_profile(&profile, OperationLockMode::Shared)?;
            config(&store, &profile)
        }
        Commands::Sync {
            profile,
            remote_home,
            provider,
        } => session_commands::sync(
            &store,
            &profile,
            remote_home.as_deref(),
            provider.as_deref(),
        ),
        Commands::Repair { provider } => session_commands::repair(&store, provider.as_deref()),
        Commands::Bridge { profile } => {
            let profile = store.load(&profile)?;
            cxs_bridge::serve(profile, store, desktop_codex_path().to_path_buf()).await
        }
        Commands::Remove {
            profile,
            remote,
            purge,
        } => {
            let _lock = store.lock_profile(&profile, OperationLockMode::Exclusive)?;
            remove(&store, &profile, remote, purge)
        }
    }
}

fn add(
    store: &ProfileStore,
    host: &str,
    requested_name: Option<&str>,
    codex: &Path,
    offline: bool,
) -> Result<()> {
    let name = requested_name.unwrap_or(host);
    validate_profile_name(name).with_context(|| {
        format!("'{name}' cannot be used as a profile name; pass a safe name with --name")
    })?;

    let snapshot = SshSnapshot::resolve(host)?;
    if !offline {
        test_connection(host)?;
    }
    let version = codex_version(codex)?;
    if let Ok(profile) = store.load(name) {
        if profile.source_host != host {
            bail!(
                "profile '{name}' already tracks SSH host '{}'; choose another --name or remove it first",
                profile.source_host
            );
        }
        if profile.codex_version != version {
            bail!(
                "profile '{name}' pins {}; current local Codex is {version}; run 'cxs update {name}'",
                profile.codex_version
            );
        }
        snapshot.save(&store.paths().profile_dir(name))?;
        rewrite_all_ssh_config(store)?;
        ensure_managed_include(store.paths())?;
        println!("Refreshed profile '{name}' from SSH host '{host}'.");
        println!("App alias: {}", profile.app_alias);
        println!("Status: {} (installation state preserved)", profile.status);
        return Ok(());
    }
    let profile = store.create_prepared(name, host, &version)?;
    let result = (|| {
        snapshot.save(&store.paths().profile_dir(name))?;
        rewrite_all_ssh_config(store)?;
        ensure_managed_include(store.paths())?;
        Ok(())
    })();
    if let Err(error) = result {
        let _ = store.remove(name);
        return Err(error);
    }

    println!(
        "Prepared profile '{}' from SSH host '{}'.",
        profile.name, profile.source_host
    );
    println!("App alias: {}", profile.app_alias);
    println!(
        "Status: {} (executor adapter is not contract-verified)",
        profile.status
    );
    println!("Next: cxs install {}", profile.name);
    Ok(())
}

fn list(store: &ProfileStore) -> Result<()> {
    let profiles = store.list()?;
    if profiles.is_empty() {
        println!("No profiles. Run 'cxs add <ssh-host>'.");
        return Ok(());
    }
    println!(
        "{:<20} {:<14} {:<24} SOURCE",
        "PROFILE", "STATUS", "APP ALIAS"
    );
    for profile in profiles {
        println!(
            "{:<20} {:<14} {:<24} {}",
            profile.name, profile.status, profile.app_alias, profile.source_host
        );
    }
    Ok(())
}

fn status(store: &ProfileStore, name: &str) -> Result<()> {
    let profile = store.load(name)?;
    println!("Profile:       {}", profile.name);
    println!("Status:        {}", profile.status);
    println!("Source host:   {}", profile.source_host);
    println!("App alias:     {}", profile.app_alias);
    println!("Codex version: {}", profile.codex_version);
    println!("Local socket:  {}", profile.local_socket.display());
    println!("Transport:     SSH stdio mux via {}", profile.app_alias);
    println!("Environment:   {}", profile.environment_id);
    println!(
        "Exec channel:  mux 127.0.0.1:{} -> remote 127.0.0.1:{}",
        profile.local_exec_port, profile.remote_exec_port
    );
    println!(
        "Bridge:        {}",
        if bridge_running(store, name)? {
            "running"
        } else {
            "stopped"
        }
    );
    if let Some(target) = &profile.installed_target {
        println!("Remote target: {target}");
    }
    if let Some(release) = &profile.remote_release {
        println!("Remote release:{release:>21}");
    }
    if let Some(source) = &profile.executor_source {
        println!("Executor:      {source}");
    }
    if let Some(path) = &profile.executor_path {
        println!("Executor path: {path}");
    }
    if profile.status == ProfileStatus::Ready {
        println!("Usable in App: yes");
    } else {
        println!("Usable in App: no");
    }
    Ok(())
}

#[derive(Debug, Serialize)]
struct DoctorReport {
    profile: String,
    ready: bool,
    checks: Vec<DoctorCheck>,
}

#[derive(Debug, Serialize)]
struct DoctorCheck {
    name: String,
    status: &'static str,
    detail: String,
}

impl DoctorCheck {
    fn from_result(name: &str, result: Result<String>) -> Self {
        match result {
            Ok(detail) => Self {
                name: name.to_owned(),
                status: "pass",
                detail,
            },
            Err(error) => Self {
                name: name.to_owned(),
                status: "fail",
                detail: error.to_string(),
            },
        }
    }

    fn skip(name: &str, detail: &str) -> Self {
        Self {
            name: name.to_owned(),
            status: "skip",
            detail: detail.to_owned(),
        }
    }
}

async fn doctor(
    store: &ProfileStore,
    name: &str,
    codex: &Path,
    offline: bool,
    json: bool,
) -> Result<()> {
    let profile = store.load(name)?;
    let snapshot = SshSnapshot::load(&store.paths().profile_dir(name))?;
    let mut checks = Vec::new();

    checks.push(DoctorCheck::from_result(
        "profile-state",
        if profile.status == ProfileStatus::Ready {
            Ok("profile contract suite has passed".to_owned())
        } else {
            Err(anyhow::anyhow!(
                "status is {}; remote artifacts and end-to-end contracts are incomplete",
                profile.status
            ))
        },
    ));
    checks.push(DoctorCheck::from_result(
        "local-bridge",
        if bridge_running(store, name)? {
            Ok("background bridge is running".to_owned())
        } else {
            Err(anyhow::anyhow!("bridge is stopped; run 'cxs up {name}'"))
        },
    ));
    let token_check = store
        .read_token(&profile)
        .map(|_| "private profile token is valid".to_owned());
    checks.push(DoctorCheck::from_result("profile-token", token_check));

    for check in local_codex_checks(codex) {
        checks.push(DoctorCheck {
            name: check.name,
            status: if check.passed { "pass" } else { "fail" },
            detail: check.detail,
        });
    }
    let current_version = codex_version(codex);
    checks.push(DoctorCheck::from_result(
        "version-match",
        current_version.and_then(|current| {
            if current == profile.codex_version {
                Ok(format!("pinned version {current}"))
            } else {
                bail!("profile={} current={current}", profile.codex_version)
            }
        }),
    ));

    checks.push(DoctorCheck::from_result(
        "ssh-config",
        SshSnapshot::resolve(&profile.source_host).and_then(|current| {
            if current == snapshot {
                Ok("resolved SSH configuration matches snapshot".to_owned())
            } else {
                bail!("source SSH configuration changed; recreate the profile")
            }
        }),
    ));

    if offline {
        for name in [
            "ssh-connection",
            "remote-platform",
            "remote-install",
            "ssh-agent-alias",
            "execution-transport",
        ] {
            checks.push(DoctorCheck::skip(name, "offline mode"));
        }
    } else {
        checks.extend(doctor_remote(&profile).await);
    }

    let ready = checks.iter().all(|check| check.status != "fail");
    let report = DoctorReport {
        profile: name.to_owned(),
        ready,
        checks,
    };
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        for check in &report.checks {
            println!(
                "{:<5} {:<24} {}",
                check.status.to_ascii_uppercase(),
                check.name,
                check.detail
            );
        }
    }
    if !ready {
        bail!("profile '{name}' is not ready");
    }
    Ok(())
}

async fn doctor_remote(profile: &Profile) -> Vec<DoctorCheck> {
    let mut checks = Vec::new();
    checks.push(DoctorCheck::from_result(
        "ssh-connection",
        test_connection(&profile.source_host).map(|()| "non-interactive SSH succeeded".to_owned()),
    ));
    checks.push(DoctorCheck::from_result(
        "remote-platform",
        query_remote(&profile.source_host).and_then(|facts| {
            if facts.kernel != "Linux" {
                bail!("unsupported kernel {}", facts.kernel);
            }
            if !matches!(facts.arch.as_str(), "x86_64" | "aarch64") {
                bail!("unsupported architecture {}", facts.arch);
            }
            Ok(format!(
                "{} {} home={}",
                facts.kernel, facts.arch, facts.home
            ))
        }),
    ));
    checks.push(DoctorCheck::from_result(
        "remote-install",
        cxs_install::inspect(profile).and_then(|remote| {
            verify_remote_metadata(profile, &remote)?;
            cxs_install::verify_executor(profile, &remote)?;
            let source = remote
                .executor_source
                .map_or_else(|| "legacy-package".to_owned(), |source| source.to_string());
            Ok(format!("{} ({}, {source})", remote.release, remote.target))
        }),
    ));
    checks.push(DoctorCheck::from_result(
        "ssh-agent-alias",
        test_connection(&profile.app_alias)
            .map(|()| "generated alias accepts ordinary SSH commands".to_owned()),
    ));
    checks.push(DoctorCheck::from_result(
        "execution-transport",
        probe_ready(profile).await.map(|()| {
            "environment ready; remote filesystem and command execution passed".to_owned()
        }),
    ));
    checks
}

fn config(store: &ProfileStore, name: &str) -> Result<()> {
    let profile = store.load(name)?;
    let snapshot = SshSnapshot::load(&store.paths().profile_dir(name))?;
    print!("{}", render_host(&profile, &snapshot)?);
    Ok(())
}

fn remove(store: &ProfileStore, name: &str, remote: bool, purge: bool) -> Result<()> {
    let profile = store.load(name)?;
    stop_bridge(store, name, true)?;
    if remote {
        cxs_install::uninstall(&profile, purge)?;
    }
    store.remove(name)?;
    rewrite_all_ssh_config(store)?;
    println!(
        "Removed profile '{}' and SSH alias '{}'.",
        profile.name, profile.app_alias
    );
    if remote {
        println!(
            "Removed the remote profile configuration{}.",
            if purge { " and cached releases" } else { "" }
        );
    } else {
        println!("Remote files were preserved; pass --remote to remove them.");
    }
    Ok(())
}

async fn install(
    store: &ProfileStore,
    name: &str,
    codex: &Path,
    artifacts: InstallArtifacts,
) -> Result<()> {
    let mut profile = store.load(name)?;
    let previous_profile = profile.clone();
    let checks = local_codex_checks(codex);
    if let Some(failed) = checks.iter().find(|check| !check.passed) {
        bail!(
            "local Codex compatibility check '{}' failed: {}",
            failed.name,
            failed.detail
        );
    }
    let current_version = codex_version(codex)?;
    test_connection(&profile.app_alias)?;
    let facts = query_remote(&profile.source_host)?;
    let token = store.read_token(&profile)?;
    let candidate_version = profile.codex_version.clone();
    profile.codex_version = current_version;
    let options = InstallOptions {
        runtime_package: artifacts.runtime_package,
        local_download: artifacts.local_download,
        remote_codex: artifacts.remote_codex,
        shim: artifacts.shim,
        ..InstallOptions::default()
    };
    if stop_bridge(store, name, false)? {
        println!("Stopped the existing bridge before replacing remote artifacts.");
    }
    println!(
        "Installing {} executor for {} on '{}'...",
        profile.codex_version, facts.arch, profile.source_host
    );
    let record = match cxs_install::install(&profile, &token, &facts, &options) {
        Ok(record) => record,
        Err(error) => {
            profile.codex_version = candidate_version;
            return Err(error);
        }
    };
    println!(
        "Executor: {} ({})",
        record.executor_source, record.executor_path
    );
    profile.status = ProfileStatus::Installed;
    profile.installed_target = Some(record.target);
    profile.remote_home = Some(record.remote_home);
    profile.remote_release = Some(record.remote_release);
    profile.package_sha256 = record.package_sha256;
    profile.shim_sha256 = Some(record.shim_sha256);
    profile.executor_source = Some(record.executor_source.to_string());
    profile.executor_path = Some(record.executor_path);
    store.save(&profile)?;
    rewrite_all_ssh_config(store)?;

    let verification = match up(store, name, codex) {
        Ok(()) => {
            println!("Verifying the App Server-to-remote Exec Server path...");
            probe_ready(&profile).await
        }
        Err(error) => Err(error),
    };
    if let Err(error) = verification {
        let _ = stop_bridge(store, name, true);
        let recovery = recover_failed_install(store, &mut profile, &previous_profile);
        return match recovery {
            Ok(detail) => Err(error.context(format!(
                "installed artifacts failed verification; {detail}"
            ))),
            Err(recovery_error) => Err(error.context(format!(
                "installed artifacts failed verification, and automatic recovery also failed: {recovery_error:#}"
            ))),
        };
    }
    profile.status = ProfileStatus::Ready;
    store.save(&profile)?;
    rewrite_all_ssh_config(store)?;
    println!("Profile '{}' is ready.", profile.name);
    println!("Codex App SSH alias: {}", profile.app_alias);
    Ok(())
}

fn recover_failed_install(
    store: &ProfileStore,
    profile: &mut Profile,
    previous: &Profile,
) -> Result<String> {
    if previous.status == ProfileStatus::Prepared {
        cxs_install::uninstall(profile, false)?;
        *profile = previous.clone();
        store.save(profile)?;
        rewrite_all_ssh_config(store)?;
        return Ok(
            "removed the unverified first install and restored the prepared profile".to_owned(),
        );
    }

    let remote = cxs_install::rollback(profile)?;
    profile.codex_version = remote.codex_version;
    profile.installed_target = Some(remote.target);
    profile.remote_release = Some(remote.release);
    profile.package_sha256 = remote.package_sha256;
    profile.shim_sha256 = Some(remote.shim_sha256);
    profile.executor_source = remote.executor_source.map(|source| source.to_string());
    profile.executor_path = remote.executor_path;
    profile.status = ProfileStatus::Installed;
    store.save(profile)?;
    rewrite_all_ssh_config(store)?;
    Ok("rolled the remote host back to the previous verified release".to_owned())
}

async fn rollback(store: &ProfileStore, name: &str, codex: &Path) -> Result<()> {
    let mut profile = store.load(name)?;
    let previous_profile = profile.clone();
    stop_bridge(store, name, true)?;
    let remote = cxs_install::rollback(&profile)?;
    profile.codex_version = remote.codex_version;
    profile.installed_target = Some(remote.target);
    profile.remote_release = Some(remote.release);
    profile.package_sha256 = remote.package_sha256;
    profile.shim_sha256 = Some(remote.shim_sha256);
    profile.executor_source = remote.executor_source.map(|source| source.to_string());
    profile.executor_path = remote.executor_path;
    profile.status = ProfileStatus::Installed;
    store.save(&profile)?;
    rewrite_all_ssh_config(store)?;
    let verification = match up(store, name, codex) {
        Ok(()) => probe_ready(&profile).await,
        Err(error) => Err(error),
    };
    if let Err(error) = verification {
        let _ = stop_bridge(store, name, true);
        let recovery = cxs_install::rollback(&profile).and_then(|_| {
            store.save(&previous_profile)?;
            rewrite_all_ssh_config(store)?;
            up(store, name, codex)
        });
        return match recovery {
            Ok(()) => Err(error.context("rollback verification failed; restored the newer release")),
            Err(recovery_error) => Err(error.context(format!(
                "rollback verification failed and restoring the newer release also failed: {recovery_error:#}"
            ))),
        };
    }
    profile.status = ProfileStatus::Ready;
    store.save(&profile)?;
    rewrite_all_ssh_config(store)?;
    println!(
        "Rolled '{}' back to {} and verified it.",
        name, profile.codex_version
    );
    Ok(())
}

fn up(store: &ProfileStore, name: &str, codex: &Path) -> Result<()> {
    let profile = store.load(name)?;
    if profile.status == ProfileStatus::Prepared {
        bail!("profile '{name}' is not installed; run 'cxs install {name}' first");
    }
    let current = codex_version(codex)?;
    if current != profile.codex_version {
        bail!(
            "Codex version mismatch: profile={} current={current}",
            profile.codex_version
        );
    }
    if bridge_running(store, name)? {
        println!("Bridge for '{name}' is already running.");
        return Ok(());
    }
    clear_stale_runtime(store, &profile)?;
    let executable = std::env::current_exe().context("could not locate the cxs executable")?;
    let log_path = store.paths().bridge_log(name);
    let log = OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(&log_path)?;
    let stderr = log.try_clone()?;
    let child = Command::new(executable)
        .arg("bridge")
        .arg(name)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(stderr))
        .process_group(0)
        .spawn()
        .context("could not start the background bridge")?;
    let pid = child.id();
    write_pid_file(&store.paths().bridge_pid(name), pid)?;
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        if profile.local_socket.exists() && pid_alive(pid) {
            println!("Started bridge for '{name}' (pid {pid}).");
            return Ok(());
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let _ = stop_bridge(store, name, true);
    bail!(
        "bridge did not become ready; inspect {}",
        log_path.display()
    )
}

fn down(store: &ProfileStore, name: &str) -> Result<()> {
    store.load(name)?;
    if stop_bridge(store, name, false)? {
        println!("Stopped bridge for '{name}'.");
    } else {
        println!("Bridge for '{name}' is already stopped.");
    }
    Ok(())
}

fn bridge_running(store: &ProfileStore, name: &str) -> Result<bool> {
    let path = store.paths().bridge_pid(name);
    let Some(pid) = read_pid_file(&path)? else {
        return Ok(false);
    };
    Ok(pid_alive(pid) && pid_is_bridge(pid, name))
}

fn stop_bridge(store: &ProfileStore, name: &str, quiet: bool) -> Result<bool> {
    let pid_path = store.paths().bridge_pid(name);
    let Some(pid) = read_pid_file(&pid_path)? else {
        return Ok(false);
    };
    if pid_alive(pid) && pid_is_bridge(pid, name) {
        let process = nix::unistd::Pid::from_raw(i32::try_from(pid)?);
        let _ = nix::sys::signal::killpg(process, nix::sys::signal::Signal::SIGTERM);
        let deadline = Instant::now() + Duration::from_secs(3);
        while Instant::now() < deadline && pid_alive(pid) {
            std::thread::sleep(Duration::from_millis(50));
        }
        if pid_alive(pid) {
            let _ = nix::sys::signal::killpg(process, nix::sys::signal::Signal::SIGKILL);
        }
    }
    fs::remove_file(&pid_path).or_else(ignore_not_found)?;
    if !quiet {
        let profile = store.load(name)?;
        remove_socket_if_socket(&profile.local_socket)?;
    }
    Ok(true)
}

fn clear_stale_runtime(store: &ProfileStore, profile: &Profile) -> Result<()> {
    fs::remove_file(store.paths().bridge_pid(&profile.name)).or_else(ignore_not_found)?;
    remove_socket_if_socket(&profile.local_socket)
}

fn remove_socket_if_socket(path: &Path) -> Result<()> {
    let Ok(metadata) = fs::symlink_metadata(path) else {
        return Ok(());
    };
    if !metadata.file_type().is_socket() {
        bail!(
            "refusing to remove non-socket runtime path {}",
            path.display()
        );
    }
    fs::remove_file(path)?;
    Ok(())
}

fn write_pid_file(path: &Path, pid: u32) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .mode(0o600)
        .open(path)?;
    writeln!(file, "{pid}")?;
    file.sync_all()?;
    Ok(())
}

fn read_pid_file(path: &Path) -> Result<Option<u32>> {
    match fs::read_to_string(path) {
        Ok(value) => Ok(Some(
            value.trim().parse().context("bridge pid file is invalid")?,
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn pid_alive(pid: u32) -> bool {
    let Ok(pid) = i32::try_from(pid) else {
        return false;
    };
    nix::sys::signal::kill(nix::unistd::Pid::from_raw(pid), None).is_ok()
}

fn pid_is_bridge(pid: u32, name: &str) -> bool {
    let output = Command::new("ps")
        .arg("-p")
        .arg(pid.to_string())
        .args(["-o", "command="])
        .output();
    let Ok(output) = output else {
        return false;
    };
    if !output.status.success() {
        return false;
    }
    let command = String::from_utf8_lossy(&output.stdout);
    command.contains("cxs") && command.contains(&format!("bridge {name}"))
}

fn ignore_not_found(error: std::io::Error) -> std::io::Result<()> {
    if error.kind() == std::io::ErrorKind::NotFound {
        Ok(())
    } else {
        Err(error)
    }
}

fn verify_remote_metadata(profile: &Profile, remote: &RemoteInstall) -> Result<()> {
    if remote.profile != profile.name {
        bail!(
            "remote profile is {}, expected {}",
            remote.profile,
            profile.name
        );
    }
    if remote.codex_version != profile.codex_version {
        bail!(
            "remote={} local={}",
            remote.codex_version,
            profile.codex_version
        );
    }
    if profile.package_sha256.as_deref() != remote.package_sha256.as_deref() {
        bail!("remote Codex package digest does not match local state");
    }
    if profile.shim_sha256.as_deref() != Some(&remote.shim_sha256) {
        bail!("remote shim digest does not match local state");
    }
    if let Some(expected) = profile.executor_source.as_deref()
        && remote
            .executor_source
            .map(|source| source.to_string())
            .as_deref()
            != Some(expected)
    {
        bail!("remote executor source does not match local state");
    }
    if let Some(expected) = profile.executor_path.as_deref()
        && remote.executor_path.as_deref() != Some(expected)
    {
        bail!("remote executor path does not match local state");
    }
    Ok(())
}

async fn probe_ready(profile: &Profile) -> Result<()> {
    let probe_control_directory = format!("/tmp/cxs-{}-probe", profile.name);
    let probe_control_socket = format!("{probe_control_directory}/app.sock");
    let probe_pid_file = format!("{probe_control_directory}/app.pid");
    let result = probe_ready_inner(
        profile,
        &probe_control_directory,
        &probe_control_socket,
        &probe_pid_file,
    )
    .await;
    let cleanup = cleanup_remote_probe(
        profile,
        &probe_control_directory,
        &probe_control_socket,
        &probe_pid_file,
    )
    .await;
    match (result, cleanup) {
        (Ok(()), Ok(())) => Ok(()),
        (Err(error), Ok(())) => Err(error),
        (Ok(()), Err(error)) => Err(error).context("remote App Server probe cleanup failed"),
        (Err(error), Err(cleanup)) => Err(anyhow::anyhow!(
            "{error:#}; remote App Server probe cleanup also failed: {cleanup:#}"
        )),
    }
}

#[allow(clippy::too_many_lines)]
async fn probe_ready_inner(
    profile: &Profile,
    probe_control_directory: &str,
    probe_control_socket: &str,
    probe_pid_file: &str,
) -> Result<()> {
    use tokio::io::AsyncReadExt;

    let bootstrap_command = format!(
        r#"mkdir -p "$HOME/.config/codex-shuttle" "{probe_control_directory}"; chmod 700 "{probe_control_directory}"; rm -f "{probe_control_socket}" "{probe_pid_file}"; probe_token="cxs-probe-$$-$(date +%s)"; CXS_PROBE_TOKEN="$probe_token" CXS_CONTROL_SOCKET="{probe_control_socket}" nohup "$HOME/.local/bin/codex" -c features.code_mode_host=true app-server --listen unix:// >"$HOME/.config/codex-shuttle/probe-{}.log" 2>&1 </dev/null & probe_pid=$!; printf '%s %s\n' "$probe_pid" "$probe_token" >"{probe_pid_file}"; chmod 600 "{probe_pid_file}"; probe_tries=0; while [ "$probe_tries" -lt 100 ]; do [ -S "{probe_control_socket}" ] && exit 0; kill -0 "$probe_pid" 2>/dev/null || break; probe_tries=$((probe_tries + 1)); sleep 0.1; done; cat "$HOME/.config/codex-shuttle/probe-{}.log" >&2; exit 1"#,
        profile.name, profile.name
    );
    let bootstrap = tokio::time::timeout(
        Duration::from_secs(15),
        tokio::process::Command::new("ssh")
            .args([
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                &profile.app_alias,
                &bootstrap_command,
            ])
            .status(),
    )
    .await
    .context("remote App Server bootstrap timed out")??;
    if !bootstrap.success() {
        bail!("remote App Server bootstrap exited with {bootstrap}");
    }

    let proxy_command = format!(
        r#"CXS_CONTROL_SOCKET="{probe_control_socket}" "$HOME/.local/bin/codex" app-server proxy"#
    );
    let mut child = tokio::process::Command::new("ssh")
        .args([
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            &profile.app_alias,
            &proxy_command,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| {
            format!(
                "could not start end-to-end probe through {}",
                profile.app_alias
            )
        })?;
    let write = child.stdin.take().context("probe stdin was unavailable")?;
    let read = child
        .stdout
        .take()
        .context("probe stdout was unavailable")?;
    let mut stderr = child
        .stderr
        .take()
        .context("probe stderr was unavailable")?;
    let handshake = tokio::time::timeout(
        Duration::from_secs(15),
        tokio_tungstenite::client_async("ws://codex-app-server/rpc", ProcessIo { read, write }),
    )
    .await;
    let (mut websocket, _) = match handshake {
        Ok(Ok(websocket)) => websocket,
        result => {
            let _ = child.kill().await;
            let mut diagnostic = Vec::new();
            let _ =
                tokio::time::timeout(Duration::from_secs(1), stderr.read_to_end(&mut diagnostic))
                    .await;
            let detail = String::from_utf8_lossy(&diagnostic);
            match result {
                Ok(Err(error)) => bail!(
                    "could not open the SSH App Server WebSocket: {error}; SSH diagnostic: {}",
                    detail.trim()
                ),
                Err(_) => bail!(
                    "SSH App Server WebSocket handshake timed out; SSH diagnostic: {}",
                    detail.trim()
                ),
                Ok(Ok(_)) => unreachable!(),
            }
        }
    };
    let initialize = serde_json::json!({
        "id": "cxs/probe/initialize",
        "method": "initialize",
        "params": {
            "clientInfo": {"name": "cxs", "title": "Codex Shuttle", "version": env!("CARGO_PKG_VERSION")},
            "capabilities": {"experimentalApi": true}
        }
    });
    let status = serde_json::json!({
        "id": "cxs/probe/status",
        "method": "environment/status",
        "params": {"environmentId": profile.environment_id}
    });
    let remote_home = profile.remote_home.as_deref().unwrap_or("/");
    let read_directory = serde_json::json!({
        "id": "cxs/probe/read-directory",
        "method": "fs/readDirectory",
        "params": {"path": remote_home}
    });
    let run_command = serde_json::json!({
        "id": "cxs/probe/run-command",
        "method": "command/exec",
        "params": {
            "command": [
                "/bin/sh",
                "-c",
                "printf 'cxs-probe-linux\\n'; pwd; uname -s; uname -m"
            ],
            "cwd": remote_home,
            "timeoutMs": 5000
        }
    });
    websocket
        .send(Message::Text(serde_json::to_string(&initialize)?.into()))
        .await?;
    websocket
        .send(Message::Text(serde_json::to_string(&status)?.into()))
        .await?;
    websocket
        .send(Message::Text(
            serde_json::to_string(&read_directory)?.into(),
        ))
        .await?;
    websocket
        .send(Message::Text(serde_json::to_string(&run_command)?.into()))
        .await?;

    let result = tokio::time::timeout(Duration::from_secs(20), async {
        let mut status_ready = false;
        let mut directory_ready = false;
        let mut command_ready = false;
        while let Some(message) = websocket.next().await {
            let message = message?;
            if !message.is_text() {
                continue;
            }
            let message: serde_json::Value = serde_json::from_str(message.to_text()?)?;
            if message.get("id").and_then(serde_json::Value::as_str) == Some("cxs/probe/status") {
                if let Some(error) = message.get("error") {
                    bail!("environment/status failed: {error}");
                }
                let ready = message
                    .pointer("/result/status")
                    .and_then(serde_json::Value::as_str)
                    == Some("ready")
                    || message.get("result").and_then(serde_json::Value::as_str) == Some("ready");
                if !ready {
                    bail!(
                        "remote execution environment is not ready: {}",
                        message["result"]
                    );
                }
                status_ready = true;
            } else if message.get("id").and_then(serde_json::Value::as_str)
                == Some("cxs/probe/read-directory")
            {
                if let Some(error) = message.get("error") {
                    bail!("remote fs/readDirectory failed: {error}");
                }
                if !message
                    .pointer("/result/entries")
                    .is_some_and(serde_json::Value::is_array)
                {
                    bail!("remote fs/readDirectory returned an invalid result: {message}");
                }
                directory_ready = true;
            } else if message.get("id").and_then(serde_json::Value::as_str)
                == Some("cxs/probe/run-command")
            {
                if let Some(error) = message.get("error") {
                    bail!("remote command/exec failed: {error}");
                }
                let exit_code = message
                    .pointer("/result/exitCode")
                    .and_then(serde_json::Value::as_i64);
                let stdout = message
                    .pointer("/result/stdout")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or_default();
                if exit_code != Some(0)
                    || !stdout.lines().any(|line| line == "cxs-probe-linux")
                    || !stdout.lines().any(|line| line == remote_home)
                    || !stdout.lines().any(|line| line == "Linux")
                    || !stdout
                        .lines()
                        .any(|line| matches!(line, "aarch64" | "x86_64"))
                {
                    bail!("remote command/exec returned an invalid result: {message}");
                }
                command_ready = true;
            }
            if status_ready && directory_ready && command_ready {
                return Ok(());
            }
        }
        bail!(
            "probe connection closed before environment, remote filesystem, and command checks completed"
        )
    })
    .await
    .context("end-to-end probe timed out")?;
    let _ = websocket.close(None).await;
    let _ = child.kill().await;
    let mut diagnostic = Vec::new();
    let _ = tokio::time::timeout(Duration::from_secs(1), stderr.read_to_end(&mut diagnostic)).await;
    result.with_context(|| {
        let detail = String::from_utf8_lossy(&diagnostic);
        format!("SSH probe diagnostic: {}", detail.trim())
    })
}

async fn cleanup_remote_probe(
    profile: &Profile,
    probe_control_directory: &str,
    probe_control_socket: &str,
    probe_pid_file: &str,
) -> Result<()> {
    let cleanup_command = format!(
        r#"probe_pid=''; probe_token=''; if [ -f "{probe_pid_file}" ]; then read -r probe_pid probe_token <"{probe_pid_file}" || true; fi; probe_owned=false; case "$probe_pid" in ''|*[!0-9]*) ;; *) case "$probe_token" in ''|*[!0-9A-Za-z_-]*) ;; *) if [ -r "/proc/$probe_pid/environ" ] && tr '\000' '\n' <"/proc/$probe_pid/environ" | grep -Fqx "CXS_PROBE_TOKEN=$probe_token"; then probe_owned=true; fi ;; esac ;; esac; if [ "$probe_owned" = true ]; then kill -TERM "$probe_pid" 2>/dev/null || true; cleanup_tries=0; while kill -0 "$probe_pid" 2>/dev/null && [ "$cleanup_tries" -lt 20 ]; do cleanup_tries=$((cleanup_tries + 1)); sleep 0.1; done; if kill -0 "$probe_pid" 2>/dev/null && [ -r "/proc/$probe_pid/environ" ] && tr '\000' '\n' <"/proc/$probe_pid/environ" | grep -Fqx "CXS_PROBE_TOKEN=$probe_token"; then kill -KILL "$probe_pid" 2>/dev/null || true; fi; fi; rm -f "{probe_control_socket}" "{probe_pid_file}"; rmdir "{probe_control_directory}" 2>/dev/null || true"#
    );
    let status = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new("ssh")
            .args([
                "-T",
                "-o",
                "BatchMode=yes",
                "-o",
                "ConnectTimeout=10",
                &profile.app_alias,
                &cleanup_command,
            ])
            .status(),
    )
    .await
    .context("remote probe cleanup timed out")??;
    if !status.success() {
        bail!("remote probe cleanup exited with {status}");
    }
    Ok(())
}

struct ProcessIo {
    read: tokio::process::ChildStdout,
    write: tokio::process::ChildStdin,
}

impl AsyncRead for ProcessIo {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.read).poll_read(context, buffer)
    }
}

impl AsyncWrite for ProcessIo {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.write).poll_write(context, buffer)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.write).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut TaskContext<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.write).poll_shutdown(context)
    }
}

fn rewrite_all_ssh_config(store: &ProfileStore) -> Result<()> {
    let mut entries: Vec<(Profile, SshSnapshot)> = Vec::new();
    for profile in store.list()? {
        let snapshot = SshSnapshot::load(&store.paths().profile_dir(&profile.name))?;
        entries.push((profile, snapshot));
    }
    rewrite_managed_config(&store.paths().managed_ssh_config, &entries)
}
