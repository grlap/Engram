use std::{
    collections::HashMap,
    fmt::{self, Write as _},
};

use super::{
    COMPLETED_WORK_LATE_FINDING_REFUSAL, DateTime, MAX_COMPACT_HOLDER_BYTES,
    MAX_COMPACT_LABEL_BYTES, MAX_COMPACT_LABEL_ITEMS, MAX_COMPACT_NEXT_JSON_BYTES,
    MAX_COMPACT_REMINDER_ITEMS, MAX_COMPACT_TITLE_BYTES, MAX_TEXT_NEXT_COMMANDS,
    ProjectMemorySignal, ReadyWorkSummary, Serialize, SessionId, StoreError, Utc, Value, WorkId,
    WorkItemKind, WorkNextSection, WorkNextView, WorkSectionOmissionReason, clock,
    compact_state_word, json, kind_word, lifecycle_word, render_agent_receipt_text, short,
    short_ref_for_work_id, short_with_limit,
};

/// Fits the complete list receipt, including guidance, inside one byte budget.
pub(super) fn fit_list_receipt(
    verbose: bool,
    source_items: &[ReadyWorkSummary],
    total: usize,
    claims: &HashMap<WorkId, (SessionId, DateTime<Utc>)>,
    budget: usize,
) -> Result<Receipt, super::VerbError> {
    let first_ref = source_items.first().map(|item| item.work.short_ref.clone());
    let mut byte_limited = false;
    let (mut lower, mut upper) = (0, source_items.len());
    let mut visible = upper;
    let mut best = None;
    loop {
        let items = &source_items[..visible];
        let compact_items = items
            .iter()
            .map(|item| compact_row(item, claims))
            .collect::<Vec<_>>();
        let omitted = total.saturating_sub(items.len());
        let hint = if omitted == 0 {
            None
        } else if visible == 0
            && let Some(work_ref) = &first_ref
        {
            Some(format!(
                "page is byte-bounded; first match is {work_ref}; its row exceeds the page budget; use show for detail"
            ))
        } else if byte_limited {
            Some(
                "page is byte-bounded; narrow with --search or --label, or show an item".to_owned(),
            )
        } else {
            Some("raise --limit or narrow with --search or --label".to_owned())
        };
        let mut lines = vec![format!("showing {} of {total} item(s):", items.len())];
        lines.extend(
            compact_items
                .iter()
                .map(|item| format!("  {}", compact_row_line(item))),
        );
        if let Some(hint) = &hint {
            lines.push(format!("  ({hint})"));
        }
        let next = vec![first_ref.as_ref().map_or_else(
            || "engram work add \"…\"".into(),
            |work_ref| format!("engram work show {work_ref}"),
        )];
        let mut value = json!({
            "items": if verbose { serde_json::to_value(items)? } else { serde_json::to_value(&compact_items)? },
            "total": total,
            "omitted": omitted,
            "more": omitted > 0,
        });
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
        if serde_json::to_vec_pretty(&receipt.value)?.len() <= budget
            && receipt.text().len() <= budget
        {
            lower = visible;
            best = Some(receipt);
        } else {
            if visible == 0 {
                // Defensive: bounded metadata alone should always fit.
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
        // Count only final emitted rows; the exact total and the hint
        // participate in the bounded prefix search, not after fitting.
        visible = lower + (upper - lower).div_ceil(2);
    }
}

/// Short list row used by the agent words. Host-only `work core focus` remains
/// the rich-object boundary; absent claim and parent fields are omitted to
/// keep repeated navigation inexpensive.
#[derive(Clone, Debug, Serialize)]
pub(super) struct CompactWorkRow {
    #[serde(rename = "ref")]
    pub(super) work_ref: String,
    pub(super) title: String,
    pub(super) state: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) holder: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) held_until: Option<String>,
    pub(super) priority: i32,
    pub(super) kind: WorkItemKind,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub(super) labels: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) labels_omitted: Option<usize>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) parent_ref: Option<String>,
}

pub(super) fn ready_line(item: &ReadyWorkSummary) -> String {
    compact_row_line(&compact_row(item, &HashMap::new()))
}

pub(super) fn compact_row_line(item: &CompactWorkRow) -> String {
    let mut line = format!(
        "{} [{}] p{} {} \"{}\"",
        item.work_ref,
        kind_word(item.kind),
        item.priority,
        item.state,
        item.title,
    );
    if !item.labels.is_empty() {
        let _ = write!(line, " labels:{}", item.labels.join(","));
    }
    if let Some(omitted) = item.labels_omitted {
        let _ = write!(line, " (+{omitted} labels)");
    }
    if let Some(parent_ref) = &item.parent_ref {
        let _ = write!(line, " ← {parent_ref}");
    }
    if let (Some(holder), Some(held_until)) = (&item.holder, &item.held_until) {
        let _ = write!(line, " held by {holder} until {held_until}");
    }
    line
}

/// One deliberately bounded part of a compact `next` response. This mirrors
/// the core omission shape while adding the verb-owned `held`, `reminders`,
/// and `next` sections that do not exist in [`WorkNextSection`].
#[derive(Clone, Debug, Serialize)]
pub(super) struct CompactSectionOmission {
    pub(super) section: String,
    pub(super) reason: WorkSectionOmissionReason,
    pub(super) omitted_count: usize,
}

#[derive(Clone)]
pub(super) struct CompactNextReceipt {
    pub(super) focus: Option<CompactWorkRow>,
    pub(super) held: Vec<CompactWorkRow>,
    pub(super) ready: Vec<CompactWorkRow>,
    pub(super) changes: Vec<String>,
    pub(super) memories: Option<ProjectMemorySignal>,
    pub(super) omissions: Vec<CompactSectionOmission>,
    pub(super) guidance: Guidance,
}

#[derive(Clone, Copy)]
pub(super) enum CompactRowLocation {
    Ready(usize),
    Held(usize),
    Focus,
}

/// Words and commands derived for the agent from one core response.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Guidance {
    pub reminders: Vec<String>,
    pub next: Vec<String>,
}

/// One word's answer: text lines for the shell, the exact structured receipt
/// (existing shape plus `reminders` and `next`) for `--json` and MCP, and
/// whether something is still owed.
#[derive(Clone, Debug)]
pub struct Receipt {
    lines: Vec<String>,
    build_footer: Option<String>,
    pub reminders: Vec<String>,
    pub next: Vec<String>,
    pub value: Value,
    /// A typed completion refusal: the shell exits with status 2.
    pub owed: bool,
}

impl Receipt {
    pub(super) fn with_build_identity(mut self) -> Self {
        let identity = crate::build_identity::current();
        self.value["build_fingerprint"] = json!(identity.build_fingerprint);
        self.build_footer = Some(format!(
            "build: {}",
            crate::build_identity::short_hash(
                identity
                    .build_fingerprint
                    .as_ref()
                    .map(crate::ObjectHash::as_str)
            ),
        ));
        self
    }

    pub(super) fn with_reminder(mut self, reminder: String) -> Result<Self, super::VerbError> {
        self.reminders.push(reminder);
        self.value["reminders"] = json!(self.reminders);
        if self.text().len() > super::MAX_AGENT_WORK_RESPONSE_BYTES
            || serde_json::to_vec_pretty(&self.value)?.len() > super::MAX_AGENT_WORK_RESPONSE_BYTES
        {
            return Err(StoreError::InvalidWorkProjection(
                "add receipt exceeds the response budget".into(),
            )
            .into());
        }
        Ok(self)
    }

    pub(super) fn assemble(
        lines: Vec<String>,
        guidance: Guidance,
        value: Value,
        owed: bool,
    ) -> Self {
        let mut value = match value {
            Value::Object(map) => Value::Object(map),
            other => json!({ "result": other }),
        };
        value["reminders"] = json!(guidance.reminders);
        value["next"] = json!(guidance.next);
        Self {
            lines,
            build_footer: None,
            reminders: guidance.reminders,
            next: guidance.next,
            value,
            owed,
        }
    }

    /// Adds the caller's process-defaulted session handle to a successful CLI
    /// mutation receipt without changing the shared verb or MCP receipt by
    /// default. Owed completion refusals remain unchanged.
    #[must_use]
    pub fn with_effective_session_id(mut self, session_id: &SessionId) -> Self {
        if self.owed {
            return self;
        }
        if let Value::Object(fields) = &mut self.value {
            debug_assert!(!fields.contains_key("effective_session_id"));
            fields
                .entry("effective_session_id")
                .or_insert_with(|| Value::String(session_id.0.clone()));
        }
        self
    }

    /// Shell rendering: the receipt lines, then `reminders:` and at most four
    /// `next:` commands plus an explicit omission marker. Never contains
    /// object hashes, fences, or idempotency keys. Only `next` appends the
    /// compact diagnostic build token after all guidance.
    #[must_use]
    pub fn text(&self) -> String {
        let mut text = render_agent_receipt_text(&self.lines, &self.reminders, &self.next);
        if let Some(footer) = &self.build_footer {
            text.push('\n');
            text.push_str(footer);
        }
        text
    }
}

/// Full-note mode fits whole notes after projecting away internal identities.
/// Its count includes inherited history and every native execution generation.
pub(super) fn fit_show_notes(
    mut receipt: Receipt,
    page: crate::storage::WorkNotePage,
    current_actor: &str,
    budget: usize,
) -> Result<Receipt, super::VerbError> {
    receipt.lines.retain(|line| !line.starts_with("notes:"));
    if let Some(omissions) = receipt
        .value
        .get_mut("omissions")
        .and_then(Value::as_array_mut)
    {
        omissions.retain(|omission| omission["reason"] != "evidence_count_limit");
    }
    if receipt
        .value
        .get("omissions")
        .and_then(Value::as_array)
        .is_some_and(Vec::is_empty)
        && let Some(fields) = receipt.value.as_object_mut()
    {
        fields.remove("omissions");
    }
    let base_lines = receipt.lines.clone();
    let notes = page
        .items
        .into_iter()
        .map(|note| super::show::ShowNote {
            kind: note.kind,
            non_holder: note
                .actor
                .provenance_chain
                .iter()
                .any(crate::domain::is_non_holder_note_marker),
            summary: note.summary,
            refs: note.refs,
            by: Some(super::show::relative_actor_label(
                &note.actor.actor_id,
                note.actor.attribution_context(),
                current_actor,
            )),
            created_at: note.recorded_at,
        })
        .collect::<Vec<_>>();
    let (mut lower, mut upper) = (0, notes.len());
    let mut visible = upper;
    let mut best = None;
    loop {
        #[cfg(test)]
        SHOW_NOTE_FIT_PROBES.with(|count| count.set(count.get() + 1));
        render_note_prefix(&mut receipt, &base_lines, &notes[..visible], page.total)?;
        if receipt.text().len() <= budget
            && serde_json::to_vec_pretty(&receipt.value)?.len() <= budget
        {
            lower = visible;
            best = Some(receipt.clone());
        } else if visible == 0 {
            return Err(StoreError::InvalidWorkProjection(
                "show metadata exceeds the response budget".into(),
            )
            .into());
        } else {
            upper = visible - 1;
        }
        if lower == upper {
            if best.is_none() && visible != 0 {
                visible = 0;
                continue;
            }
            return best.ok_or_else(|| {
                StoreError::InvalidWorkProjection(
                    "show metadata exceeds the response budget".into(),
                )
                .into()
            });
        }
        visible = lower + (upper - lower).div_ceil(2);
    }
}

fn render_note_prefix(
    receipt: &mut Receipt,
    base_lines: &[String],
    notes: &[super::show::ShowNote],
    total: usize,
) -> Result<(), serde_json::Error> {
    receipt.value["notes"] = serde_json::to_value(notes)?;
    receipt.value["notes_omitted"] = json!(total - notes.len());
    receipt.lines.clear();
    receipt.lines.extend_from_slice(base_lines);
    receipt
        .lines
        .push(format!("notes: {total} recorded (oldest first)"));
    for note in notes {
        receipt.lines.push(format!(
            "  - {}{} by {} at {}:\n{}",
            super::evidence_kind_word(note.kind),
            if note.non_holder { " (non-holder)" } else { "" },
            super::terminal_safe_multiline(note.by.as_deref().unwrap_or("another actor")),
            note.created_at,
            super::terminal_safe_multiline(&note.summary)
                .lines()
                .map(|line| format!("    {line}"))
                .collect::<Vec<_>>()
                .join("\n")
        ));
        for reference in &note.refs {
            receipt.lines.push(format!(
                "    ref: {}",
                super::terminal_safe_multiline(reference).replace('\n', "\n         ")
            ));
        }
    }
    if total > notes.len() {
        receipt.lines.push(format!(
            "  ({} notes omitted by the response budget)",
            total - notes.len()
        ));
    }
    Ok(())
}

#[cfg(test)]
thread_local! {
    pub(super) static SHOW_NOTE_FIT_PROBES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// A core failure plus the item it concerned, when known.
#[derive(Debug)]
pub struct VerbError {
    pub error: StoreError,
    pub work_ref: Option<String>,
}

impl VerbError {
    pub(super) fn at(error: StoreError, work_ref: &str) -> Self {
        Self {
            error,
            work_ref: Some(work_ref.to_owned()),
        }
    }

    /// Words and commands that resolve the failure, when a fixed table knows.
    #[must_use]
    #[allow(
        clippy::too_many_lines,
        reason = "the fixed refusal-to-guidance table stays contiguous and exhaustively reviewable"
    )]
    pub fn guidance(&self) -> Guidance {
        let target = self.work_ref.as_deref().unwrap_or("<ref>");
        let message = self.error.to_string();
        let (reminders, next): (Vec<String>, Vec<String>) = match &self.error {
            StoreError::WorkClaimHeld { expires_at, .. } => (
                vec![format!(
                    "held by another session until {}",
                    DateTime::<Utc>::from_timestamp_millis(*expires_at)
                        .map_or_else(|| "its claim expires".into(), |at| clock(at, Utc::now()))
                )],
                vec![format!("engram work show {target}")],
            ),
            StoreError::WorkClaimMismatch { .. } => (
                vec!["this operation needs current claim authority; show the item before retrying".into()],
                vec![format!("engram work show {target}")],
            ),
            StoreError::WorkClaimLapsed { expired_at, .. } => (
                vec![format!(
                    "claim lapsed at {}",
                    clock(*expired_at, Utc::now())
                )],
                vec![format!("engram work claim {target}")],
            ),
            StoreError::WorkCompletionRefused { reason, .. } => {
                let words = if reason.contains("at least one evidence")
                    || reason.contains("no checkpoint")
                {
                    "nothing has been noted for this execution yet; say what was delivered".into()
                } else {
                    reason.clone()
                };
                (
                    vec![words],
                    vec![
                        format!("engram work done {target} \"…\""),
                        format!("engram work note {target} \"…\""),
                    ],
                )
            }
            StoreError::WorkCompletionRecoveryRequired { cause, .. } => (
                vec![format!("completion recovery is required: {cause:?}")],
                vec![format!("engram work show {target}")],
            ),
            StoreError::WorkNotOpen(_) => (
                vec!["this item is not open".into()],
                vec![format!("engram work show {target}")],
            ),
            StoreError::WorkParentNotOpen { lifecycle, .. } => (
                vec![crate::storage::parent_not_open_remedy(*lifecycle).into()],
                if *lifecycle == super::WorkLifecycle::Proposed {
                    vec![format!("engram work show {target}")]
                } else {
                    vec!["engram work add \"Follow-up title\" --accept \"Delivery criterion\"".into()]
                },
            ),
            StoreError::WorkPeerDecompositionRefused { .. } => (
                vec!["ask the parent holder to add required children or prerequisites; peers may propose optional children".into()],
                vec![format!("engram work add \"Proposal title\" --under {target} --optional")],
            ),
            StoreError::WorkPrerequisiteAlreadySatisfied(_) => (
                vec!["this prerequisite is already satisfied; no edge is needed".into()],
                vec![format!("engram work show {target}")],
            ),
            StoreError::WorkNotFound(_) => {
                (vec!["no such item".into()], vec!["engram work ls".into()])
            }
            StoreError::WorkReferenceAmbiguous {
                candidates, more, ..
            } => ambiguous_reference_guidance(candidates, *more),
            StoreError::ProjectMemoryExists(key) => (
                vec![format!(
                    "project memory {key} already exists; retry remember with an explicit --key"
                )],
                vec![
                    format!("engram work memories {key} --full"),
                    "engram work memories".into(),
                ],
            ),
            StoreError::ProjectMemoryRetired(key) => (
                vec![format!(
                    "project memory {key} is retired permanently; retry remember with an explicit --key"
                )],
                vec!["engram work memories".into()],
            ),
            StoreError::ProjectMemoryNotFound(_) => (
                vec!["no project memory uses that key".into()],
                vec!["engram work memories".into()],
            ),
            StoreError::ProjectMemoryBindingInvalid => (
                vec![
                    "the asserted actor/session binding for that project-memory action is absent or inconsistent".into(),
                ],
                Vec::new(),
            ),
            StoreError::InvalidProjectMemory(reason) if reason.contains("context_generation") => {
                (
                    vec![reason.clone()],
                    vec!["engram work next".into()],
                )
            }
            StoreError::InvalidProjectMemory(reason) => {
                (vec![reason.clone()], vec!["engram work memories".into()])
            }
            StoreError::InvalidWork(reason) if reason.contains("does not exist") => {
                (vec!["no such item".into()], vec!["engram work ls".into()])
            }
            StoreError::InvalidWork(reason) if reason == super::GATE_WORK_REF_REQUIRED => (
                vec![super::GATE_WORK_REF_REQUIRED.into()],
                Vec::new(),
            ),
            StoreError::InvalidWork(reason) if reason.contains("no focused work") => (
                vec!["no item is selected; name one or claim one first".into()],
                vec!["engram work next".into()],
            ),
            StoreError::InvalidWork(reason) if reason == crate::storage::PENDING_HANDOFF_REFUSAL => (
                vec![reason.clone()],
                vec![format!("engram work handoff {target} --cancel \"…\"")],
            ),
            StoreError::InvalidWork(reason) if reason.starts_with("work is not ready:") => (
                vec!["this item is not ready; inspect its blockers or deferral".into()],
                vec![format!("engram work show {target}")],
            ),
            StoreError::InvalidWork(reason)
                if reason.contains("claim recovery requires an explicit attributed reason") =>
            {
                (
                    vec![reason.clone()],
                    vec![format!("engram work claim {target} --recover \"…\"")],
                )
            }
            StoreError::InvalidWork(reason)
                if reason == COMPLETED_WORK_LATE_FINDING_REFUSAL =>
            {
                (
                    vec![reason.clone()],
                    vec![format!("engram work note {target} \"…\"")],
                )
            }
            StoreError::WorkRevisionConflict { .. } => (
                vec!["the item changed underneath this call; look again and repeat it".into()],
                vec![format!("engram work show {target}")],
            ),
            _ => (vec![message], Vec::new()),
        };
        Guidance { reminders, next }
    }
}

impl fmt::Display for VerbError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.error.fmt(formatter)
    }
}

impl std::error::Error for VerbError {}

impl From<StoreError> for VerbError {
    fn from(error: StoreError) -> Self {
        Self {
            error,
            work_ref: None,
        }
    }
}

impl From<serde_json::Error> for VerbError {
    fn from(error: serde_json::Error) -> Self {
        StoreError::from(error).into()
    }
}

pub(super) fn append_changes_lines(
    lines: &mut Vec<String>,
    changes: &[String],
    not_delivered: usize,
) {
    if changes.is_empty() && not_delivered == 0 {
        return;
    }
    if not_delivered > 0 && changes.is_empty() {
        lines.push(format!(
            "changes by others (none on this page; {not_delivered} more arrive with your next call):"
        ));
    } else if not_delivered > 0 {
        lines.push(format!(
            "changes by others ({} shown, {} more arrive with your next call):",
            changes.len(),
            not_delivered
        ));
    } else {
        lines.push(format!(
            "changes by others ({} since your last call):",
            changes.len()
        ));
    }
    lines.extend(changes.iter().map(|change| format!("  {change}")));
}

pub(super) fn ambiguous_reference_guidance(
    candidates: &[crate::WorkReferenceCandidate],
    more: usize,
) -> (Vec<String>, Vec<String>) {
    let mut reminders = candidates
        .iter()
        .map(|candidate| {
            format!(
                "{} \"{}\" is {}; use its full id {}",
                candidate.short_ref,
                short(&candidate.title),
                lifecycle_word(candidate.lifecycle),
                candidate.work_id.0
            )
        })
        .collect::<Vec<_>>();
    if more > 0 {
        reminders.push(format!(
            "{more} additional ambiguous candidates were omitted"
        ));
    }
    let next = candidates
        .iter()
        .map(|candidate| format!("engram work show {}", candidate.work_id.0))
        .collect();
    (reminders, next)
}

pub(super) fn compact_row(
    status: &ReadyWorkSummary,
    claims: &HashMap<WorkId, (SessionId, DateTime<Utc>)>,
) -> CompactWorkRow {
    let work = &status.work;
    let (holder, held_until) =
        claims
            .get(&work.work_id)
            .map_or((None, None), |(holder, expires_at)| {
                (
                    Some(short_with_limit(&holder.0, MAX_COMPACT_HOLDER_BYTES)),
                    Some(expires_at.format("%H:%M").to_string()),
                )
            });
    let (labels, labels_omitted) = compact_labels(&work.labels);
    CompactWorkRow {
        work_ref: work.short_ref.clone(),
        title: short_with_limit(&work.title, MAX_COMPACT_TITLE_BYTES),
        state: compact_state_word(work.lifecycle, status.availability).into(),
        holder,
        held_until,
        priority: work.priority,
        kind: work.kind,
        labels,
        labels_omitted,
        parent_ref: work.parent_id.map(short_ref_for_work_id),
    }
}

pub(super) fn compact_labels(labels: &[String]) -> (Vec<String>, Option<usize>) {
    let compact = labels
        .iter()
        .take(MAX_COMPACT_LABEL_ITEMS)
        .map(|label| short_with_limit(label, MAX_COMPACT_LABEL_BYTES))
        .collect::<Vec<_>>();
    let omitted = labels.len().saturating_sub(compact.len());
    (compact, (omitted > 0).then_some(omitted))
}

pub(super) fn compact_next_receipt(
    view: &WorkNextView,
    held: &[(ReadyWorkSummary, DateTime<Utc>)],
    ready: &[ReadyWorkSummary],
    changes: &[String],
    claims: &HashMap<WorkId, (SessionId, DateTime<Utc>)>,
    guidance: &Guidance,
) -> Result<CompactNextReceipt, VerbError> {
    let mut compact = CompactNextReceipt {
        focus: view
            .focus
            .as_ref()
            .map(|focus| compact_row(&focus.status, claims)),
        held: held
            .iter()
            .map(|(item, _)| compact_row(item, claims))
            .collect(),
        ready: ready.iter().map(|item| compact_row(item, claims)).collect(),
        changes: changes.iter().map(|change| short(change)).collect(),
        memories: view.memories.clone(),
        omissions: view
            .omissions
            .iter()
            .map(|omission| CompactSectionOmission {
                section: compact_section_name(omission.section).into(),
                reason: omission.reason,
                omitted_count: omission.omitted_count,
            })
            .collect(),
        guidance: Guidance {
            reminders: guidance
                .reminders
                .iter()
                .map(|reminder| short(reminder))
                .collect(),
            next: guidance.next.clone(),
        },
    };
    if compact.guidance.reminders.len() > MAX_COMPACT_REMINDER_ITEMS {
        let omitted = compact.guidance.reminders.len() - MAX_COMPACT_REMINDER_ITEMS;
        compact
            .guidance
            .reminders
            .truncate(MAX_COMPACT_REMINDER_ITEMS);
        record_compact_omission(&mut compact.omissions, "reminders", omitted);
    }
    if compact.guidance.next.len() > MAX_TEXT_NEXT_COMMANDS {
        let omitted = compact.guidance.next.len() - MAX_TEXT_NEXT_COMMANDS;
        compact.guidance.next.truncate(MAX_TEXT_NEXT_COMMANDS);
        record_compact_omission(&mut compact.omissions, "next", omitted);
    }
    fit_compact_next(compact)
}

pub(super) fn fit_compact_next(
    compact: CompactNextReceipt,
) -> Result<CompactNextReceipt, VerbError> {
    fit_compact_next_to(compact, MAX_COMPACT_NEXT_JSON_BYTES)
}

pub(super) fn fit_compact_next_to(
    mut compact: CompactNextReceipt,
    max_json_bytes: usize,
) -> Result<CompactNextReceipt, VerbError> {
    loop {
        let value = compact_next_value(&compact);
        let current_bytes = serde_json::to_vec_pretty(&value)?.len();
        if current_bytes < max_json_bytes {
            return Ok(compact);
        }
        if compact.memories.take().is_some() {
            // Keep this fixed-size advisory omission silent: the signal stays
            // unacknowledged and reannounces, while an omission row would be
            // larger than the value being removed.
            continue;
        }
        if compact.changes.pop().is_some() {
            record_compact_omission(&mut compact.omissions, "changes", 1);
            continue;
        }
        if shed_compact_labels(&mut compact, current_bytes)? {
            continue;
        }
        if compact.ready.pop().is_some() {
            record_compact_omission(&mut compact.omissions, "ready", 1);
            continue;
        }
        if compact.held.pop().is_some() {
            record_compact_omission(&mut compact.omissions, "held", 1);
            continue;
        }
        if compact.guidance.reminders.pop().is_some() {
            record_compact_omission(&mut compact.omissions, "reminders", 1);
            continue;
        }
        if compact.guidance.next.len() > 1 {
            compact.guidance.next.pop();
            record_compact_omission(&mut compact.omissions, "next", 1);
            continue;
        }
        if compact.focus.take().is_some() {
            record_compact_omission(&mut compact.omissions, "focus", 1);
            continue;
        }
        // Every remaining string is fixed or explicitly byte-bounded. This
        // final value is therefore below the budget without making a valid
        // `next` call fail merely because advisory sections were large.
        return Ok(compact);
    }
}

pub(super) fn shed_compact_labels(
    compact: &mut CompactNextReceipt,
    current_bytes: usize,
) -> Result<bool, VerbError> {
    for index in (0..compact.ready.len()).rev() {
        if try_shed_compact_labels(compact, CompactRowLocation::Ready(index), current_bytes)? {
            return Ok(true);
        }
    }
    for index in (0..compact.held.len()).rev() {
        if try_shed_compact_labels(compact, CompactRowLocation::Held(index), current_bytes)? {
            return Ok(true);
        }
    }
    try_shed_compact_labels(compact, CompactRowLocation::Focus, current_bytes)
}

pub(super) fn try_shed_compact_labels(
    compact: &mut CompactNextReceipt,
    location: CompactRowLocation,
    current_bytes: usize,
) -> Result<bool, VerbError> {
    let (labels, previous_omitted) = {
        let Some(row) = compact_row_at_mut(compact, location) else {
            return Ok(false);
        };
        if row.labels.is_empty() {
            return Ok(false);
        }
        let labels = std::mem::take(&mut row.labels);
        let previous_omitted = row.labels_omitted;
        row.labels_omitted = Some(previous_omitted.unwrap_or(0) + labels.len());
        (labels, previous_omitted)
    };
    let candidate_bytes = serde_json::to_vec_pretty(&compact_next_value(compact))?.len();
    if candidate_bytes < current_bytes {
        return Ok(true);
    }
    if let Some(row) = compact_row_at_mut(compact, location) {
        row.labels = labels;
        row.labels_omitted = previous_omitted;
    }
    Ok(false)
}

pub(super) fn compact_row_at_mut(
    compact: &mut CompactNextReceipt,
    location: CompactRowLocation,
) -> Option<&mut CompactWorkRow> {
    match location {
        CompactRowLocation::Ready(index) => compact.ready.get_mut(index),
        CompactRowLocation::Held(index) => compact.held.get_mut(index),
        CompactRowLocation::Focus => compact.focus.as_mut(),
    }
}

pub(super) fn compact_next_value(compact: &CompactNextReceipt) -> Value {
    json!({
        // Identity participates in byte fitting, not a post-fit append.
        "build_fingerprint": crate::build_identity::current().build_fingerprint,
        "focus": compact.focus,
        "held": compact.held,
        "ready": compact.ready,
        "changes": compact.changes,
        "memories": compact.memories,
        "omissions": compact.omissions,
        "reminders": compact.guidance.reminders,
        "next": compact.guidance.next,
    })
}

pub(super) fn record_compact_omission(
    omissions: &mut Vec<CompactSectionOmission>,
    section: &str,
    omitted_count: usize,
) {
    if let Some(omission) = omissions.iter_mut().find(|omission| {
        omission.section == section && omission.reason == WorkSectionOmissionReason::ByteBudget
    }) {
        omission.omitted_count += omitted_count;
    } else {
        omissions.push(CompactSectionOmission {
            section: section.into(),
            reason: WorkSectionOmissionReason::ByteBudget,
            omitted_count,
        });
    }
}

pub(super) fn compact_section_name(section: WorkNextSection) -> &'static str {
    match section {
        WorkNextSection::Focus => "focus",
        WorkNextSection::Ready => "ready",
        WorkNextSection::Catalog => "catalog",
        WorkNextSection::Changes => "changes",
        WorkNextSection::Memories => "memories",
    }
}

pub(super) fn compact_next_lines(compact: &CompactNextReceipt) -> Vec<String> {
    let mut lines = Vec::new();
    match &compact.focus {
        Some(focus) => lines.push(format!("focus: {}", compact_row_line(focus))),
        None if compact_omitted(compact, "focus") > 0 => {
            lines.push("focus: omitted (byte budget)".into());
        }
        None => lines.push("focus: none".into()),
    }
    lines.push(format!("held by you ({} shown):", compact.held.len()));
    for held in &compact.held {
        lines.push(format!("  {}", compact_row_line(held)));
    }
    lines.push(format!("ready ({} shown):", compact.ready.len()));
    for ready in &compact.ready {
        lines.push(format!("  {}", compact_row_line(ready)));
    }
    let staged_changes =
        compact_omitted_for_reason(compact, "changes", WorkSectionOmissionReason::Staged);
    let byte_budget_changes =
        compact_omitted_for_reason(compact, "changes", WorkSectionOmissionReason::ByteBudget);
    append_changes_lines(&mut lines, &compact.changes, staged_changes);
    if byte_budget_changes > 0 {
        if compact.changes.is_empty() && staged_changes == 0 {
            lines.push("changes by others (none shown):".into());
        }
        lines.push(format!(
            "  ({byte_budget_changes} change entries omitted from this response by byte budget)"
        ));
    }
    if let Some(memories) = &compact.memories {
        lines.push(format!(
            "memories: {} retained{}",
            memories.count,
            if memories.changed { " (changed)" } else { "" }
        ));
    }
    for omission in compact
        .omissions
        .iter()
        .filter(|omission| omission.section != "changes" && omission.section != "focus")
    {
        lines.push(format!(
            "  ({} more {} not shown)",
            omission.omitted_count,
            compact_section_word(&omission.section)
        ));
    }
    lines
}

pub(super) fn compact_omitted(compact: &CompactNextReceipt, section: &str) -> usize {
    compact
        .omissions
        .iter()
        .filter(|omission| omission.section == section)
        .map(|omission| omission.omitted_count)
        .sum()
}

pub(super) fn compact_omitted_for_reason(
    compact: &CompactNextReceipt,
    section: &str,
    reason: WorkSectionOmissionReason,
) -> usize {
    compact
        .omissions
        .iter()
        .filter(|omission| omission.section == section && omission.reason == reason)
        .map(|omission| omission.omitted_count)
        .sum()
}

pub(super) fn compact_section_word(section: &str) -> &'static str {
    match section {
        "focus" => "focus items",
        "held" => "held items",
        "ready" => "ready items",
        "catalog" => "catalog items",
        "changes" => "changes",
        "memories" => "memory signals",
        "reminders" => "reminders",
        "next" => "next commands",
        _ => "items",
    }
}
