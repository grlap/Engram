//! `engram backup list` and `engram backup fetch`: read the copies the
//! configured target holds for this project. They read the target's records
//! under the Engram home and never open or create the store, so they work in
//! a home that has only configured the target. Every request to the target
//! runs on a worker under one deadline.

use std::{
    fs, io,
    path::{Path, PathBuf},
    thread::JoinHandle,
    time::{Duration, Instant},
};

use engram::{
    ProjectId,
    backup::{
        CopyKind,
        freshness::{KindRecords, kind_records},
        record::StoredManifest,
    },
};

use super::{
    adapter::{AdapterError, BackupAdapter},
    directory::{move_without_replacing, remove_with_retry},
    push::{FreeSpace, Target, Transport},
};

/// How long the requests of one list or fetch may take together when no
/// deadline is given.
pub(crate) const DEFAULT_READ_DEADLINE: Duration = Duration::from_mins(30);

/// How much earlier than the whole fetch the decode's own deadline falls, so
/// a decode that runs out of time removes its partial file itself before the
/// process gives up on the worker.
const DECODE_MARGIN: Duration = Duration::from_secs(2);

/// How a list or fetch runs.
pub(crate) struct ReadSettings {
    pub deadline: Duration,
    pub local_free_space: FreeSpace,
    /// Makes the fetch's move leave its staging file behind, as the
    /// hard-link fallback does when its unlink fails.
    #[cfg(test)]
    pub leave_staging: bool,
}

impl ReadSettings {
    pub(crate) fn new(deadline: Duration) -> Self {
        Self {
            deadline,
            local_free_space: engram::backup::available_space,
            #[cfg(test)]
            leave_staging: false,
        }
    }
}

/// The copies a target holds for the project.
#[derive(Debug, Default)]
pub(crate) struct Listing {
    pub manifests: Vec<StoredManifest>,
    /// Names of manifest files the target holds that could not be used.
    pub unreadable: Vec<String>,
    /// Copies whose manifest names another project.
    pub foreign: Vec<String>,
}

/// Why a list or fetch stopped, with its stable code.
#[derive(Debug)]
pub(crate) struct ReadFailure {
    pub code: &'static str,
    pub message: String,
}

impl ReadFailure {
    pub(super) fn new(code: &'static str, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

impl From<AdapterError> for ReadFailure {
    fn from(error: AdapterError) -> Self {
        Self::new(error.code(), error.to_string())
    }
}

/// A list or fetch's outcome, and a worker left running past the deadline;
/// the caller ends the process rather than wait for it.
pub(crate) struct ReadRun<T> {
    pub outcome: Result<T, ReadFailure>,
    pub abandoned: Option<JoinHandle<()>>,
}

/// Lists every copy the configured target of `kind` holds for `project`.
pub(crate) fn list(
    home: &Path,
    project: &ProjectId,
    kind: CopyKind,
    settings: &ReadSettings,
) -> ReadRun<Listing> {
    let target = match configured(home, project, kind, settings) {
        Ok(target) => target,
        Err(failure) => return failed(failure),
    };
    run(settings.deadline, move |left| {
        list_all(&target.adapter(left), &target.project).map_err(ReadFailure::from)
    })
}

/// A fetched copy: its manifest, the file it was written to, and what the
/// operator should know about it.
#[derive(Debug)]
pub(crate) struct Fetched {
    pub manifest: StoredManifest,
    pub out: PathBuf,
    pub warnings: Vec<String>,
}

/// Writes copy `copy` of `kind`, decoded and checked against its manifest,
/// to `out`, which must not exist. The copy is decoded into a hidden file
/// beside `out` and moved to `out` only once it is checked, so `out` never
/// holds a partial or unchecked copy. That hidden `.<name>.<pid>.fetching`
/// file is left behind by a process that ends in the middle, or by a removal
/// that fails, and the refusal then names it. A move that succeeded can also
/// leave it, which a warning beside the fetched copy names.
pub(crate) fn fetch(
    home: &Path,
    project: &ProjectId,
    kind: CopyKind,
    copy: &str,
    out: &Path,
    settings: &ReadSettings,
) -> ReadRun<Fetched> {
    let target = match configured(home, project, kind, settings) {
        Ok(target) => target,
        Err(failure) => return failed(failure),
    };
    let out = match std::path::absolute(out) {
        Ok(out) => out,
        Err(source) => return failed(io_failure(out, &source)),
    };
    let Some(name) = out
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
    else {
        return failed(ReadFailure::new(
            "backup_io",
            format!("{} names no file to write", out.display()),
        ));
    };
    if fs::symlink_metadata(&out).is_ok() {
        return failed(exists(&out));
    }
    let staging = out.with_file_name(format!(".{name}.{}.fetching", std::process::id()));
    let copy = copy.to_owned();
    let left_behind = staging.clone();
    #[cfg(test)]
    let leave_staging = settings.leave_staging;
    let mut fetched = run(settings.deadline, move |left| {
        let started = Instant::now();
        let listing = list_all(&target.adapter(left), &target.project)?;
        let manifest = named(listing, &copy, &target.project)?;
        let decode_left = left
            .saturating_sub(started.elapsed())
            .saturating_sub(DECODE_MARGIN);
        target
            .adapter(decode_left)
            .get(&target.project, &manifest, &staging)?;
        // The move never replaces a file that appeared at `out` meanwhile.
        #[cfg(test)]
        let moved = if leave_staging {
            super::directory::move_leaving_source(&staging, &out)
        } else {
            move_without_replacing(&staging, &out)
        };
        #[cfg(not(test))]
        let moved = move_without_replacing(&staging, &out);
        match moved {
            Ok(moved) => Ok(Fetched {
                manifest,
                out,
                warnings: leftover_warning(&moved).into_iter().collect(),
            }),
            Err(source) => {
                let failure = if source.kind() == io::ErrorKind::AlreadyExists {
                    exists(&out)
                } else {
                    io_failure(&out, &source)
                };
                Err(removing(failure, &staging))
            }
        }
    });
    if fetched.abandoned.is_some()
        && let Err(failure) = &mut fetched.outcome
    {
        failure.message = format!(
            "{}; {} may be left behind",
            failure.message,
            left_behind.display()
        );
    }
    fetched
}

/// The warning that names a staging file a successful move left behind.
pub(super) fn leftover_warning(moved: &super::directory::Moved) -> Option<String> {
    moved.leftover.as_ref().map(|left| {
        format!(
            "the hidden staging file {} may remain after the move: it still stood, or whether it did could not be checked; the fetched copy is complete, and the staging file can be removed",
            left.display()
        )
    })
}

/// Removes `staging` after `failure`, naming it in the failure when it stays.
pub(super) fn removing(mut failure: ReadFailure, staging: &Path) -> ReadFailure {
    if let Err(source) = remove_with_retry(staging) {
        failure.message = format!(
            "{}; {} could not be removed after it: {source}",
            failure.message,
            staging.display()
        );
    }
    failure
}

/// The manifest of copy `copy` in `listing`. A copy whose manifest names
/// another project is refused as such, and one the listing does not hold as
/// unknown.
pub(super) fn named(
    listing: Listing,
    copy: &str,
    project: &ProjectId,
) -> Result<StoredManifest, ReadFailure> {
    if listing.foreign.iter().any(|foreign| foreign == copy) {
        return Err(ReadFailure::new(
            "backup_project_mismatch",
            format!(
                "the manifest of copy {copy} names another project than {}",
                project.0
            ),
        ));
    }
    listing
        .manifests
        .into_iter()
        .find(|manifest| manifest.copy == copy)
        .ok_or_else(|| {
            ReadFailure::new(
                "backup_copy_unknown",
                format!("the target holds no usable copy named {copy}"),
            )
        })
}

fn exists(out: &Path) -> ReadFailure {
    ReadFailure::new(
        "backup_copy_exists",
        format!(
            "{} already exists; fetch never replaces a file",
            out.display()
        ),
    )
}

fn io_failure(path: &Path, source: &io::Error) -> ReadFailure {
    ReadFailure::new(
        "backup_io",
        format!("{} could not be written: {source}", path.display()),
    )
}

/// The configured target of `kind`, or why there is none to read.
fn configured(
    home: &Path,
    project: &ProjectId,
    kind: CopyKind,
    settings: &ReadSettings,
) -> Result<Target, ReadFailure> {
    match kind_records(home, project, kind) {
        KindRecords::Configured {
            config, identity, ..
        } => Ok(Target {
            root: PathBuf::from(&config.dir),
            identity,
            project: project.clone(),
            free_space: settings.local_free_space,
        }),
        KindRecords::NotConfigured => Err(ReadFailure::new(
            "backup_not_configured",
            format!(
                "no {} target is configured for this project; run `engram backup target set` first",
                kind.as_str()
            ),
        )),
        KindRecords::Unreadable { path, reason } => Err(ReadFailure::new(
            "backup_record_unreadable",
            format!("{} cannot be used: {reason}", path.display()),
        )),
    }
}

/// Every page of the target's listing.
pub(super) fn list_all(
    adapter: &impl BackupAdapter,
    project: &ProjectId,
) -> Result<Listing, AdapterError> {
    let mut listing = Listing::default();
    let mut cursor: Option<String> = None;
    loop {
        let page = adapter.list(project, cursor.as_deref())?;
        listing.manifests.extend(page.manifests);
        listing.unreadable.extend(page.unreadable);
        listing.foreign.extend(page.foreign);
        match page.next {
            Some(next) => cursor = Some(next),
            None => return Ok(listing),
        }
    }
}

fn run<T: Send + 'static>(
    deadline: Duration,
    work: impl FnOnce(Duration) -> Result<T, ReadFailure> + Send + 'static,
) -> ReadRun<T> {
    let mut transport = Transport {
        budget: deadline,
        used: Duration::ZERO,
    };
    match transport.run(work) {
        Ok(outcome) => ReadRun {
            outcome,
            abandoned: None,
        },
        Err(abandoned) => ReadRun {
            outcome: Err(ReadFailure::new(
                "backup_transport_deadline",
                format!("the requests to the target passed their deadline of {deadline:?}"),
            )),
            abandoned,
        },
    }
}

fn failed<T>(failure: ReadFailure) -> ReadRun<T> {
    ReadRun {
        outcome: Err(failure),
        abandoned: None,
    }
}
