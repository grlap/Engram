//! One verb-owned mutation envelope. Core Summary focus, including next and
//! propose, bounds `outcome` to 192 bytes; repeated planning/history
//! projections are available through `full_detail`.

use super::{
    DateTime, Guidance, Holder, Receipt, Serialize, Utc, Value, VerbError, WorkFocusView,
    WorkItemSummary, WorkLifecycle, WorkObligationPage, WorkObligationState, WorkSectionOmission,
    lifecycle_word,
};

#[derive(Serialize)]
pub(super) struct MutationWork<'a> {
    short_ref: &'a str,
    title: &'a str,
    lifecycle: WorkLifecycle,
    revision: i64,
}

impl<'a> From<&'a WorkItemSummary> for MutationWork<'a> {
    fn from(work: &'a WorkItemSummary) -> Self {
        Self {
            short_ref: &work.short_ref,
            title: &work.title,
            lifecycle: work.lifecycle,
            revision: work.revision,
        }
    }
}

#[derive(Serialize)]
struct ClaimAuthority {
    holder: String,
    held_until: DateTime<Utc>,
}

#[derive(Serialize)]
struct ObligationCounts {
    open: usize,
    omitted: usize,
}

/// Every open obligation on the run, including any the page leaves out. A
/// page stored before the count existed can only count the ones it shows.
fn open_obligations(page: &WorkObligationPage) -> usize {
    page.open_total.unwrap_or_else(|| {
        page.items
            .iter()
            .filter(|item| item.state == WorkObligationState::Open)
            .count()
    })
}

#[derive(Serialize)]
struct MutationEnvelope<'a, T> {
    operation: &'a str,
    work: MutationWork<'a>,
    #[serde(skip_serializing_if = "Option::is_none")]
    claim: Option<ClaimAuthority>,
    #[serde(flatten)]
    result: T,
    obligations: ObligationCounts,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    omissions: &'a Vec<WorkSectionOmission>,
    full_detail: String,
}

/// Navigation is emitted once, outside the lifecycle `next` list. Short refs
/// come from the bound core result and use the shared ASCII shell framing.
pub(super) fn full_detail(work_ref: &str, suffix: &str) -> String {
    format!(
        "engram work show {}{suffix}",
        super::listing::shell_quote(work_ref)
    )
}

pub(super) fn full_contract(work_ref: &str) -> String {
    format!(
        "engram work show {} --full",
        super::listing::shell_quote(work_ref)
    )
}

pub(super) fn needs_full_contract(view: &WorkFocusView) -> bool {
    view.title_truncated
        || view.outcome_omitted_bytes.is_some()
        || view.status.work.acceptance_count > view.status.work.acceptance.len()
}

/// `result` must serialize as an object/map: its operation-specific fields
/// are flattened into the common envelope rather than nested as a value.
pub(super) fn receipt(
    view: &WorkFocusView,
    operation: &str,
    result: impl Serialize,
    mut lines: Vec<String>,
    mut guidance: Guidance,
    holder: Holder<'_>,
    owed: bool,
) -> Result<Receipt, VerbError> {
    let work = &view.status.work;
    let suffix = match operation {
        "gate" => " --notes --gates",
        "note" | "add" => " --notes",
        _ => "",
    };
    let detail = full_detail(&work.short_ref, suffix);
    // A full-detail read replaces the old generic focus read, but never a
    // refusal's specific recovery or another item's navigation.
    if !owed {
        guidance.next.retain(|command| {
            command != &detail && command != &format!("engram work show {}", work.short_ref)
        });
    }
    if let Some(line) = lines.first_mut() {
        use std::fmt::Write as _;
        let _ = write!(
            line,
            " [{}; revision {}]",
            lifecycle_word(work.lifecycle),
            work.revision
        );
    }
    lines.push(format!("full detail: {}", super::terminal_command(&detail)));
    let claim = match holder {
        Holder::You(held_until) => Some(ClaimAuthority {
            holder: "you".into(),
            held_until,
        }),
        Holder::Other(session, held_until, identity) => Some(ClaimAuthority {
            holder: identity.session(session),
            held_until,
        }),
        Holder::Nobody => None,
    };
    let value = serde_json::to_value(MutationEnvelope {
        operation,
        work: work.into(),
        claim,
        result,
        obligations: ObligationCounts {
            open: open_obligations(&view.obligation_page),
            omitted: view.obligation_page.omitted_count,
        },
        omissions: &view.omissions,
        full_detail: detail,
    })?;
    Ok(Receipt::assemble(lines, guidance, value, owed))
}

#[derive(Serialize)]
pub(super) struct NoteResult<'a> {
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub non_holder: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub checkpoint: Option<&'a Value>,
    pub evidence: &'a Value,
}

impl<'a> From<&'a crate::work_service::WorkNoteResult> for NoteResult<'a> {
    fn from(result: &'a crate::work_service::WorkNoteResult) -> Self {
        Self {
            non_holder: result.non_holder,
            checkpoint: (result.receipt.result != result.evidence.result)
                .then_some(&result.receipt.result),
            evidence: &result.evidence.result,
        }
    }
}
