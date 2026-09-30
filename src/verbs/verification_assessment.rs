//! The reconstructed obligation assessment on a verification record's
//! detail: at most one page of candidates, each with what the current matching
//! rules say at the record's own run-feed position and, apart from it, the
//! obligation's recorded end.

use super::{Value, json};
use crate::control::{ObligationAssessment, ObligationSkip};
use crate::storage::{RecordedObligationEnd, VerificationObligationAssessment};
use crate::work_service::VerificationAssessmentPage;
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
    value
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
