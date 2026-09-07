//! Bounded disclosure of absent per-criterion links in a frozen seal.
//!
//! The `focus/byte_budget` omission bucket deliberately aggregates criterion
//! positions with other omitted focus content into one entry. The criterion
//! portion remains separately attributable in `acceptance_evidence.omitted_count`.

use super::{Receipt, Serialize, VerbError, WorkSectionOmission, json};
use crate::work_service::{WorkAcceptanceEvidence, WorkNextSection, WorkSectionOmissionReason};

pub(super) const UNLINKED_LABEL: &str = "no evidence linked to this criterion";
pub(super) const RESTORED_UNAVAILABLE: &str =
    "this store holds no per-criterion evidence record for this restored completion";
pub(super) const REPLAY_UNAVAILABLE: &str =
    "per-criterion evidence unavailable for this completed work";

/// The total survives byte shedding; positions name only the visible subset.
#[derive(Clone, Debug, Serialize)]
pub(super) struct AcceptanceEvidence {
    criteria_count: usize,
    unlinked_count: usize,
    unlinked_positions: Vec<usize>,
    unlinked_label: &'static str,
    omitted_count: usize,
}

impl AcceptanceEvidence {
    pub(super) fn new(facts: &WorkAcceptanceEvidence) -> Self {
        Self {
            criteria_count: facts.criteria_count,
            unlinked_count: facts.unlinked_count,
            unlinked_positions: facts.unlinked_positions.clone(),
            unlinked_label: UNLINKED_LABEL,
            omitted_count: facts
                .unlinked_count
                .saturating_sub(facts.unlinked_positions.len()),
        }
    }

    pub(super) fn lines(&self) -> Vec<String> {
        if self.criteria_count == 0 {
            return Vec::new();
        }
        let mut lines = vec![format!(
            "criterion evidence: {} of {} criteria unlinked ({} shown)",
            self.unlinked_count,
            self.criteria_count,
            self.unlinked_positions.len()
        )];
        for position in &self.unlinked_positions {
            lines.push(format!("  criterion {position}: {UNLINKED_LABEL}"));
        }
        if self.omitted_count > 0 {
            lines.push(format!(
                "  ({} more unlinked criteria not shown)",
                self.omitted_count
            ));
        }
        lines
    }

    pub(super) fn append(&self, receipt: &Receipt) -> Result<Receipt, VerbError> {
        if self.criteria_count == 0 {
            return Ok(receipt.clone());
        }
        let mut lines = receipt.lines.clone();
        lines.extend(self.lines());
        let mut value = receipt.value.clone();
        let fields = value.as_object_mut().ok_or_else(|| {
            super::StoreError::InvalidWorkProjection(
                "acceptance disclosure requires an object receipt".into(),
            )
        })?;
        fields.insert("acceptance_evidence".into(), json!(self));
        if self.omitted_count > 0 {
            let mut omissions: Vec<WorkSectionOmission> = serde_json::from_value(
                fields
                    .get("omissions")
                    .cloned()
                    .unwrap_or_else(|| json!([])),
            )?;
            if let Some(entry) = omissions.iter_mut().find(|entry| {
                entry.section == WorkNextSection::Focus
                    && entry.reason == WorkSectionOmissionReason::ByteBudget
            }) {
                entry.omitted_count += self.omitted_count;
            } else {
                omissions.push(WorkSectionOmission {
                    section: WorkNextSection::Focus,
                    reason: WorkSectionOmissionReason::ByteBudget,
                    omitted_count: self.omitted_count,
                });
            }
            fields.insert("omissions".into(), json!(omissions));
        }
        Ok(Receipt::assemble(
            lines,
            super::Guidance {
                reminders: receipt.reminders.clone(),
                next: receipt.next.clone(),
            },
            value,
            receipt.owed,
        ))
    }

    pub(super) fn facts(&self) -> WorkAcceptanceEvidence {
        WorkAcceptanceEvidence {
            criteria_count: self.criteria_count,
            unlinked_count: self.unlinked_count,
            unlinked_positions: self.unlinked_positions.clone(),
        }
    }

    pub(super) fn omitted_count(&self) -> usize {
        self.omitted_count
    }
}

/// Fit whole positional rows against both final twins, including child guidance.
/// No truncation or text matching can change which criterion a row names.
pub(super) fn fit_done(
    facts: &WorkAcceptanceEvidence,
    render: impl Fn(&AcceptanceEvidence) -> Result<Receipt, VerbError>,
    budget: usize,
) -> Result<Receipt, VerbError> {
    let mut page = AcceptanceEvidence::new(facts);
    let fits = |receipt: &Receipt| -> Result<bool, VerbError> {
        Ok(receipt.text().len() < budget
            && serde_json::to_vec_pretty(&receipt.value)?.len() < budget)
    };
    let full = render(&page)?;
    if fits(&full)? {
        return Ok(full);
    }
    let (mut lower, mut upper) = (0, facts.unlinked_positions.len());
    let mut best = None;
    while lower < upper {
        let visible = lower + (upper - lower) / 2;
        page.unlinked_positions = facts.unlinked_positions[..visible].to_vec();
        page.omitted_count = facts.unlinked_count.saturating_sub(visible);
        let receipt = render(&page)?;
        if fits(&receipt)? {
            best = Some(receipt);
            lower = visible + 1;
        } else {
            upper = visible;
        }
    }
    // Completion is already committed. Preserve truthful counts even when a
    // caller's artificial budget cannot contain the fixed receipt metadata.
    if let Some(receipt) = best {
        Ok(receipt)
    } else {
        page.unlinked_positions.clear();
        page.omitted_count = facts.unlinked_count;
        render(&page)
    }
}

pub(super) fn unavailable(
    receipt: Receipt,
    error_class: Option<&'static str>,
) -> Result<Receipt, VerbError> {
    let mut value = receipt.value;
    let fields = value.as_object_mut().ok_or_else(|| {
        super::StoreError::InvalidWorkProjection(
            "acceptance disclosure requires an object receipt".into(),
        )
    })?;
    fields.insert(
        "acceptance_evidence_unavailable".into(),
        json!(REPLAY_UNAVAILABLE),
    );
    let mut lines = receipt.lines;
    lines.push(format!("criterion evidence: {REPLAY_UNAVAILABLE}"));
    if let Some(class) = error_class {
        fields.insert("acceptance_evidence_error_class".into(), json!(class));
        lines.push(format!("  diagnostic class: {class}"));
    }
    Ok(Receipt::assemble(
        lines,
        super::Guidance {
            reminders: receipt.reminders,
            next: receipt.next,
        },
        value,
        receipt.owed,
    ))
}
