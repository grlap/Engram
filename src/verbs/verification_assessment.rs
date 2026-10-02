//! The reconstructed obligation assessment on a verification record's
//! detail: at most one page of candidates, each with what the current matching
//! rules say at the record's own run-feed position and, apart from it, the
//! obligation's recorded end.

use super::{Value, json};
use crate::control::{ObligationAssessment, ObligationSkip};
use crate::domain::{StaleSourceDecider, StaleVerificationSource};
use crate::storage::{RecordedObligationEnd, VerificationObligationAssessment};
use crate::work_service::{VerificationAssessmentPage, bounded_shown_field};
use std::fmt::Write as _;

/// The words that say what the block is and is not: a reconstruction at the
/// record's position, not a decision the store recorded.
pub(super) fn label(page: &VerificationAssessmentPage) -> String {
    format!(
        "reconstructed at record position {} under the current matching rules",
        page.record_position
    )
}

/// The block as JSON. `continuation` is the complete command to the next
/// page, when there is one.
pub(super) fn value(page: &VerificationAssessmentPage, continuation: Option<&str>) -> Value {
    let mut value = json!({
        "label": label(page),
        "record_position": page.record_position,
        "cut_position": page.cut_position,
        "total": page.total,
        "shown": page.rows.len(),
        "earlier": page.earlier,
        "omitted": page.omitted(),
        "rows": page.rows.iter().map(row_value).collect::<Vec<_>>(),
    });
    if let Some(command) = continuation {
        value["continuation"] = json!(command);
    }
    value
}

fn row_value(row: &VerificationObligationAssessment) -> Value {
    let mut value = json!({
        "rule": row.rule.rule_id,
        "rule_version": row.rule.rule_version,
        "check_kind": word(&row.check_kind),
        "pinned": row.pinned,
        "trigger_position": row.trigger_position,
        "recorded": word(&row.recorded),
    });
    if let Some(criterion) = row.criterion {
        value["criterion"] = json!(criterion);
    }
    match row.assessment {
        ObligationAssessment::Matches => value["status"] = json!("matches"),
        ObligationAssessment::Mismatch(mismatch) => {
            value["status"] = json!("mismatch");
            value["mismatch"] = json!(word(&mismatch));
        }
        ObligationAssessment::Skipped(skip) => {
            value["status"] = json!("left_out");
            value["left_out"] = json!(word(&skip));
        }
    }
    if let Some(source) = &row.stale_source {
        value["stale_source"] = stale_source_value(source);
    }
    value
}

/// The record that decided a stale mismatch, with host-recorded text bounded
/// per field as every show surface bounds it.
fn stale_source_value(source: &StaleVerificationSource) -> Value {
    let shown = |value: Option<&String>| value.map(|value| bounded_shown_field(value));
    json!({
        "decider": word(&source.decider),
        "position": source.position,
        "source_changed": source.source_changed,
        "workspace": shown(source.workspace.as_ref()),
        "revision": shown(source.revision.as_ref()),
        "root_generation": source.root_generation,
        "verification_workspace": bounded_shown_field(&source.verification_workspace),
        "verification_revision": bounded_shown_field(&source.verification_revision),
    })
}

/// The check an open obligation waits for, as a done refusal shows it beside
/// its cause: host-recorded text bounded per field.
pub(super) fn open_obligation_check_value(check: &crate::domain::OpenObligationCheck) -> Value {
    match check {
        crate::domain::OpenObligationCheck::NoneFollowed => json!({ "state": "none_followed" }),
        crate::domain::OpenObligationCheck::Newest {
            verification,
            position,
            mismatch,
            left_out,
            stale_source,
        } => {
            let mut value = json!({
                "state": "newest",
                "verification": verification.as_str(),
                "position": position,
            });
            if let Some(mismatch) = mismatch {
                value["mismatch"] = json!(word(mismatch));
            }
            if let Some(left_out) = left_out {
                value["left_out"] = json!(left_out);
            }
            if let Some(source) = stale_source {
                value["stale_source"] = stale_source_value(source);
            }
            value
        }
    }
}

/// One reminder naming the check an open obligation waits for and why it
/// does not satisfy it, or that none followed; host-recorded text is
/// bounded and terminal-safe.
pub(super) fn open_obligation_check_line(
    check: &crate::domain::OpenObligationCheck,
    required_check: crate::domain::VerificationKind,
) -> String {
    let kind = word(&required_check);
    match check {
        crate::domain::OpenObligationCheck::NoneFollowed => {
            format!("no passed {kind} check was recorded after that obligation opened")
        }
        crate::domain::OpenObligationCheck::Newest {
            verification,
            position,
            mismatch,
            left_out,
            stale_source,
        } => {
            let why = match (mismatch, left_out) {
                (Some(mismatch), _) => format!("does not match it: {}", spaced(&word(mismatch))),
                (None, Some(left_out)) => {
                    format!("is left out before matching: {}", spaced(left_out))
                }
                (None, None) => {
                    "matches it at the completion cut but did not satisfy it when it was recorded"
                        .to_owned()
                }
            };
            let mut line = format!(
                "the newest passed {kind} check after that obligation opened ({}, at run position {position}) {why}",
                verification.as_str()
            );
            if let Some(source) = stale_source {
                line.push_str("; ");
                line.push_str(&stale_source_line(source));
            }
            line
        }
    }
}

/// One line naming the record that decided a stale mismatch beside the
/// check's own source; host-recorded text is bounded and terminal-safe.
pub(super) fn stale_source_line(source: &StaleVerificationSource) -> String {
    let field = |value: Option<&String>| {
        value.map_or_else(
            || "not recorded".to_owned(),
            |value| super::terminal_safe_line(&bounded_shown_field(value)),
        )
    };
    let check = format!(
        "this check ran on revision {} in workspace {}",
        super::terminal_safe_line(&bounded_shown_field(&source.verification_revision)),
        super::terminal_safe_line(&bounded_shown_field(&source.verification_workspace)),
    );
    let kind = |changed: Option<bool>| {
        if changed == Some(false) {
            "a sighting"
        } else {
            "a change"
        }
    };
    match source.decider {
        StaleSourceDecider::LatestChange => format!(
            "decided by the run's latest source change at run position {}: {}, workspace {}, revision {}; {check}",
            source.position,
            kind(source.source_changed),
            field(source.workspace.as_ref()),
            field(source.revision.as_ref()),
        ),
        StaleSourceDecider::RootSighting => format!(
            "decided by the named root's newest sighting at run position {}: {}, workspace {}, revision {}; {check}",
            source.position,
            kind(source.source_changed),
            field(source.workspace.as_ref()),
            field(source.revision.as_ref()),
        ),
        StaleSourceDecider::RootBinding => format!(
            "decided by the named root's binding at run position {}: workspace {}, generation {}; {check}, not of that root's workspace and generation or not after its binding",
            source.position,
            field(source.workspace.as_ref()),
            source.root_generation.map_or_else(
                || "not recorded".to_owned(),
                |generation| generation.to_string()
            ),
        ),
        StaleSourceDecider::MeasuredSighting => format!(
            "decided by the newest measured sighting after an unadmitted change at run position {}: workspace {}, revision {}; {check}",
            source.position,
            field(source.workspace.as_ref()),
            field(source.revision.as_ref()),
        ),
    }
}

/// The serde word of a unit enum variant.
fn word<T: serde::Serialize>(value: &T) -> String {
    json!(value).as_str().unwrap_or_default().to_owned()
}

/// The block as terminal lines.
pub(super) fn append_lines(
    lines: &mut Vec<String>,
    page: &VerificationAssessmentPage,
    continuation: Option<&str>,
) {
    let kind = page
        .rows
        .first()
        .map(|row| word(&row.check_kind))
        .unwrap_or_default();
    lines.push(if page.total == 0 {
        format!(
            "  obligations of this check kind on the run: none ({})",
            label(page)
        )
    } else {
        format!(
            "  {} obligations of check kind {kind} on the run; {} shown, {} earlier, {} omitted ({}):",
            page.total,
            page.rows.len(),
            page.earlier,
            page.omitted(),
            label(page)
        )
    });
    for row in &page.rows {
        let mut subject = format!(
            "{} version {}",
            super::terminal_safe_line(&row.rule.rule_id),
            row.rule.rule_version
        );
        if let Some(criterion) = row.criterion {
            let _ = write!(subject, ", criterion {criterion}");
        }
        if row.pinned {
            subject.push_str(", pinned check");
        }
        let status = match row.assessment {
            ObligationAssessment::Matches => "matches at that position".to_owned(),
            ObligationAssessment::Mismatch(mismatch) => {
                format!("does not match: {}", spaced(&word(&mismatch)))
            }
            ObligationAssessment::Skipped(skip) => {
                format!("left out before matching: {}", skip_words(skip))
            }
        };
        lines.push(format!(
            "    - {subject}, opened at run position {}: {status}; recorded: {}",
            row.trigger_position,
            recorded_words(row.recorded)
        ));
        if let Some(source) = &row.stale_source {
            lines.push(format!("      {}", stale_source_line(source)));
        }
    }
    if let Some(command) = continuation {
        lines.push(format!("  more: {}", super::terminal_command(command)));
    }
}

fn spaced(code: &str) -> String {
    code.replace('_', " ")
}

fn skip_words(skip: ObligationSkip) -> &'static str {
    match skip {
        ObligationSkip::OtherRun => "another run",
        ObligationSkip::NotYetDefined => "not yet defined for this record",
        ObligationSkip::NoSourceContext => "no usable source context",
        ObligationSkip::AlreadyClosed => "already closed",
        ObligationSkip::ForeignOrDisplaced => "foreign or displaced workspace",
    }
}

fn recorded_words(recorded: RecordedObligationEnd) -> &'static str {
    match recorded {
        RecordedObligationEnd::Open => "open",
        RecordedObligationEnd::SatisfiedByThisRecord => "satisfied by this record",
        RecordedObligationEnd::SatisfiedByAnotherRecord => "satisfied by another record",
        RecordedObligationEnd::Waived => "waived",
        RecordedObligationEnd::Displaced => "displaced",
    }
}
