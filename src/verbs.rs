//! Nine-word agent surface over the unchanged six-operation work core.
//!
//! Every word here is a thin translation of flat CLI flags or MCP arguments
//! into existing [`LocalWorkService`] calls. The agent never supplies JSON,
//! hashes, fences, or idempotency keys: keys are server-derived, focus is
//! ambient, and every receipt carries `reminders` (what is owed, in words)
//! and `next` (commands the agent can run now) derived by fixed tables from
//! the core's readiness strings, obligation page, and `allowed_next` tags.

use std::{
    fmt::{self, Write as _},
    path::PathBuf,
    sync::Arc,
};

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    ChildRequirement, LocalWorkService, ObjectHash, ProjectId, SessionId, VerificationKind,
    WorkAvailability, WorkBlockerKind, WorkChildInput, WorkClaim, WorkClaimState,
    WorkCompleteInput, WorkCompleteResult, WorkCompletionCaptureInput, WorkFocusView,
    WorkHandoffInput, WorkHandoffState, WorkItemKind, WorkLifecycle, WorkNextQuery,
    WorkNextSection, WorkNextView, WorkObligationPage, WorkObligationState, WorkProposeInput,
    WorkProposeResult, WorkRevisionPatch, WorkUpdateInput,
    storage::StoreError,
    work_service::{ReadyWorkSummary, WorkChange, WorkChangeProjection},
};

const DEFAULT_LIMIT: u32 = 20;
const MAX_TEXT_LINE_BYTES: usize = 96;
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

/// Host context for one agent connection. Authority comes from the host, never
/// from a word's arguments.
#[derive(Clone, Debug)]
pub struct AgentVerbs {
    service: Arc<LocalWorkService>,
    actor_id: String,
    session_id: SessionId,
}

/// `next`: what is ready, what this session holds, and what changed.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct NextInput {
    pub limit: Option<u32>,
}

/// `ls` / `search`: catalog listing with flat filters.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct LsInput {
    pub search: Option<String>,
    pub blocked: bool,
    /// Assigned to this actor, or held by this session.
    pub mine: bool,
    /// Include completed, cancelled, and superseded items.
    pub all: bool,
    pub label: Option<String>,
    pub limit: Option<u32>,
}

/// `add`: a root, or one required child under `under`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AddInput {
    pub title: String,
    pub outcome: Option<String>,
    pub acceptance: Vec<String>,
    pub under: Option<String>,
    pub priority: Option<i32>,
    pub labels: Vec<String>,
    pub assignee: Option<String>,
    pub kind: Option<WorkItemKind>,
}

/// `claim`: hold one item; later words default to it.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ClaimInput {
    pub work_ref: String,
    pub ttl_seconds: Option<i64>,
    /// Attributed reason for recovering a lapsed prior claim.
    pub recover: Option<String>,
}

/// `update`: exactly one action against an item.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct UpdateInput {
    pub work_ref: Option<String>,
    pub action: UpdateAction,
}

/// The single action an `update` performs.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum UpdateAction {
    Release {
        reason: Option<String>,
    },
    Blocked {
        detail: String,
    },
    Unblock,
    /// Any combination of planning fields, applied as one revision.
    Revise {
        title: Option<String>,
        outcome: Option<String>,
        assignee: Option<String>,
        priority: Option<i32>,
        defer: Option<DateTime<Utc>>,
    },
    Cancel {
        reason: String,
    },
}

/// `note`: one finding, decision, or evidence pointer.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct NoteInput {
    pub work_ref: Option<String>,
    pub text: String,
    pub refs: Vec<String>,
}

/// `done`: complete the held item.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DoneInput {
    pub work_ref: Option<String>,
    pub summary: Option<String>,
    pub note: Option<String>,
}

/// `handoff`: offer, accept, or cancel a transfer of the held item.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct HandoffInput {
    pub work_ref: Option<String>,
    pub action: HandoffAction,
}

/// The single action a `handoff` performs.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum HandoffAction {
    Offer {
        to: String,
        summary: Option<String>,
        ttl_seconds: Option<i64>,
    },
    Accept,
    Cancel {
        reason: String,
    },
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
    pub reminders: Vec<String>,
    pub next: Vec<String>,
    pub value: Value,
    /// A typed completion refusal: the shell exits with status 2.
    pub owed: bool,
}

impl Receipt {
    fn assemble(lines: Vec<String>, guidance: Guidance, value: Value, owed: bool) -> Self {
        let mut value = match value {
            Value::Object(map) => Value::Object(map),
            other => json!({ "result": other }),
        };
        value["reminders"] = json!(guidance.reminders);
        value["next"] = json!(guidance.next);
        Self {
            lines,
            reminders: guidance.reminders,
            next: guidance.next,
            value,
            owed,
        }
    }

    /// Shell rendering: the receipt lines, then `reminders:` and `next:`.
    /// Never contains object hashes, fences, or idempotency keys.
    #[must_use]
    pub fn text(&self) -> String {
        let mut out = self.lines.join("\n");
        out.push_str("\nreminders:");
        if self.reminders.is_empty() {
            out.push_str(" none");
        }
        for reminder in &self.reminders {
            out.push_str("\n  - ");
            out.push_str(reminder);
        }
        out.push_str("\nnext:");
        if self.next.is_empty() {
            out.push_str(" none");
        }
        for command in &self.next {
            out.push_str("\n  ");
            out.push_str(command);
        }
        out
    }
}

/// A core failure plus the item it concerned, when known.
#[derive(Debug)]
pub struct VerbError {
    pub error: StoreError,
    pub work_ref: Option<String>,
}

impl VerbError {
    fn at(error: StoreError, work_ref: &str) -> Self {
        Self {
            error,
            work_ref: Some(work_ref.to_owned()),
        }
    }

    /// Words and commands that resolve the failure, when a fixed table knows.
    #[must_use]
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
                vec!["you do not hold this item; claim it before you note or complete it".into()],
                vec![format!("engram work claim {target}")],
            ),
            StoreError::WorkClaimLapsed { expired_at, .. } => (
                vec![format!(
                    "claim lapsed at {}",
                    clock(*expired_at, Utc::now())
                )],
                vec![format!(
                    "engram work claim {target} --recover \"explain why the lapsed claim is being recovered\""
                )],
            ),
            StoreError::WorkCompletionRefused { reason, .. } => {
                let words = if reason.contains("at least one evidence")
                    || reason.contains("no checkpoint")
                {
                    "nothing has been noted on this item yet; say what was delivered".into()
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
            StoreError::WorkNotFound(_) => {
                (vec!["no such item".into()], vec!["engram work ls".into()])
            }
            StoreError::WorkReferenceAmbiguous {
                candidates, more, ..
            } => ambiguous_reference_guidance(candidates, *more),
            StoreError::InvalidWork(reason) if reason.contains("does not exist") => {
                (vec!["no such item".into()], vec!["engram work ls".into()])
            }
            StoreError::InvalidWork(reason) if reason.contains("no focused work") => (
                vec!["no item is selected; name one or claim one first".into()],
                vec!["engram work next".into()],
            ),
            StoreError::InvalidWork(reason)
                if reason.starts_with("work authority grant expired at") =>
            {
                (
                    vec![format!(
                        "your host grant {}; ask the host for a new one",
                        reason.trim_start_matches("work authority grant ")
                    )],
                    Vec::new(),
                )
            }
            StoreError::InvalidWork(reason)
                if reason.starts_with("work authority grant was revoked") =>
            {
                (
                    vec!["your host grant was revoked; ask the host for a new one".into()],
                    Vec::new(),
                )
            }
            StoreError::InvalidWork(reason) if reason.starts_with("work authority grant") => (
                vec![format!(
                    "your host grant does not cover this ({}); ask the host for one that does",
                    reason.trim_start_matches("work authority grant ")
                )],
                Vec::new(),
            ),
            StoreError::InvalidWork(reason) if reason.contains("work-authority grant") => (
                vec!["the host has not granted this session work authority".into()],
                Vec::new(),
            ),
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

#[derive(Clone, Copy)]
enum Holder<'a> {
    You(DateTime<Utc>),
    Other(&'a SessionId, DateTime<Utc>),
    Nobody,
}

impl AgentVerbs {
    /// Builds the agent surface for one host-fixed actor/session.
    #[must_use]
    pub fn new(
        database: PathBuf,
        project_id: ProjectId,
        actor_id: String,
        session_id: SessionId,
        source_skill: Option<String>,
        authority_grant: Option<ObjectHash>,
    ) -> Self {
        Self::with_shared_service(
            Arc::new(LocalWorkService::new(
                database,
                project_id,
                actor_id.clone(),
                session_id.clone(),
                source_skill,
                authority_grant,
            )),
            actor_id,
            session_id,
        )
    }

    /// Builds the agent surface over a service retained by its host process.
    #[must_use]
    pub(crate) fn with_shared_service(
        service: Arc<LocalWorkService>,
        actor_id: String,
        session_id: SessionId,
    ) -> Self {
        Self {
            service,
            actor_id,
            session_id,
        }
    }

    /// `next`: focus, ready candidates, and the changes since the last call.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] when the core cannot read or stage the view.
    #[allow(
        clippy::too_many_lines,
        reason = "focus, held, ready, changes, and guidance are assembled in one readable pass"
    )]
    pub fn next(&self, input: &NextInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        let limit = input.limit.unwrap_or(DEFAULT_LIMIT);
        let view = self.service.work_next(
            limit,
            WorkNextQuery {
                sections: vec![WorkNextSection::Focus, WorkNextSection::Changes],
                ..WorkNextQuery::default()
            },
            now,
        )?;
        let held = self.held_items(limit, now)?;
        // The core's ready section is byte-bounded; the catalog pages densely.
        let ready = self.catalog(
            &WorkNextQuery {
                sections: vec![WorkNextSection::Catalog],
                lifecycles: vec![WorkLifecycle::Open],
                availabilities: vec![WorkAvailability::Ready],
                ..WorkNextQuery::default()
            },
            limit,
            now,
        )?;
        let mut lines = Vec::new();
        match &view.focus {
            Some(focus) => lines.push(format!(
                "focus: {}",
                item_line(&focus.status, self.holder(focus, now), now)
            )),
            None => lines.push("focus: none".into()),
        }
        lines.push(format!("held by you ({}):", held.len()));
        for (item, expires_at) in &held {
            lines.push(format!(
                "  {} \"{}\" until {}",
                item.work.short_ref,
                short(&item.work.title),
                clock(*expires_at, now)
            ));
        }
        lines.push(format!("ready ({}):", ready.len()));
        for item in &ready {
            lines.push(format!("  {}", ready_line(item)));
        }
        let mut changes =
            collapse_changes(view.changes.as_deref().unwrap_or_default(), &self.actor_id);
        let mut not_delivered = changes_not_delivered(&view);
        // A page that held only this actor's own actions is not worth another
        // call: keep reading, bounded, until another actor's change shows up
        // or the backlog is drained.
        let mut pages = 1;
        while changes.is_empty() && not_delivered > 0 && pages < MAX_NEXT_PAGES {
            let more = self.service.work_next(
                limit,
                WorkNextQuery {
                    sections: vec![WorkNextSection::Changes],
                    ..WorkNextQuery::default()
                },
                now,
            )?;
            changes = collapse_changes(more.changes.as_deref().unwrap_or_default(), &self.actor_id);
            not_delivered = changes_not_delivered(&more);
            pages += 1;
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
        for change in &changes {
            lines.push(format!("  {change}"));
        }
        for omission in view
            .omissions
            .iter()
            .filter(|omission| omission.section != WorkNextSection::Changes)
        {
            lines.push(format!(
                "  ({} more {} not shown)",
                omission.omitted_count,
                section_word(omission.section)
            ));
        }
        let mut guidance = view
            .focus
            .as_ref()
            .map(|focus| self.guidance(focus, "next", now))
            .unwrap_or_default();
        if let Some(first) = ready.iter().find(|item| {
            item.availability == WorkAvailability::Ready
                && view
                    .focus
                    .as_ref()
                    .is_none_or(|focus| focus.status.work.work_id != item.work.work_id)
        }) {
            let command = format!("engram work claim {}", first.work.short_ref);
            if !guidance.next.contains(&command) {
                guidance.next.insert(0, command);
            }
        }
        if let Some((item, _)) = held.iter().find(|(item, _)| {
            view.focus
                .as_ref()
                .is_none_or(|focus| focus.status.work.work_id != item.work.work_id)
        }) {
            let command = format!("engram work show {}", item.work.short_ref);
            if !guidance.next.contains(&command) {
                guidance.next.push(command);
            }
        }
        if guidance.next.is_empty() {
            guidance.next.push("engram work add \"…\"".into());
        }
        let mut value = serde_json::to_value(&view)?;
        value["ready"] = serde_json::to_value(&ready)?;
        value["changes_by_others"] = json!(changes);
        value["held"] = serde_json::to_value(
            held.iter()
                .map(|(item, expires_at)| json!({ "work": item.work, "expires_at": expires_at }))
                .collect::<Vec<_>>(),
        )?;
        Ok(Receipt::assemble(lines, guidance, value, false))
    }

    /// Open items this session holds under a live claim, in catalog order.
    fn held_items(
        &self,
        limit: u32,
        now: DateTime<Utc>,
    ) -> Result<Vec<(ReadyWorkSummary, DateTime<Utc>)>, VerbError> {
        // One claim query names what this session holds; the catalog then
        // supplies the summaries without a focus view per item.
        let mine = self
            .service
            .held_work(now)?
            .into_iter()
            .collect::<std::collections::HashMap<_, _>>();
        if mine.is_empty() {
            return Ok(Vec::new());
        }
        let items = self.catalog(
            &WorkNextQuery {
                sections: vec![WorkNextSection::Catalog],
                lifecycles: vec![WorkLifecycle::Open],
                availabilities: vec![WorkAvailability::Claimed, WorkAvailability::Active],
                ..WorkNextQuery::default()
            },
            limit,
            now,
        )?;
        Ok(items
            .into_iter()
            .filter_map(|item| {
                mine.get(&item.work.work_id)
                    .map(|expires_at| (item, *expires_at))
            })
            .collect())
    }

    /// Reads catalog pages until `limit` items or the end; the core bounds
    /// each page by bytes, so one call rarely returns the whole list.
    fn catalog(
        &self,
        query: &WorkNextQuery,
        limit: u32,
        now: DateTime<Utc>,
    ) -> Result<Vec<ReadyWorkSummary>, VerbError> {
        let wanted = usize::try_from(limit).unwrap_or(usize::MAX).max(1);
        let mut items: Vec<ReadyWorkSummary> = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let remaining = u32::try_from(wanted - items.len()).unwrap_or(u32::MAX);
            let view = self.service.work_next(
                remaining,
                WorkNextQuery {
                    sections: vec![WorkNextSection::Catalog],
                    search: query.search.clone(),
                    lifecycles: query.lifecycles.clone(),
                    availabilities: query.availabilities.clone(),
                    blocked_only: query.blocked_only,
                    assigned_to: query.assigned_to.clone(),
                    label: query.label.clone(),
                    after: after.clone(),
                },
                now,
            )?;
            let Some(page) = view.catalog else {
                break;
            };
            let page_len = page.items.len();
            items.extend(page.items);
            match page.next_after {
                Some(next) if page_len > 0 && items.len() < wanted => {
                    after = Some(next.0.to_string());
                }
                _ => break,
            }
        }
        items.truncate(wanted);
        Ok(items)
    }

    /// `ls`: open items by default; `search` is `ls` over every lifecycle.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] when the catalog cannot be read.
    pub fn ls(&self, input: &LsInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        let limit = input.limit.unwrap_or(DEFAULT_LIMIT);
        let query = WorkNextQuery {
            sections: vec![WorkNextSection::Catalog],
            search: input.search.clone(),
            lifecycles: if input.all {
                Vec::new()
            } else {
                vec![WorkLifecycle::Open]
            },
            availabilities: Vec::new(),
            blocked_only: input.blocked,
            assigned_to: input.mine.then(|| self.actor_id.clone()),
            label: input.label.clone(),
            after: None,
        };
        let mut items = self.catalog(&query, limit.saturating_add(1), now)?;
        let more = items.len() > limit as usize;
        items.truncate(limit as usize);
        if input.mine {
            for (held, _) in self.held_items(limit, now)? {
                if catalog_filters_admit(&held, input)
                    && !items
                        .iter()
                        .any(|item| item.work.work_id == held.work.work_id)
                {
                    items.insert(0, held);
                }
            }
        }
        let mut lines = vec![format!("{} item(s):", items.len())];
        for item in &items {
            lines.push(format!("  {}", catalog_line(item)));
        }
        if more {
            lines.push(format!(
                "  (more than {limit}; raise --limit or narrow with --search or --label)"
            ));
        }
        let mut next = Vec::new();
        if let Some(first) = items.first() {
            next.push(format!("engram work show {}", first.work.short_ref));
        }
        if let Some(ready) = items
            .iter()
            .find(|item| item.availability == WorkAvailability::Ready)
        {
            next.push(format!("engram work claim {}", ready.work.short_ref));
        }
        if next.is_empty() {
            next.push("engram work add \"…\"".into());
        }
        Ok(Receipt::assemble(
            lines,
            Guidance {
                reminders: Vec::new(),
                next,
            },
            json!({ "items": items, "more": more }),
            false,
        ))
    }

    /// `show`: one item in full; selects it as ambient focus without claiming.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] when the reference is unknown.
    pub fn show(&self, work_ref: &str, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        let view = self
            .service
            .work_focus(work_ref, now)
            .map_err(|error| VerbError::at(error, work_ref))?;
        let holder = self.holder(&view, now);
        let work = &view.status.work;
        let mut lines = vec![item_line(&view.status, holder, now)];
        let mut facts = vec![
            format!("kind: {}", kind_word(work.kind)),
            format!("priority: {}", work.priority),
        ];
        if !work.labels.is_empty() {
            facts.push(format!("labels: {}", work.labels.join(", ")));
        }
        if let Some(assignee) = &work.assigned_to {
            facts.push(format!("assignee: {assignee}"));
        }
        lines.push(facts.join("  "));
        lines.push(format!("outcome: {}", view.outcome));
        lines.push("acceptance:".into());
        for criterion in &work.acceptance {
            lines.push(format!("  - {criterion}"));
        }
        if work.acceptance_count > work.acceptance.len() {
            lines.push(format!(
                "  ({} more not shown)",
                work.acceptance_count - work.acceptance.len()
            ));
        }
        if !view.blockers.is_empty() {
            lines.push("blockers:".into());
            for blocker in &view.blockers {
                lines.push(format!(
                    "  - {}: {}",
                    blocker_word(blocker.kind),
                    blocker.detail
                ));
            }
        }
        if !view.children.is_empty() {
            lines.push(format!(
                "children: {}",
                view.children
                    .iter()
                    .map(|child| format!(
                        "{} \"{}\" ({})",
                        child.short_ref,
                        short(&child.title),
                        lifecycle_word(child.lifecycle)
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        if !view.prerequisites.is_empty() {
            lines.push(format!(
                "prerequisites: {}",
                view.prerequisites
                    .iter()
                    .map(|item| format!(
                        "{} \"{}\" ({})",
                        item.short_ref,
                        short(&item.title),
                        lifecycle_word(item.lifecycle)
                    ))
                    .collect::<Vec<_>>()
                    .join(", ")
            ));
        }
        for offer in view
            .handoffs
            .iter()
            .filter(|offer| offer.state == WorkHandoffState::Offered && offer.expires_at > now)
        {
            lines.push(format!(
                "handoff: offered by {} to {} until {}",
                offer.from.0,
                offer.to.0,
                clock(offer.expires_at, now)
            ));
        }
        if !view.evidence_items.is_empty() {
            let last = view
                .evidence_items
                .last()
                .map(|item| short(&item.summary))
                .unwrap_or_default();
            lines.push(format!(
                "notes: {} recorded; latest: \"{last}\"",
                view.evidence_items.len()
            ));
        }
        let guidance = self.guidance(&view, "show", now);
        Ok(Receipt::assemble(
            lines,
            guidance,
            serde_json::to_value(&view)?,
            false,
        ))
    }

    /// `add`: a root, or one required child beneath `under`.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] when input is empty, the host grant is absent, or
    /// the core refuses admission.
    pub fn add(&self, input: AddInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        let title = input.title.trim().to_owned();
        if title.is_empty() {
            return Err(StoreError::InvalidWork("title must not be empty".into()).into());
        }
        let outcome = input
            .outcome
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| title.clone());
        let mut acceptance = input
            .acceptance
            .iter()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty())
            .collect::<Vec<_>>();
        if acceptance.is_empty() {
            acceptance.push(format!("{title} is done"));
        }
        let priority = validate_priority(input.priority)?;
        let labels = trimmed(&input.labels);
        let assigned_to = input
            .assignee
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty());
        if let Some(under) = input.under.as_deref() {
            return self.add_child(
                under,
                WorkChildInput {
                    key: slug(&title),
                    title,
                    outcome,
                    acceptance,
                    requirement: Some(ChildRequirement::Required),
                    kind: input.kind,
                    priority,
                    labels,
                    assigned_to,
                    deferred_until: None,
                },
                now,
            );
        }
        let result = self.service.work_propose(
            WorkProposeInput::Root {
                title,
                outcome,
                acceptance,
                work_kind: input.kind,
                priority,
                labels,
                assigned_to,
                deferred_until: None,
                authority_policy_ref: None,
                idempotency_key: String::new(),
            },
            now,
        )?;
        let WorkProposeResult::Root { work, focus } = &result else {
            return Err(StoreError::InvalidWorkProjection(
                "root proposal returned a decomposition receipt".into(),
            )
            .into());
        };
        let guidance = self.guidance(focus, "add", now);
        let lines = vec![format!(
            "added {} \"{}\"",
            work.short_ref,
            short(&work.title)
        )];
        Ok(Receipt::assemble(
            lines,
            guidance,
            serde_json::to_value(&result)?,
            false,
        ))
    }

    /// One required child through `work_propose:decompose`; the child becomes
    /// the focus exactly as a new root does.
    fn add_child(
        &self,
        under: &str,
        child: WorkChildInput,
        now: DateTime<Utc>,
    ) -> Result<Receipt, VerbError> {
        let parent = self
            .service
            .work_focus(under, now)
            .map_err(|error| VerbError::at(error, under))?;
        let parent_ref = parent.status.work.short_ref.clone();
        let result = self
            .service
            .work_propose(
                WorkProposeInput::Decompose {
                    children: vec![child],
                    prerequisites: Vec::new(),
                    idempotency_key: String::new(),
                },
                now,
            )
            .map_err(|error| VerbError::at(error, &parent_ref))?;
        let WorkProposeResult::Decomposition(summary) = &result else {
            return Err(StoreError::InvalidWorkProjection(
                "decomposition returned a root receipt".into(),
            )
            .into());
        };
        let child_ref = summary
            .children
            .first()
            .map(|child| child.short_ref.clone())
            .ok_or_else(|| {
                StoreError::InvalidWorkProjection("decomposition created no child".into())
            })?;
        let focus = self
            .service
            .work_focus(&child_ref, now)
            .map_err(|error| VerbError::at(error, &child_ref))?;
        let guidance = self.guidance(&focus, "add", now);
        let mut value = serde_json::to_value(&result)?;
        value["work"] = serde_json::to_value(&focus.status.work)?;
        value["focus"] = serde_json::to_value(&focus)?;
        let lines = vec![format!(
            "added {child_ref} \"{}\" under {parent_ref} \"{}\"",
            short(&focus.status.work.title),
            short(&parent.status.work.title)
        )];
        Ok(Receipt::assemble(lines, guidance, value, false))
    }

    /// `claim`: hold the item; later words default to it.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] when the item is unknown, held elsewhere, or the
    /// host grant does not admit claiming.
    pub fn claim(&self, input: ClaimInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        let view = self
            .service
            .work_focus(&input.work_ref, now)
            .map_err(|error| VerbError::at(error, &input.work_ref))?;
        let work_ref = view.status.work.short_ref.clone();
        let result = self
            .service
            .work_update(
                WorkUpdateInput::Claim {
                    ttl_seconds: input.ttl_seconds,
                    recovery_reason: input
                        .recover
                        .map(|value| value.trim().to_owned())
                        .filter(|value| !value.is_empty()),
                    idempotency_key: String::new(),
                },
                now,
            )
            .map_err(|error| VerbError::at(error, &work_ref))?;
        let after = self.refreshed(view, now)?;
        let lines = vec![format!(
            "claimed {work_ref} \"{}\"{}",
            short(&after.status.work.title),
            held_suffix(self.holder(&after, now), now)
        )];
        let guidance = self.guidance(&after, "claim", now);
        Ok(Receipt::assemble(
            lines,
            guidance,
            serde_json::to_value(&result)?,
            false,
        ))
    }

    /// `update`: release, block, unblock, revise fields, or cancel.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] when no action applies or the core refuses it.
    pub fn update(&self, input: UpdateInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        let view = self.target(input.work_ref.as_deref(), now)?;
        let work_ref = view.status.work.short_ref.clone();
        let title = short(&view.status.work.title);
        let (core, line) = self.update_translation(input.action, &work_ref, &title)?;
        let result = self
            .service
            .work_update(core, now)
            .map_err(|error| VerbError::at(error, &work_ref))?;
        let after = self.refreshed(view, now)?;
        let line = format!("{line}{}", held_suffix(self.holder(&after, now), now));
        let guidance = self.guidance(&after, "update", now);
        Ok(Receipt::assemble(
            vec![line],
            guidance,
            serde_json::to_value(&result)?,
            false,
        ))
    }

    /// Maps one flat `update` action onto the typed core update and the
    /// receipt line the shell prints.
    fn update_translation(
        &self,
        action: UpdateAction,
        work_ref: &str,
        title: &str,
    ) -> Result<(WorkUpdateInput, String), VerbError> {
        Ok(match action {
            UpdateAction::Release { reason } => (
                WorkUpdateInput::Release {
                    reason: reason
                        .map(|value| value.trim().to_owned())
                        .filter(|value| !value.is_empty())
                        .unwrap_or_else(|| format!("released by {}", self.actor_id)),
                    waiver_reason: None,
                    idempotency_key: String::new(),
                },
                format!("released {work_ref} \"{title}\""),
            ),
            UpdateAction::Blocked { detail } => {
                let detail = detail.trim().to_owned();
                if detail.is_empty() {
                    return Err(
                        StoreError::InvalidWork("say why the item is blocked".into()).into(),
                    );
                }
                (
                    WorkUpdateInput::Block {
                        blocker_kind: WorkBlockerKind::Manual,
                        detail: detail.clone(),
                        idempotency_key: String::new(),
                    },
                    format!("blocked {work_ref} \"{title}\": {}", short(&detail)),
                )
            }
            UpdateAction::Unblock => (
                WorkUpdateInput::Unblock {
                    blocker_id: None,
                    idempotency_key: String::new(),
                },
                format!("unblocked {work_ref} \"{title}\""),
            ),
            UpdateAction::Revise {
                title: new_title,
                outcome,
                assignee,
                priority,
                defer,
            } => {
                let patch = WorkRevisionPatch {
                    title: nonempty(new_title),
                    outcome: nonempty(outcome),
                    acceptance: None,
                    priority: validate_priority(priority)?,
                    labels: None,
                    assigned_to: nonempty(assignee),
                    clear_assignment: false,
                    deferred_until: defer,
                    clear_deferral: false,
                };
                let mut fields = Vec::new();
                if patch.title.is_some() {
                    fields.push("title");
                }
                if patch.outcome.is_some() {
                    fields.push("outcome");
                }
                if patch.assigned_to.is_some() {
                    fields.push("assignee");
                }
                if patch.priority.is_some() {
                    fields.push("priority");
                }
                if patch.deferred_until.is_some() {
                    fields.push("deferral");
                }
                if fields.is_empty() {
                    return Err(StoreError::InvalidWork(
                        "update needs one action: --release, --blocked, --unblock, --cancel, or a field to change"
                            .into(),
                    )
                    .into());
                }
                (
                    WorkUpdateInput::Revise {
                        patch,
                        idempotency_key: String::new(),
                    },
                    format!("updated {work_ref} \"{title}\" ({})", fields.join(", ")),
                )
            }
            UpdateAction::Cancel { reason } => {
                let reason = reason.trim().to_owned();
                if reason.is_empty() {
                    return Err(
                        StoreError::InvalidWork("say why the item is cancelled".into()).into(),
                    );
                }
                (
                    WorkUpdateInput::Cancel {
                        reason: reason.clone(),
                        idempotency_key: String::new(),
                    },
                    format!("cancelled {work_ref} \"{title}\": {}", short(&reason)),
                )
            }
        })
    }

    /// `note`: record evidence, then checkpoint it, both keyless.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] when the text is empty or this session does not
    /// hold the item.
    pub fn note(&self, input: &NoteInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        let text = input.text.trim().to_owned();
        if text.is_empty() {
            return Err(StoreError::InvalidWork("note text must not be empty".into()).into());
        }
        let view = self.target(input.work_ref.as_deref(), now)?;
        let work_ref = view.status.work.short_ref.clone();
        let evidence = self
            .service
            .work_update(
                WorkUpdateInput::Evidence {
                    summary: text.clone(),
                    refs: trimmed(&input.refs),
                    attach: None,
                    idempotency_key: String::new(),
                },
                now,
            )
            .map_err(|error| VerbError::at(error, &work_ref))?;
        let checkpoint = self
            .service
            .work_update(
                WorkUpdateInput::Checkpoint {
                    summary: text.clone(),
                    evidence: None,
                    idempotency_key: String::new(),
                },
                now,
            )
            .map_err(|error| VerbError::at(error, &work_ref))?;
        let after = self.refreshed(view, now)?;
        let guidance = self.guidance(&after, "note", now);
        let mut value = serde_json::to_value(&checkpoint)?;
        value["operation"] = json!("note");
        value["evidence"] = serde_json::to_value(&evidence.receipt)?;
        let lines = vec![format!(
            "noted on {work_ref} \"{}\": {}{}",
            short(&after.status.work.title),
            short(&text),
            held_suffix(self.holder(&after, now), now)
        )];
        Ok(Receipt::assemble(lines, guidance, value, false))
    }

    /// `done`: complete the held item; a typed refusal says what is owed.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] when this session does not hold the item, nothing
    /// has been noted, or a lifecycle fence moved.
    pub fn done(&self, input: DoneInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        let view = self.target(input.work_ref.as_deref(), now)?;
        let work_ref = view.status.work.short_ref.clone();
        let title = short(&view.status.work.title);
        let result = self
            .service
            .work_complete(
                WorkCompleteInput {
                    capture: nonempty(input.summary).map(|summary| WorkCompletionCaptureInput {
                        summary,
                        refs: Vec::new(),
                    }),
                    evidence: Vec::new(),
                    acceptance: None,
                    note: nonempty(input.note),
                    idempotency_key: String::new(),
                },
                now,
            )
            .map_err(|error| VerbError::at(error, &work_ref))?;
        let after = self.refreshed(view, now)?;
        let (lines, guidance, owed) = match &result {
            WorkCompleteResult::Completed(_) => {
                let mut guidance = self.guidance(&after, "done", now);
                guidance.reminders.clear();
                // Finishing a child points back at the parent that is still
                // open, with the parent's own next commands.
                if let Some(parent_id) = after.status.work.parent_id
                    && let Ok(parent) = self.service.inspect_work(&parent_id.0.to_string(), now)
                    && parent.status.work.lifecycle == WorkLifecycle::Open
                {
                    let parent_ref = parent.status.work.short_ref.clone();
                    guidance = self.guidance(&parent, "done", now);
                    // Every reminder about the parent names the parent.
                    for reminder in &mut guidance.reminders {
                        *reminder = format!("{parent_ref}: {reminder}");
                    }
                    guidance.reminders.insert(
                        0,
                        format!(
                            "{parent_ref} \"{}\" is still open",
                            short(&parent.status.work.title)
                        ),
                    );
                }
                (
                    vec![format!("done {work_ref} \"{title}\"")],
                    guidance,
                    false,
                )
            }
            WorkCompleteResult::Refused(refusal) => {
                let mut guidance = self.guidance(&after, "done", now);
                guidance
                    .reminders
                    .push(completion_recovery_reminder(&refusal.recovery));
                guidance.next = vec![refusal.recovery.command.clone()];
                for reminder in obligation_reminders(&refusal.obligation_page) {
                    if !guidance.reminders.contains(&reminder) {
                        guidance.reminders.push(reminder);
                    }
                }
                (
                    vec![format!(
                        "not done {work_ref} \"{title}\": something is still owed"
                    )],
                    guidance,
                    true,
                )
            }
        };
        Ok(Receipt::assemble(
            lines,
            guidance,
            serde_json::to_value(&result)?,
            owed,
        ))
    }

    /// `search`: `ls` over every lifecycle for a text query.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] when the catalog cannot be read.
    pub fn search(
        &self,
        query: &str,
        limit: Option<u32>,
        now: DateTime<Utc>,
    ) -> Result<Receipt, VerbError> {
        self.ls(
            &LsInput {
                search: Some(query.to_owned()),
                all: true,
                limit,
                ..LsInput::default()
            },
            now,
        )
    }

    /// `handoff`: offer the held item, accept an offer, or cancel one.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] when no matching offer or claim exists.
    pub fn handoff(&self, input: HandoffInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        let view = self.target(input.work_ref.as_deref(), now)?;
        let work_ref = view.status.work.short_ref.clone();
        let title = short(&view.status.work.title);
        let (core, verb) = match input.action {
            HandoffAction::Offer {
                to,
                summary,
                ttl_seconds,
            } => {
                let to = to.trim().to_owned();
                if to.is_empty() {
                    return Err(
                        StoreError::InvalidWork("say who receives the handoff".into()).into(),
                    );
                }
                (
                    WorkHandoffInput::Offer {
                        checkpoint_summary: nonempty(summary)
                            .unwrap_or_else(|| format!("handoff from {} to {to}", self.actor_id)),
                        to: to.clone(),
                        ttl_seconds,
                        idempotency_key: String::new(),
                    },
                    format!("offered {work_ref} \"{title}\" to {to}"),
                )
            }
            HandoffAction::Accept => (
                WorkHandoffInput::Accept {
                    idempotency_key: String::new(),
                },
                format!("accepted {work_ref} \"{title}\""),
            ),
            HandoffAction::Cancel { reason } => {
                let reason = reason.trim().to_owned();
                if reason.is_empty() {
                    return Err(
                        StoreError::InvalidWork("say why the handoff is cancelled".into()).into(),
                    );
                }
                (
                    WorkHandoffInput::Cancel {
                        reason,
                        idempotency_key: String::new(),
                    },
                    format!("cancelled handoff of {work_ref} \"{title}\""),
                )
            }
        };
        let result = self
            .service
            .work_handoff(core, now)
            .map_err(|error| VerbError::at(error, &work_ref))?;
        let after = self.refreshed(view, now)?;
        let holder = self.holder(&after, now);
        let line = format!("{verb}{}", held_suffix(holder, now));
        let guidance = self.guidance(&after, "handoff", now);
        Ok(Receipt::assemble(
            vec![line],
            guidance,
            serde_json::to_value(&result)?,
            false,
        ))
    }

    fn target(
        &self,
        work_ref: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<WorkFocusView, VerbError> {
        match work_ref {
            Some(work_ref) => self
                .service
                .work_focus(work_ref, now)
                .map_err(|error| VerbError::at(error, work_ref)),
            None => self.focused(now)?.ok_or_else(|| {
                StoreError::InvalidWork(
                    "this session has no focused work; name the item or claim one first".into(),
                )
                .into()
            }),
        }
    }

    /// Reads the ambient focus without staging or acknowledging deliveries.
    fn focused(&self, now: DateTime<Utc>) -> Result<Option<WorkFocusView>, VerbError> {
        let view: WorkNextView = self.service.work_next(
            1,
            WorkNextQuery {
                sections: vec![WorkNextSection::Focus],
                ..WorkNextQuery::default()
            },
            now,
        )?;
        Ok(view.focus)
    }

    fn refreshed(
        &self,
        previous: WorkFocusView,
        now: DateTime<Utc>,
    ) -> Result<WorkFocusView, VerbError> {
        Ok(self.focused(now)?.unwrap_or(previous))
    }

    fn holder<'a>(&self, view: &'a WorkFocusView, now: DateTime<Utc>) -> Holder<'a> {
        match &view.claim {
            Some(claim) if live(claim, now) => {
                if claim.holder == self.session_id {
                    Holder::You(claim.expires_at)
                } else {
                    Holder::Other(&claim.holder, claim.expires_at)
                }
            }
            _ => Holder::Nobody,
        }
    }

    fn guidance(&self, view: &WorkFocusView, word: &str, now: DateTime<Utc>) -> Guidance {
        let holder = self.holder(view, now);
        let blockers = view
            .blockers
            .iter()
            .map(|blocker| short(&blocker.detail))
            .collect::<Vec<_>>();
        let mut reminders = Vec::new();
        for reason in &view.status.why {
            if let Some(words) = reminder_for_reason(reason, holder, &blockers)
                && !reminders.contains(&words)
            {
                reminders.push(words);
            }
        }
        for words in obligation_reminders(&view.obligation_page) {
            if !reminders.contains(&words) {
                reminders.push(words);
            }
        }
        let next = next_commands(
            &view.allowed_next,
            &view.status.work.short_ref,
            word,
            !view.blockers.is_empty(),
            view.status.work.lifecycle == WorkLifecycle::Open,
        );
        Guidance { reminders, next }
    }
}

fn ambiguous_reference_guidance(
    candidates: &[crate::WorkReferenceCandidate],
    more: usize,
) -> (Vec<String>, Vec<String>) {
    let mut reminders = candidates
        .iter()
        .map(|candidate| {
            format!(
                "{} \"{}\" is {:?}; use its full id {}",
                candidate.short_ref,
                short(&candidate.title),
                candidate.lifecycle,
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

fn completion_recovery_reminder(recovery: &crate::WorkCompletionRecovery) -> String {
    let item = &recovery.item;
    match &recovery.cause {
        crate::WorkCompletionRecoveryCause::LapsedClaim { expired_at } => format!(
            "{} \"{}\" has a claim that lapsed at {expired_at}",
            item.short_ref,
            short(&item.title)
        ),
        crate::WorkCompletionRecoveryCause::OpenObligation {
            obligation_id,
            required_check,
            ..
        } => format!(
            "{} \"{}\" still owes {required_check:?} for obligation {}",
            item.short_ref,
            short(&item.title),
            obligation_id.0
        ),
        crate::WorkCompletionRecoveryCause::RequiredChildUnsealed { .. } => format!(
            "required child {} \"{}\" is {:?} without a completion seal or waiver",
            item.short_ref,
            short(&item.title),
            item.lifecycle
        ),
        crate::WorkCompletionRecoveryCause::MissingContribution { participant } => format!(
            "{} \"{}\" is missing the contribution or waiver for participant {}",
            item.short_ref,
            short(&item.title),
            participant.0
        ),
        crate::WorkCompletionRecoveryCause::MissingAcceptance { criterion } => format!(
            "{} \"{}\" is missing acceptance for \"{}\"",
            item.short_ref,
            short(&item.title),
            short(criterion)
        ),
    }
}

/// Fixed table from one readiness reason to the words an agent needs.
fn reminder_for_reason(reason: &str, holder: Holder<'_>, blockers: &[String]) -> Option<String> {
    let reason = reason.trim();
    if let Some(lifecycle) = reason.strip_prefix("lifecycle is ") {
        return match lifecycle.trim().to_ascii_lowercase().as_str() {
            "completed" => None,
            "cancelled" => Some("this item was cancelled".into()),
            "superseded" => Some("this item was superseded by another item".into()),
            "proposed" => Some("this item is proposed and not yet open".into()),
            _ => Some("this item is closed".into()),
        };
    }
    match reason {
        "the ancestor or root-execution generation does not admit execution" => {
            Some("a parent item does not admit execution yet".into())
        }
        "deferred wake time has not arrived" => {
            Some("deferred: its wake time has not arrived".into())
        }
        "one or more prerequisites are incomplete" => {
            Some("waiting: one or more prerequisites are not complete".into())
        }
        "one or more typed blockers remain active" => Some(if blockers.is_empty() {
            "blocked: one or more blockers remain active".into()
        } else {
            format!("blocked: {}", blockers.join("; "))
        }),
        "open, admitted, unblocked, and unclaimed" => {
            Some("unclaimed: claim it before you change anything".into())
        }
        "prior claim is recoverable" => {
            Some("a previous holder's claim lapsed; claiming needs a recovery reason".into())
        }
        "live claim has checkpointed progress" => match holder {
            Holder::Other(session, _) => Some(format!("held by {}", session.0)),
            Holder::You(_) | Holder::Nobody => None,
        },
        "live claim has not checkpointed progress" => Some(match holder {
            Holder::You(_) => "you hold this item but have not noted progress yet".into(),
            Holder::Other(session, _) => {
                format!("held by {}; no progress noted yet", session.0)
            }
            Holder::Nobody => "held; no progress noted yet".into(),
        }),
        other => Some(other.to_owned()),
    }
}

/// Fixed table from open typed obligations to words. Waiver authority and
/// identities stay host-private.
fn obligation_reminders(page: &WorkObligationPage) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for item in page
        .items
        .iter()
        .filter(|item| item.state == WorkObligationState::Open)
    {
        let words = match item.requirement.check_kind {
            VerificationKind::Test => {
                "tests have not run since your last source change — run them; the host records the result"
            }
            VerificationKind::Build => {
                "the build has not run since your last source change — run it; the host records the result"
            }
            VerificationKind::Lint => {
                "lint has not run since your last source change — run it; the host records the result"
            }
            VerificationKind::Review => {
                "a review is still owed for your last source change; the host records the result"
            }
            VerificationKind::Acceptance => {
                "acceptance verification is still owed; the host records the result"
            }
        };
        if !out.iter().any(|existing| existing == words) {
            out.push(words.into());
        }
    }
    if page.omitted_count > 0 {
        out.push("more obligations are open than shown here".into());
    }
    out
}

/// How many lifecycle moves a receipt suggests before deferring to `show`.
const NEXT_LIFECYCLE_LIMIT: usize = 3;

/// Fixed table from `allowed_next` tags to literal commands. Only the moves
/// that change who holds the item or whether it is finished are suggested
/// (accept, claim, note, done, unblock) — at most [`NEXT_LIFECYCLE_LIMIT`] in
/// priority order — followed by `engram work show REF` for the rest. Planning
/// edits (block, release, handoff offers, decomposition, revision, cancel) and
/// entries the agent cannot run through the nine words stay in `allowed_next`
/// on the structured receipt only.
fn next_commands(
    allowed_next: &[String],
    work_ref: &str,
    word: &str,
    blocked: bool,
    open: bool,
) -> Vec<String> {
    let has = |tag: &str| allowed_next.iter().any(|entry| entry == tag);
    let mut out: Vec<String> = Vec::new();
    let mut push = |command: String| {
        if out.len() < NEXT_LIFECYCLE_LIMIT && !out.contains(&command) {
            out.push(command);
        }
    };
    if has("work_handoff:accept") {
        push(format!("engram work handoff {work_ref} --accept"));
    }
    if has("work_update:claim") {
        push(format!("engram work claim {work_ref}"));
    }
    if has("work_update:claim(recovery_reason_required)") {
        push(format!("engram work claim {work_ref} --recover \"…\""));
    }
    if has("work_update:checkpoint") || has("work_update:evidence") {
        push(format!("engram work note {work_ref} \"…\""));
    }
    if has("work_complete") {
        push(format!("engram work done {work_ref} \"…\""));
    }
    if blocked && has("work_update:unblock") {
        push(format!("engram work update {work_ref} --unblock"));
    }
    let lifecycle = !out.is_empty();
    // Looking at a closed item again or calling `next` from `next` changes
    // nothing, so neither is suggested.
    if open && has("work_focus") && word != "show" {
        out.push(format!("engram work show {work_ref}"));
    }
    if !lifecycle && word != "next" {
        out.push("engram work next".into());
    }
    out
}

fn catalog_filters_admit(status: &ReadyWorkSummary, input: &LsInput) -> bool {
    let work = &status.work;
    if !input.all && work.lifecycle != WorkLifecycle::Open {
        return false;
    }
    if input.blocked && status.blocker_count == 0 && status.blocked_by.is_empty() {
        return false;
    }
    if let Some(label) = input.label.as_deref().map(str::trim)
        && !label.is_empty()
        && !work
            .labels
            .iter()
            .any(|value| value.eq_ignore_ascii_case(label))
    {
        return false;
    }
    if let Some(search) = input.search.as_deref().map(str::trim)
        && !search.is_empty()
    {
        let needle = search.to_lowercase();
        let haystack = format!(
            "{} {} {} {}",
            work.short_ref,
            work.title,
            work.outcome,
            work.labels.join(" ")
        )
        .to_lowercase();
        if !haystack.contains(&needle) {
            return false;
        }
    }
    true
}

fn live(claim: &WorkClaim, now: DateTime<Utc>) -> bool {
    claim.state == WorkClaimState::Active && claim.expires_at > now
}

fn item_line(status: &ReadyWorkSummary, holder: Holder<'_>, now: DateTime<Utc>) -> String {
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

fn ready_line(item: &ReadyWorkSummary) -> String {
    let work = &item.work;
    format!(
        "{} \"{}\" p{} {}",
        work.short_ref,
        short(&work.title),
        work.priority,
        availability_words(item)
    )
}

fn catalog_line(item: &ReadyWorkSummary) -> String {
    let work = &item.work;
    let mut line = format!(
        "{}  p{}  {:<9} \"{}\"",
        work.short_ref,
        work.priority,
        availability_words(item),
        short(&work.title)
    );
    if !work.labels.is_empty() {
        let _ = write!(line, "  [{}]", work.labels.join(", "));
    }
    if let Some(assignee) = &work.assigned_to {
        let _ = write!(line, "  @{assignee}");
    }
    line
}

/// One line per change by another actor. Your own actions are already in
/// your receipts and are skipped. A single `note` appends an
/// evidence object, an evidence-added event, and a checkpoint; they collapse
/// into one `noted` line. Summaries that repeat their change kind as a prefix
/// lose the prefix.
fn collapse_changes(changes: &[WorkChange], own_actor: &str) -> Vec<String> {
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
            )),
            WorkChangeProjection::Omitted(_) => None,
        })
        .collect::<Vec<_>>();
    let mut lines: Vec<String> = Vec::new();
    let mut last_note: Option<(String, String)> = None;
    for (index, change) in changes.iter().enumerate() {
        let Some((subject, kind, text, actor_id)) = visible[index].as_ref() else {
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
        if actor_id.as_deref() == Some(own_actor) {
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
                    .is_some_and(|(next_subject, next_kind, next_text, _)| {
                        next_subject == subject && next_kind == "completed" && next_text == text
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
            .map(|actor| format!(" by {actor}"))
            .unwrap_or_default();
        lines.push(format!("{subject} {verb}{actor}: {}", short(text)));
    }
    lines
}

fn strip_kind_prefix(summary: &str, kind: &str) -> String {
    let prefix = format!("{kind}: ");
    summary
        .strip_prefix(&prefix)
        .unwrap_or(summary)
        .trim()
        .to_owned()
}

fn availability_words(status: &ReadyWorkSummary) -> &'static str {
    match status.availability {
        WorkAvailability::Ready => "ready",
        WorkAvailability::Claimed => "held",
        WorkAvailability::Active => "active",
        WorkAvailability::Blocked => "blocked",
        WorkAvailability::Deferred => "deferred",
        WorkAvailability::Waiting => "waiting",
        WorkAvailability::Closed => lifecycle_word(status.work.lifecycle),
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
    let text = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if text.len() <= MAX_TEXT_LINE_BYTES {
        return text;
    }
    let mut end = MAX_TEXT_LINE_BYTES;
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

#[cfg(test)]
mod tests {
    use std::str::FromStr;

    use super::*;
    use crate::{BuiltinObligationRuleRef, VerificationRequirement, WorkObligationGuidance};

    fn hash(fill: char) -> ObjectHash {
        ObjectHash::from_str(&fill.to_string().repeat(64)).expect("hash")
    }

    fn page(kind: VerificationKind, state: WorkObligationState) -> WorkObligationPage {
        let requirement = VerificationRequirement {
            check_kind: kind,
            check_fingerprint: None,
            required_environment: None,
        };
        WorkObligationPage {
            items: vec![crate::WorkObligationSummary {
                obligation_id: crate::WorkObligationId(uuid::Uuid::nil()),
                definition: hash('a'),
                rule_set: hash('c'),
                state,
                rule: BuiltinObligationRuleRef {
                    rule_id: "source_mutation_requires_test".into(),
                    rule_version: 1,
                },
                requirement: requirement.clone(),
                triggering_observation: hash('b'),
                resolution: None,
                evidence: None,
                waived_by: None,
                guidance: WorkObligationGuidance::RecordVerificationThenCheckpoint {
                    requirement,
                    host_waiver_requestable: true,
                },
            }],
            omitted_count: 0,
        }
    }

    #[test]
    fn readiness_reasons_become_words() {
        let session = SessionId("peer".into());
        let now = Utc::now();
        assert_eq!(
            reminder_for_reason(
                "live claim has not checkpointed progress",
                Holder::You(now),
                &[]
            )
            .as_deref(),
            Some("you hold this item but have not noted progress yet")
        );
        assert_eq!(
            reminder_for_reason(
                "live claim has not checkpointed progress",
                Holder::Other(&session, now),
                &[]
            )
            .as_deref(),
            Some("held by peer; no progress noted yet")
        );
        assert_eq!(
            reminder_for_reason(
                "one or more typed blockers remain active",
                Holder::Nobody,
                &["waiting on review".into()]
            )
            .as_deref(),
            Some("blocked: waiting on review")
        );
        assert_eq!(
            reminder_for_reason("lifecycle is Completed", Holder::Nobody, &[]),
            None
        );
        assert_eq!(
            reminder_for_reason(
                "open, admitted, unblocked, and unclaimed",
                Holder::Nobody,
                &[]
            )
            .as_deref(),
            Some("unclaimed: claim it before you change anything")
        );
    }

    #[test]
    fn open_test_obligation_becomes_the_test_reminder() {
        let reminders =
            obligation_reminders(&page(VerificationKind::Test, WorkObligationState::Open));
        assert_eq!(
            reminders,
            vec![
                "tests have not run since your last source change — run them; the host records the result"
                    .to_owned()
            ]
        );
        assert!(
            obligation_reminders(&page(
                VerificationKind::Test,
                WorkObligationState::Satisfied
            ))
            .is_empty()
        );
    }

    #[test]
    fn allowed_next_tags_become_commands_and_host_only_entries_vanish() {
        let tags = [
            "work_focus",
            "work_update:claim",
            "work_update:reopen",
            "work_update:supersede",
            "work_update:waive_required_child",
            "work_update:add_prerequisite",
            "work_propose:decompose",
        ]
        .map(String::from);
        assert_eq!(
            next_commands(&tags, "w-0123456789ab", "add", false, true),
            vec![
                "engram work claim w-0123456789ab",
                "engram work show w-0123456789ab",
            ]
        );
        // Planning edits the holder could make (release, offer, block,
        // decompose, revise, cancel) stay behind `show`; only the moves that
        // change who holds the item or whether it is finished are suggested.
        let held = [
            "work_focus",
            "work_update:checkpoint",
            "work_update:evidence",
            "work_update:release",
            "work_complete",
            "work_handoff:offer",
            "work_update:block",
            "work_update:unblock",
            "work_propose:decompose",
            "work_update:revise",
            "work_update:cancel",
        ]
        .map(String::from);
        assert_eq!(
            next_commands(&held, "w-0123456789ab", "show", false, true),
            vec![
                "engram work note w-0123456789ab \"…\"",
                "engram work done w-0123456789ab \"…\"",
            ]
        );
        assert_eq!(
            next_commands(&held, "w-0123456789ab", "claim", true, true),
            vec![
                "engram work note w-0123456789ab \"…\"",
                "engram work done w-0123456789ab \"…\"",
                "engram work update w-0123456789ab --unblock",
                "engram work show w-0123456789ab",
            ]
        );
        // Lifecycle moves are capped at three in priority order; `show` is
        // still the one trailing entry, so no receipt lists more than four.
        let crowded = [
            "work_focus",
            "work_handoff:accept",
            "work_update:claim",
            "work_update:checkpoint",
            "work_complete",
            "work_update:unblock",
        ]
        .map(String::from);
        assert_eq!(
            next_commands(&crowded, "w-0123456789ab", "next", true, true),
            vec![
                "engram work handoff w-0123456789ab --accept",
                "engram work claim w-0123456789ab",
                "engram work note w-0123456789ab \"…\"",
                "engram work show w-0123456789ab",
            ]
        );
        assert_eq!(
            next_commands(
                &["work_focus".into()],
                "w-0123456789ab",
                "done",
                false,
                true
            ),
            vec!["engram work show w-0123456789ab", "engram work next",]
        );
        // A closed item is not worth showing again, and `next` never
        // suggests itself.
        assert_eq!(
            next_commands(
                &["work_focus".into()],
                "w-0123456789ab",
                "done",
                false,
                false
            ),
            vec!["engram work next"]
        );
        assert!(
            next_commands(
                &["work_focus".into()],
                "w-0123456789ab",
                "next",
                false,
                false
            )
            .is_empty()
        );
    }

    #[test]
    fn text_receipts_never_carry_hashes_or_keys() {
        let receipt = Receipt::assemble(
            vec!["claimed w-0123456789ab \"Baseline\" (held by you until 13:05 UTC)".into()],
            Guidance {
                reminders: vec!["you hold this item but have not noted progress yet".into()],
                next: vec!["engram work note w-0123456789ab \"…\"".into()],
            },
            json!({
                "receipt": { "control_binding": { "claim_fence": 7 } },
                "seal": "a".repeat(64),
                "idempotency_key": "k",
            }),
            false,
        );
        let text = receipt.text();
        assert!(!text.contains(&"a".repeat(64)));
        assert!(!text.contains("fence"));
        assert!(!text.contains("idempotency"));
        assert!(text.ends_with("next:\n  engram work note w-0123456789ab \"…\""));
        assert_eq!(receipt.value["reminders"].as_array().map(Vec::len), Some(1));
        assert_eq!(receipt.value["seal"], json!("a".repeat(64)));
    }

    #[test]
    fn lapsed_claim_guidance_names_the_expiry_and_recovery_command() {
        let expired_at = Utc::now();
        let error = VerbError::at(
            StoreError::WorkClaimLapsed {
                work: crate::domain::WorkId::new(),
                expired_at,
            },
            "w-0123456789ab",
        );
        let guidance = error.guidance();
        assert_eq!(
            guidance.reminders,
            vec![format!("claim lapsed at {}", clock(expired_at, Utc::now()))]
        );
        assert_eq!(
            guidance.next,
            vec![String::from(
                "engram work claim w-0123456789ab --recover \"explain why the lapsed claim is being recovered\""
            )]
        );
    }

    #[test]
    fn ambiguous_reference_guidance_names_candidates_and_uses_full_ids() {
        let first = crate::WorkId::new();
        let second = crate::WorkId::new();
        let error = VerbError::at(
            StoreError::WorkReferenceAmbiguous {
                reference: "w-collision".into(),
                candidates: vec![
                    crate::WorkReferenceCandidate {
                        work_id: first,
                        short_ref: "w-collision".into(),
                        title: "First candidate".into(),
                        lifecycle: WorkLifecycle::Open,
                    },
                    crate::WorkReferenceCandidate {
                        work_id: second,
                        short_ref: "w-collision".into(),
                        title: "Second candidate".into(),
                        lifecycle: WorkLifecycle::Completed,
                    },
                ],
                more: 3,
            },
            "w-collision",
        );
        let guidance = error.guidance();
        assert_eq!(guidance.reminders.len(), 3);
        assert!(guidance.reminders[0].contains("First candidate"));
        assert!(guidance.reminders[1].contains("Second candidate"));
        assert_eq!(
            guidance.reminders[2],
            "3 additional ambiguous candidates were omitted"
        );
        assert!(
            error
                .error
                .to_string()
                .contains("3 additional candidates omitted")
        );
        assert_eq!(
            guidance.next,
            vec![
                format!("engram work show {}", first.0),
                format!("engram work show {}", second.0),
            ]
        );
    }

    #[test]
    fn slugs_and_refs_and_dates_parse_predictably() {
        assert_eq!(slug("  Ship the parity test! "), "ship-the-parity-test");
        assert_eq!(slug("***"), "child");
        assert!(looks_like_work_ref("w-0123456789ab"));
        assert!(looks_like_work_ref(&uuid::Uuid::nil().to_string()));
        assert!(!looks_like_work_ref("Delivered the thing"));
        assert!(!looks_like_work_ref("w-xyz"));
        assert_eq!(
            parse_defer_date("2026-09-01").expect("date"),
            DateTime::parse_from_rfc3339("2026-09-01T00:00:00Z")
                .expect("rfc")
                .with_timezone(&Utc)
        );
        assert!(parse_defer_date("tomorrow").is_err());
        assert_eq!(short("a  b\n c"), "a b c");
        assert!(short(&"x".repeat(200)).ends_with('…'));
    }
}
