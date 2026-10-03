//! `engram backup restore`: installs one `store` copy from the configured
//! target onto a home that has no store for the project. It holds the push
//! lock throughout, fetches the copy into a staging file beside the store,
//! checks it fully, records a pending provenance record, moves it into place
//! without replacing anything, checks the installed store and marks the record
//! completed. It changes no row of the store. A pending restore can instead be
//! abandoned, bound to its copy: its own staging file is removed and its record
//! archived.

use std::{
    ffi::{OsStr, OsString},
    io::{self, Read},
    path::{Component, Path, PathBuf},
    thread::JoinHandle,
};

use cap_fs_ext::DirExt;
#[cfg(windows)]
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
#[cfg(windows)]
use cap_std::fs::OpenOptions;
use cap_std::{ambient_authority, fs::Dir};

use chrono::{DateTime, Utc};
use engram::{
    ProjectId, SqliteStore, StoreError,
    backup::{
        CopyKind,
        record::StoredManifest,
        restore::{
            AbandonError, RestoreRecord, RestoreRecords, RestoreState, archive_abandoned_pending,
            keep_completed_record, pending_ways_on, read_restore_record, write_restore_record,
        },
        target::{PushLock, RECORD_FORMAT_VERSION, RecordPaths, Statement},
    },
    storage::{RestoreCopyReport, installed_sidecar_problem},
};
use sha2::{Digest, Sha256};

use super::{
    directory::{
        SHARING_PAUSE, SHARING_RETRIES, is_sharing_violation, move_without_replacing,
        remove_with_retry,
    },
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
    /// Runs between the check that a staging file is the restore's own and
    /// its removal, with the checked path, so a test can change what stands
    /// there.
    #[cfg(test)]
    pub before_removal: Option<fn(&Path)>,
}

impl RestoreSettings {
    pub(crate) fn new(read: ReadSettings) -> Self {
        Self {
            read,
            #[cfg(test)]
            stop: None,
            #[cfg(test)]
            before_removal: None,
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
    /// The full check of the fetched copy fails.
    FailCheck,
    /// A retry cannot point the pending record at its newly fetched copy.
    FailPointerWrite,
    /// Abandoning cannot remove the pending restore's staging file.
    FailCleanup,
    /// Abandoning cannot write the archive.
    FailArchive,
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
    let mut pending = None;
    match read_restore_record(home, project) {
        RestoreRecords::Unreadable { path, reason } => {
            return RestoreRun::failed(unreadable_record(&path, &reason));
        }
        RestoreRecords::Recorded(record) if record.state == RestoreState::Pending => {
            if record.copy != copy {
                return RestoreRun::failed(pending_other(&record));
            }
            if !occupied.is_empty() {
                return RestoreRun {
                    outcome: complete_interrupted(
                        database, project, &lock, home, *record, &occupied,
                    ),
                    abandoned: None,
                };
            }
            pending = Some(*record);
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
    let staging = match std::path::absolute(directory.join(format!(
        "{STAGING_PREFIX}{}{STAGING_SUFFIX}",
        uuid::Uuid::now_v7()
    ))) {
        Ok(staging) => staging,
        Err(source) => return RestoreRun::failed(io_failure(directory, &source)),
    };
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
    if let Some(record) = pending
        && let Err(failure) = switch_staging(home, project, &lock, record, &staging, settings)
    {
        return RestoreRun::failed(failure);
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

/// Points a pending restore's record at the copy its retry fetched again to
/// `staging`, and only then removes the earlier staging file. The new copy
/// must hold the pending copy's bytes. When the record cannot be written, the
/// earlier staging file and the record stay as they were and the new file is
/// removed. A process that ends between the write and the removal leaves the
/// earlier staging file behind, no longer named by the record.
fn switch_staging(
    home: &Path,
    project: &ProjectId,
    lock: &PushLock,
    mut record: RestoreRecord,
    staging: &Path,
    settings: &RestoreSettings,
) -> Result<(), ReadFailure> {
    let earlier = PathBuf::from(&record.staging);
    let unchanged = format!(
        "the pending record and its staged copy {} stay as they were",
        earlier.display()
    );
    match file_sha256(staging) {
        Ok(sha256) if sha256 == record.sha256 => {}
        Ok(sha256) => {
            return Err(removing_staging(
                ReadFailure::new(
                    "backup_restore_pending_mismatch",
                    format!(
                        "the copy fetched again for the pending restore of {} has SHA-256 {sha256}, not the pending copy's {}; {unchanged}",
                        record.copy, record.sha256
                    ),
                ),
                staging,
            ));
        }
        Err(source) => return Err(removing_staging(io_failure(staging, &source), staging)),
    }
    record.staging = staging.display().to_string();
    let write = || {
        write_restore_record(home, project, lock, &record)
            .map_err(|error| ReadFailure::new(error.code(), format!("{error}; {unchanged}")))
    };
    #[cfg(test)]
    let written = if settings.stop == Some(Stop::FailPointerWrite) {
        Err(ReadFailure::new(
            "test_stop",
            format!("the pending record was not written; {unchanged}"),
        ))
    } else {
        write()
    };
    #[cfg(not(test))]
    let written = {
        let _ = settings;
        write()
    };
    written.map_err(|failure| removing_staging(failure, staging))?;
    // Only now that the record names the copy fetched again is the earlier
    // run's staging file, which held the same checked copy, removed.
    remove_own_staging(home, project, &earlier, settings);
    Ok(())
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
    #[cfg(not(test))]
    let _ = settings;
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
    #[cfg(test)]
    if settings.stop == Some(Stop::FailCheck) {
        return Err(kept_for_another_way(
            "backup_restore_check_failed",
            "the full check of the copy failed (stopped by a test)",
            manifest,
            staging,
        ));
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
    // A store that appeared since the first look is never replaced.
    let occupied = occupied(database);
    if !occupied.is_empty() {
        return Err(store_exists(database, &occupied));
    }
    // A staging file left after the move is not reported yet; surfacing it
    // is tracked separately.
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

/// Removes an earlier run's staging file, but only one [`owned_staging`]
/// takes for this restore's own; anything else is named in a warning.
fn remove_own_staging(
    home: &Path,
    project: &ProjectId,
    staging: &Path,
    settings: &RestoreSettings,
) {
    match owned_staging(home, project, staging) {
        Ok(None) => {}
        Ok(Some(owned)) => {
            if let Err(error) = owned.remove(settings) {
                eprintln!(
                    "warning: the earlier restore's staging file {} could not be removed: {error}",
                    staging.display()
                );
            }
        }
        Err(failure) => eprintln!("warning: {}", failure.message),
    }
}

/// A restore's own staging file, checked and ready to remove: its name in
/// the store's directory, held open from the Engram home through real
/// directories only. On Unix the removal is made through the held directory
/// itself. On Windows the held directories deny delete sharing, so neither
/// they nor the directories above them can be renamed or replaced while the
/// file is removed, and the removal names the file by the directory's full
/// path, which was checked to lead to the directory held.
struct OwnedStaging {
    _projects: Dir,
    directory: Dir,
    #[cfg(windows)]
    full_directory: PathBuf,
    name: OsString,
}

impl OwnedStaging {
    /// Removes the file by its name in the held store directory, never by a
    /// path resolved again.
    fn remove(self, settings: &RestoreSettings) -> io::Result<()> {
        #[cfg(test)]
        if let Some(hook) = settings.before_removal {
            hook(Path::new(&self.name));
        }
        #[cfg(not(test))]
        let _ = settings;
        // On Windows cap-std removes by a path it rebuilds from the held
        // handle, which drops the `\\?\` of a network path and so turns
        // `\\?\UNC\…` into a path relative to the working directory; the
        // full path checked against the held handle is used instead.
        #[cfg(windows)]
        let remove = || std::fs::remove_file(self.full_directory.join(&self.name));
        #[cfg(not(windows))]
        let remove = || self.directory.remove_file(&self.name);
        let mut attempt = 0;
        loop {
            match remove() {
                Ok(()) => break,
                Err(error) if error.kind() == io::ErrorKind::NotFound => break,
                Err(error) if is_sharing_violation(&error) && attempt < SHARING_RETRIES => {
                    attempt += 1;
                    std::thread::sleep(SHARING_PAUSE);
                }
                Err(error) => return Err(error),
            }
        }
        // The removal reports what it reached; whether the file is gone is
        // asked of the held directory itself.
        match self.directory.symlink_metadata(&self.name) {
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(error),
            Ok(_) => Err(io::Error::other(
                "it still stands in the store's directory after its removal",
            )),
        }
    }
}

/// Whether `staging`, as a pending record names it, is a restore's own
/// staging file, and still stands. It must be named
/// `.backup-restore-<uuid>.staging`, be spelled through no `..`, and lie
/// directly in `<home>/projects/<project digest>/`. That directory and
/// `projects` are opened from the home without following a link, and either
/// one that is a link or reparse point is refused; the file must be a regular
/// file and no link or reparse point. `Ok(None)` when it is already gone.
/// Anything else is refused, and so is a path that cannot be examined.
fn owned_staging(
    home: &Path,
    project: &ProjectId,
    staging: &Path,
) -> Result<Option<OwnedStaging>, ReadFailure> {
    let refused = |why: String| {
        ReadFailure::new(
            "backup_restore_staging_unowned",
            format!(
                "the pending restore's staging file {} {why}; nothing was removed",
                staging.display()
            ),
        )
    };
    if staging
        .components()
        .any(|part| matches!(part, Component::ParentDir))
    {
        return Err(refused("is named through `..`".into()));
    }
    let digest = engram::project_digest(project);
    let expected = home.join("projects").join(&digest);
    let (Ok(absolute), Ok(expected)) =
        (std::path::absolute(staging), std::path::absolute(&expected))
    else {
        return Err(refused("cannot be resolved to its absolute form".into()));
    };
    if absolute.parent() != Some(expected.as_path()) {
        return Err(refused(format!(
            "is not directly in the store's directory {}, as this home spells it",
            expected.display()
        )));
    }
    let Some(name) = absolute.file_name().map(OsStr::to_os_string) else {
        return Err(refused("names no file".into()));
    };
    let text = name.to_str();
    let named = text
        .and_then(|name| name.strip_prefix(STAGING_PREFIX))
        .and_then(|rest| rest.strip_suffix(STAGING_SUFFIX))
        .and_then(|id| uuid::Uuid::parse_str(id).ok())
        .is_some_and(|id| {
            text == Some(&format!(
                "{STAGING_PREFIX}{}{STAGING_SUFFIX}",
                id.hyphenated()
            ))
        });
    if !named {
        return Err(refused("is not named as a restore's staging file".into()));
    }
    let home_directory = match Dir::open_ambient_dir(home, ambient_authority()) {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(refused(format!(
                "lies under the home {}, which cannot be opened: {error}",
                home.display()
            )));
        }
    };
    let Some(projects) = open_real_directory(
        &home_directory,
        OsStr::new("projects"),
        &home.join("projects"),
    )
    .map_err(refused)?
    else {
        return Ok(None);
    };
    let Some(directory) =
        open_real_directory(&projects, OsStr::new(&digest), &expected).map_err(refused)?
    else {
        return Ok(None);
    };
    match directory.symlink_metadata(&name) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(refused(format!("cannot be examined: {error}"))),
        Ok(metadata) if metadata.file_type().is_symlink() => {
            return Err(refused("is a link".into()));
        }
        Ok(metadata) if !metadata.is_file() => {
            return Err(refused("is not a regular file".into()));
        }
        Ok(_) => {}
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        let opened = directory
            .open_with(&name, &options)
            .and_then(|file| file.into_std().metadata());
        match opened {
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(refused(format!("cannot be examined: {error}"))),
            Ok(metadata) if metadata.file_attributes() & REPARSE_POINT != 0 => {
                return Err(refused("is a link or other reparse point".into()));
            }
            Ok(metadata) if !metadata.is_file() => {
                return Err(refused("is not a regular file".into()));
            }
            Ok(_) => {}
        }
    }
    #[cfg(windows)]
    let full_directory = {
        let full_directory = std::fs::canonicalize(&expected).map_err(|error| {
            refused(format!(
                "lies in {}, whose full path cannot be resolved: {error}",
                expected.display()
            ))
        })?;
        let held = directory
            .try_clone()
            .and_then(|copy| same_file::Handle::from_file(copy.into_std_file()));
        let named = same_file::Handle::from_path(&full_directory);
        match (held, named) {
            (Ok(held), Ok(named)) if held == named => {}
            _ => {
                return Err(refused(format!(
                    "lies in {}, whose full path {} does not lead to the directory held",
                    expected.display(),
                    full_directory.display()
                )));
            }
        }
        full_directory
    };
    Ok(Some(OwnedStaging {
        _projects: projects,
        directory,
        #[cfg(windows)]
        full_directory,
        name,
    }))
}

/// The Windows attribute of a reparse point, a link or junction among them.
#[cfg(windows)]
const REPARSE_POINT: u32 = 0x400;

/// Opens `name` in `parent` as a directory without following a link, and
/// refuses one that is a link or reparse point. `Ok(None)` when it is gone.
fn open_real_directory(parent: &Dir, name: &OsStr, path: &Path) -> Result<Option<Dir>, String> {
    let directory = match parent.open_dir_nofollow(name) {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            let link = parent
                .symlink_metadata(name)
                .is_ok_and(|metadata| metadata.file_type().is_symlink());
            return Err(if link {
                format!("lies in {}, which is a link", path.display())
            } else {
                format!(
                    "lies in {}, which cannot be opened as a directory: {error}",
                    path.display()
                )
            });
        }
    };
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt as _;
        let attributes = directory
            .try_clone()
            .and_then(|copy| copy.into_std_file().metadata())
            .map(|metadata| metadata.file_attributes())
            .map_err(|error| {
                format!(
                    "lies in {}, which cannot be examined: {error}",
                    path.display()
                )
            })?;
        if attributes & REPARSE_POINT != 0 {
            return Err(format!(
                "lies in {}, which is a link or other reparse point",
                path.display()
            ));
        }
    }
    Ok(Some(directory))
}

/// A pending restore abandoned by the operator.
#[derive(Debug)]
pub(crate) struct Abandonment {
    pub copy: String,
    pub abandoned_by: String,
    pub abandoned_at: DateTime<Utc>,
    /// The staging file the pending record named.
    pub staging: PathBuf,
    /// Whether it stood and was removed; `false` when it was already gone.
    pub staging_removed: bool,
    /// Where the pending record was archived.
    pub archive: PathBuf,
    /// What the operator should know about the archive.
    pub warnings: Vec<String>,
}

/// Abandons the pending restore of `copy`, only when it is the copy the
/// pending record names. It holds the push lock, contacts no target, and needs
/// no store or sidecar where the store would go. It removes the restore's own
/// staging file first, refusing any other, then archives the pending record
/// and clears it. A removal that fails leaves the restore pending; an archive
/// that fails leaves it pending too, after the staging file is gone.
pub(crate) fn abandon_pending(
    home: &Path,
    project: &ProjectId,
    database: &Path,
    copy: &str,
    abandoned_by: Option<&str>,
    settings: &RestoreSettings,
) -> Result<Abandonment, ReadFailure> {
    let by = match abandoned_by {
        Some(by) if !by.trim().is_empty() && !by.chars().any(char::is_control) => by,
        _ => {
            return Err(ReadFailure::new(
                "backup_restore_abandoner_unstated",
                "--abandoned-by must name the operator who abandons the pending restore, without control characters",
            ));
        }
    };
    let paths = RecordPaths::new(home, project, CopyKind::Store);
    let lock = PushLock::try_acquire(&paths)
        .map_err(|error| ReadFailure::new(error.code(), error.to_string()))?;
    let record = match read_restore_record(home, project) {
        RestoreRecords::Unreadable { path, reason } => {
            return Err(unreadable_record(&path, &reason));
        }
        RestoreRecords::Recorded(record) if record.state == RestoreState::Pending => *record,
        RestoreRecords::Recorded(_) | RestoreRecords::None => {
            return Err(ReadFailure::new(
                "backup_restore_not_pending",
                "no restore is pending for this project, so there is nothing to abandon",
            ));
        }
    };
    if record.copy != copy {
        return Err(pending_other(&record));
    }
    let occupied = occupied(database);
    if !occupied.is_empty() {
        let mut failure = store_exists(database, &occupied);
        failure.message = format!(
            "{}; a pending restore is abandoned only while nothing stands where its store would go",
            failure.message
        );
        return Err(failure);
    }
    let staging = PathBuf::from(&record.staging);
    let owned = owned_staging(home, project, &staging).map_err(|mut failure| {
        failure.message = format!("{}; the restore stays pending", failure.message);
        failure
    })?;
    let stands = owned.is_some();
    if let Some(owned) = owned {
        #[cfg(test)]
        let removed = if settings.stop == Some(Stop::FailCleanup) {
            Err(io::Error::other("stopped by a test"))
        } else {
            owned.remove(settings)
        };
        #[cfg(not(test))]
        let removed = owned.remove(settings);
        removed.map_err(|source| {
            ReadFailure::new(
                "backup_io",
                format!(
                    "the pending restore's staging file {} could not be removed: {source}; the restore stays pending and is not abandoned",
                    staging.display()
                ),
            )
        })?;
    }
    let at = Utc::now();
    #[cfg(test)]
    let archived = if settings.stop == Some(Stop::FailArchive) {
        Err(AbandonError::Archive(
            engram::backup::target::TargetError::Invalid {
                reason: "the archive was not written (stopped by a test)".into(),
            },
        ))
    } else {
        archive_abandoned_pending(home, project, &lock, by, at)
    };
    #[cfg(not(test))]
    let archived = {
        let _ = settings;
        archive_abandoned_pending(home, project, &lock, by, at)
    };
    let removed_note = if stands {
        format!(
            "; its staging file {} was already removed",
            staging.display()
        )
    } else {
        String::new()
    };
    match archived {
        Ok(archived) => Ok(Abandonment {
            copy: record.copy,
            abandoned_by: by.to_owned(),
            abandoned_at: at,
            staging,
            staging_removed: stands,
            archive: archived.archive,
            warnings: archived.warnings,
        }),
        Err(AbandonError::Archive(error)) => Err(ReadFailure::new(
            error.code(),
            format!(
                "{error}; the restore stays pending and is not abandoned, and abandoning it can be run again{removed_note}"
            ),
        )),
        Err(AbandonError::Clear { archive, error }) => Err(ReadFailure::new(
            error.code(),
            format!(
                "the pending restore was archived at {}, but {error}; the restore stays pending and is not abandoned, and abandoning it can be run again{removed_note}",
                archive.display()
            ),
        )),
    }
}

/// The refusal of a record that cannot be used.
fn unreadable_record(path: &Path, reason: &str) -> ReadFailure {
    ReadFailure::new(
        "backup_record_unreadable",
        format!("{} cannot be used: {reason}", path.display()),
    )
}

/// The refusal of a restore or abandonment of a copy other than the pending
/// one, naming both ways on from the pending one.
fn pending_other(record: &RestoreRecord) -> ReadFailure {
    ReadFailure::new(
        "backup_restore_pending_other",
        format!(
            "a restore of copy {} is pending since {}; {}",
            record.copy,
            record.pending_at.to_rfc3339(),
            pending_ways_on(&record.copy, Some(&record.origin_retired.by))
        ),
    )
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
