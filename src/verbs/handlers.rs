use std::collections::HashMap;

use crate::domain::normalize_gate_evidence_input;

use super::{
    Arc, ChildRequirement, DEFAULT_LIMIT, DateTime, Deserialize, Guidance, Holder,
    LocalWorkService, MAX_AGENT_WORK_RESPONSE_BYTES, MAX_COMPACT_CHANGE_ITEMS, MAX_NEXT_PAGES,
    PathBuf, ProjectId, ReadyWorkSummary, Receipt, Serialize, SessionId, StoreError, Utc,
    VerbError, VerificationKind, WORK_UPDATE_CLAIM_ACTION, WORK_UPDATE_CLAIM_RECOVERY_ACTION,
    WorkAttributionDefaults, WorkAvailability, WorkBlockerKind, WorkChildInput, WorkCompleteInput,
    WorkCompleteResult, WorkCompletionCaptureInput, WorkFocusView, WorkHandoffInput, WorkId,
    WorkItemKind, WorkLifecycle, WorkNextQuery, WorkNextSection, WorkNextView, WorkObligationPage,
    WorkObligationState, WorkPrerequisiteState, WorkProposeInput, WorkProposeResult,
    WorkRevisionPatch, WorkUpdateInput, changes_not_delivered, clock, collapse_changes,
    held_suffix, item_line, json, lifecycle_word, nonempty,
    receipts::{
        append_changes_lines, compact_next_lines, compact_next_receipt, compact_next_value,
        fit_list_receipt, ready_line,
    },
    section_word, short,
    show::{live, show_lines, show_receipt_value},
    slug, terminal_safe_actor_label, terminal_safe_multiline, trimmed, validate_priority,
};

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
    /// Return the full host-oriented projection instead of compact rows.
    #[serde(default)]
    pub verbose: bool,
    pub context_generation: Option<String>,
}

/// `ls` / `search`: catalog listing with flat filters.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "the four booleans are independent flat CLI/MCP list switches"
)]
pub struct LsInput {
    pub search: Option<String>,
    pub blocked: bool,
    /// Assigned to this actor, or held by this session.
    pub mine: bool,
    /// Include completed, cancelled, and superseded items.
    pub all: bool,
    pub label: Option<String>,
    pub limit: Option<u32>,
    /// Return the full host-oriented projection instead of compact rows.
    #[serde(default)]
    pub verbose: bool,
}

/// `add`: a root, or one required/optional child under `under`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AddInput {
    #[serde(default)]
    pub notes: Vec<String>,
    pub title: String,
    pub outcome: Option<String>,
    pub acceptance: Vec<String>,
    pub under: Option<String>,
    /// Make the child non-blocking for parent completion. Valid only with
    /// `under`.
    pub optional: bool,
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
    /// Attributed reason for taking over a different holder's lapsed claim.
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
        /// Replace the whole acceptance list; omission leaves it unchanged.
        acceptance: Option<Vec<String>>,
        assignee: Option<String>,
        priority: Option<i32>,
        defer: Option<DateTime<Utc>>,
        kind: Option<WorkItemKind>,
        #[serde(default)]
        labels: Vec<String>,
        #[serde(default)]
        unlabels: Vec<String>,
    },
    Cancel {
        reason: String,
    },
    After {
        prerequisite: String,
    },
    DropAfter {
        prerequisite: String,
    },
    WaiveRequiredChild {
        child: String,
        reason: String,
    },
    Supersede {
        replacement: String,
        reason: String,
    },
}

/// `gate`: one observation on held open work or late evidence on completed focus.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct GateInput {
    #[serde(default)]
    pub work_ref: Option<String>,
    pub name: String,
    #[serde(default)]
    pub failed: Vec<String>,
    pub evidence_ref: Option<String>,
}

pub(super) fn normalize_gate_input(input: &GateInput) -> Result<GateInput, VerbError> {
    let normalized =
        normalize_gate_evidence_input(&input.name, &input.failed, input.evidence_ref.as_deref())
            .map_err(StoreError::InvalidWork)?;

    Ok(GateInput {
        work_ref: input.work_ref.clone(),
        name: normalized.name,
        failed: normalized.failed,
        evidence_ref: normalized.evidence_ref,
    })
}

/// `remember`: one attributed, immutable project episode.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RememberInput {
    pub text: String,
    pub key: Option<String>,
}

/// `memories`: compact list/search or one dedicated full read.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct MemoriesInput {
    pub query: Option<String>,
    pub after: Option<String>,
    #[serde(default)]
    pub full: bool,
}

/// `forget`: permanently retire one project-memory key.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ForgetInput {
    pub key: String,
}

/// `note`: one finding on held open work or late evidence on completed work.
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

impl AgentVerbs {
    /// Builds the agent surface for one host-fixed actor/session.
    #[must_use]
    pub fn new(
        database: PathBuf,
        project_id: ProjectId,
        actor_id: String,
        session_id: SessionId,
        source_skill: Option<String>,
    ) -> Self {
        Self::new_with_attribution(
            database,
            project_id,
            actor_id,
            session_id,
            source_skill,
            None,
            WorkAttributionDefaults::default(),
        )
    }

    /// Builds the shell word surface with optional host-asserted actor context
    /// and explicit local-attribution defaults.
    #[must_use]
    pub fn new_with_attribution(
        database: PathBuf,
        project_id: ProjectId,
        actor_id: String,
        session_id: SessionId,
        source_skill: Option<String>,
        actor_context: Option<String>,
        attribution_defaults: WorkAttributionDefaults,
    ) -> Self {
        Self::with_shared_service(
            Arc::new(LocalWorkService::new_with_attribution(
                database,
                project_id,
                actor_id.clone(),
                session_id.clone(),
                source_skill,
                actor_context,
                attribution_defaults,
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
        let change_limit = if input.verbose {
            limit
        } else {
            limit.min(MAX_COMPACT_CHANGE_ITEMS)
        };
        let query = WorkNextQuery {
            sections: vec![
                WorkNextSection::Focus,
                WorkNextSection::Changes,
                WorkNextSection::Memories,
            ],
            context_generation: input.context_generation.clone(),
            ..WorkNextQuery::default()
        };
        let view = self.service.work_next_for_agent(change_limit, query, now)?;
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
        let mut changes = collapse_changes(view.changes.as_deref().unwrap_or_default());
        let mut not_delivered = changes_not_delivered(&view);
        // Compact output may drain own-session-only pages within its bound.
        // Verbose output exposes the original exact page and its cursor, so
        // it must not acknowledge additional pages behind that receipt.
        let mut pages = 1;
        while !input.verbose && changes.is_empty() && not_delivered > 0 && pages < MAX_NEXT_PAGES {
            let more = self.service.work_next(
                change_limit,
                WorkNextQuery {
                    sections: vec![WorkNextSection::Changes],
                    ..WorkNextQuery::default()
                },
                now,
            )?;
            changes = collapse_changes(more.changes.as_deref().unwrap_or_default());
            not_delivered = changes_not_delivered(&more);
            pages += 1;
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
            // Catalog cards do not carry session-specific `allowed_next`.
            // Resolve the exact ordinary-vs-recovery claim action via `show`.
            let command = format!("engram work show {}", first.work.short_ref);
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
        let (lines, value, guidance) = if input.verbose {
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
            append_changes_lines(&mut lines, &changes, not_delivered);
            if let Some(memories) = &view.memories {
                lines.push(format!(
                    "memories: {} retained{}",
                    memories.count,
                    if memories.changed { " (changed)" } else { "" }
                ));
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
            let mut value = serde_json::to_value(&view)?;
            value["ready"] = serde_json::to_value(&ready)?;
            value["changes_by_others"] = json!(changes);
            value["held"] = serde_json::to_value(
                held.iter()
                    .map(
                        |(item, expires_at)| json!({ "work": item.work, "expires_at": expires_at }),
                    )
                    .collect::<Vec<_>>(),
            )?;
            (lines, value, guidance)
        } else {
            let claims = self.live_claim_map(now)?;
            let compact = compact_next_receipt(&view, &held, &ready, &changes, &claims, &guidance)?;
            let lines = compact_next_lines(&compact);
            let value = compact_next_value(&compact);
            (lines, value, compact.guidance)
        };
        if value
            .get("memories")
            .is_some_and(|memories| !memories.is_null())
        {
            self.service.acknowledge_work_next_memories(&view, now);
        }
        Ok(Receipt::assemble(lines, guidance, value, false))
    }

    fn live_claim_map(
        &self,
        now: DateTime<Utc>,
    ) -> Result<HashMap<WorkId, (SessionId, DateTime<Utc>)>, VerbError> {
        Ok(self.service.live_work_claims(now)?.into_iter().fold(
            HashMap::new(),
            |mut claims, (work_id, holder, expires_at)| {
                claims.insert(work_id, (holder, expires_at));
                claims
            },
        ))
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
                    context_generation: None,
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
        self.ls_with_budget(input, now, MAX_AGENT_WORK_RESPONSE_BYTES)
    }

    // Production always uses the protocol budget; tests can exercise the
    // zero-row boundary without bypassing the bounded item projection.
    pub(super) fn ls_with_budget(
        &self,
        input: &LsInput,
        now: DateTime<Utc>,
        budget: usize,
    ) -> Result<Receipt, VerbError> {
        let limit = input.limit.unwrap_or(DEFAULT_LIMIT).clamp(1, 1_000);
        let (source_items, total, claims) = self.service.work_catalog_page(
            &crate::domain::WorkCatalogQuery {
                search: input.search.clone(),
                lifecycles: if input.all {
                    Vec::new()
                } else {
                    vec![WorkLifecycle::Open]
                },
                blocked_only: input.blocked,
                assigned_to: input.mine.then(|| self.actor_id.clone()),
                held_by: input.mine.then(|| self.session_id.clone()),
                label: input.label.clone(),
                limit,
                ..crate::domain::WorkCatalogQuery::default()
            },
            now,
        )?;
        let claims = claims
            .into_iter()
            .map(|claim| (claim.work_id, (claim.holder, claim.expires_at)))
            .collect::<HashMap<_, _>>();
        fit_list_receipt(input.verbose, &source_items, total, &claims, budget)
    }

    /// `show`: one item in agent detail; selects it as ambient focus without
    /// claiming. Host authority and integrity fields remain on `work core
    /// focus`.
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
        let lines = show_lines(&view, holder, &self.actor_id, &self.session_id, now);
        let guidance = self.guidance(&view, "show", now);
        Ok(Receipt::assemble(
            lines,
            guidance,
            serde_json::to_value(show_receipt_value(
                &view,
                holder,
                &self.actor_id,
                &self.session_id,
                now,
            ))?,
            false,
        ))
    }

    /// Like `show`, optionally returning the full oldest-first note prefix
    /// within the complete receipt budget. Default show remains unchanged.
    ///
    /// # Errors
    /// Returns [`VerbError`] for unknown work, invalid notes or oversized metadata.
    pub fn show_with_notes(
        &self,
        work_ref: &str,
        notes: bool,
        now: DateTime<Utc>,
    ) -> Result<Receipt, VerbError> {
        let receipt = self.show(work_ref, now)?;
        if !notes {
            return Ok(receipt);
        }
        let page = self
            .service
            .work_notes(work_ref, now)
            .map_err(|error| VerbError::at(error, work_ref))?;
        super::receipts::fit_show_notes(
            receipt,
            page,
            &self.actor_id,
            MAX_AGENT_WORK_RESPONSE_BYTES,
        )
    }

    /// `add`: a root, or one required/optional child beneath `under`.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] when input is empty or the core refuses admission.
    pub fn add(&self, mut input: AddInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        input.notes = crate::domain::normalize_initial_work_notes(&input.notes)
            .map_err(StoreError::InvalidWork)?;
        if input
            .acceptance
            .iter()
            .any(|criterion| criterion.trim().is_empty())
        {
            return Err(
                StoreError::InvalidWork("acceptance criteria must not be blank".into()).into(),
            );
        }
        let reminder = input.acceptance.is_empty().then(|| {
            format!(
                "acceptance defaulted to {} is done; set --accept",
                short(&terminal_safe_multiline(input.title.trim()))
            )
        });
        let has_initial_notes = !input.notes.is_empty();
        let mut receipt = self.add_inner(input, now)?;
        if has_initial_notes {
            receipt = receipt.with_reminder(
                "initial observations (no execution credit) recorded at creation".into(),
            )?;
        }
        match reminder {
            Some(text) => receipt.with_reminder(text),
            None => Ok(receipt),
        }
    }

    fn add_inner(&self, input: AddInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
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
        if input.optional && input.under.is_none() {
            return Err(StoreError::InvalidWork(
                "optional work needs a parent; use --under REF with --optional".into(),
            )
            .into());
        }
        if let Some(under) = input.under.as_deref() {
            let requirement = if input.optional {
                ChildRequirement::Optional
            } else {
                ChildRequirement::Required
            };
            return self.add_child(
                under,
                WorkChildInput {
                    notes: input.notes,
                    key: slug(&title),
                    title,
                    outcome,
                    acceptance,
                    requirement: Some(requirement),
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
                notes: input.notes,
                title,
                outcome,
                acceptance,
                work_kind: input.kind,
                priority,
                labels,
                assigned_to,
                deferred_until: None,
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

    /// One required or optional child through `work_propose:decompose`; it becomes
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
            .work_propose_on(
                Some(&parent_ref),
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
        let requirement = if focus.status.work.child_requirement == ChildRequirement::Optional {
            " optional"
        } else {
            ""
        };
        let lines = vec![format!(
            "added{requirement} {child_ref} \"{}\" under {parent_ref} \"{}\"",
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
    /// core does not admit claiming.
    pub fn claim(&self, input: ClaimInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        let view = self.target(Some(&input.work_ref), now)?;
        let work_ref = view.status.work.short_ref.clone();
        let target = view.status.work.work_id.0.to_string();
        let result = self
            .service
            .work_update_on(
                Some(&target),
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
        let after = self.refreshed(&view, now)?;
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

    /// `update`: revise planning/lifecycle state or waive one disposed required child.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] when no action applies or the core refuses it.
    pub fn update(&self, input: UpdateInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        let view = self.target(input.work_ref.as_deref(), now)?;
        let work_ref = view.status.work.short_ref.clone();
        let title = short(&view.status.work.title);
        let prerequisite_target = match &input.action {
            UpdateAction::After { prerequisite } | UpdateAction::DropAfter { prerequisite }
                if !prerequisite.trim().is_empty() =>
            {
                let prerequisite = self
                    .service
                    .resolve_work_reference(prerequisite, now)
                    .map_err(|error| VerbError::at(error, prerequisite))?;
                Some((prerequisite.work_id, prerequisite.short_ref))
            }
            UpdateAction::WaiveRequiredChild { child, .. } if !child.trim().is_empty() => {
                self.service
                    .resolve_work_reference(child, now)
                    .map_err(|error| VerbError::at(error, child))?;
                None
            }
            _ => None,
        };
        let (core, line) = self.update_translation(input.action, &work_ref, &title)?;
        let target = view.status.work.work_id.0.to_string();
        let result = self
            .service
            .work_update_on(Some(&target), core, now)
            .map_err(|error| {
                if let Some((prerequisite_id, prerequisite_ref)) = &prerequisite_target
                    && match &error {
                        StoreError::WorkNotOpen(closed)
                        | StoreError::WorkPrerequisiteAlreadySatisfied(closed)
                        | StoreError::WorkNotFound(closed) => closed == prerequisite_id,
                        _ => false,
                    }
                {
                    return VerbError::at(error, prerequisite_ref);
                }
                VerbError::at(error, &work_ref)
            })?;
        let after = self.refreshed(&view, now)?;
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
    #[allow(
        clippy::too_many_lines,
        reason = "the flat update actions stay together so the agent-to-core mapping remains reviewable"
    )]
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
                acceptance,
                assignee,
                priority,
                defer,
                kind,
                labels,
                unlabels,
            } => {
                let add_labels = trimmed(&labels);
                let remove_labels = trimmed(&unlabels);
                let patch = WorkRevisionPatch {
                    title: nonempty(new_title),
                    outcome: nonempty(outcome),
                    acceptance,
                    kind,
                    priority: validate_priority(priority)?,
                    labels: None,
                    add_labels,
                    remove_labels,
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
                if patch.acceptance.is_some() {
                    fields.push("acceptance");
                }
                if patch.assigned_to.is_some() {
                    fields.push("assignee");
                }
                if patch.priority.is_some() {
                    fields.push("priority");
                }
                if patch.kind.is_some() {
                    fields.push("kind");
                }
                if !patch.add_labels.is_empty() || !patch.remove_labels.is_empty() {
                    fields.push("labels");
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
            UpdateAction::After { prerequisite } => {
                let prerequisite = prerequisite.trim().to_owned();
                if prerequisite.is_empty() {
                    return Err(StoreError::InvalidWork(
                        "adding a prerequisite needs the prerequisite item ref".into(),
                    )
                    .into());
                }
                (
                    WorkUpdateInput::AddPrerequisite {
                        prerequisite: prerequisite.clone(),
                        idempotency_key: String::new(),
                    },
                    format!("made {work_ref} \"{title}\" wait for {prerequisite}"),
                )
            }
            UpdateAction::DropAfter { prerequisite } => {
                let prerequisite = prerequisite.trim().to_owned();
                if prerequisite.is_empty() {
                    return Err(StoreError::InvalidWork(
                        "removing a prerequisite needs the prerequisite item ref".into(),
                    )
                    .into());
                }
                (
                    WorkUpdateInput::RemovePrerequisite {
                        prerequisite: prerequisite.clone(),
                        idempotency_key: String::new(),
                    },
                    format!("removed {prerequisite} as a prerequisite of {work_ref} \"{title}\""),
                )
            }
            UpdateAction::WaiveRequiredChild { child, reason } => {
                let child = child.trim().to_owned();
                let reason = reason.trim().to_owned();
                if child.is_empty() {
                    return Err(StoreError::InvalidWork(
                        "a required-child waiver needs the child item ref".into(),
                    )
                    .into());
                }
                if reason.is_empty() {
                    return Err(StoreError::InvalidWork(
                        "a required-child waiver needs a reason".into(),
                    )
                    .into());
                }
                (
                    WorkUpdateInput::WaiveRequiredChild {
                        child: child.clone(),
                        reason: reason.clone(),
                        idempotency_key: String::new(),
                    },
                    format!(
                        "waived required child {child} for {work_ref} \"{title}\": {}",
                        short(&reason)
                    ),
                )
            }
            UpdateAction::Supersede {
                replacement,
                reason,
            } => {
                let replacement = replacement.trim().to_owned();
                let reason = reason.trim().to_owned();
                if replacement.is_empty() {
                    return Err(StoreError::InvalidWork(
                        "a supersession needs the replacement item ref".into(),
                    )
                    .into());
                }
                if reason.is_empty() {
                    return Err(
                        StoreError::InvalidWork("a supersession needs a reason".into()).into(),
                    );
                }
                (
                    WorkUpdateInput::Supersede {
                        replacement: replacement.clone(),
                        reason: reason.clone(),
                        idempotency_key: String::new(),
                    },
                    format!(
                        "superseded {work_ref} \"{title}\" with {replacement}: {}",
                        short(&reason)
                    ),
                )
            }
        })
    }

    /// `gate`: record an observation on held open work or completed focus.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] when the input exceeds the documented bounds,
    /// text contains unsafe control/format characters, or this session does
    /// not hold the item.
    pub fn gate(&self, input: GateInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        let normalized = normalize_gate_input(&input)?;
        let GateInput {
            work_ref: target_ref,
            name,
            failed,
            evidence_ref,
        } = input;
        let view = self.target(target_ref.as_deref(), now).map_err(|error| {
            if matches!(&error.error, StoreError::InvalidWork(reason) if reason.contains("no focused work")) {
                VerbError::from(StoreError::InvalidWork(super::GATE_WORK_REF_REQUIRED.into()))
            } else {
                error
            }
        })?;
        let work_ref = view.status.work.short_ref.clone();
        let passed = normalized.failed.is_empty();
        let result = self
            .service
            .work_gate_on(
                Some(&view.status.work.work_id.0.to_string()),
                &name,
                &failed,
                evidence_ref.as_deref(),
                now,
            )
            .map_err(|error| VerbError::at(error, &work_ref))?;
        let after = self.refreshed(&view, now)?;
        let guidance = self.guidance(&after, "gate", now);
        let mut value = serde_json::to_value(&result)?;
        value["operation"] = json!("gate");
        value["gate"] = json!({
            "name": &normalized.name,
            "passed": passed,
            "failed_count": normalized.failed.len(),
            "referenced": normalized.evidence_ref.is_some(),
        });
        let state = if passed {
            "passed".to_owned()
        } else {
            format!("failed ({} failures)", normalized.failed.len())
        };
        let lines = vec![format!(
            "recorded gate {} {state} on {work_ref} \"{}\"{}",
            short(&normalized.name),
            short(&after.status.work.title),
            held_suffix(self.holder(&after, now), now)
        )];
        Ok(Receipt::assemble(lines, guidance, value, false))
    }

    /// `remember`: create one attributed project episode.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] when authorization, key, size, redaction, or
    /// create-only lifecycle admission fails.
    pub fn remember(&self, input: RememberInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        let receipt = self
            .service
            .remember_project_memory(input.text, input.key, now)?;
        let guidance = Guidance {
            reminders: Vec::new(),
            next: vec![
                format!("engram work memories {} --full", receipt.key),
                "engram work memories".into(),
            ],
        };
        let replay = if receipt.duplicate { " (replayed)" } else { "" };
        let lines = vec![format!("remembered project memory {}{replay}", receipt.key)];
        Ok(Receipt::assemble(
            lines,
            guidance,
            serde_json::to_value(receipt)?,
            false,
        ))
    }

    /// `memories`: list/search compact rows or return one dedicated full body.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] when the query/full-read shape is invalid or the
    /// core refuses authorization, key resolution, or projection validation.
    pub fn memories(
        &self,
        input: &MemoriesInput,
        now: DateTime<Utc>,
    ) -> Result<Receipt, VerbError> {
        if input.full {
            if input.after.is_some() {
                return Err(StoreError::InvalidProjectMemory(
                    "--full cannot be combined with --after".into(),
                )
                .into());
            }
            let key = input.query.as_deref().ok_or_else(|| {
                StoreError::InvalidProjectMemory("--full requires a memory key".into())
            })?;
            let envelope = self.service.project_memory_full(key, now)?;
            let lines = envelope.terminal_lines();
            return Ok(Receipt::assemble(
                lines,
                Guidance {
                    reminders: envelope.reminders.clone(),
                    next: envelope.next.clone(),
                },
                serde_json::to_value(envelope)?,
                false,
            ));
        }
        let filtered = input
            .query
            .as_deref()
            .is_some_and(|query| !query.trim().is_empty());
        let mut result =
            self.service
                .project_memories(input.query.as_deref(), input.after.as_deref(), now)?;
        loop {
            let receipt = project_memory_list_receipt(&result, filtered)?;
            let structured_bytes = serde_json::to_vec(&receipt.value)?.len();
            let terminal_bytes = receipt.text().len();
            if structured_bytes <= MAX_AGENT_WORK_RESPONSE_BYTES
                && terminal_bytes <= MAX_AGENT_WORK_RESPONSE_BYTES
            {
                return Ok(receipt);
            }
            if result.memories.pop().is_none() {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "project-memory list response cannot fit the {MAX_AGENT_WORK_RESPONSE_BYTES}-byte agent protocol limit"
                ))
                .into());
            }
            if filtered {
                result.omitted_count = result.omitted_count.saturating_add(1);
            }
            result.exhausted = false;
            if !filtered {
                if result.memories.is_empty() {
                    return Err(StoreError::InvalidWorkProjection(format!(
                        "one project-memory list row cannot fit the {MAX_AGENT_WORK_RESPONSE_BYTES}-byte agent protocol limit"
                    ))
                    .into());
                }
                result.next_after = result.memories.last().map(|row| row.key.clone());
            }
        }
    }

    /// `forget`: append an attributed terminal tombstone.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] when authorization, key resolution, or terminal
    /// lifecycle validation fails.
    pub fn forget(&self, input: ForgetInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        let receipt = self.service.forget_project_memory(input.key, now)?;
        let replay = if receipt.duplicate { " (replayed)" } else { "" };
        let lines = vec![format!("forgot project memory {}{replay}", receipt.key)];
        Ok(Receipt::assemble(
            lines,
            Guidance {
                reminders: vec!["forget is a tombstone, not erasure".into()],
                next: vec!["engram work memories".into()],
            },
            serde_json::to_value(receipt)?,
            false,
        ))
    }

    /// `note`: record an attributed observation; only a live holder also
    /// checkpoints its execution. Non-holders need no claim on open work.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] for empty text, invalid project/lifecycle binding,
    /// or a stale holder authority basis.
    pub fn note(&self, input: &NoteInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        let text = input.text.trim().to_owned();
        if text.is_empty() {
            return Err(StoreError::InvalidWork("note text must not be empty".into()).into());
        }
        let view = self.target(input.work_ref.as_deref(), now)?;
        let work_ref = view.status.work.short_ref.clone();
        let target = view.status.work.work_id.0.to_string();
        let result = self
            .service
            .work_note_on(Some(&target), &text, &trimmed(&input.refs), now)
            .map_err(|error| VerbError::at(error, &work_ref))?;
        let after = self.refreshed(&view, now)?;
        let guidance = self.guidance(&after, "note", now);
        let value = serde_json::to_value(&result)?;
        let observation = if result.non_holder {
            " (observation, no run credit)"
        } else {
            ""
        };
        let lines = vec![format!(
            "noted on {work_ref} \"{}\"{observation}: {}{}",
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
        let target = view.status.work.work_id.0.to_string();
        let result = self
            .service
            .work_complete_on(
                Some(&target),
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
        let after = self.refreshed(&view, now)?;
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
        let target = view.status.work.work_id.0.to_string();
        let result = self
            .service
            .work_handoff_on(Some(&target), core, now)
            .map_err(|error| VerbError::at(error, &work_ref))?;
        let after = self.refreshed(&view, now)?;
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
            Some(work_ref) => {
                self.service
                    .select_work(work_ref, now)
                    .map_err(|error| VerbError::at(error, work_ref))?;
                self.service
                    .inspect_work(work_ref, now)
                    .map_err(|error| VerbError::at(error, work_ref))
            }
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
        previous: &WorkFocusView,
        now: DateTime<Utc>,
    ) -> Result<WorkFocusView, VerbError> {
        let work_ref = previous.status.work.short_ref.clone();
        self.service
            .inspect_work(&previous.status.work.work_id.0.to_string(), now)
            .map_err(|error| VerbError::at(error, &work_ref))
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
        let claim_recovery_required = view
            .allowed_next
            .iter()
            .any(|action| action == WORK_UPDATE_CLAIM_RECOVERY_ACTION);
        let blockers = view
            .blockers
            .iter()
            .map(|blocker| short(&blocker.detail))
            .collect::<Vec<_>>();
        let mut reminders = Vec::new();
        for reason in &view.status.why {
            if let Some(words) =
                reminder_for_reason(reason, holder, &blockers, claim_recovery_required)
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
            &view.prerequisites,
        );
        Guidance { reminders, next }
    }
}

pub(super) fn completion_recovery_reminder(recovery: &crate::WorkCompletionRecovery) -> String {
    let item = &recovery.item;
    match &recovery.cause {
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
            "required child {} \"{}\" is {} without a completion seal or waiver",
            item.short_ref,
            short(&item.title),
            lifecycle_word(item.lifecycle)
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
pub(super) fn reminder_for_reason(
    reason: &str,
    holder: Holder<'_>,
    blockers: &[String],
    claim_recovery_required: bool,
) -> Option<String> {
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
        "one or more prerequisites are dead and must be removed" => {
            Some("waiting: a dead prerequisite must be removed".into())
        }
        "one or more typed blockers remain active" => Some(if blockers.is_empty() {
            "blocked: one or more blockers remain active".into()
        } else {
            format!("blocked: {}", blockers.join("; "))
        }),
        "open, admitted, unblocked, and unclaimed" => {
            Some("unclaimed: claim it before execution".into())
        }
        "prior claim is recoverable" => claim_recovery_required
            .then(|| "a previous holder's claim lapsed; claiming needs a recovery reason".into()),
        "live claim has checkpointed progress" => match holder {
            Holder::Other(_, _) => Some("held by another session".into()),
            Holder::You(_) | Holder::Nobody => None,
        },
        "live claim has not checkpointed progress" => Some(match holder {
            Holder::You(_) => "you hold this item but have not noted progress yet".into(),
            Holder::Other(_, _) => "held by another session; no progress noted yet".into(),
            Holder::Nobody => "held; no progress noted yet".into(),
        }),
        other => Some(other.to_owned()),
    }
}

/// Fixed table from open typed obligations to words. Waiver authority and
/// identities stay host-private.
pub(super) fn obligation_reminders(page: &WorkObligationPage) -> Vec<String> {
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
/// priority order — followed by `engram work show REF` for the rest. The one
/// planning exception is removing a dead prerequisite that can never
/// satisfy its edge. Other planning edits and entries the agent cannot run
/// through the agent words stay in `allowed_next` on the structured receipt.
pub(super) fn next_commands(
    allowed_next: &[String],
    work_ref: &str,
    word: &str,
    blocked: bool,
    open: bool,
    prerequisites: &[crate::work_service::WorkItemSummary],
) -> Vec<String> {
    let has = |tag: &str| allowed_next.iter().any(|entry| entry == tag);
    let mut out: Vec<String> = Vec::new();
    let mut push = |command: String| {
        if out.len() < NEXT_LIFECYCLE_LIMIT && !out.contains(&command) {
            out.push(command);
        }
    };
    if has("work_update:remove_prerequisite") {
        for prerequisite in prerequisites
            .iter()
            .filter(|prerequisite| {
                prerequisite.prerequisite_state == Some(WorkPrerequisiteState::Dead)
            })
            .take(1)
        {
            push(format!(
                "engram work update {work_ref} --drop-after {}",
                prerequisite.short_ref
            ));
        }
    }
    if has("work_handoff:accept") {
        push(format!("engram work handoff {work_ref} --accept"));
    }
    if has(WORK_UPDATE_CLAIM_ACTION) {
        push(format!("engram work claim {work_ref}"));
    }
    if has(WORK_UPDATE_CLAIM_RECOVERY_ACTION) {
        push(format!("engram work claim {work_ref} --recover \"…\""));
    }
    if has("work_update:checkpoint")
        || has("work_update:evidence")
        || (open && has("work_update:note"))
        || (word == "show" && (has("work_update:note") || has("work_update:gate")))
    {
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

fn project_memory_list_receipt(
    result: &crate::domain::ProjectMemoryList,
    filtered: bool,
) -> Result<Receipt, VerbError> {
    let mut lines = vec![format!("{} project memory item(s):", result.memories.len())];
    for row in &result.memories {
        lines.push(format!(
            "  {} — {} — by {} ({})",
            row.key,
            short(&terminal_safe_multiline(&row.first_line)),
            terminal_safe_actor_label(&row.actor_id, row.actor_context.as_deref()),
            row.remembered_at.format("%Y-%m-%d %H:%M UTC")
        ));
    }
    if result.exhausted {
        lines.push("  (end of project memories)".into());
    }
    let mut guidance = Guidance::default();
    if let Some(after) = &result.next_after {
        guidance
            .next
            .push(format!("engram work memories --after {after}"));
    }
    if filtered && result.omitted_count > 0 {
        guidance.reminders.push(format!(
            "{} more matches were omitted; refine the memory query",
            result.omitted_count
        ));
    }
    Ok(Receipt::assemble(
        lines,
        guidance,
        serde_json::to_value(result)?,
        false,
    ))
}
