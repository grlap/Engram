//! A bounded read of complete sealed associations, in their recorded order.

use super::{AgentVerbs, DateTime, Guidance, Receipt, StoreError, Utc, VerbError, json};
use crate::work_service::{WorkCriterionLinkRow, WorkCriterionLinksWindow};

#[cfg(test)]
#[path = "tests/criterion_links.rs"]
mod tests;

impl AgentVerbs {
    pub(super) fn show_criterion_links(
        &self,
        work_ref: &str,
        after: Option<&str>,
        now: DateTime<Utc>,
        budget: usize,
    ) -> Result<Receipt, VerbError> {
        let command = format!(
            "engram work show {} --criterion-links",
            super::record_windows::safe_reference_argument(work_ref)
        );
        let page = self
            .service
            .work_criterion_links_window(work_ref, after, now)
            .map_err(|error| VerbError::for_listing(error, &command))?;
        fit_window(&page, budget)
    }
}

fn fit_window(page: &WorkCriterionLinksWindow, budget: usize) -> Result<Receipt, VerbError> {
    // Test each candidate: dropping the final cursor can make the last page
    // smaller than its predecessor. Sixteen candidates keep this bounded.
    let minimum = usize::from(!page.rows.is_empty());
    for previews in [true, false] {
        for shown in (minimum..=page.rows.len()).rev() {
            // Shed previews before shedding rows, retaining all candidate rows
            // when their identity and detail alone fit.
            if previews && shown != page.rows.len() {
                continue;
            }
            let receipt = render(page, shown, previews, budget)?;
            if super::receipts::agent_receipt_fits(&receipt, budget)? {
                return Ok(receipt);
            }
        }
    }
    Err(VerbError::at(
        StoreError::InvalidWorkProjection(
            "criterion links window exceeds the agent response byte budget".into(),
        ),
        &page.short_ref,
    ))
}

fn render(
    page: &WorkCriterionLinksWindow,
    shown: usize,
    previews: bool,
    budget: usize,
) -> Result<Receipt, VerbError> {
    let after = page
        .continuation(shown)
        .map_err(|error| VerbError::at(error, &page.short_ref))?;
    let remaining = page.total - page.earlier - shown;
    let command = format!("engram work show {} --criterion-links", page.short_ref);
    let mut next = Vec::new();
    if let Some(after) = &after {
        next.push(format!("{command} --after {after}"));
    }
    next.push(format!("engram work show {}", page.short_ref));
    let mut lines = vec![
        format!(
            "{} criterion links: {shown} of {}; {} earlier, {remaining} remaining; recorded links, not verification or eligibility",
            page.short_ref, page.total, page.earlier
        ),
        format!(
            "  frozen basis: project {}, work {}, run {}, seal {}; byte budget {budget}",
            super::terminal_safe_line(&page.project.0),
            page.work.0,
            page.run.0,
            page.seal.as_str()
        ),
    ];
    let rows = page.rows[..shown].iter().map(|row| {
        let detail = format!("engram work show {} --note {}", page.short_ref, row.locator.as_str());
        lines.push(format!("  criterion {} member {}: {}", row.criterion, row.evidence_member, row.locator.as_str()));
        lines.push(format!("    detail: {detail}"));
        let mut value = json!({"criterion":row.criterion,"evidence_member":row.evidence_member,"locator":row.locator,"detail":detail});
        append_preview(row, previews, &mut lines, &mut value);
        value
    }).collect::<Vec<_>>();
    Ok(Receipt::assemble(
        lines,
        Guidance {
            reminders: Vec::new(),
            next: next.clone(),
        },
        json!({
            "work":{"short_ref":page.short_ref},"criterion_links":rows,
            "criterion_links_window":{"basis":{"project_id":page.project,"work_id":page.work,"run_id":page.run,"seal_id":page.seal},
                "order":"recorded","total":page.total,"earlier":page.earlier,"shown":shown,"remaining":remaining,"after":after,"byte_budget":budget},
            "next":next
        }),
        false,
    ))
}

fn append_preview(
    row: &WorkCriterionLinkRow,
    previews: bool,
    lines: &mut Vec<String>,
    value: &mut serde_json::Value,
) {
    if previews && let Some(preview) = &row.preview {
        value["preview"] = json!(preview);
        lines.push(format!("    {}", super::terminal_safe_line(preview)));
    }
    if let Some(class) = row.preview_error_class {
        value["preview_error_class"] = json!(class);
        lines.push(format!("    preview unavailable: {class}"));
    }
}
