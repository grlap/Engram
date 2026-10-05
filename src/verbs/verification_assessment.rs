//! The reconstructed obligation assessment on a verification record's
//! detail, each candidate with what the current matching rules say at the
//! record's own run-feed position and, apart from it, the obligation's
//! recorded end. The default summary gives exact counts by status and reason
//! and shows in full every candidate a reader must act on: one that matches
//! or mismatches at that position, or whose obligation is still recorded
//! open; the exhaustive history pages through every candidate, one
//! continuation away at the same cut.

use super::{Value, VerbError, json};
use crate::control::{ObligationAssessment, ObligationSkip};
use crate::domain::{AcceptanceBoundVerification, StaleSourceDecider, StaleVerificationSource};
use crate::storage::{RecordedObligationEnd, VerificationObligationAssessment};
use crate::work_service::{AssessmentView, VerificationAssessmentPage, bounded_shown_field};
use std::fmt::Write as _;

/// Bytes of must-show rows a summary page shows, as compact JSON: whole rows,
/// and always at least one, so every page advances.
pub(super) const MUST_SHOW_BUDGET: usize = 6 * 1024;

/// The assessment block of a verification record's detail, rendered for
/// both surfaces.
pub(super) struct Rendered {
    pub(super) value: Value,
    pub(super) lines: Vec<String>,
}

/// Renders a page of either view, with the commands that continue it, which
/// name `work_ref` and the note's `locator`.
pub(super) fn render(
    page: &VerificationAssessmentPage,
    work_ref: &str,
    locator: &str,
) -> Result<Rendered, VerbError> {
    let command =
        |token: String| format!("engram work show {work_ref} --note {locator} --after {token}");
    match page.view {
        AssessmentView::History => {
            let continuation = match page.rows.last() {
                Some(last) if page.more_after(page.rows.len()) => {
                    Some(command(page.continuation_after(last)?))
                }
                _ => None,
            };
            let mut lines = Vec::new();
            append_lines(&mut lines, page, continuation.as_deref());
            Ok(Rendered {
                value: value(page, continuation.as_deref()),
                lines,
            })
        }
        AssessmentView::Summary => summary(page, &command),
    }
}

/// The words that say what the block is and is not: a reconstruction at the
/// record's position, not a decision the store recorded.
pub(super) fn label(page: &VerificationAssessmentPage) -> String {
    format!(
        "reconstructed at record position {} under the current matching rules",
        page.record_position
    )
}

/// A history page as JSON. `continuation` is the complete command to the next
/// page, when there is one.
pub(super) fn value(page: &VerificationAssessmentPage, continuation: Option<&str>) -> Value {
    let mut value = json!({
        "view": "history",
        "label": label(page),
        "record_position": page.record_position,
        "cut_position": page.cut_position,
        "check_kind": word(&page.check_kind),
        "total": page.total,
        "shown": page.rows.len(),
        "earlier": page.earlier,
        "omitted": page.omitted(),
        "rows": page.rows.iter().map(row_value).collect::<Vec<_>>(),
    });
    if let Some(command) = continuation {
        value["continuation"] = json!(command);
    }
    if let Some(bound) = &page.bound {
        value["bound"] = bound_value(bound);
    }
    value
}

/// Where a bound record's check ran and how it was bound here, with
/// host-recorded text bounded per field. The check's own time and the
/// binding's time both stay visible.
fn bound_value(bound: &AcceptanceBoundVerification) -> Value {
    let basis = &bound.original_basis;
    json!({
        "verification": bound.verification.as_str(),
        "work_ref": bound.work_ref,
        "run": bound.run.0.to_string(),
        "original_position": bound.original_position,
        "original_basis": {
            "workspace": bounded_shown_field(&basis.workspace_id),
            "revision": bounded_shown_field(&basis.source_revision),
            "root_generation": basis.source_root_generation,
        },
        "original_completed_at": bound.original_completed_at,
        "binder": {
            "actor": bounded_shown_field(&bound.binder.actor_id),
            "session": bound.binder.session_id.as_ref().map(|session| bounded_shown_field(&session.0)),
        },
        "bound_at": bound.bound_at,
        "sighting": bound.sighting.as_str(),
        "measurement": {
            "workspace": bounded_shown_field(&bound.measurement.workspace_id),
            "revision": bounded_shown_field(&bound.measurement.source_revision),
            "measured_at": bound.measurement.measured_at,
        },
        "criteria": bound.criteria,
    })
}

/// The bound branch as terminal lines.
fn append_bound_lines(lines: &mut Vec<String>, bound: &AcceptanceBoundVerification) {
    let safe = |value: &str| super::terminal_safe_line(&bounded_shown_field(value));
    let basis = &bound.original_basis;
    lines.push(format!(
        "  bound from verification {} of {} (run {}, run position {}); its producer's position is on that run",
        bound.verification.as_str(),
        bound.work_ref,
        bound.run.0,
        bound.original_position,
    ));
    lines.push(format!(
        "    the check completed at {} on revision {} in workspace {}{}",
        bound.original_completed_at.to_rfc3339(),
        safe(&basis.source_revision),
        safe(&basis.workspace_id),
        basis
            .source_root_generation
            .map(|generation| format!(", root generation {generation}"))
            .unwrap_or_default(),
    ));
    lines.push(format!(
        "    bound here at {} by {}{}, on sighting {}; measured revision {} in workspace {} at {}",
        bound.bound_at.to_rfc3339(),
        safe(&bound.binder.actor_id),
        bound
            .binder
            .session_id
            .as_ref()
            .map(|session| format!(" (session {})", safe(&session.0)))
            .unwrap_or_default(),
        bound.sighting.as_str(),
        safe(&bound.measurement.source_revision),
        safe(&bound.measurement.workspace_id),
        bound.measurement.measured_at.to_rfc3339(),
    ));
    if !bound.criteria.is_empty() {
        lines.push(format!(
            "    intended for criteria {} (intent, not credit)",
            bound
                .criteria
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        ));
    }
}

/// The summary: exact counts over every candidate, then the must-show rows
/// that fit the budget, the command to the rest, and the full history.
fn summary(
    page: &VerificationAssessmentPage,
    command: &dyn Fn(String) -> String,
) -> Result<Rendered, VerbError> {
    let mut shown = Vec::new();
    let mut bytes = 0;
    for row in &page.rows {
        let value = row_value(row);
        let size = serde_json::to_vec(&value)
            .map_err(crate::storage::StoreError::Json)?
            .len()
            + 1;
        if !shown.is_empty() && bytes + size > MUST_SHOW_BUDGET {
            break;
        }
        bytes += size;
        shown.push((row, value));
    }
    let remaining = page.view_total - page.earlier - shown.len();
    let continuation = match shown.last() {
        Some((last, _)) if remaining > 0 => Some(command(page.continuation_after(last)?)),
        _ => None,
    };
    let history = command(page.history_start()?);
    let counts = page
        .counts
        .iter()
        .map(|count| {
            let mut value = json!({ "status": count.status, "count": count.count });
            if let Some(reason) = &count.reason {
                value["reason"] = json!(reason);
            }
            value
        })
        .collect::<Vec<_>>();
    let mut value = json!({
        "view": "summary",
        "label": label(page),
        "record_position": page.record_position,
        "cut_position": page.cut_position,
        "check_kind": word(&page.check_kind),
        "total": page.total,
        "counts": counts,
        "must_show_total": page.view_total,
        "must_show_earlier": page.earlier,
        "must_show_remaining": remaining,
        "must_show": shown.iter().map(|(_, value)| value.clone()).collect::<Vec<_>>(),
        "history": history,
    });
    if let Some(command) = &continuation {
        value["continuation"] = json!(command);
    }
    if let Some(bound) = &page.bound {
        value["bound"] = bound_value(bound);
    }
    let kind = word(&page.check_kind);
    let cut = page.cut_position;
    let mut lines = Vec::new();
    if let Some(bound) = &page.bound {
        append_bound_lines(&mut lines, bound);
    }
    lines.push(if page.total == 0 {
        format!(
            "  obligations of check kind {kind} on the run at cut position {cut}: none ({})",
            label(page)
        )
    } else {
        format!(
            "  {} obligations of check kind {kind} on the run at cut position {cut} ({}): {}",
            page.total,
            label(page),
            page.counts
                .iter()
                .map(|count| format!("{} {}", count.count, count_words(count)))
                .collect::<Vec<_>>()
                .join(", ")
        )
    });
    if page.total > 0 && page.view_total == 0 {
        lines.push("  none matches, mismatches or is still open at this record; the counts cover every one".into());
    } else if page.view_total > 0 {
        lines.push(format!(
            "  {} to act on (matching, mismatching or still open), shown in full{}:",
            page.view_total,
            if page.earlier > 0 {
                format!(" ({} on earlier pages)", page.earlier)
            } else {
                String::new()
            }
        ));
    }
    for (row, _) in &shown {
        append_row_lines(&mut lines, row);
    }
    if let Some(command) = &continuation {
        lines.push(format!(
            "  more must-show rows: {remaining}; next: {}",
            super::terminal_command(command)
        ));
    }
    lines.push(format!(
        "  full history: {}",
        super::terminal_command(&history)
    ));
    Ok(Rendered { value, lines })
}

/// What one count counts, in words.
fn count_words(count: &crate::work_service::AssessmentCount) -> String {
    match (count.status.as_str(), count.reason.as_deref()) {
        ("matches", _) => "match at that position".into(),
        ("mismatch", Some(reason)) => format!("do not match ({})", spaced(reason)),
        ("left_out", Some(reason)) => format!("left out ({})", spaced(reason)),
        (status, reason) => format!(
            "{}{}",
            spaced(status),
            reason.map_or(String::new(), |reason| format!(" ({})", spaced(reason)))
        ),
    }
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
    if let Some(bound) = &page.bound {
        append_bound_lines(lines, bound);
    }
    let kind = word(&page.check_kind);
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
        append_row_lines(lines, row);
    }
    if let Some(command) = continuation {
        lines.push(format!("  more: {}", super::terminal_command(command)));
    }
}

/// One candidate as terminal lines, in either view.
fn append_row_lines(lines: &mut Vec<String>, row: &VerificationObligationAssessment) {
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
