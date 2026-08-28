use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File, OpenOptions};
use std::io::{self, Cursor, Read, Write};
#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use std::path::{Component, Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, bail};
use rusqlite::{Connection, TransactionBehavior, backup::Backup};
use serde_json::Value;
use tempfile::{NamedTempFile, TempDir};

const EMPTY_ARCHIVE_MARKER: &[u8; 4] = b"CXS0";
const MAX_ROLLOUT_BYTES: u64 = 512 * 1024 * 1024;
const MAX_TOTAL_BYTES: u64 = 4 * 1024 * 1024 * 1024;
const SESSION_DIRS: [&str; 2] = ["sessions", "archived_sessions"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SyncOptions<'a> {
    pub host: &'a str,
    pub remote_codex_home: Option<&'a str>,
    pub local_codex_home: &'a Path,
    pub provider: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SyncReport {
    pub remote_files: usize,
    pub imported: usize,
    pub duplicates: usize,
    pub conflicts: usize,
    pub encrypted: usize,
    pub provider: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RepairOptions<'a> {
    pub codex_home: &'a Path,
    pub backup_root: &'a Path,
    pub provider: Option<&'a str>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct RepairReport {
    pub scanned: usize,
    pub rollout_files_updated: usize,
    pub sqlite_rows_updated: usize,
    pub encrypted: usize,
    pub provider: String,
    pub backup_dir: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct RolloutMeta {
    id: String,
    cwd: Option<String>,
    provider: Option<String>,
    encrypted: bool,
}

#[derive(Debug, Clone)]
struct RolloutChange {
    path: PathBuf,
    relative_path: PathBuf,
    meta: RolloutMeta,
    updated: Vec<u8>,
}

#[derive(Debug, Clone)]
struct RolloutRecord {
    path: PathBuf,
    relative_path: PathBuf,
    meta: RolloutMeta,
    updated: Vec<u8>,
    rollout_needs_update: bool,
}

/// Pull missing Codex rollout files from an SSH host into the local Codex home.
///
/// Existing session IDs are never replaced. Imported rollout metadata is aligned
/// with the active local provider so Codex can discover and index it normally.
pub fn sync_from_remote(options: &SyncOptions<'_>) -> Result<SyncReport> {
    validate_host(options.host)?;
    let provider = resolve_provider(options.local_codex_home, options.provider)?;
    let staging = TempDir::new().context("could not create session staging directory")?;
    let remote_files = receive_remote_rollouts(options, staging.path())?;
    import_staged_rollouts(
        options.local_codex_home,
        staging.path(),
        remote_files,
        &provider,
    )
}

fn import_staged_rollouts(
    local_codex_home: &Path,
    staging: &Path,
    remote_files: Vec<PathBuf>,
    provider: &str,
) -> Result<SyncReport> {
    let mut local_ids = index_local_sessions(local_codex_home)?;
    let mut incoming_ids = BTreeSet::new();
    let mut report = SyncReport {
        remote_files: remote_files.len(),
        provider: provider.to_owned(),
        ..SyncReport::default()
    };

    for relative_path in remote_files {
        let staged_path = staging.join(&relative_path);
        let bytes = fs::read(&staged_path)
            .with_context(|| format!("could not read staged rollout {}", staged_path.display()))?;
        let (meta, updated) = inspect_and_rewrite(&bytes, Some(provider))
            .with_context(|| format!("invalid remote rollout {}", relative_path.display()))?;
        if meta.encrypted {
            report.encrypted += 1;
        }
        if !incoming_ids.insert(meta.id.clone()) || local_ids.contains_key(&meta.id) {
            report.duplicates += 1;
            continue;
        }

        let destination = local_codex_home.join(&relative_path);
        if destination.exists() {
            report.conflicts += 1;
            continue;
        }
        install_new_rollout(&destination, &updated)?;
        local_ids.insert(meta.id, destination);
        report.imported += 1;
    }
    Ok(report)
}

/// Repair provider metadata in an existing local Codex home.
///
/// Rollout files and matching `SQLite` rows are updated as one coordinated
/// operation. A restorable snapshot is written before any mutation.
pub fn repair_local(options: &RepairOptions<'_>) -> Result<RepairReport> {
    let provider = resolve_provider(options.codex_home, options.provider)?;
    let rollouts = list_local_rollouts(options.codex_home)?;
    let mut records = Vec::new();
    let mut encrypted = 0;
    for path in &rollouts {
        let bytes =
            fs::read(path).with_context(|| format!("could not read rollout {}", path.display()))?;
        let (meta, updated) = inspect_and_rewrite(&bytes, Some(&provider))?;
        if meta.encrypted {
            encrypted += 1;
        }
        let rollout_needs_update = meta.provider.as_deref() != Some(provider.as_str());
        let relative_path = path
            .strip_prefix(options.codex_home)
            .context("rollout escaped Codex home")?
            .to_path_buf();
        records.push(RolloutRecord {
            path: path.clone(),
            relative_path,
            meta,
            updated,
            rollout_needs_update,
        });
    }

    let mut report = RepairReport {
        scanned: rollouts.len(),
        encrypted,
        provider,
        ..RepairReport::default()
    };
    let database_path = resolve_state_db(options.codex_home)?;
    let mut database = if let Some(path) = database_path.as_deref() {
        let connection = Connection::open(path)
            .with_context(|| format!("could not open Codex state DB {}", path.display()))?;
        connection.busy_timeout(Duration::from_secs(5))?;
        Some(connection)
    } else {
        None
    };
    let changes = collect_repair_changes(database.as_ref(), records, &report.provider)?;
    if changes.is_empty() {
        return Ok(report);
    }

    let backup_dir = create_backup_dir(options.backup_root)?;
    backup_rollouts(options.codex_home, &backup_dir, &changes)?;
    if let Some(connection) = database.as_ref() {
        backup_database(connection, &backup_dir)?;
    }

    let result = apply_repair(database.as_mut(), &changes, &report.provider);
    match result {
        Ok((sqlite_rows_updated, rollout_files_updated)) => {
            report.rollout_files_updated = rollout_files_updated;
            report.sqlite_rows_updated = sqlite_rows_updated;
            report.backup_dir = Some(backup_dir);
            Ok(report)
        }
        Err(error) => {
            restore_rollouts(options.codex_home, &backup_dir, &changes)?;
            Err(error.context("provider repair failed; rollout files were restored from backup"))
        }
    }
}

fn collect_repair_changes(
    database: Option<&Connection>,
    records: Vec<RolloutRecord>,
    provider: &str,
) -> Result<Vec<RolloutChange>> {
    let has_cwd = database
        .map(|database| table_columns(database, "threads"))
        .transpose()?
        .is_some_and(|columns| columns.contains("cwd"));
    let query = if has_cwd {
        "SELECT model_provider, cwd FROM threads WHERE id = ?1"
    } else {
        "SELECT model_provider, NULL FROM threads WHERE id = ?1"
    };
    let mut statement = if let Some(database) = database {
        Some(database.prepare(query)?)
    } else {
        None
    };
    let mut changes = Vec::new();
    for record in records {
        let sqlite_needs_update =
            if let Some(statement) = statement.as_mut() {
                let mut rows = statement.query([&record.meta.id])?;
                if let Some(row) = rows.next()? {
                    let sqlite_provider: String = row.get(0)?;
                    let sqlite_cwd: Option<String> = row.get(1)?;
                    sqlite_provider != provider
                        || record.meta.cwd.as_deref().is_some_and(|cwd| {
                            sqlite_cwd.as_deref().is_some_and(|value| cwd != value)
                        })
                } else {
                    false
                }
            } else {
                false
            };
        if record.rollout_needs_update || sqlite_needs_update {
            changes.push(RolloutChange {
                path: record.path,
                relative_path: record.relative_path,
                meta: record.meta,
                updated: record.updated,
            });
        }
    }
    Ok(changes)
}

fn apply_repair(
    database: Option<&mut Connection>,
    changes: &[RolloutChange],
    provider: &str,
) -> Result<(usize, usize)> {
    let mut transaction = if let Some(database) = database {
        Some(
            database
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .context("Codex state DB is busy; close Codex and retry")?,
        )
    } else {
        None
    };

    if let Some(tx) = transaction.as_ref() {
        let columns = table_columns(tx, "threads")?;
        if !columns.contains("model_provider") {
            bail!("Codex threads table has no model_provider column");
        }
    }

    let mut rollout_updates = 0;
    for change in changes {
        if change.meta.provider.as_deref() != Some(provider) {
            atomic_replace(&change.path, &change.updated)?;
            rollout_updates += 1;
        }
    }

    let mut updated_rows = 0;
    if let Some(tx) = transaction.as_mut() {
        let columns = table_columns(tx, "threads")?;
        let has_cwd = columns.contains("cwd");
        for change in changes {
            let row_count = if has_cwd {
                tx.execute(
                    "UPDATE threads SET model_provider = ?1, cwd = COALESCE(?2, cwd) WHERE id = ?3",
                    rusqlite::params![provider, change.meta.cwd, change.meta.id],
                )?
            } else {
                tx.execute(
                    "UPDATE threads SET model_provider = ?1 WHERE id = ?2",
                    rusqlite::params![provider, change.meta.id],
                )?
            };
            updated_rows += row_count;
        }
    }
    if let Some(tx) = transaction {
        tx.commit()?;
    }
    Ok((updated_rows, rollout_updates))
}

fn receive_remote_rollouts(options: &SyncOptions<'_>, staging: &Path) -> Result<Vec<PathBuf>> {
    let root = options
        .remote_codex_home
        .map_or_else(|| "$HOME/.codex".to_owned(), shell_quote);
    let script = format!(
        "set -eu; root={root}; if ! cd \"$root\" 2>/dev/null; then printf CXS0; exit 0; fi; set --; test ! -d sessions || set -- \"$@\" sessions; test ! -d archived_sessions || set -- \"$@\" archived_sessions; if test \"$#\" -eq 0; then printf CXS0; else tar -cf - \"$@\"; fi"
    );
    let mut child = Command::new("ssh")
        .args([
            "-T",
            "-o",
            "BatchMode=yes",
            "-o",
            "ConnectTimeout=10",
            options.host,
            &script,
        ])
        .stdout(Stdio::piped())
        .spawn()
        .with_context(|| format!("could not start SSH for '{}'", options.host))?;
    let mut stdout = child.stdout.take().context("SSH stdout was not captured")?;
    let mut prefix = [0_u8; 4];
    stdout
        .read_exact(&mut prefix)
        .context("remote session stream ended before its header")?;
    if &prefix == EMPTY_ARCHIVE_MARKER {
        let status = child.wait()?;
        if !status.success() {
            bail!("remote session inventory failed for '{}'", options.host);
        }
        return Ok(Vec::new());
    }

    let reader = Cursor::new(prefix).chain(stdout);
    let mut archive = tar::Archive::new(reader);
    let mut files = Vec::new();
    let mut total_bytes = 0_u64;
    for entry in archive
        .entries()
        .context("could not read remote tar stream")?
    {
        let mut entry = entry.context("could not read remote tar entry")?;
        if entry.header().entry_type().is_dir() {
            continue;
        }
        if !entry.header().entry_type().is_file() {
            bail!("remote session archive contains a non-regular entry");
        }
        let relative = entry.path()?.into_owned();
        validate_rollout_path(&relative)?;
        let size = entry.size();
        if size > MAX_ROLLOUT_BYTES {
            bail!(
                "remote rollout {} exceeds the 512 MiB limit",
                relative.display()
            );
        }
        total_bytes = total_bytes
            .checked_add(size)
            .context("session archive size overflow")?;
        if total_bytes > MAX_TOTAL_BYTES {
            bail!("remote session archive exceeds the 4 GiB limit");
        }
        let destination = staging.join(&relative);
        create_private_parent(&destination)?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        options.mode(0o600);
        let mut output = options.open(&destination)?;
        io::copy(&mut entry, &mut output)?;
        output.sync_all()?;
        files.push(relative);
    }
    drop(archive);
    let status = child.wait()?;
    if !status.success() {
        bail!("remote session archive failed for '{}'", options.host);
    }
    files.sort();
    Ok(files)
}

fn validate_rollout_path(path: &Path) -> Result<()> {
    if path.is_absolute()
        || path
            .components()
            .any(|component| !matches!(component, Component::Normal(_)))
    {
        bail!("unsafe path in remote session archive: {}", path.display());
    }
    let first = path.components().next().context("empty archive path")?;
    let Component::Normal(first) = first else {
        bail!("unsafe archive path");
    };
    if !SESSION_DIRS.iter().any(|name| first == *name) {
        bail!("unexpected remote session path: {}", path.display());
    }
    let name = path
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or_default();
    if !name.starts_with("rollout-")
        || !Path::new(name)
            .extension()
            .is_some_and(|extension| extension.eq_ignore_ascii_case("jsonl"))
    {
        bail!(
            "unexpected file in remote session archive: {}",
            path.display()
        );
    }
    Ok(())
}

fn index_local_sessions(codex_home: &Path) -> Result<BTreeMap<String, PathBuf>> {
    let mut result = BTreeMap::new();
    for path in list_local_rollouts(codex_home)? {
        let bytes = fs::read(&path)?;
        let (meta, _) = inspect_and_rewrite(&bytes, None)
            .with_context(|| format!("invalid local rollout {}", path.display()))?;
        result.entry(meta.id).or_insert(path);
    }
    Ok(result)
}

fn list_local_rollouts(codex_home: &Path) -> Result<Vec<PathBuf>> {
    let mut result = Vec::new();
    for directory in SESSION_DIRS {
        collect_rollouts(&codex_home.join(directory), &mut result)?;
    }
    result.sort();
    Ok(result)
}

fn collect_rollouts(directory: &Path, result: &mut Vec<PathBuf>) -> Result<()> {
    if !directory.exists() {
        return Ok(());
    }
    for entry in fs::read_dir(directory)? {
        let entry = entry?;
        let file_type = entry.file_type()?;
        if file_type.is_symlink() {
            continue;
        }
        if file_type.is_dir() {
            collect_rollouts(&entry.path(), result)?;
        } else if file_type.is_file() {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if name.starts_with("rollout-") && name.ends_with(".jsonl") {
                result.push(entry.path());
            }
        }
    }
    Ok(())
}

fn inspect_and_rewrite(
    bytes: &[u8],
    target_provider: Option<&str>,
) -> Result<(RolloutMeta, Vec<u8>)> {
    let newline = bytes.iter().position(|byte| *byte == b'\n');
    let first_end = newline.unwrap_or(bytes.len());
    let first_line = bytes
        .get(..first_end)
        .context("invalid rollout first line")?
        .strip_suffix(b"\r")
        .unwrap_or(&bytes[..first_end]);
    let mut record: Value =
        serde_json::from_slice(first_line).context("invalid session_meta JSON")?;
    if record.get("type").and_then(Value::as_str) != Some("session_meta") {
        bail!("first rollout record is not session_meta");
    }
    let payload = record
        .get_mut("payload")
        .and_then(Value::as_object_mut)
        .context("session_meta has no payload object")?;
    let id = payload
        .get("id")
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .context("session_meta has no thread id")?
        .to_owned();
    let cwd = payload
        .get("cwd")
        .and_then(Value::as_str)
        .map(str::to_owned);
    let provider = payload
        .get("model_provider")
        .and_then(Value::as_str)
        .map(str::to_owned);
    if let Some(target) = target_provider {
        payload.insert(
            "model_provider".to_owned(),
            Value::String(target.to_owned()),
        );
    }
    let encrypted = bytes
        .windows(b"encrypted_content".len())
        .any(|window| window == b"encrypted_content");
    let mut updated = serde_json::to_vec(&record)?;
    if newline.is_some() {
        updated.push(b'\n');
        updated.extend_from_slice(&bytes[first_end + 1..]);
    }
    Ok((
        RolloutMeta {
            id,
            cwd,
            provider,
            encrypted,
        },
        updated,
    ))
}

fn resolve_provider(codex_home: &Path, explicit: Option<&str>) -> Result<String> {
    if let Some(provider) = explicit {
        validate_provider(provider)?;
        return Ok(provider.to_owned());
    }
    let config_path = codex_home.join("config.toml");
    let text = match fs::read_to_string(&config_path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok("openai".to_owned()),
        Err(error) => {
            return Err(error).with_context(|| format!("could not read {}", config_path.display()));
        }
    };
    let config: toml::Value = toml::from_str(&text)
        .with_context(|| format!("could not parse {}", config_path.display()))?;
    let provider = config
        .get("model_provider")
        .and_then(toml::Value::as_str)
        .unwrap_or("openai");
    validate_provider(provider)?;
    Ok(provider.to_owned())
}

fn validate_provider(provider: &str) -> Result<()> {
    if provider.is_empty()
        || !provider
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    {
        bail!("provider id contains unsafe characters");
    }
    Ok(())
}

fn validate_host(host: &str) -> Result<()> {
    if host.is_empty()
        || host.starts_with('-')
        || host
            .bytes()
            .any(|byte| byte.is_ascii_whitespace() || byte == 0)
    {
        bail!("unsafe SSH host alias");
    }
    Ok(())
}

fn shell_quote(value: &str) -> String {
    format!("'{}'", value.replace('\'', "'\"'\"'"))
}

fn install_new_rollout(destination: &Path, contents: &[u8]) -> Result<()> {
    create_private_parent(destination)?;
    let parent = destination
        .parent()
        .context("rollout destination has no parent")?;
    let mut temporary = NamedTempFile::new_in(parent)?;
    set_private_file_permissions(temporary.as_file())?;
    temporary.write_all(contents)?;
    temporary.as_file_mut().sync_all()?;
    temporary
        .persist_noclobber(destination)
        .map_err(|error| error.error)
        .with_context(|| format!("could not install {}", destination.display()))?;
    Ok(())
}

fn atomic_replace(destination: &Path, contents: &[u8]) -> Result<()> {
    let parent = destination.parent().context("rollout path has no parent")?;
    let permissions = fs::metadata(destination)?.permissions();
    let mut temporary = NamedTempFile::new_in(parent)?;
    temporary.as_file().set_permissions(permissions)?;
    temporary.write_all(contents)?;
    temporary.as_file_mut().sync_all()?;
    temporary
        .persist(destination)
        .map_err(|error| error.error)
        .with_context(|| format!("could not replace {}", destination.display()))?;
    Ok(())
}

fn create_private_parent(path: &Path) -> Result<()> {
    let parent = path.parent().context("path has no parent directory")?;
    fs::create_dir_all(parent)?;
    set_private_directory_permissions(parent)?;
    Ok(())
}

fn create_backup_dir(root: &Path) -> Result<PathBuf> {
    let timestamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis();
    let path = root.join(format!("session-repair-{timestamp}"));
    fs::create_dir_all(&path)?;
    set_private_directory_permissions(&path)?;
    Ok(path)
}

fn backup_rollouts(codex_home: &Path, backup: &Path, changes: &[RolloutChange]) -> Result<()> {
    for change in changes {
        let destination = backup.join("rollouts").join(&change.relative_path);
        create_private_parent(&destination)?;
        fs::copy(codex_home.join(&change.relative_path), &destination)?;
        set_path_private_file_permissions(&destination)?;
    }
    Ok(())
}

fn restore_rollouts(codex_home: &Path, backup: &Path, changes: &[RolloutChange]) -> Result<()> {
    for change in changes {
        let source = backup.join("rollouts").join(&change.relative_path);
        let contents = fs::read(&source)?;
        atomic_replace(&codex_home.join(&change.relative_path), &contents)?;
    }
    Ok(())
}

fn resolve_state_db(codex_home: &Path) -> Result<Option<PathBuf>> {
    let config_path = codex_home.join("config.toml");
    let configured = if config_path.exists() {
        let text = fs::read_to_string(&config_path)?;
        let value: toml::Value = toml::from_str(&text)?;
        value
            .get("sqlite_home")
            .and_then(toml::Value::as_str)
            .map(PathBuf::from)
    } else {
        None
    };
    let home = configured
        .or_else(|| std::env::var_os("CODEX_SQLITE_HOME").map(PathBuf::from))
        .unwrap_or_else(|| codex_home.to_path_buf());
    let path = home.join("state_5.sqlite");
    Ok(path.exists().then_some(path))
}

fn backup_database(source: &Connection, backup_dir: &Path) -> Result<()> {
    let destination_path = backup_dir.join("state_5.sqlite");
    let mut destination = Connection::open(&destination_path)?;
    {
        let backup = Backup::new(source, &mut destination)?;
        backup.run_to_completion(128, Duration::from_millis(10), None)?;
    }
    destination.close().map_err(|(_, error)| error)?;
    set_path_private_file_permissions(&destination_path)?;
    Ok(())
}

fn set_private_file_permissions(file: &File) -> Result<()> {
    #[cfg(unix)]
    file.set_permissions(fs::Permissions::from_mode(0o600))?;
    #[cfg(windows)]
    let _ = file.metadata()?;
    Ok(())
}

fn set_path_private_file_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))?;
    #[cfg(windows)]
    let _ = fs::metadata(path)?;
    Ok(())
}

fn set_private_directory_permissions(path: &Path) -> Result<()> {
    #[cfg(unix)]
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    #[cfg(windows)]
    let _ = fs::metadata(path)?;
    Ok(())
}

fn table_columns(connection: &Connection, table: &str) -> Result<BTreeSet<String>> {
    let mut statement = connection.prepare(&format!("PRAGMA table_info({table})"))?;
    let values = statement.query_map([], |row| row.get::<_, String>(1))?;
    let columns = values.collect::<rusqlite::Result<BTreeSet<_>>>()?;
    if columns.is_empty() {
        bail!("Codex state DB has no {table} table");
    }
    Ok(columns)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rollout(id: &str, provider: &str, cwd: &str) -> Vec<u8> {
        format!(
            "{{\"timestamp\":\"2026-01-01T00:00:00Z\",\"type\":\"session_meta\",\"payload\":{{\"id\":\"{id}\",\"cwd\":\"{cwd}\",\"model_provider\":\"{provider}\"}}}}\n{{\"type\":\"event_msg\",\"payload\":{{\"type\":\"user_message\"}}}}\n"
        )
        .into_bytes()
    }

    #[test]
    fn rewrites_only_session_meta_provider() -> Result<()> {
        let input = rollout("thread-1", "old", "/srv/project");
        let (meta, output) = inspect_and_rewrite(&input, Some("openai"))?;
        assert_eq!(meta.id, "thread-1");
        assert_eq!(meta.provider.as_deref(), Some("old"));
        assert!(String::from_utf8(output)?.contains("\"model_provider\":\"openai\""));
        Ok(())
    }

    #[test]
    fn repair_updates_rollout_and_sqlite_with_backup() -> Result<()> {
        let home = TempDir::new()?;
        let backup = TempDir::new()?;
        fs::write(
            home.path().join("config.toml"),
            "model_provider = \"openai\"\n",
        )?;
        let rollout_path = home.path().join("sessions/2026/01/01/rollout-test.jsonl");
        create_private_parent(&rollout_path)?;
        fs::write(&rollout_path, rollout("thread-1", "old", "/srv/project"))?;
        let database = Connection::open(home.path().join("state_5.sqlite"))?;
        database.execute_batch(
            "CREATE TABLE threads (id TEXT PRIMARY KEY, model_provider TEXT NOT NULL, cwd TEXT NOT NULL);\
             INSERT INTO threads VALUES ('thread-1', 'old', '/old');",
        )?;
        drop(database);

        let report = repair_local(&RepairOptions {
            codex_home: home.path(),
            backup_root: backup.path(),
            provider: None,
        })?;
        assert_eq!(report.rollout_files_updated, 1);
        assert_eq!(report.sqlite_rows_updated, 1);
        assert!(
            report
                .backup_dir
                .as_ref()
                .is_some_and(|path| path.join("state_5.sqlite").exists())
        );
        let database = Connection::open(home.path().join("state_5.sqlite"))?;
        let row: (String, String) = database.query_row(
            "SELECT model_provider, cwd FROM threads WHERE id = 'thread-1'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        assert_eq!(row, ("openai".to_owned(), "/srv/project".to_owned()));
        Ok(())
    }

    #[test]
    fn rejects_archive_traversal() {
        assert!(validate_rollout_path(Path::new("../sessions/rollout-x.jsonl")).is_err());
        assert!(validate_rollout_path(Path::new("sessions/rollout-x.jsonl")).is_ok());
    }

    #[test]
    fn import_deduplicates_by_id_and_does_not_overwrite_paths() -> Result<()> {
        let local = TempDir::new()?;
        let staging = TempDir::new()?;
        let existing = local
            .path()
            .join("sessions/2026/01/01/rollout-existing.jsonl");
        create_private_parent(&existing)?;
        fs::write(&existing, rollout("thread-existing", "openai", "/local"))?;

        let duplicate = PathBuf::from("sessions/2026/01/02/rollout-duplicate.jsonl");
        let imported = PathBuf::from("sessions/2026/01/02/rollout-imported.jsonl");
        for (relative, bytes) in [
            (
                &duplicate,
                rollout("thread-existing", "remote", "/server/old"),
            ),
            (
                &imported,
                rollout("thread-imported", "remote", "/server/new"),
            ),
        ] {
            let path = staging.path().join(relative);
            create_private_parent(&path)?;
            fs::write(path, bytes)?;
        }

        let report = import_staged_rollouts(
            local.path(),
            staging.path(),
            vec![duplicate, imported.clone()],
            "openai",
        )?;
        assert_eq!(report.imported, 1);
        assert_eq!(report.duplicates, 1);
        assert!(
            !local
                .path()
                .join("sessions/2026/01/02/rollout-duplicate.jsonl")
                .exists()
        );
        let imported_text = fs::read_to_string(local.path().join(imported))?;
        assert!(imported_text.contains("\"model_provider\":\"openai\""));
        assert!(String::from_utf8(fs::read(existing)?)?.contains("\"cwd\":\"/local\""));
        Ok(())
    }
}
