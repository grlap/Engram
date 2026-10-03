//! `engram backup status --check-target`: asks each configured target to
//! confirm the newest copy under a deadline, and records what it found only
//! under the push lock and only when the newest receipt is still the one it
//! checked. A target that cannot be reached, or a read that passes its
//! deadline, is reported beside the recorded evidence and changes nothing.

use std::{
    fs,
    path::{Path, PathBuf},
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread::JoinHandle,
    time::Duration,
};

use chrono::{DateTime, Utc};
use engram::{
    ProjectId,
    backup::{
        CopyKind,
        freshness::{KindRecords, kind_records},
        target::{PushLock, RecordPaths, TargetError, read_for_push, write_state},
    },
};
use serde::Serialize;

use super::{
    adapter::{BackupAdapter, Confirmation},
    push::{FreeSpace, Target, Transport},
};

/// How long a check may take when no deadline is given.
pub(crate) const DEFAULT_CHECK_DEADLINE: Duration = Duration::from_mins(10);

/// How a check runs.
#[derive(Clone)]
pub(crate) struct CheckSettings {
    pub deadline: Duration,
    pub target_free_space: FreeSpace,
    /// Runs on the worker before it tries to reach the target, so a test can
    /// hold a worker that never reached it past the deadline.
    #[cfg(test)]
    pub before_reach: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Runs on the worker once it has reached the target, so a test can hold
    /// the read past its deadline.
    #[cfg(test)]
    pub after_reach: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Runs when the deadline has passed with the worker still running, just
    /// before the check tells whether it reached the target, so a test can
    /// wait for a milestone the worker reaches late.
    #[cfg(test)]
    pub before_classify: Option<Arc<dyn Fn() + Send + Sync>>,
    /// Runs after the target answered and before the result is recorded, so
    /// a test can let a push record a newer receipt in between.
    #[cfg(test)]
    pub before_record: Option<Arc<dyn Fn() + Send + Sync>>,
}

impl CheckSettings {
    pub(crate) fn new(deadline: Duration) -> Self {
        Self {
            deadline,
            target_free_space: engram::backup::available_space,
            #[cfg(test)]
            before_reach: None,
            #[cfg(test)]
            after_reach: None,
            #[cfg(test)]
            before_classify: None,
            #[cfg(test)]
            before_record: None,
        }
    }
}

/// What a check of one kind found.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum CheckOutcome {
    /// The target holds exactly the newest copy.
    Confirmed,
    /// The target no longer holds the copy, or holds other bytes.
    Missing,
    /// The configured location cannot be reached.
    Unreachable,
    /// The target was reached, and the read passed its deadline before it
    /// could say; nothing against the copy.
    TimedOut,
    /// The target was reached but could not be read well enough to say, or,
    /// for a caller that gave a zero deadline, no request was started.
    Unknown,
    /// No target is configured, or it has no copy of its own to check.
    NothingToCheck,
}

/// One kind's check, as the command reports it.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct CheckReport {
    pub kind: CopyKind,
    pub outcome: CheckOutcome,
    /// The typed code of an outcome that is not a confirmation.
    pub code: Option<&'static str>,
    pub reason: Option<String>,
    /// The copy that was checked.
    pub copy: Option<String>,
    /// Whether the result was recorded.
    pub recorded: bool,
    /// Why a confirmation or a missing finding was not recorded.
    pub not_recorded: Option<String>,
}

impl CheckReport {
    fn new(kind: CopyKind, outcome: CheckOutcome) -> Self {
        Self {
            kind,
            outcome,
            code: None,
            reason: None,
            copy: None,
            recorded: false,
            not_recorded: None,
        }
    }
}

/// The checks of every kind, and a worker left running past its deadline.
/// The caller ends the process rather than wait for that worker.
pub(crate) struct CheckRun {
    pub reports: Vec<CheckReport>,
    pub abandoned: Option<JoinHandle<()>>,
}

/// Checks every configured kind of `project` under `home` at `now`.
pub(crate) fn check_targets(
    home: &Path,
    project: &ProjectId,
    settings: &CheckSettings,
    now: DateTime<Utc>,
) -> CheckRun {
    let mut reports = Vec::new();
    let mut abandoned = None;
    for kind in CopyKind::ALL {
        let (report, worker) = check_kind(home, project, kind, settings, now);
        reports.push(report);
        if worker.is_some() {
            abandoned = worker;
            break;
        }
    }
    CheckRun { reports, abandoned }
}

fn check_kind(
    home: &Path,
    project: &ProjectId,
    kind: CopyKind,
    settings: &CheckSettings,
    now: DateTime<Utc>,
) -> (CheckReport, Option<JoinHandle<()>>) {
    let KindRecords::Configured {
        config,
        identity,
        state,
    } = kind_records(home, project, kind)
    else {
        return (CheckReport::new(kind, CheckOutcome::NothingToCheck), None);
    };
    let Some(receipt) = state
        .newest_receipt
        .clone()
        .filter(|receipt| receipt.target_identity == identity)
    else {
        return (CheckReport::new(kind, CheckOutcome::NothingToCheck), None);
    };
    let target = Target {
        root: PathBuf::from(&config.dir),
        identity: identity.clone(),
        project: project.clone(),
        free_space: settings.target_free_space,
    };
    let reached = Arc::new(AtomicBool::new(false));
    let worker_reached = Arc::clone(&reached);
    #[cfg(test)]
    let before_reach = settings.before_reach.clone();
    #[cfg(test)]
    let after_reach = settings.after_reach.clone();
    let manifest = receipt.manifest.clone();
    let mut transport = Transport {
        budget: settings.deadline,
        used: Duration::ZERO,
    };
    let answer = transport.run(move |left| {
        #[cfg(test)]
        if let Some(hook) = before_reach {
            hook();
        }
        // The milestone that tells a target that was reached from one that
        // never answered, when the deadline passes before the read is done.
        if fs::metadata(&target.root).is_ok_and(|metadata| metadata.is_dir()) {
            worker_reached.store(true, Ordering::SeqCst);
            #[cfg(test)]
            if let Some(hook) = after_reach {
                hook();
            }
        }
        target.adapter(left).confirm(&target.project, &manifest)
    });
    let mut report = CheckReport::new(kind, CheckOutcome::Unknown);
    report.copy = Some(receipt.manifest.copy.clone());
    let confirmation = match answer {
        Ok(confirmation) => confirmation,
        Err(None) => {
            // Only a zero deadline starts no request, and the command refuses
            // one, so only a direct caller, such as a test, gets here. The
            // target was never asked, so nothing is said about it either way.
            classify(
                &mut report,
                &Confirmation::Unknown {
                    reason: "no request was started because the check deadline was zero".into(),
                },
            );
            return (report, None);
        }
        Err(Some(worker)) => {
            #[cfg(test)]
            if let Some(hook) = &settings.before_classify {
                hook();
            }
            let worker = Some(worker);
            let reason = format!("the check passed its deadline of {:?}", settings.deadline);
            let confirmation = if reached.load(Ordering::SeqCst) {
                Confirmation::TimedOut { reason }
            } else {
                Confirmation::Unreachable { reason }
            };
            classify(&mut report, &confirmation);
            return (report, worker);
        }
    };
    classify(&mut report, &confirmation);
    if matches!(
        report.outcome,
        CheckOutcome::Confirmed | CheckOutcome::Missing
    ) {
        #[cfg(test)]
        if let Some(hook) = &settings.before_record {
            hook();
        }
        let checked = Checked {
            identity,
            receipt,
            last_confirmation: state.last_confirmation.clone(),
            missing_copy: state.missing_copy.clone(),
        };
        match record(home, project, kind, &checked, &confirmation, now) {
            Ok(()) => report.recorded = true,
            Err(why) => report.not_recorded = Some(why),
        }
    }
    (report, None)
}

/// The outcome and code of a target's answer.
fn classify(report: &mut CheckReport, confirmation: &Confirmation) {
    let (outcome, code, reason) = match confirmation {
        Confirmation::Confirmed => (CheckOutcome::Confirmed, None, None),
        Confirmation::Missing { reason } => (
            CheckOutcome::Missing,
            Some("backup_copy_missing"),
            Some(reason),
        ),
        Confirmation::Unreachable { reason } => (
            CheckOutcome::Unreachable,
            Some("backup_target_unreachable"),
            Some(reason),
        ),
        Confirmation::TimedOut { reason } => (
            CheckOutcome::TimedOut,
            Some("backup_check_timed_out"),
            Some(reason),
        ),
        Confirmation::Unknown { reason } => (
            CheckOutcome::Unknown,
            Some("backup_target_unconfirmed"),
            Some(reason),
        ),
    };
    report.outcome = outcome;
    report.code = code;
    report.reason = reason.cloned();
}

/// What the check read before it asked the target.
struct Checked {
    identity: engram::ObjectId,
    receipt: engram::backup::record::BackupReceipt,
    last_confirmation: Option<engram::backup::record::CopyConfirmed>,
    missing_copy: Option<engram::backup::record::CopyMissing>,
}

/// Records a confirmation or a missing finding, under the push lock, only
/// when the target, the newest receipt and its evidence are still what the
/// check read. Returns why nothing was recorded otherwise.
fn record(
    home: &Path,
    project: &ProjectId,
    kind: CopyKind,
    checked: &Checked,
    confirmation: &Confirmation,
    now: DateTime<Utc>,
) -> Result<(), String> {
    let paths = RecordPaths::new(home, project, kind);
    let lock = match PushLock::try_acquire(&paths) {
        Ok(lock) => lock,
        Err(TargetError::PushRunning { .. }) => {
            return Err("a push holds the lock; its own confirmation supersedes this one".into());
        }
        Err(error) => return Err(error.to_string()),
    };
    let records = read_for_push(&paths, project, kind, &lock)
        .map_err(|error| error.to_string())?
        .ok_or_else(|| "the target was cleared meanwhile".to_owned())?;
    let mut state = records.state;
    let unchanged = records.identity == checked.identity
        && state.newest_receipt.as_ref() == Some(&checked.receipt)
        && state.last_confirmation == checked.last_confirmation
        && state.missing_copy == checked.missing_copy;
    if !unchanged {
        return Err(
            "the newest receipt or its evidence changed while the target was checked".into(),
        );
    }
    match confirmation {
        Confirmation::Confirmed => state.confirm_newest(now),
        Confirmation::Missing { reason } => state.mark_newest_missing(now, reason.clone()),
        _ => return Err("only a confirmation or a missing copy is recorded".into()),
    }
    write_state(&paths, &lock, &state).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests;
