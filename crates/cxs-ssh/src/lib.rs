use std::collections::BTreeMap;
use std::fs::{self, File};
use std::io::{BufReader, BufWriter, Write as IoWrite};
use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, bail};
use cxs_core::{AppPaths, Profile, validate_host_alias};
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;

const INCLUDE_LINE: &str = "Include ~/.ssh/codex-shuttle.conf";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SshSnapshot {
    pub source_host: String,
    pub values: BTreeMap<String, Vec<String>>,
    #[serde(default)]
    pub inherited_values: BTreeMap<String, Vec<String>>,
}

impl SshSnapshot {
    pub fn resolve(source_host: &str) -> Result<Self> {
        validate_host_alias(source_host)?;
        let values = resolve_ssh_values(source_host)?;
        for required in ["hostname", "user", "port"] {
            if first_value(&values, required).is_none() {
                bail!("ssh -G output did not include '{required}'");
            }
        }
        // Resolve a name that should match only generic Host blocks. The
        // generated alias inherits those same blocks, so only source-specific
        // differences need to be repeated in the managed file. Pin the probe's
        // connection identity to the source values so tokens such as %C expand
        // identically without hiding a source-specific ControlPath override.
        let inherited_values = resolve_inherited_ssh_values(&values)?;
        Ok(Self {
            source_host: source_host.to_owned(),
            values,
            inherited_values,
        })
    }

    pub fn save(&self, profile_directory: &Path) -> Result<()> {
        let destination = profile_directory.join("ssh-snapshot.json");
        atomic_json_write(&destination, self)
    }

    pub fn load(profile_directory: &Path) -> Result<Self> {
        let path = profile_directory.join("ssh-snapshot.json");
        let file =
            File::open(&path).with_context(|| format!("could not open {}", path.display()))?;
        serde_json::from_reader(BufReader::new(file))
            .with_context(|| format!("could not parse {}", path.display()))
    }

    #[must_use]
    pub fn value(&self, key: &str) -> Option<&str> {
        first_value(&self.values, key)
    }
}

pub fn test_connection(source_host: &str) -> Result<()> {
    validate_host_alias(source_host)?;
    let status = Command::new("ssh")
        .args([
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            source_host,
            "true",
        ])
        .status()
        .with_context(|| format!("could not run ssh for host '{source_host}'"))?;
    if !status.success() {
        bail!("non-interactive SSH test failed for '{source_host}'");
    }
    Ok(())
}

pub fn query_remote(source_host: &str) -> Result<RemoteFacts> {
    validate_host_alias(source_host)?;
    let script = "printf 'home=%s\\n' \"$HOME\"; printf 'kernel=%s\\n' \"$(uname -s)\"; printf 'arch=%s\\n' \"$(uname -m)\"";
    let output = Command::new("ssh")
        .args([
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            source_host,
            script,
        ])
        .output()
        .with_context(|| format!("could not query SSH host '{source_host}'"))?;
    if !output.status.success() {
        bail!(
            "remote prerequisite query failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }
    let output = String::from_utf8(output.stdout)
        .context("remote prerequisite query returned non-UTF-8 output")?;
    let mut facts = BTreeMap::new();
    for line in output.lines() {
        if let Some((key, value)) = line.split_once('=') {
            facts.insert(key.to_owned(), value.to_owned());
        }
    }
    let home = facts
        .remove("home")
        .context("remote query did not return HOME")?;
    let kernel = facts
        .remove("kernel")
        .context("remote query did not return uname -s")?;
    let arch = facts
        .remove("arch")
        .context("remote query did not return uname -m")?;
    Ok(RemoteFacts { home, kernel, arch })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemoteFacts {
    pub home: String,
    pub kernel: String,
    pub arch: String,
}

pub fn ensure_managed_include(paths: &AppPaths) -> Result<()> {
    let ssh_directory = paths
        .ssh_config
        .parent()
        .context("SSH config path has no parent directory")?;
    fs::create_dir_all(ssh_directory)?;
    fs::set_permissions(ssh_directory, fs::Permissions::from_mode(0o700))?;

    let existing = if paths.ssh_config.exists() {
        fs::read_to_string(&paths.ssh_config)
            .with_context(|| format!("could not read {}", paths.ssh_config.display()))?
    } else {
        String::new()
    };
    if existing.lines().any(|line| line.trim() == INCLUDE_LINE) {
        return Ok(());
    }

    let mut updated = String::new();
    updated.push_str(INCLUDE_LINE);
    updated.push_str("\n\n");
    updated.push_str(&existing);
    atomic_text_write(&paths.ssh_config, &updated, 0o600)
}

pub fn rewrite_managed_config(
    destination: &Path,
    profiles: &[(Profile, SshSnapshot)],
) -> Result<()> {
    let mut output = String::from("# Managed by Codex Shuttle. Manual edits will be replaced.\n\n");
    for (profile, snapshot) in profiles {
        output.push_str(&render_host(profile, snapshot)?);
        output.push('\n');
    }
    atomic_text_write(destination, &output, 0o600)
}

pub fn render_host(profile: &Profile, snapshot: &SshSnapshot) -> Result<String> {
    if profile.source_host != snapshot.source_host {
        bail!("profile source host does not match its SSH snapshot");
    }
    let hostname = snapshot
        .value("hostname")
        .context("SSH snapshot has no hostname")?;
    let user = snapshot.value("user").context("SSH snapshot has no user")?;
    let port = snapshot.value("port").context("SSH snapshot has no port")?;

    let mut output = format!(
        "# profile={} source={} status={}\nHost {}\n  HostName {}\n  User {}\n  Port {}\n",
        profile.name,
        profile.source_host,
        profile.status,
        profile.app_alias,
        quote_value(hostname)?,
        quote_value(user)?,
        quote_value(port)?
    );

    for key in [
        "addressfamily",
        "bindaddress",
        "canonicalizehostname",
        "compression",
        "connectionattempts",
        "connecttimeout",
        "controlmaster",
        "controlpath",
        "controlpersist",
        "forwardagent",
        "hostkeyalias",
        "identitiesonly",
        "identityagent",
        "ipqos",
        "proxyjump",
        "serveralivecountmax",
        "serveraliveinterval",
        "stricthostkeychecking",
        "userknownhostsfile",
    ] {
        append_source_values(&mut output, snapshot, key)?;
    }
    append_source_values(&mut output, snapshot, "identityfile")?;

    if let Some(proxy_command) = snapshot.value("proxycommand")
        && proxy_command != "none"
    {
        reject_newline(proxy_command)?;
        output.push_str("  ProxyCommand ");
        output.push_str(proxy_command);
        output.push('\n');
    }

    output.push_str("  RequestTTY no\n");
    Ok(output)
}

fn parse_ssh_g(text: &str) -> Result<BTreeMap<String, Vec<String>>> {
    let mut values: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (index, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Some((key, value)) = line.split_once(char::is_whitespace) else {
            bail!("malformed ssh -G line {}", index + 1);
        };
        let value = value.trim();
        reject_newline(value)?;
        values
            .entry(key.to_ascii_lowercase())
            .or_default()
            .push(value.to_owned());
    }
    Ok(values)
}

fn resolve_ssh_values(host: &str) -> Result<BTreeMap<String, Vec<String>>> {
    let output = Command::new("ssh")
        .args(["-G", host])
        .output()
        .with_context(|| "could not run 'ssh -G'; install OpenSSH and ensure ssh is on PATH")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("ssh -G {host} failed: {}", stderr.trim());
    }
    let text = String::from_utf8(output.stdout).context("ssh -G returned non-UTF-8 output")?;
    parse_ssh_g(&text)
}

fn resolve_inherited_ssh_values(
    source_values: &BTreeMap<String, Vec<String>>,
) -> Result<BTreeMap<String, Vec<String>>> {
    let hostname =
        first_value(source_values, "hostname").context("SSH snapshot has no hostname")?;
    let user = first_value(source_values, "user").context("SSH snapshot has no user")?;
    let port = first_value(source_values, "port").context("SSH snapshot has no port")?;
    let mut command = Command::new("ssh");
    command.args([
        "-G",
        "-o",
        &format!("HostName={hostname}"),
        "-o",
        &format!("User={user}"),
        "-o",
        &format!("Port={port}"),
    ]);
    if let Some(proxy_jump) = first_value(source_values, "proxyjump")
        && proxy_jump != "none"
    {
        command.args(["-o", &format!("ProxyJump={proxy_jump}")]);
    }
    let output = command
        .arg("cxs-inherited-probe.invalid")
        .output()
        .with_context(|| "could not run inherited 'ssh -G' probe")?;
    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("inherited ssh -G probe failed: {}", stderr.trim());
    }
    let text = String::from_utf8(output.stdout).context("ssh -G returned non-UTF-8 output")?;
    parse_ssh_g(&text)
}

fn first_value<'a>(values: &'a BTreeMap<String, Vec<String>>, key: &str) -> Option<&'a str> {
    values
        .get(key)
        .and_then(|items| items.first())
        .map(String::as_str)
}

fn append_source_values(output: &mut String, snapshot: &SshSnapshot, key: &str) -> Result<()> {
    let Some(values) = snapshot.values.get(key) else {
        return Ok(());
    };
    if snapshot.inherited_values.get(key) == Some(values) {
        return Ok(());
    }
    let source_values: Vec<&String> = if key == "identityfile" {
        let inherited = snapshot.inherited_values.get(key);
        values
            .iter()
            .filter(|value| inherited.is_none_or(|items| !items.contains(value)))
            .collect()
    } else {
        values.iter().collect()
    };
    let directive = directive_name(key);
    for value in source_values {
        if value == "none" && matches!(key, "identityfile" | "proxyjump") {
            continue;
        }
        output.push_str("  ");
        output.push_str(directive);
        output.push(' ');
        if matches!(key, "ipqos" | "userknownhostsfile") {
            // `ssh -G` emits these directives as a canonical list of arguments.
            // Quoting the complete value would incorrectly collapse the list.
            reject_newline(value)?;
            output.push_str(value);
        } else {
            output.push_str(&quote_value(value)?);
        }
        output.push('\n');
    }
    Ok(())
}

fn directive_name(key: &str) -> &'static str {
    match key {
        "addressfamily" => "AddressFamily",
        "bindaddress" => "BindAddress",
        "canonicalizehostname" => "CanonicalizeHostname",
        "compression" => "Compression",
        "connectionattempts" => "ConnectionAttempts",
        "connecttimeout" => "ConnectTimeout",
        "controlmaster" => "ControlMaster",
        "controlpath" => "ControlPath",
        "controlpersist" => "ControlPersist",
        "forwardagent" => "ForwardAgent",
        "hostkeyalias" => "HostKeyAlias",
        "identitiesonly" => "IdentitiesOnly",
        "identityagent" => "IdentityAgent",
        "identityfile" => "IdentityFile",
        "ipqos" => "IPQoS",
        "proxyjump" => "ProxyJump",
        "serveralivecountmax" => "ServerAliveCountMax",
        "serveraliveinterval" => "ServerAliveInterval",
        "stricthostkeychecking" => "StrictHostKeyChecking",
        "userknownhostsfile" => "UserKnownHostsFile",
        _ => unreachable!("directive whitelist is exhaustive"),
    }
}

fn quote_value(value: &str) -> Result<String> {
    reject_newline(value)?;
    if value
        .bytes()
        .all(|byte| !byte.is_ascii_whitespace() && !matches!(byte, b'"' | b'\\' | b'#'))
    {
        return Ok(value.to_owned());
    }
    Ok(format!(
        "\"{}\"",
        value.replace('\\', "\\\\").replace('"', "\\\"")
    ))
}

fn reject_newline(value: &str) -> Result<()> {
    if value
        .bytes()
        .any(|byte| matches!(byte, b'\n' | b'\r' | b'\0'))
    {
        bail!("SSH configuration value contains a forbidden control character");
    }
    Ok(())
}

fn atomic_json_write<T: Serialize>(destination: &Path, value: &T) -> Result<()> {
    let parent = destination.parent().context("destination has no parent")?;
    fs::create_dir_all(parent)?;
    let mut temporary = NamedTempFile::new_in(parent)?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(0o600))?;
    {
        let mut writer = BufWriter::new(temporary.as_file_mut());
        serde_json::to_writer_pretty(&mut writer, value)?;
        writer.write_all(b"\n")?;
        writer.flush()?;
    }
    temporary.as_file().sync_all()?;
    temporary
        .persist(destination)
        .map_err(|error| error.error)?;
    Ok(())
}

fn atomic_text_write(destination: &Path, value: &str, mode: u32) -> Result<()> {
    let parent = destination.parent().context("destination has no parent")?;
    fs::create_dir_all(parent)?;
    let mut temporary = NamedTempFile::new_in(parent)?;
    temporary
        .as_file()
        .set_permissions(fs::Permissions::from_mode(mode))?;
    temporary.write_all(value.as_bytes())?;
    temporary.as_file().sync_all()?;
    temporary
        .persist(destination)
        .map_err(|error| error.error)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use cxs_core::{PROFILE_SCHEMA_VERSION, ProfileStatus};

    use super::*;

    fn profile() -> Profile {
        Profile {
            schema_version: PROFILE_SCHEMA_VERSION,
            name: "gpu".to_owned(),
            source_host: "gpu-server".to_owned(),
            app_alias: "cxs-gpu".to_owned(),
            status: ProfileStatus::Prepared,
            codex_version: "codex-cli 0.147.0".to_owned(),
            local_socket: PathBuf::from("/tmp/local bridge.sock"),
            remote_socket: "/tmp/cxs-remote.sock".to_owned(),
            environment_id: "cxs-gpu".to_owned(),
            local_exec_port: 49_444,
            remote_exec_port: 49_445,
            token_file: PathBuf::from("/tmp/token"),
            codex_home: PathBuf::from("/tmp/codex"),
            installed_target: None,
            remote_home: None,
            remote_release: None,
            package_sha256: None,
            shim_sha256: None,
            executor_source: None,
            executor_path: None,
        }
    }

    #[test]
    fn parses_repeated_ssh_values() -> Result<()> {
        let parsed =
            parse_ssh_g("hostname example.com\nidentityfile ~/.ssh/a\nidentityfile ~/.ssh/b\n")?;
        assert_eq!(parsed["identityfile"].len(), 2);
        Ok(())
    }

    #[test]
    fn renders_managed_host() -> Result<()> {
        let snapshot = SshSnapshot {
            source_host: "gpu-server".to_owned(),
            values: parse_ssh_g(
                "hostname example.com\nuser dev\nport 22\nidentityfile ~/.ssh/id_ed25519\nproxycommand none\nuserknownhostsfile ~/.ssh/known_hosts ~/.ssh/known_hosts2\nipqos ef cs0\n",
            )?,
            inherited_values: BTreeMap::new(),
        };
        let rendered = render_host(&profile(), &snapshot)?;
        assert!(rendered.contains("Host cxs-gpu"));
        assert!(!rendered.contains("RemoteForward"));
        assert!(!rendered.contains("LocalForward"));
        assert!(!rendered.contains("ProxyCommand none"));
        assert!(rendered.contains("UserKnownHostsFile ~/.ssh/known_hosts ~/.ssh/known_hosts2"));
        assert!(rendered.contains("IPQoS ef cs0"));
        Ok(())
    }

    #[test]
    fn omits_values_inherited_by_the_generated_alias() -> Result<()> {
        let common = parse_ssh_g(
            "identityfile ~/.ssh/id_rsa\nidentityfile ~/.ssh/id_ed25519\ncontrolmaster auto\nserveraliveinterval 60\n",
        )?;
        let mut values = common.clone();
        values.insert("hostname".to_owned(), vec!["example.com".to_owned()]);
        values.insert("user".to_owned(), vec!["dev".to_owned()]);
        values.insert("port".to_owned(), vec!["22".to_owned()]);
        let snapshot = SshSnapshot {
            source_host: "gpu-server".to_owned(),
            values,
            inherited_values: common,
        };

        let rendered = render_host(&profile(), &snapshot)?;
        assert!(!rendered.contains("IdentityFile"));
        assert!(!rendered.contains("ControlMaster"));
        assert!(!rendered.contains("ServerAliveInterval"));
        assert!(rendered.contains("HostName example.com"));
        Ok(())
    }

    #[test]
    fn keeps_only_explicit_identity_files() -> Result<()> {
        let inherited =
            parse_ssh_g("identityfile ~/.ssh/id_rsa\nidentityfile ~/.ssh/id_ed25519\n")?;
        let mut values = inherited.clone();
        values
            .get_mut("identityfile")
            .expect("identity files exist")
            .insert(0, "~/.ssh/work_key".to_owned());
        values.insert("hostname".to_owned(), vec!["example.com".to_owned()]);
        values.insert("user".to_owned(), vec!["dev".to_owned()]);
        values.insert("port".to_owned(), vec!["22".to_owned()]);
        let snapshot = SshSnapshot {
            source_host: "gpu-server".to_owned(),
            values,
            inherited_values: inherited,
        };

        let rendered = render_host(&profile(), &snapshot)?;
        assert!(rendered.contains("IdentityFile ~/.ssh/work_key"));
        assert!(!rendered.contains("IdentityFile ~/.ssh/id_rsa"));
        assert!(!rendered.contains("IdentityFile ~/.ssh/id_ed25519"));
        Ok(())
    }

    #[test]
    fn omits_an_inherited_control_path_with_the_same_expansion() -> Result<()> {
        let inherited = parse_ssh_g("controlpath /tmp/probe-hash\n")?;
        let mut values = inherited.clone();
        values.insert("hostname".to_owned(), vec!["example.com".to_owned()]);
        values.insert("user".to_owned(), vec!["dev".to_owned()]);
        values.insert("port".to_owned(), vec!["22".to_owned()]);
        let snapshot = SshSnapshot {
            source_host: "gpu-server".to_owned(),
            values,
            inherited_values: inherited,
        };

        let rendered = render_host(&profile(), &snapshot)?;
        assert!(!rendered.contains("ControlPath"));
        Ok(())
    }

    #[test]
    fn retains_a_source_specific_control_path() -> Result<()> {
        let inherited = parse_ssh_g("controlpath /tmp/inherited-hash\n")?;
        let mut values = inherited.clone();
        values.insert(
            "controlpath".to_owned(),
            vec!["/tmp/source-specific".to_owned()],
        );
        values.insert("hostname".to_owned(), vec!["example.com".to_owned()]);
        values.insert("user".to_owned(), vec!["dev".to_owned()]);
        values.insert("port".to_owned(), vec!["22".to_owned()]);
        let snapshot = SshSnapshot {
            source_host: "gpu-server".to_owned(),
            values,
            inherited_values: inherited,
        };

        let rendered = render_host(&profile(), &snapshot)?;
        assert!(rendered.contains("ControlPath /tmp/source-specific"));
        Ok(())
    }
}
