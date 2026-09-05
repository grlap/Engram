//! Thirteen-word agent surface over the unchanged six-operation work core.
//!
//! Every word here is a thin translation of flat CLI flags or MCP arguments
//! into existing [`LocalWorkService`] calls. The agent never supplies JSON,
//! hashes, fences, or idempotency keys: keys are server-derived, focus is
//! ambient, and every receipt carries `reminders` (what is owed, in words)
//! and `next` (commands the agent can run now) derived by fixed tables from
//! the core's readiness strings, obligation page, and `allowed_next` tags.

use std::{path::PathBuf, sync::Arc};

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    ChildRequirement, LocalWorkService, ProjectId, SessionId, VerificationKind, WorkAvailability,
    WorkBlockerKind, WorkChildInput, WorkClaim, WorkClaimState, WorkCompleteInput,
    WorkCompleteResult, WorkCompletionCaptureInput, WorkEvidenceKind, WorkFocusView,
    WorkHandoffInput, WorkHandoffState, WorkId, WorkItemKind, WorkLifecycle, WorkNextQuery,
    WorkNextSection, WorkNextView, WorkObligationPage, WorkObligationState, WorkPrerequisiteState,
    WorkProposeInput, WorkProposeResult, WorkRevisionPatch, WorkUpdateInput,
    storage::StoreError,
    work_service::{
        COMPLETED_WORK_LATE_FINDING_REFUSAL, MAX_AGENT_WORK_RESPONSE_BYTES, MAX_TEXT_NEXT_COMMANDS,
        ProjectMemorySignal, ReadyWorkSummary, WORK_UPDATE_CLAIM_ACTION,
        WORK_UPDATE_CLAIM_RECOVERY_ACTION, WorkAttributionDefaults, WorkChange,
        WorkChangeProjection, WorkItemSummary, WorkSectionOmission, WorkSectionOmissionReason,
        actor_label, render_agent_receipt_text, terminal_safe_actor_label, terminal_safe_multiline,
    },
};

mod handlers;
mod receipts;
mod show;

#[cfg(test)]
mod tests;

pub use handlers::{
    AddInput, AgentVerbs, ClaimInput, DoneInput, ForgetInput, GateInput, HandoffAction,
    HandoffInput, LsInput, MemoriesInput, NextInput, NoteInput, RememberInput, UpdateAction,
    UpdateInput,
};
pub use receipts::{Guidance, Receipt, VerbError};

const DEFAULT_LIMIT: u32 = 20;
const MAX_TEXT_LINE_BYTES: usize = 96;
const MAX_COMPACT_NEXT_JSON_BYTES: usize = MAX_AGENT_WORK_RESPONSE_BYTES;
const MAX_COMPACT_CHANGE_ITEMS: u32 = 8;
const MAX_COMPACT_TITLE_BYTES: usize = 80;
const MAX_COMPACT_LABEL_ITEMS: usize = 2;
const MAX_COMPACT_LABEL_BYTES: usize = 24;
const MAX_COMPACT_HOLDER_BYTES: usize = 48;
const MAX_COMPACT_REMINDER_ITEMS: usize = 4;
/// How many change pages one `next` reads past pages that held only the
/// actor's own actions.
const MAX_NEXT_PAGES: usize = 8;

/// Changes the core could not fit on the delivered page.
fn changes_not_delivered(view: &WorkNextView) -> usize {
    view.omissions
        .iter()
        .filter(|omission| omission.section == WorkNextSection::Changes)
        .map(|omission| omission.omitted_count)
        .sum()
}

#[derive(Clone, Copy)]
enum Holder<'a> {
    You(DateTime<Utc>),
    Other(&'a SessionId, DateTime<Utc>),
    Nobody,
}

fn item_line(status: &ReadyWorkSummary, holder: Holder<'_>, now: DateTime<Utc>) -> String {
    // Unlike show::show_item_line, list receipts intentionally identify the
    // asserted peer session; terse show keeps that session relative.
    let work = &status.work;
    let state = match holder {
        Holder::You(expires_at) => format!("held by you until {}", clock(expires_at, now)),
        Holder::Other(session, expires_at) => {
            format!("held by {} until {}", session.0, clock(expires_at, now))
        }
        Holder::Nobody => availability_words(status).to_owned(),
    };
    format!("{} \"{}\" — {state}", work.short_ref, short(&work.title))
}

/// One line per change by another session. Your own actions are already in
/// your receipts and are skipped. A single `note` appends an
/// evidence object, an evidence-added event, and a checkpoint; they collapse
/// into one `noted` line. Summaries that repeat their change kind as a prefix
/// lose the prefix.
fn collapse_changes(changes: &[WorkChange]) -> Vec<String> {
    let visible = changes
        .iter()
        .map(|change| match &change.delivery {
            WorkChangeProjection::Visible(summary) => Some((
                summary
                    .work_ref
                    .clone()
                    .unwrap_or_else(|| summary.object_kind.clone()),
                summary.change_kind.clone(),
                strip_kind_prefix(&summary.summary, &summary.change_kind),
                summary.actor_id.clone(),
                summary.actor_context.clone(),
            )),
            WorkChangeProjection::Omitted(_) => None,
        })
        .collect::<Vec<_>>();
    let mut lines: Vec<String> = Vec::new();
    let mut last_note: Option<(String, String)> = None;
    for (index, change) in changes.iter().enumerate() {
        let Some((subject, kind, text, actor_id, actor_context)) = visible[index].as_ref() else {
            last_note = None;
            if let WorkChangeProjection::Omitted(omission) = &change.delivery {
                lines.push(format!(
                    "{} (not visible from your focus)",
                    omission.object_kind
                ));
            }
            continue;
        };
        // Your own actions are already in your receipts.
        if change.from_current_session {
            last_note = None;
            continue;
        }
        if kind == "evidence_added" {
            continue;
        }
        if kind == "checkpoint" {
            let repeats_note = last_note.as_ref().is_some_and(|(note_subject, note_text)| {
                note_subject == subject && note_text == text
            });
            let precedes_completion = visible.get(index + 1).is_some_and(|next| {
                next.as_ref()
                    .is_some_and(|(next_subject, next_kind, _, _, _)| {
                        next_subject == subject && next_kind == "completed"
                    })
            });
            if repeats_note || precedes_completion {
                continue;
            }
        }
        last_note = (kind == "evidence").then(|| (subject.clone(), text.clone()));
        let verb = match kind.as_str() {
            "evidence" => "noted",
            "checkpoint" => "checkpointed",
            other => other,
        };
        let actor = actor_id
            .as_ref()
            .map(|actor| {
                format!(
                    " by {}",
                    terminal_safe_actor_label(actor, actor_context.as_deref())
                )
            })
            .unwrap_or_default();
        lines.push(format!("{subject} {verb}{actor}: {}", short(text)));
    }
    lines
}

pub(crate) const GATE_WORK_REF_REQUIRED: &str =
    "no item is selected for this gate; use gate NAME --work-ref REF, or show REF first";

fn strip_kind_prefix(summary: &str, kind: &str) -> String {
    let prefix = format!("{kind}: ");
    summary
        .strip_prefix(&prefix)
        .unwrap_or(summary)
        .trim()
        .to_owned()
}

fn availability_words(status: &ReadyWorkSummary) -> &'static str {
    compact_state_word(status.work.lifecycle, status.availability)
}

fn compact_state_word(lifecycle: WorkLifecycle, availability: WorkAvailability) -> &'static str {
    match lifecycle {
        WorkLifecycle::Open => availability_word(availability),
        lifecycle => lifecycle_word(lifecycle),
    }
}

fn availability_word(availability: WorkAvailability) -> &'static str {
    match availability {
        WorkAvailability::Ready => "ready",
        WorkAvailability::Claimed => "held",
        WorkAvailability::Active => "active",
        WorkAvailability::Blocked => "blocked",
        WorkAvailability::Deferred => "deferred",
        WorkAvailability::Waiting => "waiting",
        WorkAvailability::Closed => "closed",
    }
}

fn lifecycle_word(lifecycle: WorkLifecycle) -> &'static str {
    match lifecycle {
        WorkLifecycle::Proposed => "proposed",
        WorkLifecycle::Open => "open",
        WorkLifecycle::Completed => "completed",
        WorkLifecycle::Cancelled => "cancelled",
        WorkLifecycle::Superseded => "superseded",
    }
}

fn evidence_kind_word(kind: WorkEvidenceKind) -> &'static str {
    match kind {
        WorkEvidenceKind::Generic => "note",
        WorkEvidenceKind::Verification => "verification",
        WorkEvidenceKind::Environment => "environment",
    }
}

fn child_summary_line(child: &WorkItemSummary) -> String {
    let requirement = if child.child_requirement == ChildRequirement::Optional {
        ", optional"
    } else {
        ""
    };
    format!(
        "{} \"{}\" ({}{requirement})",
        child.short_ref,
        short(&child.title),
        lifecycle_word(child.lifecycle)
    )
}

fn kind_word(kind: WorkItemKind) -> &'static str {
    match kind {
        WorkItemKind::Task => "task",
        WorkItemKind::Bug => "bug",
        WorkItemKind::Feature => "feature",
        WorkItemKind::Epic => "epic",
        WorkItemKind::Chore => "chore",
        WorkItemKind::Research => "research",
    }
}

fn blocker_word(kind: WorkBlockerKind) -> &'static str {
    match kind {
        WorkBlockerKind::Manual => "blocked",
        WorkBlockerKind::HumanDecision => "needs a human decision",
        WorkBlockerKind::ExternalInput => "needs external input",
        WorkBlockerKind::Policy => "policy",
    }
}

fn section_word(section: WorkNextSection) -> &'static str {
    match section {
        WorkNextSection::Focus => "focus details",
        WorkNextSection::Ready => "ready items",
        WorkNextSection::Catalog => "items",
        WorkNextSection::Changes => "changes",
        WorkNextSection::Memories => "memory signals",
    }
}

fn held_suffix(holder: Holder<'_>, now: DateTime<Utc>) -> String {
    match holder {
        Holder::You(expires_at) => format!(" (held by you until {})", clock(expires_at, now)),
        Holder::Other(session, expires_at) => {
            format!(" (held by {} until {})", session.0, clock(expires_at, now))
        }
        Holder::Nobody => String::new(),
    }
}

fn clock(at: DateTime<Utc>, now: DateTime<Utc>) -> String {
    if at.date_naive() == now.date_naive() {
        at.format("%H:%M UTC").to_string()
    } else {
        at.format("%Y-%m-%d %H:%M UTC").to_string()
    }
}

fn short(text: &str) -> String {
    short_with_limit(text, MAX_TEXT_LINE_BYTES)
}

fn short_ref_for_work_id(work_id: WorkId) -> String {
    let simple = work_id.0.simple().to_string();
    format!("w-{}", simple.get(20..).unwrap_or(&simple))
}

fn short_with_limit(text: &str, max_bytes: usize) -> String {
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.len() <= max_bytes {
        return text;
    }
    let mut end = max_bytes.saturating_sub('…'.len_utf8());
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", text[..end].trim_end())
}

fn nonempty(value: Option<String>) -> Option<String> {
    value
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

fn trimmed(values: &[String]) -> Vec<String> {
    values
        .iter()
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .collect()
}

fn validate_priority(priority: Option<i32>) -> Result<Option<i32>, VerbError> {
    match priority {
        Some(value) if !(0..=4).contains(&value) => {
            Err(StoreError::InvalidWork("priority must be from 0 through 4".into()).into())
        }
        other => Ok(other),
    }
}

/// Stable child key derived from a title so `add --under` needs no key.
fn slug(title: &str) -> String {
    let mut out = String::new();
    let mut pending_dash = false;
    for ch in title.chars().flat_map(char::to_lowercase) {
        if out.len() >= 48 {
            break;
        }
        if ch.is_alphanumeric() {
            if pending_dash && !out.is_empty() {
                out.push('-');
            }
            out.push(ch);
            pending_dash = false;
        } else {
            pending_dash = true;
        }
    }
    if out.is_empty() { "child".into() } else { out }
}

/// Parses `--defer DATE` as RFC 3339, `YYYY-MM-DD`, or `YYYY-MM-DDTHH:MM:SS`
/// (the latter two in UTC).
///
/// # Errors
///
/// Returns the accepted forms when the text matches none of them.
pub fn parse_defer_date(value: &str) -> Result<DateTime<Utc>, String> {
    let value = value.trim();
    if let Ok(parsed) = DateTime::parse_from_rfc3339(value) {
        return Ok(parsed.with_timezone(&Utc));
    }
    if let Ok(date) = NaiveDate::parse_from_str(value, "%Y-%m-%d") {
        return Ok(date.and_hms_opt(0, 0, 0).unwrap_or_default().and_utc());
    }
    if let Ok(at) = NaiveDateTime::parse_from_str(value, "%Y-%m-%dT%H:%M:%S") {
        return Ok(at.and_utc());
    }
    Err("expected an RFC 3339 timestamp, YYYY-MM-DD, or YYYY-MM-DDTHH:MM:SS".into())
}

/// Whether text has the shape of a work reference (`w-` plus twelve hex
/// characters, or a full UUID) rather than free prose.
#[must_use]
pub fn looks_like_work_ref(value: &str) -> bool {
    let value = value.trim();
    if let Some(hex) = value.strip_prefix("w-") {
        return hex.len() == 12 && hex.chars().all(|ch| ch.is_ascii_hexdigit());
    }
    uuid::Uuid::parse_str(value).is_ok()
}
