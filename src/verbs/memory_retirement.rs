//! Bounded, informational project-memory follow-ups after work lifecycle changes.

use serde_json::json;

use super::{Receipt, VerbError, receipts};
use crate::{domain::ProjectMemoryRetirementCandidates, storage::StoreError};

/// How the item that memories name as their retiring target left open work.
pub(super) enum RetirementAction {
    Completed,
    /// Superseded by `replacement`, the short ref of the item that replaces it
    /// (for a detach, the new independent root).
    Superseded {
        replacement: String,
    },
    Cancelled,
}

impl RetirementAction {
    fn word(&self) -> &'static str {
        match self {
            Self::Completed => "completed",
            Self::Superseded { .. } => "superseded",
            Self::Cancelled => "cancelled",
        }
    }

    fn instruction(&self) -> String {
        match self {
            Self::Completed => "forget candidates after review; nothing was forgotten".into(),
            Self::Superseded { replacement } => format!(
                "review each: revise it with --retires-with local:{replacement}, clear its target, or forget it after checking; nothing was changed"
            ),
            Self::Cancelled => "review each: the memory stays in force until revised to another item, its target cleared, or forgotten; nothing was changed".into(),
        }
    }
}

fn render(
    base: &Receipt,
    result: &Result<ProjectMemoryRetirementCandidates, StoreError>,
    action: &RetirementAction,
    visible: usize,
) -> Receipt {
    let mut receipt = base.clone();
    match result {
        Err(error) => {
            let class = crate::work_service::advisory_error_class(error);
            receipt.lines.push(format!(
                "project-memory retirement candidates unavailable ({class}); the item stays {}",
                action.word()
            ));
            receipt.value["memory_retirement"] =
                json!({"action": action.word(), "error_class": class});
        }
        Ok(candidates) if candidates.total > 0 => {
            let shown = visible.min(candidates.keys.len());
            let items = candidates
                .keys
                .iter()
                .take(shown)
                .map(|key| {
                    json!({
                        "key": key,
                        "read_command": format!("engram work memories {key} --full"),
                        "forget_command": format!("engram work forget {key}"),
                    })
                })
                .collect::<Vec<_>>();
            let omitted = candidates.total.saturating_sub(shown);
            let instruction = action.instruction();
            let mut value = json!({
                "action": action.word(),
                "total": candidates.total,
                "omitted": omitted,
                "items": items,
                "instruction": instruction,
            });
            if let RetirementAction::Superseded { replacement } = action {
                value["replacement"] = json!(replacement);
            }
            receipt.value["memory_retirement"] = value;
            receipt.lines.push(format!(
                "{} project memory(ies) name this {} item as their retiring target, {omitted} omitted; {instruction}",
                candidates.total,
                action.word(),
            ));
            for key in candidates.keys.iter().take(shown) {
                receipt.lines.push(format!(
                    "  {key}: read engram work memories {key} --full first; forget with engram work forget {key}"
                ));
            }
        }
        Ok(_) => {}
    }
    receipt
}

/// Bytes the advisory needs in its smallest form (no key rows), so a caller
/// that fits other sections first leaves room for the count and the omission.
pub(super) fn reserve(
    base: &Receipt,
    result: &Result<ProjectMemoryRetirementCandidates, StoreError>,
    action: &RetirementAction,
) -> Result<usize, VerbError> {
    let minimal = render(base, result, action, 0);
    Ok(minimal.text().len().saturating_sub(base.text().len()).max(
        receipts::compact_receipt_json_bytes(&minimal.value)?
            .saturating_sub(receipts::compact_receipt_json_bytes(&base.value)?),
    ))
}

/// Appends as many candidate rows as fit the budget, never fewer than the
/// count and exact omission; the rows that do not fit are counted as omitted.
pub(super) fn append(
    base: &Receipt,
    result: &Result<ProjectMemoryRetirementCandidates, StoreError>,
    action: &RetirementAction,
    budget: usize,
) -> Result<Receipt, VerbError> {
    let count = result
        .as_ref()
        .map_or(0, |candidates| candidates.keys.len());
    for visible in (1..=count).rev() {
        let receipt = render(base, result, action, visible);
        if receipts::agent_receipt_fits(&receipt, budget)? {
            return Ok(receipt);
        }
    }
    // The count-only form, whether or not it fits: the work mutation has
    // already committed, so the receipt keeps saying which memories name the
    // item, as completion keeps its irreducible facts. `done` reserves this
    // form before fitting its other sections; an `update` receipt is small.
    Ok(render(base, result, action, 0))
}
