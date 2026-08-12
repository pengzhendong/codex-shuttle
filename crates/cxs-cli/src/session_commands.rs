use anyhow::Result;
use cxs_core::{OperationLockMode, ProfileStore};
use cxs_sessions::{RepairOptions, SyncOptions, repair_local, sync_from_remote};

pub fn sync(
    store: &ProfileStore,
    profile_name: &str,
    remote_home: Option<&str>,
    provider: Option<&str>,
) -> Result<()> {
    let _lock = store.lock_profile(profile_name, OperationLockMode::Exclusive)?;
    let profile = store.load(profile_name)?;
    let report = sync_from_remote(&SyncOptions {
        host: &profile.source_host,
        remote_codex_home: remote_home,
        local_codex_home: &profile.codex_home,
        provider,
    })?;
    println!(
        "Synced sessions from '{}': {} imported, {} already present, {} conflicts ({} remote files).",
        profile.source_host,
        report.imported,
        report.duplicates,
        report.conflicts,
        report.remote_files
    );
    println!("Provider: {}", report.provider);
    if report.encrypted > 0 {
        println!(
            "Warning: {} session(s) contain encrypted content; another account/provider may not be able to resume them.",
            report.encrypted
        );
    }
    if report.imported > 0 {
        println!("Restart or refresh Codex to index the imported sessions.");
    }
    Ok(())
}

pub fn repair(store: &ProfileStore, provider: Option<&str>) -> Result<()> {
    let backup_root = store.paths().state_root.join("backups");
    let report = repair_local(&RepairOptions {
        codex_home: &store.paths().default_codex_home,
        backup_root: &backup_root,
        provider,
    })?;
    println!(
        "Repaired local sessions: {} rollout(s), {} SQLite row(s), provider '{}'.",
        report.rollout_files_updated, report.sqlite_rows_updated, report.provider
    );
    if let Some(backup) = report.backup_dir {
        println!("Backup: {}", backup.display());
    }
    if report.encrypted > 0 {
        println!(
            "Warning: {} session(s) contain encrypted content and may require their original account/provider to resume.",
            report.encrypted
        );
    }
    Ok(())
}
