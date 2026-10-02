//! `engram backup restore`: installs one `store` copy from the configured
//! target onto a home that has no store for the project. It holds the push
//! lock throughout, fetches the copy into a staging file beside the store,
//! checks it fully, records a pending provenance record, moves it into place
//! without replacing anything, checks the installed store and marks the record
//! completed. It changes no row of the store.

use std::{
    io::{self, Read},
    path::{Path, PathBuf},
    thread::JoinHandle,
};

use chrono::{DateTime, Utc};
use engram::{
    ProjectId, SqliteStore, StoreError,
    backup::{
        CopyKind,
        record::StoredManifest,
        restore::{
            RestoreRecord, RestoreRecords, RestoreState, keep_completed_record,
            read_restore_record, write_restore_record,
        },
        target::{PushLock, RECORD_FORMAT_VERSION, RecordPaths, Statement},
    },
    storage::{RestoreCopyReport, installed_sidecar_problem},
};
use sha2::{Digest, Sha256};

use super::{
    directory::{move_without_replacing, remove_with_retry},
    fetch::{ReadFailure, ReadSettings, fetch},
};

/// The prefix and suffix of a restore's staging file. No store, sidecar or
/// fetch file is named like it, so store open, doctor and readiness never
/// take it for a store.
const STAGING_PREFIX: &str = ".backup-restore-";
const STAGING_SUFFIX: &str = ".staging";

/// How a restore runs.
pub(crate) struct RestoreSettings {
    pub read: ReadSettings,
    /// Where a test stops the restore, as a process that ended there would.
    #[cfg(test)]
    pub stop: Option<Stop>,
}

impl RestoreSettings {
    pub(crate) fn new(read: ReadSettings) -> Self {
        Self {
            read,
            #[cfg(test)]
            stop: None,
        }
    }
}

/// The points at which a test stops a restore.
#[cfg(test)]
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Stop {
    /// After the pending record is written, before the move.
    AfterPending,
    /// After the move, before the record is marked completed.
    AfterMove,
}

/// A restore that installed the store.
#[derive(Debug)]
pub(crate) struct Restored {
    pub copy: String,
    pub store: PathBuf,
    pub bytes: u64,
    pub sha256: String,
    pub origin_host: Option<String>,
    pub origin_retired_by: String,
    /// The copy's check, with the live authority it holds.
    pub report: RestoreCopyReport,
    /// Whether this run finished a restore an earlier run had moved into
    /// place but not marked completed.
    pub completed_interrupted: bool,
    /// Where an earlier completed record was kept aside.
    pub kept_record: Option<PathBuf>,
}

/// A restore's outcome, and a fetch worker left running past its deadline;
/// the caller ends the process rather than wait for it.
pub(crate) struct RestoreRun {
    pub outcome: Result<Restored, ReadFailure>,
    pub abandoned: Option<Abandoned>,
}

/// A fetch worker left running past its deadline, with the push lock, which
/// stays held until the process ends and the worker with it.
pub(crate) struct Abandoned {
    pub _worker: JoinHandle<()>,
    pub _lock: PushLock,
}

impl RestoreRun {
    fn failed(failure: ReadFailure) -> Self {
        Self {
            outcome: Err(failure),
            abandoned: None,
        }
    }
}

/// Restores copy `copy` of the project's `store` kind into `database`.
pub(crate) fn restore(
    home: &Path,
    project: &ProjectId,
    database: &Path,
    copy: &str,
    origin_retired_by: Option<&str>,
    settings: &RestoreSettings,
) -> RestoreRun {
    let by = match origin_statement(origin_retired_by) {
        Ok(by) => by,
        Err(failure) => return RestoreRun::failed(failure),
    };
    let paths = RecordPaths::new(home, project, CopyKind::Store);
    let lock = match PushLock::try_acquire(&paths) {
        Ok(lock) => lock,
        Err(error) => return RestoreRun::failed(ReadFailure::new(error.code(), error.to_string())),
    };
    let Some(directory) = database.parent() else {
        return RestoreRun::failed(ReadFailure::new(
            "backup_io",
            format!("{} names no directory", database.display()),
        ));
    };
    let occupied = occupied(database);
    let mut earlier = None;
    let mut earlier_staging = None;
    match read_restore_record(home, project) {
        RestoreRecords::Unreadable { path, reason } => {
            return RestoreRun::failed(ReadFailure::new(
                "backup_record_unreadable",
                format!("{} cannot be used: {reason}", path.display()),
            ));
        }
        RestoreRecords::Recorded(record) if record.state == RestoreState::Pending => {
            if record.copy != copy {
                return RestoreRun::failed(ReadFailure::new(
                    "backup_restore_pending_other",
                    format!(
                        "a restore of copy {} is pending since {}; finish it with `engram backup restore {} --origin-retired-by NAME`",
                        record.copy,
                        record.pending_at.to_rfc3339(),
                        record.copy
                    ),
                ));
            }
            if !occupied.is_empty() {
                return RestoreRun {
                    outcome: complete_interrupted(
                        database, project, &lock, home, *record, &occupied,
                    ),
                    abandoned: None,
                };
            }
            earlier_staging = Some(PathBuf::from(&record.staging));
        }
        // A completed restore whose store is gone: a new one proceeds, and
        // the earlier record is kept aside before the new one is written.
        RestoreRecords::Recorded(record) => earlier = Some(*record),
        RestoreRecords::None => {}
    }
    if !occupied.is_empty() {
        return RestoreRun::failed(store_exists(database, &occupied));
    }
    if let Err(source) = std::fs::create_dir_all(directory) {
        return RestoreRun::failed(io_failure(directory, &source));
    }
    let staging = directory.join(format!(
        "{STAGING_PREFIX}{}{STAGING_SUFFIX}",
        uuid::Uuid::now_v7()
    ));
    let run = fetch(
        home,
        project,
        CopyKind::Store,
        copy,
        &staging,
        &settings.read,
    );
    let fetched = match run.outcome {
        Ok(fetched) => fetched,
        Err(failure) => {
            return RestoreRun {
                outcome: Err(failure),
                abandoned: run.abandoned.map(|worker| Abandoned {
                    _worker: worker,
                    _lock: lock,
                }),
            };
        }
    };
    // Only now that the copy is fetched again is an earlier run's staging
    // file, which held the same checked copy, removed.
    if let Some(earlier) = &earlier_staging {
        remove_own_staging(directory, earlier);
    }
    let outcome = install(
        home,
        project,
        database,
        &lock,
        &fetched.manifest,
        &staging,
        &by,
        earlier.as_ref(),
        settings,
    );
    RestoreRun {
        outcome,
        abandoned: None,
    }
}

/// Checks the fetched copy at `staging`, records the pending restore, moves
/// the copy into place and completes the record.
#[allow(
    clippy::too_many_arguments,
    reason = "every value is one step's input, named at its single call"
)]
fn install(
    home: &Path,
    project: &ProjectId,
    database: &Path,
    lock: &PushLock,
    manifest: &StoredManifest,
    staging: &Path,
    by: &str,
    earlier: Option<&RestoreRecord>,
    settings: &RestoreSettings,
) -> Result<Restored, ReadFailure> {
    if manifest.capture.project_digest != engram::project_digest(project) {
        return Err(removing_staging(
            ReadFailure::new(
                "backup_project_mismatch",
                format!(
                    "the manifest of copy {} names another project than {}",
                    manifest.copy, project.0
                ),
            ),
            staging,
        ));
    }
    match engram::storage::running_schema_reference() {
        Ok(running) if running == manifest.capture.format_identity => {}
        Ok(running) => {
            return Err(kept_for_another_way(
                "backup_restore_format_unaccepted",
                &format!(
                    "this build accepts store format {}, not the copy's {}",
                    running.as_str(),
                    manifest.capture.format_identity.as_str()
                ),
                manifest,
                staging,
            ));
        }
        Err(error) => {
            return Err(kept_for_another_way(
                "backup_restore_format_unaccepted",
                &format!("this build cannot name the store format it accepts: {error}"),
                manifest,
                staging,
            ));
        }
    }
    // One clock reading, over the whole copy.
    let now = Utc::now();
    let report = match SqliteStore::verify_restore_copy(staging, now) {
        Ok(report) => report,
        Err(error) => {
            let code = if matches!(error, StoreError::DifferentBuildSchema) {
                "backup_restore_format_unaccepted"
            } else {
                "backup_restore_check_failed"
            };
            return Err(kept_for_another_way(
                code,
                &format!("the full check of the copy failed: {error}"),
                manifest,
                staging,
            ));
        }
    };
    if let Some(failure) = project_problem(&report, project) {
        return Err(removing_staging(failure, staging));
    }
    let record = RestoreRecord {
        format_version: RECORD_FORMAT_VERSION,
        project: project.0.clone(),
        copy: manifest.copy.clone(),
        sha256: report.manifest.file_sha256.clone(),
        origin_host: manifest.capture.host_name.clone(),
        origin_retired: Statement {
            by: by.to_owned(),
            at: now,
        },
        staging: staging.display().to_string(),
        state: RestoreState::Pending,
        pending_at: Utc::now(),
        completed_at: None,
    };
    let kept_record =
        match earlier.map(|earlier| keep_completed_record(home, project, lock, earlier)) {
            None => None,
            Some(Ok(kept)) => Some(kept),
            Some(Err(error)) => {
                return Err(removing_staging(
                    ReadFailure::new(error.code(), error.to_string()),
                    staging,
                ));
            }
        };
    if let Err(error) = write_restore_record(home, project, lock, &record) {
        return Err(removing_staging(
            ReadFailure::new(error.code(), error.to_string()),
            staging,
        ));
    }
    #[cfg(test)]
    if settings.stop == Some(Stop::AfterPending) {
        return Err(ReadFailure::new(
            "test_stop",
            "stopped after the pending record",
        ));
    }
    #[cfg(not(test))]
    let _ = settings;
    // A store that appeared since the first look is never replaced.
    let occupied = occupied(database);
    if !occupied.is_empty() {
        return Err(store_exists(database, &occupied));
    }
    if let Err(source) = move_without_replacing(staging, database) {
        let failure = if source.kind() == io::ErrorKind::AlreadyExists {
            store_exists(database, &[database.to_path_buf()])
        } else {
            io_failure(database, &source)
        };
        return Err(failure);
    }
    #[cfg(test)]
    if settings.stop == Some(Stop::AfterMove) {
        return Err(ReadFailure::new("test_stop", "stopped after the move"));
    }
    let installed = check_installed(database, project)?;
    finish(home, project, lock, record, report, installed, false).map(|mut restored| {
        restored.kept_record = kept_record;
        restored
    })
}

/// Finishes a restore whose earlier run moved the copy into place but did not
/// mark it completed: only when the store holds exactly the recorded bytes,
/// with nothing beside it but what a read leaves, an empty write-ahead log and
/// its index.
fn complete_interrupted(
    database: &Path,
    project: &ProjectId,
    lock: &PushLock,
    home: &Path,
    record: RestoreRecord,
    occupied: &[PathBuf],
) -> Result<Restored, ReadFailure> {
    // A near miss names what keeps the store from being this restore's own
    // output; it changes nothing.
    let not_own = |why: String| {
        let mut failure = store_exists(database, occupied);
        failure.message = format!(
            "{}; it is not taken for the pending restore of copy {}: {why}",
            failure.message, record.copy
        );
        failure
    };
    if !database.is_file() {
        return Err(not_own(format!("{} is not a file", database.display())));
    }
    if let Some(problem) = installed_sidecar_problem(database) {
        return Err(not_own(problem));
    }
    match file_sha256(database) {
        Ok(sha256) if sha256 == record.sha256 => {}
        Ok(sha256) => {
            return Err(not_own(format!(
                "its SHA-256 is {sha256}, not the pending copy's {}",
                record.sha256
            )));
        }
        Err(source) => return Err(io_failure(database, &source)),
    }
    let installed = check_installed(database, project)?;
    finish(
        home,
        project,
        lock,
        record,
        installed.clone(),
        installed,
        true,
    )
}

/// The full check of the store moved into place at `database`, with the
/// project check, as one clock reading.
fn check_installed(database: &Path, project: &ProjectId) -> Result<RestoreCopyReport, ReadFailure> {
    let report = SqliteStore::verify_installed_restore(database, Utc::now()).map_err(|error| {
        ReadFailure::new(
            "backup_restore_check_failed",
            format!(
                "the installed store at {} failed its check: {error}; the restore stays pending",
                database.display()
            ),
        )
    })?;
    match project_problem(&report, project) {
        Some(failure) => Err(failure),
        None => Ok(report),
    }
}

/// Marks the record completed once the installed store holds the restored
/// copy's bytes. `report` is the copy's check, whose live authority the
/// restore reports; `installed` is the check of the store in place.
fn finish(
    home: &Path,
    project: &ProjectId,
    lock: &PushLock,
    mut record: RestoreRecord,
    report: RestoreCopyReport,
    installed: RestoreCopyReport,
    completed_interrupted: bool,
) -> Result<Restored, ReadFailure> {
    let installed = installed.manifest;
    if installed.file_sha256 != record.sha256 {
        return Err(ReadFailure::new(
            "backup_restore_check_failed",
            format!(
                "the installed store at {} does not hold the restored copy's bytes; the restore stays pending",
                installed.path.display()
            ),
        ));
    }
    record.state = RestoreState::Completed;
    record.completed_at = Some(Utc::now());
    write_restore_record(home, project, lock, &record)
        .map_err(|error| ReadFailure::new(error.code(), error.to_string()))?;
    Ok(Restored {
        copy: record.copy,
        store: installed.path,
        bytes: installed.file_bytes,
        sha256: record.sha256,
        origin_host: record.origin_host,
        origin_retired_by: record.origin_retired.by,
        report,
        completed_interrupted,
        kept_record: None,
    })
}

/// The operator's statement that the origin store will never run again.
fn origin_statement(by: Option<&str>) -> Result<String, ReadFailure> {
    match by {
        Some(by) if !by.trim().is_empty() && !by.chars().any(char::is_control) => Ok(by.to_owned()),
        _ => Err(ReadFailure::new(
            "backup_restore_origin_unstated",
            "--origin-retired-by must name the operator who states that the origin store will never run again, without control characters",
        )),
    }
}

/// The store file and its sidecars that exist at `database`; anything that
/// stands there, a dangling link or a directory included, occupies it, and so
/// does a path that cannot be examined.
fn occupied(database: &Path) -> Vec<PathBuf> {
    let name = database.as_os_str();
    ["", "-wal", "-shm", "-journal"]
        .into_iter()
        .map(|suffix| {
            let mut path = name.to_owned();
            path.push(suffix);
            PathBuf::from(path)
        })
        .filter(|path| match std::fs::symlink_metadata(path) {
            Ok(_) => true,
            Err(error) => error.kind() != io::ErrorKind::NotFound,
        })
        .collect()
}

fn store_exists(database: &Path, occupied: &[PathBuf]) -> ReadFailure {
    let names: Vec<_> = occupied
        .iter()
        .map(|path| path.display().to_string())
        .collect();
    ReadFailure::new(
        "backup_restore_store_exists",
        format!(
            "a store already stands at {} ({}); restore never replaces one",
            database.display(),
            names.join(", ")
        ),
    )
}

/// Every project id the copy's rows name must be this project's.
fn project_problem(report: &RestoreCopyReport, project: &ProjectId) -> Option<ReadFailure> {
    let others: Vec<_> = report
        .project_ids
        .iter()
        .filter(|id| **id != project.0)
        .cloned()
        .collect();
    (!others.is_empty()).then(|| {
        ReadFailure::new(
            "backup_project_mismatch",
            format!(
                "the copy's store holds rows of {} as well as or instead of {}",
                others.join(", "),
                project.0
            ),
        )
    })
}

/// A refusal that leaves the fetched copy in place and names the two ways on.
fn kept_for_another_way(
    code: &'static str,
    reason: &str,
    manifest: &StoredManifest,
    staging: &Path,
) -> ReadFailure {
    let revision = manifest
        .capture
        .source_revision
        .as_deref()
        .filter(|revision| *revision != "unavailable");
    let build = revision.map_or_else(String::new, |revision| {
        format!(
            "install the build at source revision {revision}, the one that captured the copy, and restore with it; or "
        )
    });
    ReadFailure::new(
        code,
        format!(
            "{reason}. The fetched copy is left at {}. Ways on: {build}run `engram migration export` on that file and `engram migration import` with a build that still names every conversion since",
            staging.display()
        ),
    )
}

/// Removes the staging file after `failure`, naming it when it stays.
fn removing_staging(mut failure: ReadFailure, staging: &Path) -> ReadFailure {
    if let Err(source) = remove_with_retry(staging) {
        failure.message = format!(
            "{}; {} could not be removed after it: {source}",
            failure.message,
            staging.display()
        );
    }
    failure
}

/// Removes an earlier run's staging file, but only one this restore names:
/// a file in the store's directory with the staging prefix and suffix.
fn remove_own_staging(directory: &Path, staging: &Path) {
    let ours = staging.parent() == Some(directory)
        && staging
            .file_name()
            .and_then(|name| name.to_str())
            .is_some_and(|name| name.starts_with(STAGING_PREFIX) && name.ends_with(STAGING_SUFFIX));
    if ours && let Err(error) = remove_with_retry(staging) {
        eprintln!(
            "warning: the earlier restore's staging file {} could not be removed: {error}",
            staging.display()
        );
    }
}

fn io_failure(path: &Path, source: &io::Error) -> ReadFailure {
    ReadFailure::new(
        "backup_io",
        format!("{} could not be read or written: {source}", path.display()),
    )
}

fn file_sha256(path: &Path) -> io::Result<String> {
    let mut file = std::fs::File::open(path)?;
    let mut digest = Sha256::new();
    let mut buffer = vec![0_u8; 1 << 20];
    loop {
        let read = match file.read(&mut buffer) {
            Ok(0) => break,
            Ok(read) => read,
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) => return Err(error),
        };
        digest.update(&buffer[..read]);
    }
    Ok(format!("{:x}", digest.finalize()))
}

/// When a restored item's or grant's last expiry falls, as text.
fn expiry(at: Option<DateTime<Utc>>) -> String {
    at.map_or_else(|| "none".to_owned(), |at| at.to_rfc3339())
}

/// The plain report of a restore.
pub(crate) fn report_text(restored: &Restored) -> String {
    use std::fmt::Write as _;
    let authority = &restored.report.authority;
    let mut text = String::new();
    let verb = if restored.completed_interrupted {
        "completed an interrupted restore of"
    } else {
        "restored"
    };
    let _ = writeln!(
        text,
        "{verb} {} into {}",
        restored.copy,
        restored.store.display()
    );
    let _ = writeln!(
        text,
        "  {} bytes, sha256 {}; origin host {}",
        restored.bytes,
        restored.sha256,
        restored.origin_host.as_deref().unwrap_or("unknown")
    );
    let _ = writeln!(
        text,
        "  origin retired by {} (asserted; recorded under this home)",
        restored.origin_retired_by
    );
    if let Some(kept) = &restored.kept_record {
        let _ = writeln!(
            text,
            "  the earlier restore's record is kept at {}",
            kept.display()
        );
    }
    let _ = writeln!(
        text,
        "  live authority in the copy as of {} by this machine's clock: {} unexpired active claim(s), the last expiring {}; {} unexpired issued grant(s), the last expiring {}; {} begun turn(s)",
        authority.as_of.to_rfc3339(),
        authority.unexpired_claims,
        expiry(authority.claims_expire_by),
        authority.unexpired_grants,
        expiry(authority.grants_expire_by),
        authority.begun_turns
    );
    let _ = writeln!(
        text,
        "  no row was changed: a restored claim or grant serves only its old session, and an item still held refuses a new claim until that claim expires"
    );
    let _ = writeln!(
        text,
        "  next: run `engram doctor` and `engram readiness` before any consumer starts"
    );
    text
}

#[cfg(test)]
mod tests;
