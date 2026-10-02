//! The freshness rule: when a project's store may be reported as
//! `local_backed_up`. One pure function decides it from the recorded
//! evidence, the running build's format identities and the clock; it reads
//! nothing and contacts no target. The collector beside it gathers that
//! evidence from the files under the Engram home and the store only.

use std::path::{Path, PathBuf};

use chrono::{DateTime, Duration, Utc};
use serde::Serialize;

use super::{
    CopyKind,
    target::{RecordPaths, TargetConfig, TargetError, TargetState, read_kind_records},
};
use crate::{ObjectId, ProjectId, SqliteStore, WorkGraphSnapshotCut};

/// The durability mode a project's store is reported in.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Mode {
    /// No kind qualifies: the store's only copy is on this host.
    Local,
    /// At least one kind qualifies under the rule.
    LocalBackedUp,
}

impl Mode {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Local => "local",
            Self::LocalBackedUp => "local_backed_up",
        }
    }
}

/// Why a kind does not qualify, in the order the rule checks them: the
/// first that applies is the one reported. It serializes as its code.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub enum Reason {
    /// The configuration or state file cannot be read or used.
    RecordUnreadable,
    /// No target for this kind.
    NotConfigured,
    /// A target, but no receipt yet.
    NeverConfirmed,
    /// The newest receipt is for another target identity.
    TargetChanged,
    /// The running build does not accept the copy's format identity.
    OtherFormat,
    /// A recorded time lies in the future.
    ClockInvalid,
    /// A check found that the target no longer holds the copy.
    CopyMissing,
    /// The target last confirmed the copy longer ago than the window.
    ConfirmationExpired,
    /// The store's content was last observed in the copy longer ago than the
    /// window.
    Stale,
}

impl Serialize for Reason {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(self.code())
    }
}

impl Reason {
    /// The stable code of this reason.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::RecordUnreadable => "backup_record_unreadable",
            Self::NotConfigured => "backup_not_configured",
            Self::NeverConfirmed => "backup_never_confirmed",
            Self::TargetChanged => "backup_target_changed",
            Self::OtherFormat => "backup_other_format",
            Self::ClockInvalid => "backup_clock_invalid",
            Self::CopyMissing => "backup_copy_missing",
            Self::ConfirmationExpired => "backup_confirmation_expired",
            Self::Stale => "backup_stale",
        }
    }
}

/// One kind's records as the rule reads them.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum KindRecords {
    /// A configuration or state file exists but this build cannot use it.
    Unreadable { path: PathBuf, reason: String },
    /// No target is configured for the kind.
    NotConfigured,
    /// A usable configuration with its derived identity, and its state.
    Configured {
        config: Box<TargetConfig>,
        identity: ObjectId,
        state: Box<TargetState>,
    },
}

/// The format identities the running build accepts, per kind.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct AcceptedFormats {
    /// The store schema reference a `store` copy must carry; `None` when the
    /// running build cannot name its own, which accepts no copy.
    pub store: Option<ObjectId>,
}

/// One kind's verdict.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct KindVerdict {
    pub kind: CopyKind,
    /// `None` when the kind qualifies.
    pub reason: Option<Reason>,
}

impl KindVerdict {
    #[must_use]
    pub const fn qualifies(&self) -> bool {
        self.reason.is_none()
    }
}

/// The mode and each kind's verdict.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Freshness {
    pub mode: Mode,
    pub kinds: Vec<KindVerdict>,
}

/// Applies the rule to every kind: the mode is `local_backed_up` when at
/// least one kind qualifies, and `local` otherwise.
#[must_use]
pub fn evaluate(
    records: &[(CopyKind, KindRecords)],
    formats: &AcceptedFormats,
    now: DateTime<Utc>,
) -> Freshness {
    let kinds: Vec<_> = records
        .iter()
        .map(|(kind, records)| KindVerdict {
            kind: *kind,
            reason: kind_reason(*kind, records, formats, now),
        })
        .collect();
    let mode = if kinds.iter().any(KindVerdict::qualifies) {
        Mode::LocalBackedUp
    } else {
        Mode::Local
    };
    Freshness { mode, kinds }
}

/// The first reason in the rule's order that keeps `kind` from qualifying,
/// or `None` when it qualifies.
#[must_use]
pub fn kind_reason(
    kind: CopyKind,
    records: &KindRecords,
    formats: &AcceptedFormats,
    now: DateTime<Utc>,
) -> Option<Reason> {
    let (config, identity, state) = match records {
        KindRecords::Unreadable { .. } => return Some(Reason::RecordUnreadable),
        KindRecords::NotConfigured => return Some(Reason::NotConfigured),
        KindRecords::Configured {
            config,
            identity,
            state,
        } => (config, identity, state),
    };
    let Some(receipt) = &state.newest_receipt else {
        return Some(Reason::NeverConfirmed);
    };
    if &receipt.target_identity != identity {
        return Some(Reason::TargetChanged);
    }
    let accepted = match kind {
        CopyKind::Store => formats.store.as_ref(),
    };
    if accepted != Some(&receipt.manifest.capture.format_identity) {
        return Some(Reason::OtherFormat);
    }
    if recorded_times(config, state).any(|time| time > now) {
        return Some(Reason::ClockInvalid);
    }
    if state.missing_copy.is_some() {
        return Some(Reason::CopyMissing);
    }
    let window = Duration::hours(i64::from(config.window_hours));
    let within = |time: Option<DateTime<Utc>>| time.is_some_and(|time| now - time <= window);
    if !within(
        state
            .last_confirmation
            .as_ref()
            .map(|confirmed| confirmed.at),
    ) {
        return Some(Reason::ConfirmationExpired);
    }
    // Content age counts from the start of a capture, never from an upload
    // or a confirmation: confirming old bytes again never makes them fresh.
    if !within(state.observed_equal_at) {
        return Some(Reason::Stale);
    }
    None
}

/// Every time the records hold: the operator's statements, every receipt
/// and attempt with its capture start, the observed, confirmed and missing
/// times, and the last attempt. The brief allows no recorded time in the
/// future, whether or not the rule relies on it.
fn recorded_times<'a>(
    config: &'a TargetConfig,
    state: &'a TargetState,
) -> impl Iterator<Item = DateTime<Utc>> + 'a {
    let receipts = state
        .newest_receipt
        .iter()
        .chain(&state.receipts)
        .flat_map(|receipt| [receipt.at, receipt.manifest.capture.capture_started_at]);
    let attempts = state
        .pending
        .iter()
        .chain(&state.set_aside)
        .map(|attempt| attempt.manifest.capture.capture_started_at);
    let last_attempt = state
        .last_attempt
        .iter()
        .flat_map(|attempt| [attempt.started_at, attempt.ended_at]);
    [Some(config.disclosure_authorized.at)]
        .into_iter()
        .chain([config
            .off_host_asserted
            .as_ref()
            .map(|statement| statement.at)])
        .chain([state.observed_equal_at])
        .chain([state
            .last_confirmation
            .as_ref()
            .map(|confirmed| confirmed.at)])
        .chain([state.missing_copy.as_ref().map(|missing| missing.at)])
        .flatten()
        .chain(receipts)
        .chain(attempts)
        .chain(last_attempt)
}

/// A project's backup evidence as read from local files and the store.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Collected {
    pub kinds: Vec<(CopyKind, KindRecords)>,
    /// The store's current cut, or why it could not be read.
    pub cut: Result<WorkGraphSnapshotCut, CutUnavailable>,
}

/// Why the store's cut could not be read: the store refusal's typed code
/// and its message.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CutUnavailable {
    pub code: String,
    pub message: String,
}

/// Reads every kind's records under `home` and the current cut of the store
/// at `database`, without taking a lock and without contacting any target.
/// A record this build cannot use is reported as unreadable, never as an
/// error of the whole collection.
#[must_use]
pub fn collect(home: &Path, project: &ProjectId, database: &Path) -> Collected {
    let kinds = CopyKind::ALL
        .into_iter()
        .map(|kind| (kind, kind_records(home, project, kind)))
        .collect();
    let cut = SqliteStore::read_backup_cut(database, project).map_err(|error| CutUnavailable {
        code: crate::host::store_error_code(&error).to_owned(),
        message: error.to_string(),
    });
    Collected { kinds, cut }
}

/// One kind's records under `home`.
#[must_use]
pub fn kind_records(home: &Path, project: &ProjectId, kind: CopyKind) -> KindRecords {
    let paths = RecordPaths::new(home, project, kind);
    match read_kind_records(&paths, project, kind) {
        Ok(Some((config, identity, state))) => KindRecords::Configured {
            config: Box::new(config),
            state: Box::new(state.unwrap_or_else(|| TargetState::empty(identity.clone()))),
            identity,
        },
        Ok(None) => KindRecords::NotConfigured,
        Err(TargetError::Unreadable { path, reason }) => KindRecords::Unreadable { path, reason },
        Err(error) => KindRecords::Unreadable {
            path: paths.directory,
            reason: error.to_string(),
        },
    }
}

#[cfg(test)]
mod tests;
