//! The explicit evaluation-history window of `show` and one record's complete
//! detail. The window is fitted to the agent response budget; the detail is an
//! explicit unbounded read, like `--note` and `--full`.

use super::{
    AgentVerbs, DateTime, Guidance, MAX_AGENT_WORK_RESPONSE_BYTES, Receipt, StoreError, Utc, Value,
    VerbError, json,
};
use crate::work_service::{WorkEvaluationDetail, WorkEvaluationRow, WorkEvaluationWindow};
use std::fmt::Write as _;

impl AgentVerbs {
    /// The bounded evaluations window of the item's run.
    pub(super) fn show_evaluations(
        &self,
        work_ref: &str,
        after: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<Receipt, VerbError> {
        let command = format!(
            "engram work show {} --evaluations",
            super::record_windows::safe_reference_argument(work_ref)
        );
        let page = self
            .service
            .work_evaluation_window(work_ref, after, now)
            .map_err(|error| VerbError::for_listing(error, &command))?;
        fit_window(&page, MAX_AGENT_WORK_RESPONSE_BYTES)
    }

    /// One evaluation record of the item, complete.
    pub(super) fn show_evaluation(
        &self,
        work_ref: &str,
        record: &str,
        now: DateTime<Utc>,
    ) -> Result<Receipt, VerbError> {
        let detail = self
            .service
            .work_evaluation_detail(work_ref, record, now)
            .map_err(|error| VerbError::at(error, work_ref))?;
        Ok(detail_receipt(&detail))
    }
}

/// The largest number of newest rows that fits, never fewer than one when the
/// run has an evaluation. A row is bounded, so one row always fits.
fn fit_window(page: &WorkEvaluationWindow, budget: usize) -> Result<Receipt, VerbError> {
    let first = usize::from(!page.rows.is_empty());
    let mut best = render_window(page, first, budget)?;
    if !super::receipts::agent_receipt_fits(&best, budget)? {
        return Err(VerbError::at(
            StoreError::InvalidWorkProjection(
                "evaluations window exceeds the agent response byte budget".into(),
            ),
            &page.short_ref,
        ));
    }
    let (mut lower, mut upper) = (first, page.rows.len());
    while lower < upper {
        let visible = lower + (upper - lower).div_ceil(2);
        let candidate = render_window(page, visible, budget)?;
        if super::receipts::agent_receipt_fits(&candidate, budget)? {
            best = candidate;
            lower = visible;
        } else {
            upper = visible - 1;
        }
    }
    Ok(best)
}

fn render_window(
    page: &WorkEvaluationWindow,
    visible: usize,
    budget: usize,
) -> Result<Receipt, VerbError> {
    let work_ref = &page.short_ref;
    let command = format!("engram work show {work_ref} --evaluations");
    let after = page
        .continuation(visible)
        .map_err(|error| VerbError::at(error, work_ref))?;
    let older = page.total - page.newer - visible;
    let omitted = page.total - visible;
    // Selected newest first; shown oldest to newest.
    let shown = page.rows[..visible].iter().rev().collect::<Vec<_>>();
    let mut next = Vec::new();
    if let Some(after) = &after {
        next.push(format!("{command} --after {after}"));
    }
    // Suggested commands carry no record id; each row names its own
    // complete-record command.
    next.push(format!("engram work show {work_ref}"));
    if page.title_truncated {
        next.push(format!("engram work show {work_ref} --full"));
    }
    let cut = page.read_cut();
    let mut lines = vec![
        format!("{work_ref} \"{}\"", super::short(&page.title)),
        format!(
            "evaluations: window {visible} of {}; {omitted} omitted ({older} older, {} newer); oldest to newest within window",
            page.total, page.newer
        ),
        format!(
            "  byte budget: {budget}; read cut: project position {}, observed at {}, valid until ms {}",
            cut.project_position,
            cut.observed_at.to_rfc3339(),
            cut.valid_until_ms
                .map_or_else(|| "none".into(), |until| until.to_string())
        ),
    ];
    if page.total == 0 {
        lines.push("  no evaluation is recorded on this item's run".into());
    } else if !page.judged {
        lines.push(
            "  stale reasons: not judged; the run has ended, and ending it revised the item".into(),
        );
    }
    for row in &shown {
        append_row_lines(&mut lines, row, work_ref);
    }
    let mut work = json!({ "short_ref": work_ref, "title": page.title });
    if page.title_truncated {
        work["title_truncated"] = json!(true);
        work["title_bytes"] = json!(page.title_bytes);
    }
    let value = json!({
        "work": work,
        "evaluations": shown.iter().map(|row| row_value(row, work_ref)).collect::<Vec<_>>(),
        "evaluations_window": {
            "selection": "newest_first",
            "order": "oldest_first",
            "total": page.total,
            "shown": visible,
            "omitted": omitted,
            "older": older,
            "newer": page.newer,
            "after": after,
            "byte_budget": budget,
            "read_cut": cut,
            "stale_judged": page.judged,
        },
        "next": next,
    });
    Ok(Receipt::assemble(
        lines,
        Guidance {
            reminders: Vec::new(),
            next,
        },
        value,
        false,
    ))
}

fn evaluator_word(row: &WorkEvaluationRow) -> String {
    row.evaluator_session
        .clone()
        .unwrap_or_else(|| "no recorded session".into())
}

fn row_summary(row: &WorkEvaluationRow) -> String {
    let mut summary = format!(
        "{} by {}; attempt {}; revision {}; created {}",
        row.mode,
        evaluator_word(row),
        super::short(&row.attempt_key),
        row.work_revision,
        row.created_at.to_rfc3339()
    );
    if row.newest {
        summary.push_str("; newest");
    }
    if !row.judged {
        summary.push_str("; not judged (run ended)");
    } else if let Some(stale) = row.stale {
        summary.push_str("; stale: ");
        summary.push_str(stale);
    }
    summary
}

fn detail_command(work_ref: &str, row: &WorkEvaluationRow) -> String {
    format!(
        "engram work show {work_ref} --evaluation {}",
        row.evaluation
    )
}

fn append_row_lines(lines: &mut Vec<String>, row: &WorkEvaluationRow, work_ref: &str) {
    lines.push(format!(
        "  run position {}: {} {}",
        row.position,
        row.evaluation,
        row_summary(row)
    ));
    let mut verdicts = row
        .verdicts
        .iter()
        .map(|(position, verdict)| format!("{position} {verdict}"))
        .collect::<Vec<_>>()
        .join(", ");
    let cut = row.verdicts_total - row.verdicts.len();
    if cut > 0 {
        let _ = write!(verdicts, "; {cut} more in its complete record");
    }
    lines.push(format!("    verdicts: {verdicts}"));
    if cut > 0 {
        lines.push(format!(
            "    complete record: {}",
            detail_command(work_ref, row)
        ));
    }
    if let Some(supersedes) = &row.supersedes {
        lines.push(format!("    supersedes {supersedes}"));
    }
    if let Some(observation) = &row.stale_observation {
        lines.push(format!(
            "    {}",
            observation.line(super::terminal_safe_line)
        ));
    }
}

fn row_value(row: &WorkEvaluationRow, work_ref: &str) -> Value {
    let mut value = json!({
        "detail": detail_command(work_ref, row),
        "evaluation": row.evaluation,
        "run_position": row.position,
        "mode": row.mode,
        "evaluator_session": row.evaluator_session,
        "attempt_key": row.attempt_key,
        "created_at": row.created_at,
        "work_revision": row.work_revision,
        "verdicts": row.verdicts.iter().map(|(position, verdict)| json!({
            "position": position,
            "verdict": verdict,
        })).collect::<Vec<_>>(),
        "verdicts_total": row.verdicts_total,
        "verdicts_omitted": row.verdicts_total - row.verdicts.len(),
        "stale": row.stale,
        "stale_judged": row.judged,
        "newest": row.newest,
    });
    if let Some(supersedes) = &row.supersedes {
        value["supersedes"] = json!(supersedes);
    }
    if let Some(observation) = &row.stale_observation {
        value["stale_observation"] = json!(observation);
    }
    value
}

fn detail_receipt(detail: &WorkEvaluationDetail) -> Receipt {
    let row = &detail.row;
    let work_ref = &detail.short_ref;
    let mut lines = vec![
        format!(
            "evaluation {} of {work_ref}: run position {} (complete record)",
            row.evaluation, row.position
        ),
        format!(
            "  {}; evaluated cut {}{}",
            row_summary(row),
            detail.evaluated_cut,
            // The summary already names a stale reason or an unjudged run.
            if row.judged && row.stale.is_none() {
                "; fresh"
            } else {
                ""
            }
        ),
        format!(
            "  attempt key: {}",
            super::terminal_safe_line(&row.attempt_key)
        ),
    ];
    for (label, value) in [
        ("evaluator model", &detail.evaluator_model),
        ("execution identity", &detail.execution_identity),
        ("parent session", &detail.parent_session),
        ("source fingerprint", &detail.source_fingerprint),
    ] {
        if let Some(value) = value {
            lines.push(format!("  {label}: {}", super::terminal_safe_line(value)));
        }
    }
    if let Some(supersedes) = &row.supersedes {
        lines.push(format!("  supersedes {supersedes}"));
    }
    if let Some(observation) = &row.stale_observation {
        lines.push(format!("  {}", observation.line(super::terminal_safe_line)));
    }
    for verdict in &detail.verdicts {
        lines.push(format!(
            "  {}. {} ({})",
            verdict.position, verdict.verdict, verdict.basis
        ));
        for (label, text) in [
            ("criterion", &verdict.criterion),
            ("rationale", &verdict.rationale),
        ] {
            let safe = super::terminal_data_block(text);
            for (index, line) in safe.split('\n').enumerate() {
                if index == 0 {
                    lines.push(format!("     {label}: {line}"));
                } else {
                    lines.push(format!("       {line}"));
                }
            }
        }
        if !verdict.citations.is_empty() {
            lines.push(format!("     citations: {}", verdict.citations.join(", ")));
        }
    }
    let mut value = row_value(row, work_ref);
    value["evaluated_cut"] = json!(detail.evaluated_cut);
    for (key, field) in [
        ("evaluator_model", &detail.evaluator_model),
        ("execution_identity", &detail.execution_identity),
        ("parent_session", &detail.parent_session),
        ("source_fingerprint", &detail.source_fingerprint),
    ] {
        if let Some(field) = field {
            value[key] = json!(field);
        }
    }
    value["verdicts"] = json!(
        detail
            .verdicts
            .iter()
            .map(|verdict| json!({
                "position": verdict.position,
                "criterion": verdict.criterion,
                "verdict": verdict.verdict,
                "basis": verdict.basis,
                "rationale": verdict.rationale,
                "citations": verdict.citations,
            }))
            .collect::<Vec<_>>()
    );
    value["verdicts_omitted"] = json!(0);
    let next = vec![format!("engram work show {work_ref} --evaluations")];
    Receipt::assemble(
        lines,
        Guidance {
            reminders: Vec::new(),
            next: next.clone(),
        },
        json!({ "work_ref": work_ref, "evaluation": value, "next": next }),
        false,
    )
}
