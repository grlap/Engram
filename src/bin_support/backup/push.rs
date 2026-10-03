//! `engram backup push`: one bounded run that brings a kind's copy at its
//! configured target up to date, in the steps of the off-host backup brief.
//!
//! The run holds the kind's push lock throughout. It resolves an attempt an
//! earlier run left pending, captures the store into a local stage, and then
//! either confirms that the newest copy still holds the same bytes or records
//! a new attempt as pending and puts it. Every request to the target runs on
//! a worker thread under the transport deadline. When that deadline passes,
//! the run records the failure, with its attempt still pending, and returns
//! the worker unjoined: the command then ends its process, which is how a
//! request stalled inside the operating system is cancelled. A push is
//! therefore a command of its own and is never run inside a long-lived
//! process.

use std::{
    fs, io,
    path::{Path, PathBuf},
    sync::mpsc,
    thread::{self, JoinHandle},
    time::{Duration, Instant},
};

use chrono::{DateTime, Utc};
use engram::{
    ObjectId, ProjectId,
    backup::{
        CaptureCheck, CaptureOptions, CopyKind, StoreCapture, capture_store, host_name,
        record::{AttemptOutcome, LastAttempt},
        target::{
            PushLock, PushRecords, RecordPaths, TargetError, TargetState, read_for_push,
            remove_leftover_record_files, write_state,
        },
    },
};
use serde::Serialize;

use super::{
    adapter::{AdapterError, Attempt, BackupAdapter, BackupReceipt, Confirmation, Reconciled},
    directory::DirectoryAdapter,
};

/// How long a capture may take when no deadline is given.
pub(crate) const DEFAULT_CAPTURE_DEADLINE: Duration = Duration::from_mins(15);

/// How long the requests to the target may take together when no deadline
/// is given.
pub(crate) const DEFAULT_TRANSPORT_DEADLINE: Duration = Duration::from_mins(30);

/// Reads the free space of the file system holding a path.
pub(crate) type FreeSpace = fn(&Path) -> io::Result<u64>;

/// How one push runs.
#[derive(Clone)]
pub(crate) struct PushSettings {
    pub capture_deadline: Duration,
    /// The time every request to the target may take together.
    pub transport_deadline: Duration,
    pub stage_free_space: FreeSpace,
    pub target_free_space: FreeSpace,
    /// Runs on the transport worker just before `put`, so a test can hold the
    /// put past its deadline.
    #[cfg(test)]
    pub before_put: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    /// Runs after the confirmation of the newest copy returned, so a test can
    /// make it slow.
    #[cfg(test)]
    pub after_confirm: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    /// Makes every state write after the put fail.
    #[cfg(test)]
    pub fail_save_after_put: bool,
    /// Runs on the transport worker just before a retention removal.
    #[cfg(test)]
    pub before_remove: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    /// Runs on the transport worker just before a pending attempt is
    /// reconciled and confirmed.
    #[cfg(test)]
    pub before_resolve: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    /// Runs on the transport worker just before the newest copy is
    /// confirmed.
    #[cfg(test)]
    pub before_confirm: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    /// Runs once the capture returned, before the time it left is reckoned,
    /// so a test can spend the rest of the capture's deadline.
    #[cfg(test)]
    pub after_capture: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
    /// Runs immediately before compression of a new copy begins.
    #[cfg(test)]
    pub before_prepare: Option<std::sync::Arc<dyn Fn() + Send + Sync>>,
}

impl PushSettings {
    pub(crate) fn new(capture_deadline: Duration, transport_deadline: Duration) -> Self {
        Self {
            capture_deadline,
            transport_deadline,
            stage_free_space: engram::backup::available_space,
            target_free_space: engram::backup::available_space,
            #[cfg(test)]
            before_put: None,
            #[cfg(test)]
            after_confirm: None,
            #[cfg(test)]
            fail_save_after_put: false,
            #[cfg(test)]
            before_remove: None,
            #[cfg(test)]
            before_resolve: None,
            #[cfg(test)]
            before_confirm: None,
            #[cfg(test)]
            after_capture: None,
            #[cfg(test)]
            before_prepare: None,
        }
    }
}

/// How a push of one kind ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Outcome {
    /// No target is configured for the kind; nothing was done.
    NotConfigured,
    /// Another push holds the kind's lock; nothing was done.
    Busy,
    /// A copy was put and its receipt recorded.
    Uploaded,
    /// The capture equalled the newest copy, which the target confirmed.
    Unchanged,
    /// The push failed; the earlier receipt and copy stand as they were.
    Failed,
}

/// What one push of one kind did, as the command reports it.
#[derive(Debug, Serialize)]
pub(crate) struct KindReport {
    pub kind: CopyKind,
    pub outcome: Outcome,
    /// The typed code of a failure.
    pub code: Option<String>,
    pub message: Option<String>,
    pub target_identity: Option<ObjectId>,
    /// The newest receipt after the push.
    pub receipt: Option<BackupReceipt>,
    /// When the store's content was last observed in the newest copy.
    pub observed_equal_at: Option<DateTime<Utc>>,
    /// The copy of an earlier attempt that this push found confirmed.
    pub recovered: Option<String>,
    /// The copy of an earlier attempt that never arrived and was dropped.
    pub dropped: Option<String>,
    /// The copy of an earlier attempt recorded for another target identity,
    /// set aside as history.
    pub set_aside: Option<String>,
    /// The copy of the attempt left pending for the next push.
    pub pending: Option<String>,
    /// The copies retention removed from the target.
    pub removed: Vec<String>,
    /// How the capture was checked: in full, or by its bytes equalling the
    /// newest copy that this build checked in full. Absent when no capture
    /// was made.
    pub capture_check: Option<CaptureCheck>,
    /// What went wrong without failing the push.
    pub warnings: Vec<String>,
    pub elapsed_ms: u64,
}

impl KindReport {
    fn new(kind: CopyKind, outcome: Outcome) -> Self {
        Self {
            kind,
            outcome,
            code: None,
            message: None,
            target_identity: None,
            receipt: None,
            observed_equal_at: None,
            recovered: None,
            dropped: None,
            set_aside: None,
            pending: None,
            removed: Vec::new(),
            capture_check: None,
            warnings: Vec::new(),
            elapsed_ms: 0,
        }
    }
}

/// A finished push, and the transport worker it left running when the
/// transport deadline passed. The caller ends the process rather than wait
/// for that worker.
pub(crate) struct PushRun {
    pub report: KindReport,
    pub abandoned: Option<Abandoned>,
}

/// A transport worker that passed its deadline, with the push lock it ran
/// under. The lock is held until the process ends, so no other push or
/// target word acts on the target while the worker may still be writing.
pub(crate) struct Abandoned {
    pub worker: JoinHandle<()>,
    pub lock: PushLock,
}

/// Why a step failed: a typed code and a message.
struct Failure {
    code: String,
    message: String,
    abandoned: Option<JoinHandle<()>>,
}

impl Failure {
    fn new(code: &str, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
            abandoned: None,
        }
    }

    fn deadline(worker: Option<JoinHandle<()>>, deadline: Duration, during: &str) -> Self {
        Self {
            code: "backup_transport_deadline".into(),
            message: format!("the transport deadline of {deadline:?} passed during {during}"),
            abandoned: worker,
        }
    }
}

impl From<AdapterError> for Failure {
    fn from(error: AdapterError) -> Self {
        Self::new(error.code(), error.to_string())
    }
}

impl From<TargetError> for Failure {
    fn from(error: TargetError) -> Self {
        Self::new(error.code(), error.to_string())
    }
}

/// The time every request to the target may take together, spent request by
/// request on a worker thread.
pub(super) struct Transport {
    pub(super) budget: Duration,
    pub(super) used: Duration,
}

impl Transport {
    /// Runs `work` on a worker, giving it the time left. Returns its answer,
    /// or, when the time ran out first, the unjoined worker; with no time
    /// left at all, the request is not started.
    pub(super) fn run<T: Send + 'static>(
        &mut self,
        work: impl FnOnce(Duration) -> T + Send + 'static,
    ) -> Result<T, Option<JoinHandle<()>>> {
        let left = self.budget.saturating_sub(self.used);
        if left.is_zero() {
            return Err(None);
        }
        let started = Instant::now();
        let (answer, receive) = mpsc::channel();
        let worker = thread::spawn(move || {
            let _ = answer.send(work(left));
        });
        match receive.recv_timeout(left) {
            Ok(value) => {
                self.used += started.elapsed();
                let _ = worker.join();
                Ok(value)
            }
            Err(mpsc::RecvTimeoutError::Timeout) => Err(Some(worker)),
            Err(mpsc::RecvTimeoutError::Disconnected) => match worker.join() {
                Ok(()) => unreachable!("a worker that returned has sent its answer"),
                Err(panic) => std::panic::resume_unwind(panic),
            },
        }
    }
}

/// The configured target as a transport worker needs it.
#[derive(Clone)]
pub(super) struct Target {
    pub(super) root: PathBuf,
    pub(super) identity: ObjectId,
    pub(super) project: ProjectId,
    pub(super) free_space: FreeSpace,
}

impl Target {
    /// The adapter for this target, whose reads stop at `deadline`.
    pub(super) fn adapter(&self, deadline: Duration) -> DirectoryAdapter<'_> {
        DirectoryAdapter::new(
            self.root.clone(),
            self.identity.clone(),
            deadline,
            &self.free_space,
        )
    }
}

/// Pushes `kind` of `project` under `home` to its configured target.
pub(crate) fn push(
    home: &Path,
    project: &ProjectId,
    kind: CopyKind,
    settings: &PushSettings,
) -> PushRun {
    let started = Instant::now();
    let mut run = push_kind(home, project, kind, settings);
    run.report.elapsed_ms = u64::try_from(started.elapsed().as_millis()).unwrap_or(u64::MAX);
    run
}

fn push_kind(home: &Path, project: &ProjectId, kind: CopyKind, settings: &PushSettings) -> PushRun {
    let finished = |report| PushRun {
        report,
        abandoned: None,
    };
    let refused = |failure: Failure| {
        let mut report = KindReport::new(kind, Outcome::Failed);
        report.code = Some(failure.code);
        report.message = Some(failure.message);
        finished(report)
    };
    let paths = RecordPaths::new(home, project, kind);
    // A kind with no configuration has nothing to push, and nothing is
    // created for it, not even its lock file.
    match fs::symlink_metadata(&paths.config) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            return finished(KindReport::new(kind, Outcome::NotConfigured));
        }
        Err(error) => {
            return refused(Failure::new(
                "backup_record_unreadable",
                format!("{} cannot be used: {error}", paths.config.display()),
            ));
        }
        Ok(_) => {}
    }
    // Step 1: the lock. Another push holding it is not a failure.
    let lock = match PushLock::try_acquire(&paths) {
        Ok(lock) => lock,
        Err(TargetError::PushRunning { .. }) => {
            return finished(KindReport::new(kind, Outcome::Busy));
        }
        Err(error) => return refused(error.into()),
    };
    // A record write whose process ended before its rename left its
    // temporary file; under the lock every one found is such a leftover.
    let swept = remove_leftover_record_files(&paths, kind, &lock)
        .err()
        .map(|error| {
            format!("the leftover temporary record files could not all be removed: {error}")
        });
    let records = match read_for_push(&paths, project, kind, &lock) {
        Ok(Some(records)) => records,
        Ok(None) => {
            let mut report = KindReport::new(kind, Outcome::NotConfigured);
            report.warnings.extend(swept);
            return finished(report);
        }
        Err(error) => {
            let mut run = refused(error.into());
            run.report.warnings.extend(swept);
            return run;
        }
    };
    let mut attempt = Push {
        home,
        project,
        paths: &paths,
        lock: &lock,
        settings,
        target: Target {
            root: PathBuf::from(&records.config.dir),
            identity: records.identity.clone(),
            project: project.clone(),
            free_space: settings.target_free_space,
        },
        transport: Transport {
            budget: settings.transport_deadline,
            used: Duration::ZERO,
        },
        started_at: Utc::now(),
        report: KindReport::new(kind, Outcome::Failed),
        stage: None,
        keep: usize::try_from(records.config.keep).unwrap_or(usize::MAX),
        saved: records.state.clone(),
        #[cfg(test)]
        put_done: false,
    };
    attempt.report.target_identity = Some(records.identity.clone());
    attempt.report.warnings.extend(swept);
    let PushRecords { mut state, .. } = records;
    let result = attempt.steps(&mut state);
    let (report, worker) = attempt.finish(&mut state, result);
    PushRun {
        report,
        abandoned: worker.map(|worker| Abandoned { worker, lock }),
    }
}

/// One push of one kind, under its lock.
struct Push<'a> {
    home: &'a Path,
    project: &'a ProjectId,
    paths: &'a RecordPaths,
    lock: &'a PushLock,
    settings: &'a PushSettings,
    target: Target,
    transport: Transport,
    started_at: DateTime<Utc>,
    report: KindReport,
    /// The local stage this push owns: the capture and, once prepared, the
    /// stored file beside it.
    stage: Option<(StoreCapture, Option<PathBuf>)>,
    /// How many copies the target keeps.
    keep: usize,
    /// The state as last written, which is what the report shows.
    saved: TargetState,
    /// Whether the put returned, so a test can fail the write after it.
    #[cfg(test)]
    put_done: bool,
}

impl Push<'_> {
    fn save(&mut self, state: &TargetState) -> Result<(), Failure> {
        #[cfg(test)]
        if self.put_done && self.settings.fail_save_after_put {
            return Err(Failure::new(
                "backup_io",
                "the state could not be written (injected by a test)",
            ));
        }
        write_state(self.paths, self.lock, state).map_err(Failure::from)?;
        self.saved = state.clone();
        Ok(())
    }

    /// Steps 2 to 6 of the brief; the outcome of a run that did not fail.
    fn steps(&mut self, state: &mut TargetState) -> Result<AttemptOutcome, Failure> {
        self.remove_leftover_stages();
        self.resolve_pending(state)?;
        #[cfg(test)]
        super::halt::at(super::halt::Stage::BeforeCapture);

        // Step 4: capture into the local stage.
        let free_space = self.settings.stage_free_space;
        #[cfg(test)]
        let observer = |phase| {
            if phase == engram::backup::CapturePhase::Verify {
                super::halt::at(super::halt::Stage::CopyStaged);
            }
        };
        #[cfg(test)]
        let observer: Option<&dyn Fn(engram::backup::CapturePhase)> = Some(&observer);
        #[cfg(not(test))]
        let observer = None;
        // The newest copy at this target: when the capture's settled bytes
        // equal it, its full check stands for this capture.
        let newest = state
            .newest_receipt
            .as_ref()
            .filter(|receipt| receipt.target_identity == self.target.identity)
            .map(|receipt| receipt.manifest.capture.clone());
        let options = CaptureOptions {
            deadline: self.settings.capture_deadline,
            compressed_in_stage: true,
            host_name: host_name(),
            free_space: &free_space,
            observer,
            same_as: newest.as_ref(),
        };
        let capture_started = Instant::now();
        let capture = capture_store(self.home, self.project, &options)
            .map_err(|error| Failure::new(error.code(), error.to_string()))?;
        #[cfg(test)]
        if let Some(hook) = &self.settings.after_capture {
            hook();
        }
        // Only local work counts against the capture's deadline: the copy
        // now, a full check a replacement may still need, and the stored
        // file prepared below. Time spent asking the target belongs to the
        // transport's.
        let mut capture_left = self
            .settings
            .capture_deadline
            .saturating_sub(capture_started.elapsed());
        let captured_at = capture.manifest.capture_started_at;
        self.report.capture_check = Some(capture.check);
        self.stage = Some((capture, None));

        // Step 6: skip the upload only for the same bytes, in the same
        // format, at the same target, confirmed there now.
        let equal_copy = state.newest_receipt.as_ref().and_then(|newest| {
            let capture = &self.stage.as_ref().expect("captured above").0.manifest;
            (newest.target_identity == self.target.identity
                && newest.manifest.capture.format_identity == capture.format_identity
                && newest.sha256 == capture.sha256)
                .then(|| newest.manifest.clone())
        });
        if let Some(manifest) = equal_copy {
            let copy = manifest.copy.clone();
            let target = self.target.clone();
            #[cfg(test)]
            let before_confirm = self.settings.before_confirm.clone();
            let confirmation = self
                .transport
                .run(move |left| {
                    #[cfg(test)]
                    if let Some(hook) = before_confirm {
                        hook();
                    }
                    target.adapter(left).confirm(&target.project, &manifest)
                })
                .map_err(|worker| {
                    Failure::deadline(
                        worker,
                        self.settings.transport_deadline,
                        "the confirmation of the newest copy",
                    )
                })?;
            #[cfg(test)]
            if let Some(hook) = &self.settings.after_confirm {
                hook();
            }
            match confirmation {
                Confirmation::Confirmed => {
                    state.observed_equal_at = Some(captured_at);
                    state.confirm_newest(Utc::now());
                    return Ok(AttemptOutcome::Unchanged);
                }
                Confirmation::Unknown { reason }
                | Confirmation::Unreachable { reason }
                | Confirmation::TimedOut { reason } => {
                    return Err(Failure::new(
                        "backup_target_unconfirmed",
                        format!(
                            "the target could not say whether it holds the newest copy: {reason}"
                        ),
                    ));
                }
                Confirmation::Missing { reason } => {
                    // Recorded before a replacement is prepared, so the
                    // copy stops qualifying even if the replacement fails.
                    state.mark_newest_missing(Utc::now(), reason.clone());
                    self.save(state)?;
                    self.report.warnings.push(format!(
                        "the newest copy {copy} is no longer at the target ({reason}); a new one is put"
                    ));
                }
            }
        }

        // A copy that becomes a copy of its own is checked in full first, so
        // its manifest never names a check that was not run for it.
        let checking = Instant::now();
        if let Some((capture, _)) = &mut self.stage
            && capture.check != CaptureCheck::Full
        {
            capture
                .check_in_full(self.project, capture_left)
                .map_err(|error| Failure::new(error.code(), error.to_string()))?;
            self.report.capture_check = Some(capture.check);
        }
        capture_left = capture_left.saturating_sub(checking.elapsed());

        // The stored file is prepared in the stage, still within the
        // capture's time.
        let attempt_id = uuid::Uuid::now_v7();
        #[cfg(test)]
        if let Some(hook) = &self.settings.before_prepare {
            hook();
        }
        let (staged, manifest) = {
            let capture = &self.stage.as_ref().expect("captured above").0;
            (capture.staged.clone(), capture.manifest.clone())
        };
        let (attempt, stored) = self
            .target
            .adapter(self.settings.transport_deadline)
            .prepare_until(
                &staged,
                &manifest,
                attempt_id,
                // A capture deadline too far off for the clock to represent
                // sets no limit, as it does for the capture itself.
                Instant::now().checked_add(capture_left),
            )?;
        if let Some((_, prepared)) = &mut self.stage {
            *prepared = Some(stored.clone());
        }
        #[cfg(test)]
        super::halt::at(super::halt::Stage::Prepared);

        // The attempt is recorded before anything is put, so the next push
        // can find whatever this one leaves at the target.
        state.pending = Some(attempt.clone());
        self.save(state)?;
        self.report.pending = Some(attempt.manifest.copy.clone());
        #[cfg(test)]
        super::halt::at(super::halt::Stage::PendingRecorded);

        let target = self.target.clone();
        #[cfg(test)]
        let before_put = self.settings.before_put.clone();
        let put = attempt.clone();
        let receipt = self
            .transport
            .run(move |left| {
                #[cfg(test)]
                if let Some(hook) = before_put {
                    hook();
                }
                target.adapter(left).put(&target.project, &put, &stored)
            })
            .map_err(|worker| {
                Failure::deadline(worker, self.settings.transport_deadline, "the put")
            })??;
        #[cfg(test)]
        {
            super::halt::at(super::halt::Stage::PutReturned);
            self.put_done = true;
        }
        state.record_receipt(receipt, captured_at);
        state.pending = None;
        self.report.pending = None;
        Ok(AttemptOutcome::Uploaded)
    }

    /// Removes the stages earlier pushes of this project left behind, as one
    /// whose process ended at a deadline does. Only push captures into these
    /// directories, and only under this lock, so every one found here is a
    /// leftover. Only the files a push writes there are removed, and each
    /// directory without recursion, so anything else stays and is reported.
    fn remove_leftover_stages(&mut self) {
        let root = self
            .home
            .join(engram::backup::STAGE_DIRECTORY)
            .join(engram::project_digest(self.project));
        let entries = match fs::read_dir(&root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return,
            Err(error) => {
                self.report.warnings.push(format!(
                    "the stage directory {} could not be read: {error}",
                    root.display()
                ));
                return;
            }
        };
        for entry in entries {
            let entry = match entry {
                Ok(entry) => entry,
                Err(error) => {
                    self.report.warnings.push(format!(
                        "the stage directory {} could not be read: {error}",
                        root.display()
                    ));
                    return;
                }
            };
            let stage = entry.path();
            // A capture names its stage by a fresh id and creates it as a
            // directory. Anything else, a link above all, is not followed
            // and is left as it is.
            let is_stage = entry
                .file_type()
                .is_ok_and(|kind| kind.is_dir() && !kind.is_symlink())
                && entry
                    .file_name()
                    .to_str()
                    .is_some_and(|name| uuid::Uuid::parse_str(name).is_ok());
            if !is_stage {
                self.report.warnings.push(format!(
                    "{} is not a stage a push wrote and was left as it is",
                    stage.display()
                ));
                continue;
            }
            if let Err(error) = remove_stage(&stage) {
                self.report.warnings.push(format!(
                    "the leftover stage {} could not be removed: {error}",
                    stage.display()
                ));
            }
        }
    }

    /// Step 2: resolves an attempt an earlier push left pending.
    fn resolve_pending(&mut self, state: &mut TargetState) -> Result<(), Failure> {
        let Some(pending) = state.pending.clone() else {
            return Ok(());
        };
        if pending.manifest.target_identity != self.target.identity {
            // Made for another target: it is history, and nothing at either
            // target is touched for it.
            self.report.set_aside = Some(pending.manifest.copy.clone());
            state.set_aside.push(pending);
            state.pending = None;
            return self.save(state);
        }
        self.report.pending = Some(pending.manifest.copy.clone());
        let target = self.target.clone();
        let attempt = pending.clone();
        #[cfg(test)]
        let before_resolve = self.settings.before_resolve.clone();
        let (reconciled, confirmation) = self
            .transport
            .run(move |left| {
                #[cfg(test)]
                if let Some(hook) = before_resolve {
                    hook();
                }
                let adapter = target.adapter(left);
                let reconciled = adapter.reconcile(&target.project, &attempt);
                let confirmation = match reconciled {
                    Reconciled::Complete | Reconciled::Completed => {
                        Some(adapter.confirm(&target.project, &attempt.manifest))
                    }
                    _ => None,
                };
                (reconciled, confirmation)
            })
            .map_err(|worker| {
                Failure::deadline(
                    worker,
                    self.settings.transport_deadline,
                    "the resolution of the pending attempt",
                )
            })?;
        #[cfg(test)]
        super::halt::at(super::halt::Stage::Resolving);
        let undecided = confirmation
            .as_ref()
            .and_then(Confirmation::undecided)
            .map(str::to_owned);
        let missing = match (reconciled, undecided, confirmation) {
            (Reconciled::Unknown { reason }, _, _) | (_, Some(reason), _) => {
                return Err(Failure::new(
                    "backup_pending_unresolved",
                    format!(
                        "the pending attempt {} could not be resolved: {reason}",
                        pending.manifest.copy
                    ),
                ));
            }
            (_, None, Some(Confirmation::Confirmed)) => false,
            (Reconciled::Removed | Reconciled::Absent, None, _)
            | (_, None, Some(Confirmation::Missing { .. })) => true,
            (_, None, Some(_)) | (Reconciled::Complete | Reconciled::Completed, None, None) => {
                unreachable!(
                    "an undecided confirmation was matched above; a complete attempt is confirmed"
                )
            }
        };
        if missing {
            // It never arrived: only its own recorded files are removed.
            let target = self.target.clone();
            let attempt = pending.clone();
            self.transport
                .run(move |left| {
                    target
                        .adapter(left)
                        .remove_attempt(&target.project, &attempt)
                })
                .map_err(|worker| {
                    Failure::deadline(
                        worker,
                        self.settings.transport_deadline,
                        "the removal of the abandoned attempt",
                    )
                })??;
            self.report.dropped = Some(pending.manifest.copy.clone());
        } else {
            let receipt = receipt_for(&pending, &self.target.identity);
            state.record_receipt(receipt, pending.manifest.capture.capture_started_at);
            self.report.recovered = Some(pending.manifest.copy.clone());
        }
        state.pending = None;
        self.report.pending = None;
        self.save(state)
    }

    /// Step 8: removes the stage and records the attempt.
    fn finish(
        mut self,
        state: &mut TargetState,
        result: Result<AttemptOutcome, Failure>,
    ) -> (KindReport, Option<JoinHandle<()>>) {
        let worker_left = matches!(&result, Err(failure) if failure.abandoned.is_some());
        if worker_left {
            // A worker that passed its deadline may still be reading the
            // stored file; its stage is left to the next push, which removes
            // it under the lock before it captures anything.
            if let Some((capture, _)) = self.stage.take() {
                self.report.warnings.push(format!(
                    "the local stage {} is left for the next push to remove",
                    capture.stage().display()
                ));
            }
        }
        if let Some((capture, stored)) = self.stage.take() {
            if let Some(stored) = stored
                && let Err(error) = fs::remove_file(&stored)
                && error.kind() != io::ErrorKind::NotFound
            {
                self.report.warnings.push(format!(
                    "the stored file {} could not be removed: {error}",
                    stored.display()
                ));
            }
            let stage_dir = capture.stage().to_path_buf();
            if let Err(error) = capture.discard() {
                self.report.warnings.push(format!(
                    "the local stage {} could not be removed: {error}",
                    stage_dir.display()
                ));
            }
        }
        let ended_at = Utc::now();
        let mut abandoned = None;
        let outcome = match result {
            Ok(outcome) => {
                self.report.outcome = match outcome {
                    AttemptOutcome::Uploaded => Outcome::Uploaded,
                    AttemptOutcome::Unchanged => Outcome::Unchanged,
                    AttemptOutcome::Failed => Outcome::Failed,
                };
                state.last_attempt = Some(LastAttempt {
                    started_at: self.started_at,
                    ended_at,
                    outcome,
                    code: None,
                    message: None,
                });
                outcome
            }
            Err(failure) => {
                abandoned = failure.abandoned;
                self.report.outcome = Outcome::Failed;
                self.report.code = Some(failure.code.clone());
                self.report.message = Some(failure.message.clone());
                state.last_attempt = Some(LastAttempt {
                    started_at: self.started_at,
                    ended_at,
                    outcome: AttemptOutcome::Failed,
                    code: Some(failure.code),
                    message: Some(failure.message),
                });
                AttemptOutcome::Failed
            }
        };
        match self.save(state) {
            Ok(()) if outcome != AttemptOutcome::Failed => {
                // Step 7, only once the receipt is recorded.
                abandoned = self.retain(state);
            }
            Ok(()) => {}
            Err(failure) => {
                if outcome == AttemptOutcome::Failed {
                    self.report.warnings.push(format!(
                        "the failed attempt could not be recorded: {}",
                        failure.message
                    ));
                } else {
                    // The copy is at the target, but nothing here says so: the
                    // attempt stays pending in the earlier state for the next
                    // push.
                    self.report.outcome = Outcome::Failed;
                    self.report.code = Some(failure.code);
                    self.report.message = Some(failure.message);
                }
            }
        }
        // The report shows what is recorded, which a failed write leaves at
        // the state last written.
        self.report.receipt.clone_from(&self.saved.newest_receipt);
        self.report.observed_equal_at = self.saved.observed_equal_at;
        self.report.pending = self
            .saved
            .pending
            .as_ref()
            .map(|attempt| attempt.manifest.copy.clone());
        (self.report, abandoned)
    }
}

impl Push<'_> {
    /// Step 7: removes from the target the copies beyond the retention count,
    /// oldest first. Only copies whose receipts this home recorded for the
    /// current target identity count, and the newest is never removed, so a
    /// copy from another home or an earlier identity is never touched. A
    /// failed removal is a warning, and the copy stays in the ledger for a
    /// later push. Returns the worker of a removal that passed the transport
    /// deadline.
    fn retain(&mut self, state: &mut TargetState) -> Option<JoinHandle<()>> {
        let newest = state.newest_receipt.as_ref()?.manifest.copy.clone();
        let own: Vec<_> = state
            .receipts
            .iter()
            .filter(|receipt| receipt.target_identity == self.target.identity)
            .collect();
        let excess = own.len().saturating_sub(self.keep);
        let doomed: Vec<_> = own
            .into_iter()
            .filter(|receipt| receipt.manifest.copy != newest)
            .take(excess)
            .map(|receipt| receipt.manifest.clone())
            .collect();
        let mut abandoned = None;
        let mut removed = Vec::new();
        for manifest in doomed {
            let copy = manifest.copy.clone();
            let target = self.target.clone();
            #[cfg(test)]
            let before_remove = self.settings.before_remove.clone();
            match self.transport.run(move |left| {
                #[cfg(test)]
                if let Some(hook) = before_remove {
                    hook();
                }
                target.adapter(left).remove_copy(&target.project, &manifest)
            }) {
                Ok(Ok(())) => {
                    #[cfg(test)]
                    super::halt::at(super::halt::Stage::CopyRemoved);
                    removed.push(copy);
                }
                Ok(Err(error)) => self.report.warnings.push(format!(
                    "the copy {copy} beyond the retention count could not be removed: {error}"
                )),
                Err(worker) => {
                    self.report.warnings.push(format!(
                        "removing the copy {copy} passed the transport deadline; it is retried by a later push"
                    ));
                    abandoned = worker;
                    break;
                }
            }
        }
        if !removed.is_empty() {
            state
                .receipts
                .retain(|receipt| !removed.contains(&receipt.manifest.copy));
            if let Err(failure) = self.save(state) {
                self.report.warnings.push(format!(
                    "the removal of {} copies could not be recorded: {}",
                    removed.len(),
                    failure.message
                ));
            }
        }
        self.report.removed = removed;
        abandoned
    }
}

/// Removes the files a push writes in one stage directory, the staged copy
/// with its sidecars and the stored file, and then the directory itself,
/// without recursion.
fn remove_stage(stage: &Path) -> io::Result<()> {
    for entry in fs::read_dir(stage)? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        let written_by_push = matches!(
            name.as_ref(),
            "store.db" | "store.db-wal" | "store.db-shm" | "store.db-journal"
        ) || name.strip_suffix(".db.gz").is_some_and(|copy| {
            !copy.is_empty()
                && copy
                    .chars()
                    .all(|character| character.is_ascii_alphanumeric() || character == '-')
        });
        if written_by_push && entry.file_type()?.is_file() {
            fs::remove_file(entry.path())?;
        }
    }
    fs::remove_dir(stage)
}

/// The receipt for a pending attempt that the target confirmed.
fn receipt_for(attempt: &Attempt, identity: &ObjectId) -> BackupReceipt {
    BackupReceipt {
        sha256: attempt.manifest.capture.sha256.clone(),
        target_identity: identity.clone(),
        at: Utc::now(),
        acknowledgement: engram::backup::record::Acknowledgement::ReadBack,
        off_host: engram::backup::record::OffHost::Asserted,
        manifest: attempt.manifest.clone(),
    }
}

#[cfg(test)]
mod tests;
