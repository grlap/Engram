//! What `engram backup status` and the doctor block report: the mode, never
//! without what backs it, and per configured kind the recorded evidence as
//! of its times. It reads only the files under the Engram home and the store,
//! through the collector, and contacts no target.

use std::{fmt::Write as _, path::Path};

use chrono::{DateTime, Utc};
use serde::Serialize;

use super::{
    CopyKind,
    freshness::{
        AcceptedFormats, Collected, CutUnavailable, KindRecords, Mode, Reason, collect, kind_reason,
    },
    record::{Acknowledgement, LastAttempt},
    target::{AdapterKind, Statement},
};
use crate::{ObjectId, ProjectId, WorkGraphSnapshotCut};

/// The schema of the status receipt.
pub const STATUS_SCHEMA_VERSION: u32 = 1;

/// What a directory target's off-host assurance reads, exactly.
pub const DIRECTORY_OFF_HOST: &str = "off-host asserted; not verified";

/// What a `store` copy restores.
pub const STORE_RESTORES: &str = "the same store at the copy's cut";

/// The status of a project's backups.
#[derive(Clone, Debug, Serialize)]
pub struct BackupStatus {
    pub schema_version: u32,
    /// The mode with what backs it: never one without the other.
    pub durability: Durability,
    /// The store's current cut, or why it could not be read.
    pub store_cut: Option<WorkGraphSnapshotCut>,
    pub store_cut_unavailable: Option<CutUnavailable>,
    /// The build that reports this status.
    pub running_build: Option<ObjectId>,
    /// When the status was evaluated.
    pub as_of: DateTime<Utc>,
    pub kinds: Vec<KindStatus>,
}

/// The mode and, for each kind that qualifies, its off-host assurance and
/// what it restores.
#[derive(Clone, Debug, Serialize)]
pub struct Durability {
    pub mode: Mode,
    pub off_host: Vec<OffHostClaim>,
}

/// One qualifying kind's off-host assurance.
#[derive(Clone, Debug, Serialize)]
pub struct OffHostClaim {
    pub kind: CopyKind,
    pub off_host: &'static str,
    pub restores: &'static str,
}

/// One kind's verdict and recorded evidence.
#[derive(Clone, Debug, Serialize)]
pub struct KindStatus {
    pub kind: CopyKind,
    pub qualifies: bool,
    /// The backup_* code of the first reason it does not qualify.
    pub reason: Option<&'static str>,
    /// What made a record unusable, for an unreadable one.
    pub unreadable: Option<String>,
    pub target: Option<TargetStatus>,
}

/// A configured kind's evidence, as recorded.
#[derive(Clone, Debug, Serialize)]
pub struct TargetStatus {
    pub adapter: AdapterKind,
    pub location: String,
    pub identity: ObjectId,
    pub window_hours: u32,
    pub off_host: &'static str,
    pub disclosure_authorized: Statement,
    pub off_host_asserted: Option<Statement>,
    pub copy: Option<CopyStatus>,
    pub pending: Option<PendingStatus>,
    pub last_attempt: Option<LastAttempt>,
}

/// The newest receipt's copy.
#[derive(Clone, Debug, Serialize)]
pub struct CopyStatus {
    pub copy: String,
    pub acknowledgement: Acknowledgement,
    /// The identity of the target that issued the receipt.
    pub target_identity: ObjectId,
    /// Whether that identity is an earlier one, not the configured target's.
    pub for_earlier_target: bool,
    pub received_at: DateTime<Utc>,
    pub capture_started_at: DateTime<Utc>,
    pub capture_age_seconds: i64,
    /// When the store's content was last observed in this copy.
    pub observed_equal_at: Option<DateTime<Utc>>,
    pub cut: WorkGraphSnapshotCut,
    /// How far the store has moved since the copy's cut, position by
    /// position; absent when the store's cut could not be read.
    pub store_moved: Option<CutMovement>,
    pub last_confirmation: Option<DateTime<Utc>>,
    pub missing: Option<MissingStatus>,
    /// The build that captured and checked the copy, shown only when it is
    /// not the running build.
    pub checking_build: Option<ObjectId>,
    /// Whether the copy's manifest names no checking build, so that it
    /// cannot be shown to be the running one.
    pub checking_build_unknown: bool,
}

/// How far each position of the store's cut is past the copy's.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
pub struct CutMovement {
    pub work_feed: i64,
    pub project_memory: i64,
}

/// A recorded finding that the target no longer holds the copy.
#[derive(Clone, Debug, Serialize)]
pub struct MissingStatus {
    pub at: DateTime<Utc>,
    pub reason: String,
}

/// An attempt recorded before its put and not yet resolved.
#[derive(Clone, Debug, Serialize)]
pub struct PendingStatus {
    pub copy: String,
    pub capture_started_at: DateTime<Utc>,
    pub target_identity: ObjectId,
    /// Whether it was recorded for an earlier identity of the target.
    pub for_earlier_target: bool,
}

/// The running build's identities, as the status needs them.
#[derive(Clone, Debug)]
pub struct RunningBuild {
    pub formats: AcceptedFormats,
    pub fingerprint: Option<ObjectId>,
}

impl RunningBuild {
    /// The identities of this process's build.
    #[must_use]
    pub fn current() -> Self {
        Self {
            formats: AcceptedFormats {
                store: crate::storage::running_schema_reference().ok(),
            },
            fingerprint: crate::build_identity::current().build_fingerprint.clone(),
        }
    }
}

/// Reads the project's backup evidence and evaluates it. The clock is read
/// after the records, so a push that records something meanwhile never
/// looks as if it lay in the future.
#[must_use]
pub fn backup_status(home: &Path, project: &ProjectId, database: &Path) -> BackupStatus {
    let collected = collect(home, project, database);
    let running = RunningBuild::current();
    build_status(&collected, &running, Utc::now())
}

/// The status of `collected` evidence for a build at `now`.
#[must_use]
pub fn build_status(
    collected: &Collected,
    running: &RunningBuild,
    now: DateTime<Utc>,
) -> BackupStatus {
    let (store_cut, store_cut_unavailable) = match &collected.cut {
        Ok(cut) => (Some(cut.clone()), None),
        Err(unavailable) => (None, Some(unavailable.clone())),
    };
    let kinds: Vec<_> = collected
        .kinds
        .iter()
        .map(|(kind, records)| {
            kind_status(
                *kind,
                records,
                &running.formats,
                running,
                store_cut.as_ref(),
                now,
            )
        })
        .collect();
    let off_host: Vec<_> = kinds
        .iter()
        .filter(|kind| kind.qualifies)
        .map(|kind| OffHostClaim {
            kind: kind.kind,
            off_host: kind
                .target
                .as_ref()
                .map_or(DIRECTORY_OFF_HOST, |target| target.off_host),
            restores: restores(kind.kind),
        })
        .collect();
    let mode = if off_host.is_empty() {
        Mode::Local
    } else {
        Mode::LocalBackedUp
    };
    BackupStatus {
        schema_version: STATUS_SCHEMA_VERSION,
        durability: Durability { mode, off_host },
        store_cut,
        store_cut_unavailable,
        running_build: running.fingerprint.clone(),
        as_of: now,
        kinds,
    }
}

const fn restores(kind: CopyKind) -> &'static str {
    match kind {
        CopyKind::Store => STORE_RESTORES,
    }
}

pub(crate) const fn off_host_text(adapter: AdapterKind) -> &'static str {
    match adapter {
        AdapterKind::Directory => DIRECTORY_OFF_HOST,
    }
}

fn kind_status(
    kind: CopyKind,
    records: &KindRecords,
    formats: &AcceptedFormats,
    running: &RunningBuild,
    store_cut: Option<&WorkGraphSnapshotCut>,
    now: DateTime<Utc>,
) -> KindStatus {
    let reason = kind_reason(kind, records, formats, now);
    let mut status = KindStatus {
        kind,
        qualifies: reason.is_none(),
        reason: reason.map(Reason::code),
        unreadable: None,
        target: None,
    };
    match records {
        KindRecords::Unreadable { path, reason } => {
            status.unreadable = Some(format!("{}: {reason}", path.display()));
        }
        KindRecords::NotConfigured => {}
        KindRecords::Configured {
            config,
            identity,
            state,
        } => {
            let copy = state.newest_receipt.as_ref().map(|receipt| {
                let capture = &receipt.manifest.capture;
                CopyStatus {
                    copy: receipt.manifest.copy.clone(),
                    acknowledgement: receipt.acknowledgement,
                    target_identity: receipt.target_identity.clone(),
                    for_earlier_target: &receipt.target_identity != identity,
                    received_at: receipt.at,
                    capture_started_at: capture.capture_started_at,
                    capture_age_seconds: (now - capture.capture_started_at).num_seconds(),
                    observed_equal_at: state.observed_equal_at,
                    cut: capture.cut.clone(),
                    store_moved: store_cut.and_then(|current| {
                        Some(CutMovement {
                            work_feed: current.work_feed.checked_sub(capture.cut.work_feed)?,
                            project_memory: current
                                .project_memory
                                .checked_sub(capture.cut.project_memory)?,
                        })
                    }),
                    last_confirmation: state
                        .last_confirmation
                        .as_ref()
                        .map(|confirmed| confirmed.at),
                    missing: state.missing_copy.as_ref().map(|missing| MissingStatus {
                        at: missing.at,
                        reason: missing.reason.clone(),
                    }),
                    checking_build: capture
                        .build_fingerprint
                        .clone()
                        .filter(|checking| Some(checking) != running.fingerprint.as_ref()),
                    checking_build_unknown: capture.build_fingerprint.is_none(),
                }
            });
            status.target = Some(TargetStatus {
                adapter: config.adapter,
                location: config.dir.clone(),
                identity: identity.clone(),
                window_hours: config.window_hours,
                off_host: off_host_text(config.adapter),
                disclosure_authorized: config.disclosure_authorized.clone(),
                off_host_asserted: config.off_host_asserted.clone(),
                copy,
                pending: state.pending.as_ref().map(|attempt| PendingStatus {
                    copy: attempt.manifest.copy.clone(),
                    capture_started_at: attempt.manifest.capture.capture_started_at,
                    target_identity: attempt.manifest.target_identity.clone(),
                    for_earlier_target: &attempt.manifest.target_identity != identity,
                }),
                last_attempt: state.last_attempt.clone(),
            });
        }
    }
    status
}

/// The status as text: the mode with what backs it first, then each kind.
#[must_use]
pub fn render_status(status: &BackupStatus) -> String {
    let mut text = String::new();
    let _ = writeln!(text, "{}", durability_line(&status.durability));
    if let Some(unavailable) = &status.store_cut_unavailable {
        let _ = writeln!(
            text,
            "store cut unavailable: {}: {}",
            unavailable.code, unavailable.message
        );
    }
    for kind in &status.kinds {
        render_kind(&mut text, kind, status);
    }
    text
}

/// The one line that names the mode, always with its off-host assurance.
#[must_use]
pub fn durability_line(durability: &Durability) -> String {
    if durability.off_host.is_empty() {
        return format!(
            "backup mode: {} (no copy qualifies; nothing is known to be held off this host)",
            durability.mode.as_str()
        );
    }
    let claims: Vec<_> = durability
        .off_host
        .iter()
        .map(|claim| {
            format!(
                "{} copy: {}; restores {}",
                claim.kind.as_str(),
                claim.off_host,
                claim.restores
            )
        })
        .collect();
    format!(
        "backup mode: {} ({})",
        durability.mode.as_str(),
        claims.join("; ")
    )
}

fn render_kind(text: &mut String, kind: &KindStatus, status: &BackupStatus) {
    let verdict = kind.reason.map_or_else(
        || "qualifies".to_owned(),
        |code| format!("does not qualify: {code}"),
    );
    let _ = writeln!(text, "{}: {verdict}", kind.kind.as_str());
    if let Some(unreadable) = &kind.unreadable {
        let _ = writeln!(text, "  unreadable record: {unreadable}");
    }
    let Some(target) = &kind.target else {
        return;
    };
    let _ = writeln!(
        text,
        "  target: {} {} (window {} h)",
        target.adapter.as_str(),
        target.location,
        target.window_hours
    );
    let _ = writeln!(
        text,
        "  disclosure authorized by {} at {} (asserted)",
        target.disclosure_authorized.by,
        target.disclosure_authorized.at.to_rfc3339()
    );
    match &target.off_host_asserted {
        Some(statement) => {
            let _ = writeln!(
                text,
                "  {}: stated by {} at {}",
                target.off_host,
                statement.by,
                statement.at.to_rfc3339()
            );
        }
        None => {
            let _ = writeln!(text, "  {}", target.off_host);
        }
    }
    match &target.copy {
        None => {
            let _ = writeln!(text, "  no copy confirmed yet");
        }
        Some(copy) => render_copy(text, copy, status),
    }
    if let Some(pending) = &target.pending {
        let _ = writeln!(
            text,
            "  pending attempt{}: {} (captured {})",
            earlier(pending.for_earlier_target),
            pending.copy,
            pending.capture_started_at.to_rfc3339()
        );
    }
    if let Some(attempt) = &target.last_attempt {
        let outcome = match attempt.outcome {
            super::record::AttemptOutcome::Uploaded => "uploaded".to_owned(),
            super::record::AttemptOutcome::Unchanged => "unchanged".to_owned(),
            super::record::AttemptOutcome::Failed => match &attempt.code {
                Some(code) => format!(
                    "failed: {code}: {}",
                    attempt.message.as_deref().unwrap_or("")
                ),
                None => "failed (no code recorded)".to_owned(),
            },
        };
        let _ = writeln!(
            text,
            "  last attempt: {outcome} (started {}, ended {})",
            attempt.started_at.to_rfc3339(),
            attempt.ended_at.to_rfc3339()
        );
    }
}

fn render_copy(text: &mut String, copy: &CopyStatus, status: &BackupStatus) {
    let acknowledgement = match copy.acknowledgement {
        Acknowledgement::ReadBack => "read back",
    };
    let _ = writeln!(
        text,
        "  copy{}: {} ({acknowledgement} at {})",
        earlier(copy.for_earlier_target),
        copy.copy,
        copy.received_at.to_rfc3339()
    );
    let _ = writeln!(
        text,
        "  capture started {} (age {})",
        copy.capture_started_at.to_rfc3339(),
        age(copy.capture_age_seconds)
    );
    if let Some(observed) = copy.observed_equal_at {
        let _ = writeln!(
            text,
            "  store content last observed in this copy at {}",
            observed.to_rfc3339()
        );
    }
    let moved = match (copy.store_moved, &status.store_cut_unavailable) {
        (Some(moved), _) if moved.work_feed < 0 || moved.project_memory < 0 => format!(
            "; the store is behind the copy: work feed {:+}, memory {:+}",
            moved.work_feed, moved.project_memory
        ),
        (Some(moved), _) => format!(
            "; the store has moved {} work-feed and {} memory positions since",
            moved.work_feed, moved.project_memory
        ),
        (None, Some(unavailable)) => format!(
            "; the store's own cut could not be read: {}",
            unavailable.code
        ),
        (None, None) => String::new(),
    };
    let _ = writeln!(
        text,
        "  cut: work feed {}, memory {}{moved}",
        copy.cut.work_feed, copy.cut.project_memory
    );
    match copy.last_confirmation {
        Some(at) => {
            let _ = writeln!(text, "  last confirmed at {}", at.to_rfc3339());
        }
        None => {
            let _ = writeln!(text, "  never confirmed");
        }
    }
    if let Some(missing) = &copy.missing {
        let _ = writeln!(
            text,
            "  found missing at {}: {}",
            missing.at.to_rfc3339(),
            missing.reason
        );
    }
    if let Some(checking) = &copy.checking_build {
        let comparison = if status.running_build.is_some() {
            "not the running build"
        } else {
            "the running build could not name itself"
        };
        let _ = writeln!(
            text,
            "  checked by build {}, {comparison}",
            checking.as_str()
        );
    } else if copy.checking_build_unknown {
        let _ = writeln!(text, "  checked by a build that did not name itself");
    }
}

/// The label of a record made for an earlier identity of the target.
const fn earlier(for_earlier_target: bool) -> &'static str {
    if for_earlier_target {
        " (recorded for an earlier target identity)"
    } else {
        ""
    }
}

fn age(seconds: i64) -> String {
    if seconds < 0 {
        return format!("{seconds} s, in the future");
    }
    let hours = seconds / 3600;
    let minutes = (seconds % 3600) / 60;
    if hours > 0 {
        format!("{hours} h {minutes} min")
    } else {
        format!("{minutes} min")
    }
}

#[cfg(test)]
mod tests;
