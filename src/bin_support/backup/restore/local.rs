//! Local-file acquisition and replacement policy around the shared occurrence.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, anyhow, bail};
use chrono::Utc;
use engram::{
    ProjectId, SqliteStore,
    backup::{
        CopyKind,
        restore::{
            RestoreRecord, RestoreRecords, RestoreSource, RestoreState, read_restore_record,
        },
        target::{PushLock, RECORD_FORMAT_VERSION, RecordPaths},
    },
    storage::BackupManifest,
};

#[cfg(test)]
use super::Stop;
use super::{
    AbandonPolicy, RestoreSettings, STAGING_PREFIX, STAGING_SUFFIX, abandonment_statement,
    archive_pending, complete_record, move_without_replacing, occupied, owned_staging,
    publish_pending, switch_staging,
};

/// Restores a local verified file, recording through the backup restore owner.
/// The caller supplies resolved home/project context, never backup-derived identity.
pub(crate) fn restore_local(
    home: &Path,
    project: &ProjectId,
    database: &Path,
    from: &Path,
    replace: bool,
    settings: &RestoreSettings,
) -> Result<BackupManifest> {
    // Retain the caller's absolute selector, not a fresh filesystem lookup:
    // completion or a checked stage must survive removal/rotation of the source.
    let from = std::path::absolute(from)?;
    if from.to_string_lossy().chars().any(char::is_control) {
        bail!("restore source path must not contain control characters");
    }
    let source = RestoreSource::LocalFile {
        path: from
            .to_str()
            .context("restore source path is not Unicode")?
            .to_owned(),
        replace,
    };
    let lock = PushLock::try_acquire(&RecordPaths::new(home, project, CopyKind::Store))?;
    let earlier = match read_restore_record(home, project) {
        RestoreRecords::Unreadable { path, reason } => {
            bail!("{} cannot be used: {reason}", path.display())
        }
        RestoreRecords::Recorded(record) if record.state == RestoreState::Pending => {
            if record.source != source {
                bail!(
                    "another source or installation policy is pending; {}",
                    record.source.ways_on()
                );
            }
            let ways = record.source.ways_on();
            eprintln!(
                "Finishing pending restore {} from {} with the earlier checked bytes (SHA-256 {}), not the source file's current bytes.",
                record.occurrence_id, record.pending_at, record.sha256
            );
            return retry(home, project, database, &from, &lock, *record, settings)
                .map_err(|error| anyhow!("{error:#}; {ways}"));
        }
        RestoreRecords::Recorded(record) => Some(record),
        RestoreRecords::None => None,
    };
    let manifest = SqliteStore::verify_backup(&from)
        .with_context(|| format!("backup {} is not a current Engram store", from.display()))?;
    if fs::canonicalize(database)
        .is_ok_and(|target| fs::canonicalize(&from).is_ok_and(|source| source == target))
    {
        bail!("backup {} is the store itself", from.display());
    }
    if database.exists() && !replace {
        bail!(
            "store {} already exists; pass --replace to overwrite it with the backup",
            database.display()
        );
    }
    let staging = stage(database, &from, &manifest)?;
    #[cfg(test)]
    if settings.stop == Some(Stop::FailCheck) {
        fs::remove_file(&staging)?;
        bail!("staged copy failed verification (stopped by a test)");
    }
    let mut record = RestoreRecord {
        format_version: RECORD_FORMAT_VERSION,
        occurrence_id: uuid::Uuid::now_v7(),
        project: project.0.clone(),
        source,
        sha256: manifest.file_sha256,
        staging: staging
            .to_str()
            .context("restore staging path is not Unicode")?
            .to_owned(),
        state: RestoreState::Pending,
        pending_at: Utc::now(),
        completed_at: None,
        previous_completed: None,
    };
    publish_pending(
        home,
        project,
        &lock,
        earlier.as_deref(),
        &mut record,
        settings,
    )
    .map_err(|failure| anyhow!(failure.message))?;
    let ways = record.source.ways_on();
    install_and_finish(home, project, database, &lock, record, settings)
        .map_err(|error| anyhow!("{error:#}; {ways}"))
}

fn stage(database: &Path, from: &Path, expected: &BackupManifest) -> Result<PathBuf> {
    let directory = database.parent().context("store path has no directory")?;
    fs::create_dir_all(directory)?;
    let staging = std::path::absolute(directory.join(format!(
        "{STAGING_PREFIX}{}{STAGING_SUFFIX}",
        uuid::Uuid::now_v7()
    )))?;
    let result = (|| {
        fs::copy(from, &staging)
            .with_context(|| format!("failed to stage {} beside the store", from.display()))?;
        let staged = SqliteStore::verify_backup(&staging)
            .with_context(|| format!("staged copy {} failed verification", staging.display()))?;
        if staged.file_sha256 != expected.file_sha256 {
            bail!("staged copy bytes differ from the backup");
        }
        Ok(())
    })();
    if let Err(error) = result {
        let _ = fs::remove_file(&staging);
        return Err(error);
    }
    Ok(staging)
}

fn retry(
    home: &Path,
    project: &ProjectId,
    database: &Path,
    from: &Path,
    lock: &PushLock,
    mut record: RestoreRecord,
    settings: &RestoreSettings,
) -> Result<BackupManifest> {
    let staging = Path::new(&record.staging);
    let owned =
        owned_staging(home, project, staging).map_err(|failure| anyhow!(failure.message))?;
    let replace = matches!(
        record.source,
        RestoreSource::LocalFile { replace: true, .. }
    );
    // Every replace-intent install uses rename, including an absent destination:
    // its staging name disappears at installation.
    // Without replacement a successful hard-link fallback may leave staging.
    if database.exists() && (owned.is_none() || !replace) {
        return finish(home, project, database, lock, record, settings);
    }
    if owned.is_some() {
        let staged = SqliteStore::verify_backup(staging)?;
        if staged.file_sha256 != record.sha256 {
            bail!("pending staging bytes changed; the restore stays pending");
        }
    } else {
        let expected = SqliteStore::verify_backup(from).context(
            "the pending source cannot be verified for re-staging; the restore stays pending",
        )?;
        if expected.file_sha256 != record.sha256 {
            bail!(
                "source bytes differ from the pending restore; abandon it before choosing different bytes"
            );
        }
        let new_staging = stage(database, from, &expected)?;
        switch_staging(home, project, lock, &mut record, &new_staging, settings)
            .map_err(|failure| anyhow!(failure.message))?;
    }
    install_and_finish(home, project, database, lock, record, settings)
}

fn install_and_finish(
    home: &Path,
    project: &ProjectId,
    database: &Path,
    lock: &PushLock,
    record: RestoreRecord,
    settings: &RestoreSettings,
) -> Result<BackupManifest> {
    let replace = matches!(
        record.source,
        RestoreSource::LocalFile { replace: true, .. }
    );
    let staging = Path::new(&record.staging);
    if replace {
        if database.exists() {
            checkpoint_replacement(database)?;
        }
        #[cfg(test)]
        if settings.stop == Some(Stop::AfterCheckpoint) {
            bail!("stopped after replacement checkpoint; the restore stays pending");
        }
        fs::rename(staging, database).with_context(|| {
            format!(
                "failed to install {} as the store; the restore stays pending",
                staging.display()
            )
        })?;
    } else {
        #[cfg(test)]
        let moved = if settings.read.leave_staging {
            super::super::directory::move_leaving_source(staging, database)
        } else {
            move_without_replacing(staging, database)
        };
        #[cfg(not(test))]
        let moved = move_without_replacing(staging, database);
        let _moved = moved.with_context(|| {
            format!(
                "failed to install {} as the store; the restore stays pending",
                staging.display()
            )
        })?;
    }
    #[cfg(test)]
    if settings.stop == Some(Stop::AfterMove) {
        bail!("stopped after the move; the restore stays pending");
    }
    finish(home, project, database, lock, record, settings)
}

fn finish(
    home: &Path,
    project: &ProjectId,
    database: &Path,
    lock: &PushLock,
    record: RestoreRecord,
    settings: &RestoreSettings,
) -> Result<BackupManifest> {
    #[cfg(not(test))]
    let _ = settings;
    let installed = SqliteStore::verify_installed_restore(database, Utc::now())
        .with_context(|| {
            format!(
                "restored store {} failed verification; the restore stays pending",
                database.display()
            )
        })?
        .manifest;
    #[cfg(test)]
    if settings.stop == Some(Stop::FailCompletionWrite) {
        bail!("the completed record was not written; the restore stays pending");
    }
    complete_record(home, project, lock, record, &installed)
        .map_err(|failure| anyhow!(failure.message))?;
    Ok(installed)
}

/// The existing local replacement policy: fold and truncate WAL under an
/// exclusive SQLite lock. Continued destination exclusion is the host's job.
fn checkpoint_replacement(database: &Path) -> Result<()> {
    let old = engram::storage::open_sqlite_file(database, rusqlite::OpenFlags::default())
        .with_context(|| format!("failed to open {} for replacement", database.display()))?;
    old.execute_batch("PRAGMA locking_mode = EXCLUSIVE; BEGIN IMMEDIATE; COMMIT;")
        .context("failed to take the store being replaced; is another Engram process using it?")?;
    let (busy, log_frames, checkpointed): (i64, i64, i64) = old
        .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
            Ok((row.get(0)?, row.get(1)?, row.get(2)?))
        })
        .context("failed to checkpoint the store being replaced")?;
    if busy != 0 || log_frames != checkpointed {
        bail!(
            "another process still uses {}; stop it and retry (checkpoint busy={busy}, log={log_frames}, checkpointed={checkpointed})",
            database.display()
        );
    }
    drop(old);
    for suffix in ["-wal", "-shm", "-journal"] {
        let sidecar = PathBuf::from(format!("{}{suffix}", database.display()));
        if sidecar.exists() {
            fs::remove_file(&sidecar)
                .with_context(|| format!("failed to remove {}", sidecar.display()))?;
        }
    }
    Ok(())
}

/// Abandons only an occurrence that cannot have installed an occupied
/// destination, without opening the source or mutating the destination.
pub(crate) fn abandon_local(
    home: &Path,
    project: &ProjectId,
    database: &Path,
    from: &Path,
    by: Option<&str>,
    settings: &RestoreSettings,
) -> Result<serde_json::Value> {
    let by = abandonment_statement(by).map_err(|failure| anyhow!(failure.message))?;
    let selector = std::path::absolute(from)?;
    let lock = PushLock::try_acquire(&RecordPaths::new(home, project, CopyKind::Store))?;
    let record = match read_restore_record(home, project) {
        RestoreRecords::Recorded(record) if record.state == RestoreState::Pending => record,
        RestoreRecords::Unreadable { path, reason } => {
            bail!("{} cannot be used: {reason}", path.display())
        }
        RestoreRecords::Recorded(_) | RestoreRecords::None => {
            bail!("no local-file restore is pending")
        }
    };
    let RestoreSource::LocalFile { path, replace } = &record.source else {
        bail!("another source is pending; {}", record.source.ways_on());
    };
    if Path::new(path) != selector {
        bail!("another source is pending; {}", record.source.ways_on());
    }
    let stage = owned_staging(home, project, Path::new(&record.staging))
        .map_err(|failure| anyhow!(failure.message))?;
    // Every replace-intent installation is a rename, never a hard-link move.
    // A retained owned stage proves it has not installed yet. A no-replace
    // move may retain staging after installing: it then requires completion.
    if !occupied(database).is_empty() && (!*replace || stage.is_none()) {
        bail!(
            "the pending restore may already be installed; abandonment would hide that occurrence; {}",
            record.source.ways_on()
        );
    }
    drop(stage);
    // Archive/clear first for local replacement cancellation: deleting its
    // stage first would lose the proof needed to retry an archive failure
    // while the untouched old destination still stands. Cleanup failure is
    // then an explicit warning, never a newly stranded pending occurrence.
    let policy = AbandonPolicy::Local {
        restore_previous: database.exists(),
    };
    let archived = archive_pending(home, project, &lock, &record, by, settings, policy)
        .map_err(|failure| anyhow!(failure.message))?;
    Ok(serde_json::json!({
        "source": record.source,
        "occurrence_id": record.occurrence_id,
        "abandoned_by": by,
        "abandoned_at": archived.at,
        "staging": archived.staging,
        "staging_removed": archived.staging_removed,
        "archive": archived.archive,
        "warnings": archived.warnings,
    }))
}

#[cfg(test)]
mod tests;
