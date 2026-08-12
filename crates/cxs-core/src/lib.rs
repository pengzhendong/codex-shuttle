use std::fs::{self, File, OpenOptions};
use std::io::{BufReader, BufWriter, Write};
use std::net::TcpListener;
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use fs4::fs_std::FileExt;
use serde::{Deserialize, Serialize};
use tempfile::NamedTempFile;
use thiserror::Error;

pub mod routing;

pub const PROFILE_SCHEMA_VERSION: u32 = 1;
pub const SHIM_PROTOCOL_VERSION: u32 = 1;
pub const SHIM_MAGIC: &str = "CXS1";
pub const AGENT_PROTOCOL_VERSION: u32 = 1;
pub const AGENT_MAGIC: &str = "CXS-AGENT1";

#[derive(Debug, Error, PartialEq, Eq)]
pub enum NameError {
    #[error("profile name must be 1-48 characters")]
    InvalidLength,
    #[error("profile name must start with an ASCII letter or digit")]
    InvalidStart,
    #[error("profile name may contain only ASCII letters, digits, '.', '_' and '-'")]
    InvalidCharacter,
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ProfileStatus {
    Prepared,
    Installed,
    Ready,
    Broken,
}

impl std::fmt::Display for ProfileStatus {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let value = match self {
            Self::Prepared => "prepared",
            Self::Installed => "installed",
            Self::Ready => "ready",
            Self::Broken => "broken",
        };
        formatter.write_str(value)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct Profile {
    pub schema_version: u32,
    pub name: String,
    pub source_host: String,
    pub app_alias: String,
    pub status: ProfileStatus,
    pub codex_version: String,
    pub local_socket: PathBuf,
    pub remote_socket: String,
    pub environment_id: String,
    pub local_exec_port: u16,
    pub remote_exec_port: u16,
    pub token_file: PathBuf,
    pub codex_home: PathBuf,
    #[serde(default)]
    pub installed_target: Option<String>,
    #[serde(default)]
    pub remote_home: Option<String>,
    #[serde(default)]
    pub remote_release: Option<String>,
    #[serde(default)]
    pub package_sha256: Option<String>,
    #[serde(default)]
    pub shim_sha256: Option<String>,
    #[serde(default)]
    pub executor_source: Option<String>,
    #[serde(default)]
    pub executor_path: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ShimHandshake {
    pub magic: String,
    pub protocol_version: u32,
    pub profile: String,
    pub token: String,
    #[serde(default)]
    pub app_server_args: Vec<String>,
    #[serde(default)]
    pub transport: ShimTransport,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentHandshake {
    pub magic: String,
    pub protocol_version: u32,
    pub profile: String,
    pub token: String,
}

#[derive(Debug, Clone, Copy, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ShimTransport {
    #[default]
    Jsonl,
    WebSocket,
}

#[derive(Debug, Clone)]
pub struct AppPaths {
    pub state_root: PathBuf,
    pub ssh_config: PathBuf,
    pub managed_ssh_config: PathBuf,
    pub default_codex_home: PathBuf,
}

impl AppPaths {
    pub fn discover() -> Result<Self> {
        let home =
            dirs::home_dir().context("could not determine the current user's home directory")?;
        let state_root = std::env::var_os("XDG_STATE_HOME")
            .map_or_else(|| home.join(".local/state"), PathBuf::from)
            .join("codex-shuttle");
        let ssh_dir = home.join(".ssh");
        Ok(Self {
            state_root,
            ssh_config: ssh_dir.join("config"),
            managed_ssh_config: ssh_dir.join("codex-shuttle.conf"),
            default_codex_home: home.join(".codex"),
        })
    }

    #[must_use]
    pub fn profile_dir(&self, name: &str) -> PathBuf {
        self.state_root.join("profiles").join(name)
    }

    #[must_use]
    pub fn bridge_pid(&self, name: &str) -> PathBuf {
        self.profile_dir(name).join("bridge.pid")
    }

    #[must_use]
    pub fn bridge_log(&self, name: &str) -> PathBuf {
        self.profile_dir(name).join("bridge.log")
    }
}

#[derive(Debug, Clone)]
pub struct ProfileStore {
    paths: AppPaths,
}

#[derive(Debug, Clone, Copy)]
pub enum OperationLockMode {
    Shared,
    Exclusive,
}

#[derive(Debug)]
pub struct OperationLock {
    file: File,
}

impl Drop for OperationLock {
    fn drop(&mut self) {
        let _ = FileExt::unlock(&self.file);
    }
}

impl ProfileStore {
    #[must_use]
    pub const fn new(paths: AppPaths) -> Self {
        Self { paths }
    }

    #[must_use]
    pub const fn paths(&self) -> &AppPaths {
        &self.paths
    }

    pub fn lock_profile(&self, name: &str, mode: OperationLockMode) -> Result<OperationLock> {
        validate_profile_name(name)?;
        let directory = self.paths.state_root.join("locks");
        create_private_dir(&directory)?;
        let path = directory.join(format!("{name}.lock"));
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .mode(0o600)
            .open(&path)
            .with_context(|| format!("could not open operation lock {}", path.display()))?;
        file.set_permissions(fs::Permissions::from_mode(0o600))?;
        let acquired = match mode {
            OperationLockMode::Shared => FileExt::try_lock_shared(&file)?,
            OperationLockMode::Exclusive => FileExt::try_lock_exclusive(&file)?,
        };
        if !acquired {
            bail!("another Codex Shuttle operation is already running for profile '{name}'");
        }
        Ok(OperationLock { file })
    }

    pub fn create_prepared(
        &self,
        name: &str,
        source_host: &str,
        codex_version: &str,
    ) -> Result<Profile> {
        validate_profile_name(name)?;
        validate_host_alias(source_host)?;

        let directory = self.paths.profile_dir(name);
        if directory.exists() {
            bail!("profile '{name}' already exists");
        }
        create_private_dir(&directory)?;

        let local_socket = directory.join("bridge.sock");
        ensure_unix_socket_path_is_short(&local_socket)?;
        let token_file = directory.join("bridge.token");
        write_secret(&token_file, &random_token()?)?;

        let profile = Profile {
            schema_version: PROFILE_SCHEMA_VERSION,
            name: name.to_owned(),
            source_host: source_host.to_owned(),
            app_alias: format!("cxs-{name}"),
            status: ProfileStatus::Prepared,
            codex_version: codex_version.to_owned(),
            local_socket,
            remote_socket: format!("/tmp/cxs-{}.sock", random_id(12)?),
            environment_id: format!("cxs-{name}"),
            local_exec_port: reserve_ephemeral_port()?,
            remote_exec_port: random_high_port()?,
            token_file,
            codex_home: self.paths.default_codex_home.clone(),
            installed_target: None,
            remote_home: None,
            remote_release: None,
            package_sha256: None,
            shim_sha256: None,
            executor_source: None,
            executor_path: None,
        };
        self.save(&profile)?;
        Ok(profile)
    }

    pub fn load(&self, name: &str) -> Result<Profile> {
        validate_profile_name(name)?;
        let path = self.paths.profile_dir(name).join("profile.json");
        let file = File::open(&path)
            .with_context(|| format!("could not open profile file {}", path.display()))?;
        let profile: Profile = serde_json::from_reader(BufReader::new(file))
            .with_context(|| format!("could not parse profile file {}", path.display()))?;
        if profile.schema_version != PROFILE_SCHEMA_VERSION {
            bail!(
                "profile '{}' uses unsupported schema version {}",
                profile.name,
                profile.schema_version
            );
        }
        if profile.name != name {
            bail!("profile file name does not match its directory");
        }
        Ok(profile)
    }

    pub fn save(&self, profile: &Profile) -> Result<()> {
        validate_profile_name(&profile.name)?;
        let directory = self.paths.profile_dir(&profile.name);
        create_private_dir(&directory)?;
        let destination = directory.join("profile.json");
        let mut temporary = NamedTempFile::new_in(&directory).with_context(|| {
            format!("could not create temporary file in {}", directory.display())
        })?;
        temporary
            .as_file()
            .set_permissions(fs::Permissions::from_mode(0o600))?;
        {
            let mut writer = BufWriter::new(temporary.as_file_mut());
            serde_json::to_writer_pretty(&mut writer, profile)?;
            writer.write_all(b"\n")?;
            writer.flush()?;
        }
        temporary.as_file_mut().sync_all()?;
        temporary
            .persist(&destination)
            .map_err(|error| error.error)
            .with_context(|| format!("could not replace {}", destination.display()))?;
        Ok(())
    }

    pub fn list(&self) -> Result<Vec<Profile>> {
        let root = self.paths.state_root.join("profiles");
        if !root.exists() {
            return Ok(Vec::new());
        }
        let mut profiles = Vec::new();
        for entry in
            fs::read_dir(&root).with_context(|| format!("could not read {}", root.display()))?
        {
            let entry = entry?;
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let Some(name) = entry.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            profiles.push(self.load(&name)?);
        }
        profiles.sort_by(|left, right| left.name.cmp(&right.name));
        Ok(profiles)
    }

    pub fn remove(&self, name: &str) -> Result<()> {
        validate_profile_name(name)?;
        let directory = self.paths.profile_dir(name);
        if !directory.exists() {
            bail!("profile '{name}' does not exist");
        }
        let metadata = fs::symlink_metadata(&directory)?;
        if !metadata.file_type().is_dir() || metadata.file_type().is_symlink() {
            bail!(
                "refusing to remove non-directory profile path {}",
                directory.display()
            );
        }
        fs::remove_dir_all(&directory)
            .with_context(|| format!("could not remove {}", directory.display()))
    }

    pub fn read_token(&self, profile: &Profile) -> Result<String> {
        let token = fs::read_to_string(&profile.token_file)
            .with_context(|| format!("could not read {}", profile.token_file.display()))?;
        let token = token.trim().to_owned();
        if token.len() != 64 || !token.bytes().all(|byte| byte.is_ascii_hexdigit()) {
            bail!("profile token is malformed");
        }
        Ok(token)
    }
}

pub fn validate_profile_name(name: &str) -> std::result::Result<(), NameError> {
    if name.is_empty() || name.len() > 48 {
        return Err(NameError::InvalidLength);
    }
    let mut bytes = name.bytes();
    let first = bytes.next().ok_or(NameError::InvalidLength)?;
    if !first.is_ascii_alphanumeric() {
        return Err(NameError::InvalidStart);
    }
    if !bytes.all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-')) {
        return Err(NameError::InvalidCharacter);
    }
    Ok(())
}

pub fn validate_host_alias(host: &str) -> Result<()> {
    if host.is_empty() || host.len() > 255 {
        bail!("SSH host alias must be 1-255 characters");
    }
    if host.starts_with('-')
        || host
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte == b'\0')
    {
        bail!("SSH host alias contains unsafe characters");
    }
    Ok(())
}

fn create_private_dir(path: &Path) -> Result<()> {
    fs::create_dir_all(path).with_context(|| format!("could not create {}", path.display()))?;
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn write_secret(path: &Path, contents: &str) -> Result<()> {
    let mut options = OpenOptions::new();
    options.write(true).create_new(true).mode(0o600);
    let mut file = options
        .open(path)
        .with_context(|| format!("could not create {}", path.display()))?;
    file.write_all(contents.as_bytes())?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    Ok(())
}

fn random_token() -> Result<String> {
    random_id(32)
}

fn random_id(byte_count: usize) -> Result<String> {
    let mut bytes = vec![0_u8; byte_count];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!("could not obtain secure random bytes: {error}"))?;
    Ok(hex::encode(bytes))
}

fn reserve_ephemeral_port() -> Result<u16> {
    let listener = TcpListener::bind(("127.0.0.1", 0))
        .context("could not reserve a local loopback port for Exec Server")?;
    Ok(listener.local_addr()?.port())
}

fn random_high_port() -> Result<u16> {
    let mut bytes = [0_u8; 2];
    getrandom::fill(&mut bytes)
        .map_err(|error| anyhow::anyhow!("could not obtain a random remote port: {error}"))?;
    let value = u16::from_be_bytes(bytes);
    Ok(30_000 + value % 30_000)
}

fn ensure_unix_socket_path_is_short(path: &Path) -> Result<()> {
    const CONSERVATIVE_LIMIT: usize = 100;
    let length = path.as_os_str().as_encoded_bytes().len();
    if length > CONSERVATIVE_LIMIT {
        bail!(
            "Unix socket path is too long ({length} bytes): {}. Set XDG_STATE_HOME to a shorter path",
            path.display()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_paths(root: &Path) -> AppPaths {
        AppPaths {
            state_root: root.join("state"),
            ssh_config: root.join("ssh/config"),
            managed_ssh_config: root.join("ssh/codex-shuttle.conf"),
            default_codex_home: root.join("codex"),
        }
    }

    #[test]
    fn validates_profile_names() {
        assert_eq!(validate_profile_name("gpu-1"), Ok(()));
        assert_eq!(validate_profile_name("_gpu"), Err(NameError::InvalidStart));
        assert_eq!(
            validate_profile_name("gpu/server"),
            Err(NameError::InvalidCharacter)
        );
    }

    #[test]
    fn profile_round_trip_and_remove() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let store = ProfileStore::new(test_paths(temporary.path()));
        let created = store.create_prepared("gpu", "gpu-server", "codex-cli 0.147.0")?;
        assert_eq!(created.status, ProfileStatus::Prepared);
        assert_eq!(store.read_token(&created)?.len(), 64);
        assert_eq!(store.load("gpu")?, created);
        assert_eq!(store.list()?.len(), 1);
        store.remove("gpu")?;
        assert!(store.list()?.is_empty());
        Ok(())
    }

    #[test]
    fn operation_locks_allow_readers_and_exclude_mutations() -> Result<()> {
        let temporary = tempfile::tempdir()?;
        let store = ProfileStore::new(test_paths(temporary.path()));
        let first_reader = store.lock_profile("gpu", OperationLockMode::Shared)?;
        let second_reader = store.lock_profile("gpu", OperationLockMode::Shared)?;
        assert!(
            store
                .lock_profile("gpu", OperationLockMode::Exclusive)
                .is_err()
        );
        drop(first_reader);
        drop(second_reader);
        let writer = store.lock_profile("gpu", OperationLockMode::Exclusive)?;
        assert!(
            store
                .lock_profile("gpu", OperationLockMode::Shared)
                .is_err()
        );
        drop(writer);
        store.lock_profile("gpu", OperationLockMode::Exclusive)?;
        Ok(())
    }
}
