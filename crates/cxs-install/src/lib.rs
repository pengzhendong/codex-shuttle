use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;

use anyhow::{Context, Result, bail};
use cxs_core::Profile;
use cxs_ssh::RemoteFacts;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tempfile::TempDir;

const SHUTTLE_RELEASES: &str = "https://github.com/pengzhendong/codex-shuttle/releases/download";
const REMOTE_LAYOUT_VERSION: u32 = 3;

fn shuttle_release_tag() -> &'static str {
    option_env!("CXS_RELEASE_TAG").unwrap_or(concat!("v", env!("CARGO_PKG_VERSION")))
}

fn release_asset_url(release_base: &str, release_tag: &str, asset: &str) -> String {
    format!(
        "{}/{release_tag}/{asset}",
        release_base.trim_end_matches('/')
    )
}

#[derive(Debug, Clone)]
pub struct InstallOptions {
    pub runtime_package: Option<PathBuf>,
    pub local_download: bool,
    pub remote_codex: Option<String>,
    pub shim: Option<PathBuf>,
    pub shuttle_release_base: String,
}

impl Default for InstallOptions {
    fn default() -> Self {
        Self {
            runtime_package: None,
            local_download: false,
            remote_codex: None,
            shim: None,
            shuttle_release_base: SHUTTLE_RELEASES.to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallRecord {
    pub target: String,
    pub remote_home: String,
    pub remote_release: String,
    pub package_sha256: Option<String>,
    pub shim_sha256: String,
    pub executor_source: ExecutorSource,
    pub executor_path: String,
}

#[derive(Debug, Clone, Copy, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutorSource {
    ManagedRuntime,
    RemoteExisting,
}

impl std::fmt::Display for ExecutorSource {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ManagedRuntime => formatter.write_str("managed-runtime"),
            Self::RemoteExisting => formatter.write_str("remote-existing"),
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize, PartialEq, Eq)]
pub struct RemoteInstall {
    #[serde(default = "legacy_layout_version")]
    pub layout_version: u32,
    pub profile: String,
    pub codex_version: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub runtime_version: Option<String>,
    pub target: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub package_sha256: Option<String>,
    pub shim_sha256: String,
    pub release: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor_source: Option<ExecutorSource>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor_path: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub executor_sha256: Option<String>,
}

const fn legacy_layout_version() -> u32 {
    1
}

#[derive(Debug, Serialize)]
struct ShimConfig<'a> {
    profile: &'a str,
    codex_version: &'a str,
    agent_socket: String,
    control_socket: String,
    token_file: String,
    exec_server: String,
    exec_server_args: Vec<String>,
    exec_server_port: u16,
    codex_home: String,
    original_codex: String,
}

enum PackageSource {
    Upload {
        path: PathBuf,
        sha256: String,
    },
    LocalRuntime {
        runtime_url: String,
        runtime_manifest_url: String,
        runtime_name: String,
        path: PathBuf,
    },
    RemoteRuntime {
        runtime_url: String,
        runtime_manifest_url: String,
        runtime_name: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PackageTransfer {
    sha256: String,
}

#[derive(Debug)]
struct ReusableExecutor {
    probe_path: String,
    installed_path: String,
    sha256: String,
}

pub fn codex_release_version(version_output: &str) -> Result<&str> {
    let version = version_output
        .split_ascii_whitespace()
        .last()
        .context("Codex version output was empty")?;
    if version.is_empty()
        || version.len() > 80
        || !version
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'-' | b'+'))
    {
        bail!("unsupported Codex version string '{version}'");
    }
    Ok(version)
}

pub fn codex_source_version(version_output: &str) -> Result<&str> {
    let version = codex_release_version(version_output)?;
    Ok(version
        .split_once('-')
        .map_or(version, |(source_version, _)| source_version))
}

pub fn linux_target(facts: &RemoteFacts) -> Result<&'static str> {
    if facts.kernel != "Linux" {
        bail!("unsupported remote kernel {}", facts.kernel);
    }
    match facts.arch.as_str() {
        "x86_64" | "amd64" => Ok("x86_64-unknown-linux-musl"),
        "aarch64" | "arm64" => Ok("aarch64-unknown-linux-musl"),
        other => bail!("unsupported remote architecture {other}"),
    }
}

#[allow(clippy::too_many_lines)]
pub fn install(
    profile: &Profile,
    token: &str,
    facts: &RemoteFacts,
    options: &InstallOptions,
) -> Result<InstallRecord> {
    let target = linux_target(facts)?;
    let version = codex_source_version(&profile.codex_version)?;
    let temporary = tempfile::tempdir().context("could not create installer workspace")?;
    if usize::from(options.runtime_package.is_some())
        + usize::from(options.remote_codex.is_some())
        + usize::from(options.local_download)
        > 1
    {
        bail!("runtime package, local download, and remote Codex are mutually exclusive");
    }

    let root = format!("{}/.local/lib/codex-shuttle", facts.home);
    let reusable = if let Some(remote_codex) = options.remote_codex.as_deref() {
        Some(validate_remote_executor(
            profile,
            &root,
            &profile.codex_version,
            remote_codex,
        )?)
    } else {
        None
    };

    let runtime_name = format!("cxs-runtime-codex-{version}-{target}.tar.gz");
    let remote_package = format!("/tmp/cxs-{}-codex.tar.gz", profile.name);
    let remote_package_partial = format!("{remote_package}.partial");
    let release_tag = shuttle_release_tag();
    let runtime_url = release_asset_url(&options.shuttle_release_base, release_tag, &runtime_name);
    let runtime_manifest_url =
        release_asset_url(&options.shuttle_release_base, release_tag, "SHA256SUMS");
    let package_source = if reusable.is_some() {
        None
    } else if let Some(path) = &options.runtime_package {
        let path = canonical_file(path, "Shuttle runtime package")?;
        let digest = sha256_file(&path)?;
        Some(PackageSource::Upload {
            path,
            sha256: digest,
        })
    } else if options.local_download {
        Some(PackageSource::LocalRuntime {
            runtime_url,
            runtime_manifest_url,
            runtime_name,
            path: temporary.path().join("runtime.tar.gz"),
        })
    } else {
        Some(PackageSource::RemoteRuntime {
            runtime_url,
            runtime_manifest_url,
            runtime_name,
        })
    };

    let shim_name = format!("cxs-shim-{target}");
    let shim = match &options.shim {
        Some(path) => canonical_file(path, "Shuttle shim")?,
        None => download_shim(
            &temporary,
            release_tag,
            &shim_name,
            &options.shuttle_release_base,
        )?,
    };
    let shim_sha256 = sha256_file(&shim)?;

    let remote_shim = format!("/tmp/cxs-{}-shim", profile.name);
    let reusable_sha256 = reusable.as_ref().map(|executor| executor.sha256.clone());
    let (package_sha256, executor_source, executor_probe, exec_server) =
        if let Some(reusable) = reusable {
            upload(&profile.source_host, &shim, &remote_shim)?;
            (
                None,
                ExecutorSource::RemoteExisting,
                reusable.probe_path,
                reusable.installed_path,
            )
        } else {
            let package_source = package_source.context("Codex package source was not selected")?;
            let (package_result, shim_result) = transfer_install_artifacts(
                || transfer_package(&package_source, &profile.source_host, &remote_package),
                || upload(&profile.source_host, &shim, &remote_shim),
            );
            let transferred = match (package_result, shim_result) {
                (Ok(transferred), Ok(())) => transferred,
                (Err(package_error), Ok(())) => {
                    let _ = cleanup_remote_temp(&profile.source_host, &[&remote_shim]);
                    return Err(package_error.context("Codex package transfer failed"));
                }
                (Ok(_), Err(shim_error)) => {
                    let _ = cleanup_remote_temp(&profile.source_host, &[&remote_shim]);
                    return Err(shim_error.context(
                        "shim upload failed; the verified Codex package was retained for retry",
                    ));
                }
                (Err(package_error), Err(shim_error)) => {
                    let _ = cleanup_remote_temp(&profile.source_host, &[&remote_shim]);
                    return Err(package_error.context(format!(
                    "Codex package transfer and shim upload both failed; shim error: {shim_error:#}"
                )));
                }
            };
            let exec_server = format!("{root}/current/bin/cxs-runtime");
            (
                Some(transferred.sha256.clone()),
                ExecutorSource::ManagedRuntime,
                String::new(),
                exec_server,
            )
        };

    let token_file = format!("{root}/profiles/{}/token", profile.name);
    let codex_home = format!("{root}/profiles/{}/codex-home", profile.name);
    let agent_socket = format!("{root}/profiles/{}/agent.sock", profile.name);
    let config = ShimConfig {
        profile: &profile.name,
        codex_version: &profile.codex_version,
        control_socket: format!(
            "{root}/profiles/{}/app-{}.sock",
            profile.name,
            &shim_sha256[..12]
        ),
        agent_socket,
        token_file,
        exec_server: exec_server.clone(),
        exec_server_args: vec![
            "exec-server".to_owned(),
            "--disable".to_owned(),
            "plugins".to_owned(),
            "--disable".to_owned(),
            "remote_plugin".to_owned(),
            "--disable".to_owned(),
            "plugin_sharing".to_owned(),
            "--disable".to_owned(),
            "apps".to_owned(),
        ],
        exec_server_port: profile.remote_exec_port,
        codex_home,
        original_codex: format!("{root}/original-codex"),
    };
    let config_json = serde_json::to_string_pretty(&config)?;
    let config_sha256 = sha256_bytes(config_json.as_bytes());
    let artifact_identity = package_sha256
        .as_deref()
        .or(reusable_sha256.as_deref())
        .context("executor artifact identity was unavailable")?;
    let release_name = format!(
        "codex-{version}-r{REMOTE_LAYOUT_VERSION}-{}-{}-{}",
        &artifact_identity[..8],
        &shim_sha256[..8],
        &config_sha256[..8]
    );
    let metadata = RemoteInstall {
        layout_version: REMOTE_LAYOUT_VERSION,
        profile: profile.name.clone(),
        codex_version: profile.codex_version.clone(),
        runtime_version: (executor_source == ExecutorSource::ManagedRuntime)
            .then(|| format!("codex-cli {version}")),
        target: target.to_owned(),
        package_sha256: package_sha256.clone(),
        shim_sha256: shim_sha256.clone(),
        release: release_name.clone(),
        executor_source: Some(executor_source),
        executor_path: Some(exec_server.clone()),
        executor_sha256: reusable_sha256,
    };
    let metadata_json = serde_json::to_string_pretty(&metadata)?;
    let executor_version = metadata
        .runtime_version
        .as_deref()
        .unwrap_or(&profile.codex_version);
    let script = render_install_script(
        &profile.name,
        token,
        &profile.codex_version,
        executor_version,
        &root,
        &release_name,
        package_sha256.as_ref().map(|_| remote_package.as_str()),
        &executor_probe,
        &exec_server,
        &remote_shim,
        &config_json,
        &metadata_json,
    );
    let result = run_ssh_script(&profile.source_host, &script);
    let _ = cleanup_remote_temp(
        &profile.source_host,
        &[&remote_package, &remote_package_partial, &remote_shim],
    );
    result?;

    Ok(InstallRecord {
        target: target.to_owned(),
        remote_home: facts.home.clone(),
        remote_release: release_name,
        package_sha256,
        shim_sha256,
        executor_source,
        executor_path: exec_server,
    })
}

fn transfer_package(
    source: &PackageSource,
    host: &str,
    destination: &str,
) -> Result<PackageTransfer> {
    match source {
        PackageSource::Upload { path, sha256 } => {
            upload(host, path, destination)?;
            Ok(PackageTransfer {
                sha256: sha256.clone(),
            })
        }
        PackageSource::LocalRuntime {
            runtime_url,
            runtime_manifest_url,
            runtime_name,
            path,
        } => {
            let manifest = path.with_extension("SHA256SUMS");
            download(runtime_manifest_url, &manifest)?;
            let sha256 = checksum_from_file(&manifest, runtime_name)?;
            download(runtime_url, path)?;
            verify_sha256(path, &sha256)?;
            upload(host, path, destination)?;
            Ok(PackageTransfer { sha256 })
        }
        PackageSource::RemoteRuntime {
            runtime_url,
            runtime_manifest_url,
            runtime_name,
        } => remote_download_from_manifest(
            host,
            runtime_url,
            runtime_manifest_url,
            runtime_name,
            destination,
        )
        .map(|sha256| PackageTransfer { sha256 }),
    }
}

fn remote_download_from_manifest(
    host: &str,
    url: &str,
    manifest_url: &str,
    package_name: &str,
    destination: &str,
) -> Result<String> {
    let script = format!(
        r#"set -eu
umask 077
destination={destination}
temporary="$destination.partial"
manifest="$destination.SHA256SUMS"
trap 'unlink "$manifest" 2>/dev/null || true' EXIT HUP INT TERM
curl --http1.1 --fail --location --retry 3 --silent --show-error --output "$manifest" {manifest_url}
expected=$(awk -v wanted={package_name} '$2 == wanted || $2 == "*" wanted {{ print $1; exit }}' "$manifest")
test -n "$expected" || {{ echo "runtime checksum manifest did not contain the package" >&2; exit 1; }}
if test -f "$destination"; then
  actual=$(sha256sum "$destination" | awk '{{print $1}}')
  if test "$actual" = "$expected"; then printf '%s\n' "$expected"; exit 0; fi
  unlink "$destination"
fi
curl --http1.1 --fail --location --retry 3 --silent --show-error --continue-at - --output "$temporary" {url}
actual=$(sha256sum "$temporary" | awk '{{print $1}}')
test "$actual" = "$expected" || {{ unlink "$temporary"; echo "runtime package checksum mismatch" >&2; exit 1; }}
mv "$temporary" "$destination"
printf '%s\n' "$expected"
"#,
        destination = shell_quote(destination),
        manifest_url = shell_quote(manifest_url),
        package_name = shell_quote(package_name),
        url = shell_quote(url),
    );
    let output = run_ssh_script_with_output(host, &script)
        .context("could not download the verified Shuttle runtime package")?;
    let digest = String::from_utf8(output)
        .context("remote runtime checksum output was not UTF-8")?
        .trim()
        .to_ascii_lowercase();
    validate_sha256(&digest)?;
    Ok(digest)
}

fn validate_remote_executor(
    profile: &Profile,
    root: &str,
    expected_version: &str,
    path: &str,
) -> Result<ReusableExecutor> {
    if !path.starts_with('/') || path.contains(['\n', '\r', '\0']) {
        bail!("--remote-codex must be an absolute remote path without control characters");
    }
    let script = render_remote_executor_probe_script(root, expected_version, path);
    let mut child = Command::new("ssh")
        .args(["-T", "-o", "BatchMode=yes", &profile.source_host, "sh -s"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| {
            format!(
                "could not validate the remote Codex executor on {}",
                profile.source_host
            )
        })?;
    child
        .stdin
        .take()
        .context("remote Codex probe stdin was unavailable")?
        .write_all(script.as_bytes())?;
    let output = child.wait_with_output()?;
    if output.status.code() == Some(42) {
        bail!("remote Codex at {path} is missing, has the wrong version, or lacks exec-server");
    }
    if !output.status.success() {
        bail!(
            "remote Codex reuse probe failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    parse_reusable_executor(&output.stdout)
}

fn render_remote_executor_probe_script(root: &str, expected_version: &str, path: &str) -> String {
    format!(
        r#"set -eu
root={root}
expected={expected}
candidate={path}
managed="$root/current/cxs-shim"
managed_resolved=$(readlink -f "$managed" 2>/dev/null || printf '%s' "$managed")
test -x "$candidate" || exit 42
resolved=$(readlink -f "$candidate" 2>/dev/null || printf '%s' "$candidate")
test "$resolved" != "$managed_resolved" || exit 42
actual=$("$candidate" --version 2>/dev/null || true)
test "$actual" = "$expected" || exit 42
"$candidate" exec-server --help >/dev/null 2>&1 || exit 42
digest=$(sha256sum "$resolved" | awk '{{print $1}}')
installed="$resolved"
if test "$resolved" = "$HOME/.local/bin/codex"; then
  installed="$root/original-codex"
fi
printf '%s\n%s\n%s\n' "$resolved" "$installed" "$digest"
"#,
        root = shell_quote(root),
        expected = shell_quote(expected_version),
        path = shell_quote(path),
    )
}

fn parse_reusable_executor(output: &[u8]) -> Result<ReusableExecutor> {
    let output = String::from_utf8(output.to_vec())
        .context("remote Codex reuse probe returned non-UTF-8 output")?;
    let mut lines = output.lines();
    let probe_path = lines
        .next()
        .context("remote Codex probe omitted its path")?;
    let installed_path = lines
        .next()
        .context("remote Codex probe omitted its installed path")?;
    let sha256 = lines
        .next()
        .context("remote Codex probe omitted its checksum")?
        .to_ascii_lowercase();
    if !probe_path.starts_with('/') || !installed_path.starts_with('/') {
        bail!("remote Codex probe returned a non-absolute path");
    }
    validate_sha256(&sha256)?;
    Ok(ReusableExecutor {
        probe_path: probe_path.to_owned(),
        installed_path: installed_path.to_owned(),
        sha256,
    })
}

fn transfer_install_artifacts<P, S>(package: P, shim: S) -> (Result<PackageTransfer>, Result<()>)
where
    P: FnOnce() -> Result<PackageTransfer> + Send,
    S: FnOnce() -> Result<()> + Send,
{
    thread::scope(|scope| {
        let package = scope.spawn(package);
        let shim = scope.spawn(shim);
        let package = package
            .join()
            .map_err(|_| anyhow::anyhow!("Codex package transfer thread panicked"))
            .and_then(|result| result);
        let shim = shim
            .join()
            .map_err(|_| anyhow::anyhow!("shim upload thread panicked"))
            .and_then(|result| result);
        (package, shim)
    })
}

pub fn inspect(profile: &Profile) -> Result<RemoteInstall> {
    let output = ssh_output(
        &profile.source_host,
        "set -eu; cat \"$HOME/.config/codex-shuttle/install.json\"; printf '\\n'; \"$HOME/.local/bin/codex\" --version >&2; \"${SHELL:-/bin/sh}\" -l -i -c 'command -v codex >/dev/null' 2>/dev/null",
    )?;
    serde_json::from_slice(&output).context("remote install metadata is invalid")
}

pub fn verify_executor(profile: &Profile, install: &RemoteInstall) -> Result<()> {
    let path = install.executor_path.clone().unwrap_or_else(|| {
        format!(
            "$HOME/.local/lib/codex-shuttle/releases/{}/bin/codex",
            install.release
        )
    });
    let resolved_path = if path.starts_with("$HOME/") {
        format!(
            "\"$HOME\"/{}",
            shell_quote(path.trim_start_matches("$HOME/"))
        )
    } else {
        shell_quote(&path)
    };
    let checksum = install.executor_sha256.as_ref().map_or_else(
        String::new,
        |expected| {
            format!(
                "actual=$(sha256sum \"$executor\" | awk '{{print $1}}')\ntest \"$actual\" = {} || {{ echo \"remote executor checksum changed\" >&2; exit 1; }}",
                shell_quote(expected)
            )
        },
    );
    let script = format!(
        r#"set -eu
executor={resolved_path}
expected_version={expected_version}
test -x "$executor" || {{ echo "remote executor is unavailable: $executor" >&2; exit 1; }}
actual_version=$("$executor" --version)
test "$actual_version" = "$expected_version" || {{ echo "remote executor version changed: $actual_version" >&2; exit 1; }}
"$executor" exec-server --help >/dev/null 2>&1 || {{ echo "remote executor lacks exec-server" >&2; exit 1; }}
{checksum}
"#,
        expected_version = shell_quote(
            install
                .runtime_version
                .as_deref()
                .unwrap_or(&install.codex_version),
        ),
    );
    run_ssh_script(&profile.source_host, &script).context("remote executor verification failed")
}

pub fn rollback(profile: &Profile) -> Result<RemoteInstall> {
    let script = r#"set -eu
umask 077
root="$HOME/.local/lib/codex-shuttle"
previous="$root/previous"
current="$root/current"
test -L "$previous" || { echo "no previous Shuttle release" >&2; exit 1; }
old_current=$(readlink "$current")
new_current=$(readlink "$previous")
test -x "$new_current/cxs-shim"
if test -f "$new_current/executor.path"; then
  executor=$(cat "$new_current/executor.path")
else
  executor="$new_current/bin/codex"
fi
test -x "$executor" || { echo "previous release executor is unavailable: $executor" >&2; exit 1; }
"$executor" exec-server --help >/dev/null 2>&1 || { echo "previous release executor lacks exec-server" >&2; exit 1; }
ln -sfn "$new_current" "$current"
ln -sfn "$old_current" "$previous"
cp "$new_current/install.json" "$HOME/.config/codex-shuttle/install.json"
cp "$new_current/shim.json" "$HOME/.config/codex-shuttle/shim.json"
cat "$HOME/.config/codex-shuttle/install.json"
"$HOME/.local/bin/codex" --version >&2
"#;
    let output = run_ssh_script_with_output(&profile.source_host, script)?;
    serde_json::from_slice(&output).context("rolled-back install metadata is invalid")
}

pub fn uninstall(profile: &Profile, purge_releases: bool) -> Result<()> {
    let purge = if purge_releases { "1" } else { "0" };
    let script = format!(
        r##"set -eu
umask 077
expected={expected}
owner_file="$HOME/.config/codex-shuttle/owner"
test -f "$owner_file" || {{ echo "Shuttle is not installed" >&2; exit 1; }}
owner=$(cat "$owner_file")
test "$owner" = "$expected" || {{ echo "remote install belongs to profile $owner" >&2; exit 1; }}
managed="$HOME/.local/lib/codex-shuttle/current/cxs-shim"
link="$HOME/.local/bin/codex"
if test -L "$link" && test "$(readlink "$link")" = "$managed"; then unlink "$link"; fi
original="$HOME/.local/lib/codex-shuttle/original-codex"
if test -e "$original" || test -L "$original"; then mv "$original" "$link"; fi
unlink "$HOME/.config/codex-shuttle/shim.json" 2>/dev/null || true
unlink "$HOME/.config/codex-shuttle/install.json" 2>/dev/null || true
unlink "$owner_file"
profile_dir="$HOME/.local/lib/codex-shuttle/profiles/$expected"
if test -d "$profile_dir"; then find "$profile_dir" -depth -delete; fi
case "${{SHELL:-/bin/sh}}" in
  */zsh) shell_profile="$HOME/.zprofile" ;;
  */bash|*/sh|*/dash) shell_profile="$HOME/.profile" ;;
  *) shell_profile="" ;;
esac
if test -n "$shell_profile" && test -f "$shell_profile"; then
  awk 'BEGIN {{ skip=0 }} $0 == "# >>> Codex Shuttle >>>" {{ skip=1; next }} $0 == "# <<< Codex Shuttle <<<" {{ skip=0; next }} !skip {{ print }}' "$shell_profile" > "$shell_profile.cxs-new"
  mv "$shell_profile.cxs-new" "$shell_profile"
fi
if test {purge} = 1; then
  root="$HOME/.local/lib/codex-shuttle"
  test "$root" = "$HOME/.local/lib/codex-shuttle"
  if test -d "$root"; then find "$root" -depth -delete; fi
fi
"##,
        expected = shell_quote(&profile.name),
    );
    run_ssh_script(&profile.source_host, &script)
}

fn download_shim(
    temporary: &TempDir,
    release_tag: &str,
    shim_name: &str,
    release_base: &str,
) -> Result<PathBuf> {
    let sums = temporary.path().join("SHA256SUMS");
    download(
        &release_asset_url(release_base, release_tag, "SHA256SUMS"),
        &sums,
    )
    .with_context(|| {
        "could not download the Shuttle shim manifest; for a source checkout, build a Linux shim and pass --shim"
    })?;
    let expected = checksum_from_file(&sums, shim_name)?;
    let shim = temporary.path().join(shim_name);
    download(
        &release_asset_url(release_base, release_tag, shim_name),
        &shim,
    )?;
    verify_sha256(&shim, &expected)?;
    Ok(shim)
}

fn download(url: &str, destination: &Path) -> Result<()> {
    let status = Command::new("curl")
        .args([
            "--fail",
            "--location",
            "--retry",
            "3",
            "--silent",
            "--show-error",
        ])
        .arg("--output")
        .arg(destination)
        .arg(url)
        .status()
        .with_context(|| "could not run curl; install curl or provide local artifacts")?;
    if !status.success() {
        bail!("download failed: {url}");
    }
    Ok(())
}

fn upload(host: &str, source: &Path, remote: &str) -> Result<()> {
    let file = File::open(source)
        .with_context(|| format!("could not open upload source {}", source.display()))?;
    let command = format!("umask 077; cat > {}", shell_quote(remote));
    let status = Command::new("ssh")
        .args(["-T", "-o", "BatchMode=yes", host, &command])
        .stdin(Stdio::from(file))
        .status()
        .with_context(|| format!("could not upload {} to {host}", source.display()))?;
    if !status.success() {
        bail!("upload to {host} failed");
    }
    Ok(())
}

fn cleanup_remote_temp(host: &str, paths: &[&str]) -> Result<()> {
    let mut command = String::from("set -eu;");
    for path in paths {
        command.push_str(" unlink ");
        command.push_str(&shell_quote(path));
        command.push_str(" 2>/dev/null || true;");
    }
    let status = Command::new("ssh")
        .args(["-T", "-o", "BatchMode=yes", host, &command])
        .status()?;
    if !status.success() {
        bail!("could not clean remote installer files");
    }
    Ok(())
}

fn run_ssh_script(host: &str, script: &str) -> Result<()> {
    run_ssh_script_with_output(host, script).map(|_| ())
}

fn run_ssh_script_with_output(host: &str, script: &str) -> Result<Vec<u8>> {
    let mut child = Command::new("ssh")
        .args(["-T", "-o", "BatchMode=yes", host, "sh -s"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .with_context(|| format!("could not start installer over SSH to {host}"))?;
    child
        .stdin
        .take()
        .context("SSH installer stdin was unavailable")?
        .write_all(script.as_bytes())?;
    let output = child.wait_with_output()?;
    if !output.status.success() {
        bail!(
            "remote installer failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

fn ssh_output(host: &str, command: &str) -> Result<Vec<u8>> {
    let output = Command::new("ssh")
        .args(["-T", "-o", "BatchMode=yes", host, command])
        .output()
        .with_context(|| format!("could not run remote check on {host}"))?;
    if !output.status.success() {
        bail!(
            "remote check failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    Ok(output.stdout)
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn render_install_script(
    profile: &str,
    token: &str,
    _codex_version: &str,
    executor_version: &str,
    root: &str,
    release_name: &str,
    package: Option<&str>,
    executor_probe: &str,
    executor_installed: &str,
    shim: &str,
    config_json: &str,
    metadata_json: &str,
) -> String {
    let prepare_executor = package.map_or_else(
        || {
            format!(
                "executor_probe={}\ntest -x \"$executor_probe\" || {{ echo \"reusable Codex executor disappeared\" >&2; exit 1; }}",
                shell_quote(executor_probe)
            )
        },
        |package| {
            format!(
                "package={}\ntar -xzf \"$package\" -C \"$stage\"\nexecutor_probe=\"$stage/bin/cxs-runtime\"\ntest -x \"$executor_probe\" || {{ echo \"runtime package has no executable bin/cxs-runtime\" >&2; exit 1; }}",
                shell_quote(package),
            )
        },
    );
    format!(
        r#"set -eu
umask 077
profile={profile}
token={token}
expected_version={executor_version}
root={root}
release="$root/releases/{release_name}"
stage="$root/.stage-{release_name}"
executor_installed={executor_installed}
shim={shim}
config_dir="$HOME/.config/codex-shuttle"
owner_file="$config_dir/owner"
if test -f "$owner_file"; then
  owner=$(cat "$owner_file")
  test "$owner" = "$profile" || {{ echo "remote Shuttle install belongs to profile $owner" >&2; exit 1; }}
fi
case "${{SHELL:-/bin/sh}}" in
  */zsh) shell_profile="$HOME/.zprofile" ;;
  */bash|*/sh|*/dash) shell_profile="$HOME/.profile" ;;
  *) echo "unsupported remote login shell ${{SHELL:-unknown}}" >&2; exit 1 ;;
esac
mkdir -p "$root/releases" "$root/profiles/$profile/codex-home" "$config_dir" "$HOME/.local/bin"
if test -e "$root/current" && test ! -L "$root/current"; then echo "$root/current is not a managed symlink" >&2; exit 1; fi
if test -e "$stage"; then find "$stage" -depth -delete; fi
mkdir "$stage"
{prepare_executor}
install -m 0700 "$shim" "$stage/cxs-shim"
actual_version=$("$executor_probe" --version)
test "$actual_version" = "$expected_version" || {{ echo "Codex package version mismatch: $actual_version" >&2; exit 1; }}
"$executor_probe" exec-server --help >/dev/null 2>&1 || {{ echo "Codex executor has no exec-server command" >&2; exit 1; }}
cat > "$stage/shim.json" <<'CXS_CONFIG'
{config_json}
CXS_CONFIG
cat > "$stage/install.json" <<'CXS_METADATA'
{metadata_json}
CXS_METADATA
printf '%s\n' "$executor_installed" > "$stage/executor.path"
if test -e "$release"; then find "$stage" -depth -delete; else mv "$stage" "$release"; fi
printf '%s\n' "$token" > "$root/profiles/$profile/token"
chmod 0600 "$root/profiles/$profile/token"
if test -L "$root/current"; then
  old=$(readlink "$root/current")
  if test ! -f "$old/executor.path"; then
    old_executor=""
    if test -x "$old/bin/codex"; then
      old_executor="$old/bin/codex"
    elif test -x "$old/cxs-shim" && test -f "$old/shim.json"; then
      old_version=$(CXS_SHIM_CONFIG="$old/shim.json" "$old/cxs-shim" --version 2>/dev/null || true)
      if test "$old_version" = "$expected_version" && test -x "$executor_installed"; then
        old_executor="$executor_installed"
      fi
    fi
    if test -n "$old_executor"; then
      printf '%s\n' "$old_executor" > "$old/executor.path"
      chmod 0600 "$old/executor.path"
    fi
  fi
  if test "$old" != "$release"; then
    ln -sfn "$old" "$root/previous"
  fi
fi
ln -sfn "$release" "$root/current"
cp "$release/shim.json" "$config_dir/shim.json"
cp "$release/install.json" "$config_dir/install.json"
printf '%s\n' "$profile" > "$owner_file"
chmod 0600 "$release/executor.path" "$config_dir/shim.json" "$config_dir/install.json" "$owner_file"
touch "$shell_profile"
if ! grep -Fqx '# >>> Codex Shuttle >>>' "$shell_profile"; then
  cat >> "$shell_profile" <<'CXS_PATH'

# >>> Codex Shuttle >>>
case ":$PATH:" in
  *":$HOME/.local/bin:"*) ;;
  *) PATH="$HOME/.local/bin:$PATH" ;;
esac
export PATH
# <<< Codex Shuttle <<<
CXS_PATH
fi
managed="$root/current/cxs-shim"
link="$HOME/.local/bin/codex"
if test -e "$link" || test -L "$link"; then
  if test ! -L "$link" || test "$(readlink "$link")" != "$managed"; then
    test ! -e "$root/original-codex" && test ! -L "$root/original-codex" || {{ echo "refusing to replace an existing saved codex" >&2; exit 1; }}
    mv "$link" "$root/original-codex"
  fi
fi
ln -sfn "$root/current/cxs-shim" "$HOME/.local/bin/codex"
test -x "$executor_installed" || {{ echo "configured Codex executor is not executable after activation" >&2; exit 1; }}
"$HOME/.local/bin/codex" --version
"#,
        profile = shell_quote(profile),
        token = shell_quote(token),
        executor_version = shell_quote(executor_version),
        root = shell_quote(root),
        release_name = release_name,
        executor_installed = shell_quote(executor_installed),
        shim = shell_quote(shim),
        prepare_executor = prepare_executor,
    )
}

fn canonical_file(path: &Path, label: &str) -> Result<PathBuf> {
    let path = fs::canonicalize(path)
        .with_context(|| format!("could not find {label} {}", path.display()))?;
    if !path.is_file() {
        bail!("{label} is not a regular file: {}", path.display());
    }
    Ok(path)
}

fn checksum_from_file(path: &Path, filename: &str) -> Result<String> {
    let contents = fs::read_to_string(path)?;
    for line in contents.lines() {
        let mut fields = line.split_ascii_whitespace();
        let Some(hash) = fields.next() else { continue };
        let Some(name) = fields.next() else { continue };
        if name.trim_start_matches('*') == filename {
            validate_sha256(hash)?;
            return Ok(hash.to_ascii_lowercase());
        }
    }
    bail!("checksum manifest did not contain {filename}")
}

fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = vec![0_u8; 128 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

fn sha256_bytes(bytes: &[u8]) -> String {
    hex::encode(Sha256::digest(bytes))
}

fn verify_sha256(path: &Path, expected: &str) -> Result<()> {
    let actual = sha256_file(path)?;
    if actual != expected.to_ascii_lowercase() {
        bail!(
            "SHA-256 mismatch for {}: expected {expected}, got {actual}",
            path.display()
        );
    }
    Ok(())
}

fn validate_sha256(value: &str) -> Result<()> {
    if value.len() != 64 || !value.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        bail!("invalid SHA-256 digest in manifest");
    }
    Ok(())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::sync::mpsc;
    use std::time::Duration;

    use super::*;

    #[test]
    fn parses_and_rejects_codex_versions() {
        assert_eq!(
            codex_release_version("codex-cli 0.147.0").unwrap(),
            "0.147.0"
        );
        assert_eq!(
            codex_source_version("codex-cli 0.147.0-alpha.6.5").unwrap(),
            "0.147.0"
        );
        assert!(codex_release_version("codex-cli ../../bad").is_err());
    }

    #[test]
    fn maps_supported_linux_targets() {
        let facts = RemoteFacts {
            home: "/home/dev".to_owned(),
            kernel: "Linux".to_owned(),
            arch: "aarch64".to_owned(),
        };
        assert_eq!(linux_target(&facts).unwrap(), "aarch64-unknown-linux-musl");
    }

    #[test]
    fn quotes_shell_values() {
        assert_eq!(shell_quote("a'b"), "'a'\"'\"'b'");
    }

    #[test]
    fn binds_assets_to_one_release_tag() {
        assert_eq!(
            release_asset_url(
                "https://example.invalid/releases/download/",
                "v0.1.0-codex.0.147.0",
                "SHA256SUMS"
            ),
            "https://example.invalid/releases/download/v0.1.0-codex.0.147.0/SHA256SUMS"
        );
    }

    #[test]
    fn reads_checksum_manifest() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("sums");
        fs::write(&path, format!("{}  artifact\n", "a".repeat(64)))?;
        assert_eq!(checksum_from_file(&path, "artifact")?, "a".repeat(64));
        Ok(())
    }

    #[test]
    fn transfers_package_and_shim_concurrently() {
        let (package_started_tx, package_started_rx) = mpsc::channel();
        let (shim_started_tx, shim_started_rx) = mpsc::channel();
        let (package, shim) = transfer_install_artifacts(
            move || {
                package_started_tx.send(())?;
                shim_started_rx.recv_timeout(Duration::from_secs(1))?;
                Ok(PackageTransfer {
                    sha256: "a".repeat(64),
                })
            },
            move || {
                shim_started_tx.send(())?;
                package_started_rx.recv_timeout(Duration::from_secs(1))?;
                Ok(())
            },
        );
        let package = package.unwrap();
        assert_eq!(package.sha256, "a".repeat(64));
        shim.unwrap();
    }

    #[test]
    fn reads_legacy_install_metadata() -> Result<()> {
        let metadata: RemoteInstall = serde_json::from_str(&format!(
            r#"{{
  "profile": "gpu",
  "codex_version": "codex-cli 0.147.0",
  "target": "aarch64-unknown-linux-musl",
  "package_sha256": "{}",
  "shim_sha256": "{}",
  "release": "legacy"
}}"#,
            "a".repeat(64),
            "b".repeat(64)
        ))?;
        assert_eq!(metadata.package_sha256.as_deref(), Some(&*"a".repeat(64)));
        assert_eq!(metadata.layout_version, 1);
        assert_eq!(metadata.executor_source, None);
        Ok(())
    }

    #[test]
    fn install_script_can_reuse_an_existing_executor() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let home = directory.path().join("home");
        let executor = directory.path().join("remote-codex");
        fs::write(&executor, "#!/bin/sh\nprintf 'codex-cli 0.147.0\\n'\n")?;
        fs::set_permissions(&executor, fs::Permissions::from_mode(0o700))?;
        let shim = directory.path().join("cxs-shim");
        fs::write(&shim, "#!/bin/sh\nprintf 'codex-cli 0.147.0\\n'\n")?;
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o700))?;
        let root = home.join(".local/lib/codex-shuttle");
        let script = render_install_script(
            "gpu",
            &"a".repeat(64),
            "codex-cli 0.147.0",
            "codex-cli 0.147.0",
            root.to_str().unwrap(),
            "codex-0.147.0-reuse",
            None,
            executor.to_str().unwrap(),
            executor.to_str().unwrap(),
            shim.to_str().unwrap(),
            "{\"profile\":\"gpu\"}",
            "{\"profile\":\"gpu\"}",
        );
        let output = Command::new("sh")
            .env("HOME", &home)
            .arg("-c")
            .arg(script)
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(!root.join("current/bin/codex").exists());
        assert!(executor.is_file());
        Ok(())
    }

    #[test]
    fn install_script_accepts_a_minimal_runtime_package() -> Result<()> {
        let directory = tempfile::tempdir()?;
        let home = directory.path().join("home");
        let package_root = directory.path().join("runtime-package");
        let package_bin = package_root.join("bin");
        fs::create_dir_all(&package_bin)?;
        let runtime = package_bin.join("cxs-runtime");
        fs::write(&runtime, "#!/bin/sh\nprintf 'codex-cli 0.147.0\\n'\n")?;
        fs::set_permissions(&runtime, fs::Permissions::from_mode(0o700))?;
        let package = directory.path().join("runtime.tar.gz");
        assert!(
            Command::new("tar")
                .args(["-czf"])
                .arg(&package)
                .arg("-C")
                .arg(&package_root)
                .arg(".")
                .status()?
                .success()
        );
        let shim = directory.path().join("cxs-shim");
        fs::write(&shim, "#!/bin/sh\nprintf 'codex-cli 0.147.0\\n'\n")?;
        fs::set_permissions(&shim, fs::Permissions::from_mode(0o700))?;
        let root = home.join(".local/lib/codex-shuttle");
        let release = "codex-0.147.0-runtime";
        let executor = format!("{}/current/bin/cxs-runtime", root.display());
        let script = render_install_script(
            "gpu",
            &"a".repeat(64),
            "codex-cli 0.147.0",
            "codex-cli 0.147.0",
            root.to_str().unwrap(),
            release,
            Some(package.to_str().unwrap()),
            "",
            &executor,
            shim.to_str().unwrap(),
            "{\"profile\":\"gpu\"}",
            "{\"profile\":\"gpu\"}",
        );
        let output = Command::new("sh")
            .env("HOME", &home)
            .arg("-c")
            .arg(script)
            .output()?;
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert!(root.join("current/bin/cxs-runtime").is_file());
        assert_eq!(
            fs::read_to_string(root.join(format!("releases/{release}/executor.path")))?.trim(),
            executor
        );
        Ok(())
    }
}
