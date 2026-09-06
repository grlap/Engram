use std::collections::HashMap;

use super::receipts::{compact_row, compact_row_line};
use super::{DEFAULT_LIMIT, Guidance, LsInput, Receipt, StoreError, VerbError, json};
use crate::work_service::WorkListingPage;

/// Verbose agent rows retain the core fields while adding a typed, safe overlay.
/// Destructuring the complete source row keeps this mapping explicit at compile time.
#[derive(serde::Serialize)]
struct VerboseRow<'a> {
    work: super::child_obligations::WithChildSuccessor<&'a super::WorkItemSummary>,
    availability: super::WorkAvailability,
    #[serde(skip_serializing_if = "Option::is_none")]
    blocking_parent: Option<super::WorkLifecycle>,
    reason_codes: &'a [crate::WorkReadinessReason],
    why: &'a [String],
    blocked_by: &'a [crate::WorkId],
    blocker_count: usize,
}

impl<'a> From<&'a super::ReadyWorkSummary> for VerboseRow<'a> {
    fn from(row: &'a super::ReadyWorkSummary) -> Self {
        let super::ReadyWorkSummary {
            work,
            availability,
            blocking_parent,
            reason_codes,
            why,
            blocked_by,
            blocker_count,
        } = row;
        Self {
            work: super::child_obligations::WithChildSuccessor {
                value: work,
                child_resolution: super::child_obligations::ShowChildSuccessor::for_work(work),
            },
            availability: *availability,
            blocking_parent: *blocking_parent,
            reason_codes,
            why,
            blocked_by,
            blocker_count: *blocker_count,
        }
    }
}

impl LsInput {
    pub(super) fn validate_listing(&self) -> Result<(), VerbError> {
        if (self.optional || self.required) && self.under.is_none()
            || self.optional && self.required
        {
            return Err(StoreError::InvalidWork(
                "choose --optional or --required with --under PARENT".into(),
            )
            .into());
        }
        for value in [&self.search, &self.label, &self.under]
            .into_iter()
            .flatten()
        {
            if value
                .chars()
                .any(crate::domain::is_unsafe_rendered_text_char)
            {
                return Err(StoreError::InvalidWork(
                    "listing filters must be single-line text without terminal controls".into(),
                )
                .into());
            }
        }
        Ok(())
    }

    /// Reproduce the query in PowerShell or POSIX shells; data is not syntax.
    pub(super) fn list_command(&self) -> String {
        let mut parts = vec!["engram work ls".to_owned()];
        for (name, value) in [
            ("search", &self.search),
            ("label", &self.label),
            ("under", &self.under),
        ] {
            if let Some(value) = value {
                parts.push(format!("--{name}={}", shell_quote(value)));
            }
        }
        for (name, enabled) in [
            ("blocked", self.blocked),
            ("mine", self.mine),
            ("all", self.all),
            ("optional", self.optional),
            ("required", self.required),
            ("verbose", self.verbose),
        ] {
            if enabled {
                parts.push(format!("--{name}"));
            }
        }
        parts.push(format!(
            "--limit {}",
            self.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, 1000)
        ));
        parts.join(" ")
    }
}

pub(super) fn shell_quote(value: &str) -> String {
    let mut quoted = String::from("'");
    for ch in value.chars() {
        // PowerShell recognizes these typographic delimiters as well. A
        // double-quoted single character between literal segments works in
        // both shells; only ASCII quote characters are used as syntax.
        if matches!(ch, '\'' | '\u{2018}' | '\u{2019}' | '\u{201a}' | '\u{201b}') {
            quoted.push_str("'\"");
            quoted.push(ch);
            quoted.push_str("\"'");
        } else {
            quoted.push(ch);
        }
    }
    quoted.push('\'');
    quoted
}

/// Fits rows, footer, and the last-emitted-key continuation as one receipt.
pub(super) fn fit_list_receipt(
    input: &LsInput,
    page: &WorkListingPage,
    budget: usize,
) -> Result<Receipt, VerbError> {
    let claims = page
        .claims
        .iter()
        .map(|claim| (claim.work_id, (claim.holder.clone(), claim.expires_at)))
        .collect::<HashMap<_, _>>();
    let command = input.list_command();
    let first_ref = page.items.first().map(|item| item.work.short_ref.clone());
    let mut byte_limited = false;
    let (mut lower, mut upper) = (0, page.items.len());
    let mut visible = upper;
    let mut best = None;
    loop {
        let items = &page.items[..visible];
        let compact = items
            .iter()
            .map(|item| compact_row(item, &claims))
            .collect::<Vec<_>>();
        let omitted = page.total.saturating_sub(page.preceding + visible);
        let after = if omitted > 0 {
            items
                .last()
                .map(|item| page.continuation(item.work.work_id))
                .transpose()?
        } else {
            None
        };
        let continuation = after
            .as_ref()
            .map(|after| format!("{command} --after {after}"));
        let hint = if omitted == 0 {
            None
        } else if visible == 0 {
            first_ref.as_ref().map(|work_ref| format!("page is byte-bounded; first remaining match is {work_ref}; its row exceeds the page budget; use show for detail"))
        } else if byte_limited {
            Some("page is byte-bounded; continue with the same filters and ordering".to_owned())
        } else {
            Some("page reached --limit; continue with the same filters and ordering".to_owned())
        };
        let mut lines = vec![format!("showing {visible} of {} item(s):", page.total)];
        lines.extend(
            compact
                .iter()
                .map(|row| format!("  {}", compact_row_line(row))),
        );
        let limit = input.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, 1000);
        lines.push(format!("page: {visible} shown, {} previously shown, {omitted} remaining; --limit {limit}; byte budget {budget}{}",
            page.preceding, if byte_limited { " (byte-bounded)" } else { "" }));
        if let Some(hint) = &hint {
            lines.push(format!("  ({hint})"));
        }
        let next = vec![continuation.unwrap_or_else(|| {
            first_ref.as_ref().map_or_else(
                || "engram work add \"…\"".into(),
                |work_ref| format!("engram work show {work_ref}"),
            )
        })];
        let mut value = json!({
            "items": if input.verbose { serde_json::to_value(items.iter().map(VerboseRow::from).collect::<Vec<_>>())? } else { serde_json::to_value(compact)? },
            "total": page.total, "omitted": omitted, "more": omitted > 0,
            "shown_before": page.preceding, "limit": limit, "byte_budget": budget,
        });
        if let Some(after) = after {
            value["after"] = json!(after);
        }
        if let Some(hint) = hint {
            value["hint"] = json!(hint);
        }
        let receipt = Receipt::assemble(
            lines,
            Guidance {
                reminders: Vec::new(),
                next,
            },
            value,
            false,
        );
        // Leave room for the CLI's final newline in either representation.
        if list_fits(&receipt, budget)? {
            lower = visible;
            best = Some(receipt);
        } else {
            if visible == 1 && omitted > 0 {
                // Check the same row and envelope without continuation before
                // blaming the row. Filter/identity bytes can dominate a token.
                let mut without_continuation = receipt.clone();
                without_continuation
                    .value
                    .as_object_mut()
                    .ok_or_else(|| {
                        StoreError::InvalidWorkProjection("list receipt must be an object".into())
                    })?
                    .remove("after");
                without_continuation.next =
                    vec![format!("engram work show {}", items[0].work.short_ref)];
                without_continuation.value["next"] = json!(without_continuation.next);
                if list_fits(&without_continuation, budget)? {
                    return Err(StoreError::WorkCatalogCursorInvalid {
                        reason: "listing continuation metadata exceeds the response budget; shorten search, label or parent scope before listing again".into(),
                    }.into());
                }
            }
            if visible == 0 {
                return Err(StoreError::InvalidWorkProjection(
                    "list metadata exceeds the response budget".into(),
                )
                .into());
            }
            upper = visible - 1;
            byte_limited = true;
        }
        if lower >= upper
            && let Some(receipt) = best.take()
        {
            return Ok(receipt);
        }
        visible = lower + (upper - lower).div_ceil(2);
    }
}

fn list_fits(receipt: &Receipt, budget: usize) -> Result<bool, VerbError> {
    Ok(serde_json::to_vec_pretty(&receipt.value)?.len() < budget && receipt.text().len() < budget)
}
