//! The one line `next` adds when a configured backup needs attention: the
//! mode is `local`, or a kind's last push failed. It reads only the recorded
//! state under the Engram home, never the store and never a target, and it
//! reads nothing more when no kind has a target configured.

use std::{path::Path, sync::OnceLock};

use chrono::{DateTime, Utc};

use super::{
    CopyKind,
    freshness::{AcceptedFormats, KindRecords, Mode, evaluate, kind_records},
    record::AttemptOutcome,
    status::off_host_text,
    target::RecordPaths,
};
use crate::ProjectId;

/// The longest recorded failure code the line prints as it is.
const MAX_PRINTED_CODE_BYTES: usize = 64;

/// The reminder for `project` under `home`, or `None` when no kind has a
/// target configured or all is well. A kind counts as configured when its
/// configuration file exists, readable or not; a state file alone does not.
/// The clock is read after the records, so a push that records something
/// meanwhile never looks as if it lay in the future.
#[must_use]
pub fn backup_reminder(home: &Path, project: &ProjectId) -> Option<String> {
    let records: Vec<_> = CopyKind::ALL
        .into_iter()
        .filter(|kind| {
            // An existence check that cannot answer counts as configured, so
            // the records are read and an unreadable one is reported.
            RecordPaths::new(home, project, *kind)
                .config
                .try_exists()
                .unwrap_or(true)
        })
        .map(|kind| (kind, kind_records(home, project, kind)))
        .collect();
    if records.is_empty() {
        return None;
    }
    reminder_line(&records, running_formats(), Utc::now())
}

/// The format identities of the running build, computed once per process.
fn running_formats() -> &'static AcceptedFormats {
    static FORMATS: OnceLock<AcceptedFormats> = OnceLock::new();
    FORMATS.get_or_init(|| AcceptedFormats {
        store: crate::storage::running_schema_reference().ok(),
    })
}

/// The reminder for `records` at `now`: one line when a configured kind
/// exists and either no kind qualifies or a kind's last push failed, and
/// `None` otherwise. A kind without a target configured adds nothing.
#[must_use]
pub fn reminder_line(
    records: &[(CopyKind, KindRecords)],
    formats: &AcceptedFormats,
    now: DateTime<Utc>,
) -> Option<String> {
    let configured: Vec<_> = records
        .iter()
        .filter(|(_, records)| !matches!(records, KindRecords::NotConfigured))
        .cloned()
        .collect();
    if configured.is_empty() {
        return None;
    }
    let freshness = evaluate(&configured, formats, now);
    let failed: Vec<_> = configured
        .iter()
        .filter_map(|(kind, records)| match records {
            KindRecords::Configured { state, .. } => state
                .last_attempt
                .as_ref()
                .filter(|attempt| attempt.outcome == AttemptOutcome::Failed)
                .map(|attempt| (*kind, printable_code(attempt.code.as_deref()))),
            KindRecords::Unreadable { .. } | KindRecords::NotConfigured => None,
        })
        .collect();
    let local = freshness.mode == Mode::Local;
    if !local && failed.is_empty() {
        return None;
    }
    let mut parts = Vec::new();
    if local {
        let reasons: Vec<_> = freshness
            .kinds
            .iter()
            .filter_map(|verdict| {
                verdict
                    .reason
                    .map(|reason| format!("{}: {}", verdict.kind.as_str(), reason.code()))
            })
            .collect();
        parts.push(format!("mode local ({})", reasons.join(", ")));
    }
    for (kind, code) in &failed {
        parts.push(format!("the last {} push failed: {code}", kind.as_str()));
    }
    if !local {
        // The mode still stands on an earlier copy: name it with its
        // off-host assurance, as every output that reports the mode does.
        let qualifying: Vec<_> = configured
            .iter()
            .zip(&freshness.kinds)
            .filter(|(_, verdict)| verdict.qualifies())
            .filter_map(|((kind, records), _)| match records {
                KindRecords::Configured { config, .. } => Some(format!(
                    "{} copy: {}",
                    kind.as_str(),
                    off_host_text(config.adapter)
                )),
                KindRecords::Unreadable { .. } | KindRecords::NotConfigured => None,
            })
            .collect();
        parts.push(format!(
            "an earlier copy still qualifies ({})",
            qualifying.join("; ")
        ));
    }
    Some(format!(
        "backup: {}; see engram backup status",
        parts.join("; ")
    ))
}

/// A recorded failure code as the line prints it: a plain code as it is,
/// anything else named for what it is.
fn printable_code(code: Option<&str>) -> &str {
    match code {
        None => "no code recorded",
        Some(code)
            if !code.is_empty()
                && code.len() <= MAX_PRINTED_CODE_BYTES
                && code.bytes().all(|byte| {
                    byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_'
                }) =>
        {
            code
        }
        Some(_) => "unrecognised code",
    }
}

#[cfg(test)]
pub(crate) mod tests;
