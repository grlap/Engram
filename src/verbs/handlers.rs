use crate::domain::normalize_gate_evidence_input;

pub(super) use super::obligation_reminders::obligation_reminders;
use super::{
    Arc, ChildRequirement, DEFAULT_LIMIT, DateTime, Deserialize, Guidance, Holder,
    LocalWorkService, MAX_AGENT_WORK_RESPONSE_BYTES, MAX_COMPACT_CHANGE_ITEMS, MAX_NEXT_PAGES,
    PathBuf, ProjectId, Receipt, Serialize, SessionId, StoreError, Utc, VerbError,
    WORK_UPDATE_CLAIM_ACTION, WORK_UPDATE_CLAIM_RECOVERY_ACTION, WorkAttributionDefaults,
    WorkAvailability, WorkBlockerKind, WorkChildInput, WorkCompleteInput, WorkCompleteResult,
    WorkCompletionCaptureInput, WorkFocusView, WorkHandoffInput, WorkItemKind, WorkLifecycle,
    WorkNextQuery, WorkNextSection, WorkNextView, WorkPrerequisiteState, WorkProposeInput,
    WorkProposeResult, WorkRevisionPatch, WorkUpdateInput, changes_not_delivered, held_suffix,
    item_line, json, lifecycle_word, nonempty,
    receipts::{compact_next_lines, compact_next_receipt, compact_next_value, ready_line},
    section_word, short,
    show::{fit_show_receipt, live, show_lines, show_receipt_value},
    slug, terminal_safe_actor_label, terminal_safe_multiline, trimmed, validate_priority,
};

mod claims;
mod completion_remedy;
mod gates;
mod targets;
mod update;
pub(super) use completion_remedy::{EvaluationRemedy, completion_recovery_reminder};
pub(super) use update::unblock_command;

/// Host context for one agent connection. Authority comes from the host, never
/// from a word's arguments.
#[derive(Clone, Debug)]
pub struct AgentVerbs {
    pub(super) service: Arc<LocalWorkService>,
    pub(super) actor_id: String,
    session_id: SessionId,
    fit_effective_session: Option<SessionId>,
    pub(super) argument_names: ArgumentNames,
}

use super::argument_wording::{self as wording, ArgumentNames};

/// The status `show`, `claim` and `done` give while an item's only criterion
/// is still its title placeholder: what is observed, since the same sentence
/// may have been typed by hand. One wording serves CLI and MCP alike.
pub(super) fn placeholder_acceptance_reminder(placeholder: &str) -> String {
    format!(
        "acceptance is only the title placeholder ('{}'); set real criteria by revising acceptance with update",
        short(placeholder)
    )
}

/// `next`: what is ready, what this session holds, and what changed.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct NextInput {
    pub limit: Option<u32>,
    /// Read orientation without advancing delivery, focus, or memory advertisement.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub peek: bool,
    /// Return the full host-oriented projection instead of compact rows.
    #[serde(default)]
    pub verbose: bool,
    pub context_generation: Option<String>,
}

/// `ls` / `search`: catalog listing with flat filters.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "flat CLI/MCP list switches are validated before catalog translation"
)]
pub struct LsInput {
    pub search: Option<String>,
    pub blocked: bool,
    /// Only ready candidates; inspect an item before claiming it.
    #[serde(default)]
    pub ready: bool,
    /// Assigned to this actor, or held by this session.
    pub mine: bool,
    /// Include completed, cancelled, and superseded items. A ready listing
    /// selects open work only, so this adds nothing to it; a blocked one gains
    /// ended items that still carry an active blocker.
    pub all: bool,
    pub label: Option<String>,
    pub under: Option<String>,
    #[serde(default)]
    pub optional: bool,
    #[serde(default)]
    pub required: bool,
    pub after: Option<String>,
    pub limit: Option<u32>,
    /// Return the full host-oriented projection instead of compact rows.
    #[serde(default)]
    pub verbose: bool,
}

/// `add`: a root, or one required/optional child under `under`.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct AddInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external: Option<String>,
    #[serde(default)]
    pub notes: Vec<String>,
    pub title: String,
    pub outcome: Option<String>,
    pub acceptance: Vec<String>,
    /// `POSITION=KIND[:FINGERPRINT]` bindings of criteria to typed
    /// verification requirements.
    #[serde(default)]
    pub bindings: Vec<String>,
    pub under: Option<String>,
    /// Make the child non-blocking for parent completion. Valid only with
    /// `under`.
    pub optional: bool,
    pub priority: Option<i32>,
    pub labels: Vec<String>,
    pub assignee: Option<String>,
    pub kind: Option<WorkItemKind>,
    /// `same_session`, `sub_agent`, or `independent_session`: pin the
    /// acceptance-evaluation mode from creation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluation_mode: Option<String>,
}

/// `POSITION=KIND[:FINGERPRINT]` bindings as supplied, each read before any
/// effect; the core then checks the positions against the acceptance list.
fn parse_bindings(bindings: &[String]) -> Result<Vec<crate::domain::AcceptanceBinding>, VerbError> {
    bindings
        .iter()
        .map(|text| crate::domain::AcceptanceBinding::parse(text))
        .collect::<Result<Vec<_>, _>>()
        .map_err(|reason| StoreError::InvalidWork(reason).into())
}

/// A supplied acceptance-evaluation mode word, shared by `add` and `update`.
/// `None` is a genuine omission: no pin at creation, the explicit clear on
/// update. A supplied blank is a mistake and refuses, as does an unknown
/// word, before any effect.
fn parse_supplied_evaluation_mode(
    mode: Option<&str>,
) -> Result<Option<crate::domain::AcceptanceEvaluationMode>, VerbError> {
    let Some(word) = mode else {
        return Ok(None);
    };
    let word = word.trim();
    if word.is_empty() {
        return Err(
            StoreError::InvalidWork(wording::BLANK_EVALUATION_MODE_REFUSAL.cli.into()).into(),
        );
    }
    crate::domain::AcceptanceEvaluationMode::parse(word)
        .map(Some)
        .ok_or_else(|| {
            VerbError::from(StoreError::InvalidWork(format!(
                "unknown evaluation mode {word:?}; use same_session, sub_agent, or independent_session"
            )))
        })
}

/// `claim`: hold one item; later words default to it.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ClaimInput {
    pub work_ref: String,
    pub ttl_seconds: Option<i64>,
    /// Attributed reason for taking over a different holder's lapsed claim.
    pub recover: Option<String>,
}

/// `claim --under PARENT`: hold the parent's next ready child.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ClaimUnderInput {
    pub under: String,
    pub ttl_seconds: Option<i64>,
    /// Attributed reason that lets the selection take over a ready child
    /// whose prior claim lapsed under another holder; without it such a
    /// child is passed over.
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
    /// Clear one blocker: the one `blocker` names, as `show` prints its
    /// selector, or, when omitted, the item's only active blocker.
    Unblock {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        blocker: Option<String>,
    },
    /// Any combination of planning fields, applied as one revision.
    Revise {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        external: Option<String>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        clear_external: bool,
        title: Option<String>,
        outcome: Option<String>,
        /// Replace the whole acceptance list; omission leaves it unchanged.
        acceptance: Option<Vec<String>>,
        /// Replace the criteria bound to typed verification requirements, as
        /// `POSITION=KIND[:FINGERPRINT]`; omitted with `acceptance` replaced,
        /// the bindings are cleared.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        bindings: Option<Vec<String>>,
        assignee: Option<String>,
        priority: Option<i32>,
        defer: Option<DateTime<Utc>>,
        kind: Option<WorkItemKind>,
        #[serde(default)]
        labels: Vec<String>,
        #[serde(default)]
        unlabels: Vec<String>,
    },
    /// Pin or clear the acceptance-evaluation mode this task requires.
    EvaluationMode {
        /// `same_session`, `sub_agent`, or `independent_session`; omit to
        /// return the task to the default, independent evaluation unless the
        /// policy admits only same-session. A same-session mark its executor
        /// sets waives nothing.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode: Option<String>,
    },
    Cancel {
        reason: String,
    },
    Reject {
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
    Detach {
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

/// `evaluate`: one attributed acceptance evaluation on the targeted item's
/// active run.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct EvaluateInput {
    #[serde(default)]
    pub work_ref: Option<String>,
    pub mode: String,
    pub acceptance_basis: i64,
    pub evidence_basis: i64,
    pub verdicts: Vec<crate::WorkCriterionVerdictInput>,
    #[serde(default)]
    pub attempt: Option<String>,
    #[serde(default)]
    pub source_fingerprint: Option<String>,
    #[serde(default)]
    pub model: Option<String>,
    #[serde(default)]
    pub execution_identity: Option<String>,
    #[serde(default)]
    pub parent_session: Option<String>,
    /// Record id of the carried failing evaluation this one acknowledges.
    #[serde(default)]
    pub supersedes: Option<String>,
}

/// `remember`: one attributed, immutable project episode.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RememberInput {
    pub text: String,
    pub key: Option<String>,
    #[serde(default)]
    pub revise: bool,
    pub expected_revision: Option<u64>,
    pub retires_with: Option<String>,
    #[serde(default)]
    pub clear_retires_with: bool,
    /// Append the text to the memory as a paragraph instead of replacing it.
    #[serde(default)]
    pub append: bool,
    /// Replace only the interior of this marked section.
    #[serde(default)]
    pub section: Option<String>,
}

/// `memories`: compact list/search or one dedicated full read.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct MemoriesInput {
    pub query: Option<String>,
    pub after: Option<String>,
    #[serde(default)]
    pub full: bool,
    pub revision: Option<u64>,
    /// The host's context generation, as a peek printed it. The first page of
    /// an unfiltered listing records it with the listing; searches, full
    /// reads and continuation pages accept it and record nothing. Without it
    /// no form of `memories` records anything.
    pub context_generation: Option<String>,
}

/// `forget`: permanently retire one project-memory key.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ForgetInput {
    pub key: String,
}

/// Reads `--retires-with local:REF|external:PROJECT#REFERENCE` and
/// `--clear-retires-with` into one target change. The MCP arguments carry the
/// same strings, so both routes refuse the same combinations here.
fn parse_retiring_target(
    value: Option<&str>,
    clear: bool,
) -> Result<crate::domain::ProjectMemoryRetiringTargetChange, VerbError> {
    use crate::domain::{ProjectMemoryRetiringTargetChange, ProjectMemoryRetiringTargetInput};
    if clear {
        return if value.is_none() {
            Ok(ProjectMemoryRetiringTargetChange::Clear)
        } else {
            Err(
                StoreError::InvalidProjectMemory(wording::RETIRES_WITH_COMBINED_REFUSAL.cli.into())
                    .into(),
            )
        };
    }
    let Some(value) = value else {
        return Ok(ProjectMemoryRetiringTargetChange::Keep);
    };
    let target = if let Some(work_ref) = value.strip_prefix("local:") {
        ProjectMemoryRetiringTargetInput::Local {
            work_ref: work_ref.to_owned(),
        }
    } else if let Some(external) = value.strip_prefix("external:") {
        let (project, reference) = external.split_once('#').ok_or_else(|| {
            StoreError::InvalidProjectMemory(
                "external retirement target needs external:PROJECT#REFERENCE".into(),
            )
        })?;
        ProjectMemoryRetiringTargetInput::External {
            project: project.to_owned(),
            reference: reference.to_owned(),
        }
    } else {
        return Err(StoreError::InvalidProjectMemory(
            "retirement target needs local:REF or external:PROJECT#REFERENCE".into(),
        )
        .into());
    };
    Ok(ProjectMemoryRetiringTargetChange::Set { target })
}

/// `note`: one finding on held open work or late evidence on completed work.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct NoteInput {
    #[serde(default)]
    pub status: bool,
    pub work_ref: Option<String>,
    pub text: String,
    pub refs: Vec<String>,
}

/// `done`: complete the held item.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct DoneInput {
    // This shared core DTO is only a transport-neutral positional citation:
    // CLI and MCP must use the same shape and service-owned admission rules.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<crate::work_service::WorkCriterionLinkInput>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_basis: Option<i64>,
    pub work_ref: Option<String>,
    pub summary: Option<String>,
    pub note: Option<String>,
    /// Host-measured source fingerprint at completion time; checked against
    /// the evaluated one when the policy requires source freshness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_fingerprint: Option<String>,
    /// Where the work landed, recorded in the seal as asserted provenance.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub landing: Option<crate::domain::CompletionLanding>,
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
            fit_effective_session: None,
            argument_names: ArgumentNames::Cli,
        }
    }

    /// The MCP server's words name their arguments by MCP field in guidance,
    /// such as `acceptance` where the CLI says `--accept`.
    #[must_use]
    pub(crate) fn with_mcp_argument_names(mut self) -> Self {
        self.argument_names = ArgumentNames::Mcp;
        self
    }

    /// Process-default CLI `--json` mutations attach this session handle
    /// before receipt fitting so the delivered surfaces stay inside budget.
    #[must_use]
    pub fn with_fitted_effective_session(mut self, session_id: SessionId) -> Self {
        self.fit_effective_session = Some(session_id);
        self
    }

    fn finish_mutation(&self, receipt: Receipt) -> Receipt {
        match &self.fit_effective_session {
            Some(session) => receipt.with_effective_session_id(session),
            None => receipt,
        }
    }

    /// `next`: focus, ready candidates, and the changes since the last call.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] when the core cannot read or stage the view.
    pub fn next(&self, input: &NextInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        self.next_with_verbose_budget(input, now, MAX_AGENT_WORK_RESPONSE_BYTES)
    }

    // Production always uses the protocol budget. Tests may inject a tighter
    // verbose overlay ceiling; compact `next` ignores this parameter.
    #[allow(
        clippy::too_many_lines,
        reason = "focus, held, ready, changes, and guidance are assembled in one readable pass"
    )]
    pub(super) fn next_with_verbose_budget(
        &self,
        input: &NextInput,
        now: DateTime<Utc>,
        budget: usize,
    ) -> Result<Receipt, VerbError> {
        let limit = input.limit.unwrap_or(DEFAULT_LIMIT);
        let ready_limit = if input.verbose {
            limit
        } else {
            limit.clamp(1, super::MAX_NEXT_READY_CANDIDATES)
        };
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
                WorkNextSection::Assigned,
                WorkNextSection::Participated,
            ],
            context_generation: input.context_generation.clone(),
            ..WorkNextQuery::default()
        };
        let mut view = if input.peek {
            self.service.work_next_peek_for_agent(
                change_limit,
                ready_limit,
                input.verbose,
                query,
                now,
                |changes| {
                    !super::collapsed_changes(changes, self.service.display_identity()).is_empty()
                },
            )?
        } else {
            self.service.work_next_for_agent(
                change_limit,
                ready_limit,
                input.verbose,
                query,
                now,
            )?
        };
        view.backup_reminder = self.service.backup_reminder();
        let lists = view.agent_lists.take().ok_or_else(|| {
            StoreError::InvalidWorkProjection("agent next has no advisory list snapshot".into())
        })?;
        let mut held = lists.held;
        let mut ready = lists.ready;
        let mut compact_changes = super::collapsed_changes(
            view.changes.as_deref().unwrap_or_default(),
            self.service.display_identity(),
        );
        let mut not_delivered = changes_not_delivered(&view);
        // Compact output may drain own-session-only pages within its bound.
        // Verbose output exposes the original exact page and its cursor, so
        // it must not acknowledge additional pages behind that receipt.
        let mut pages = 1;
        while !input.verbose
            && !input.peek
            && compact_changes.is_empty()
            && not_delivered > 0
            && pages < MAX_NEXT_PAGES
        {
            let more = self.service.work_next(
                change_limit,
                WorkNextQuery {
                    sections: vec![WorkNextSection::Changes],
                    ..WorkNextQuery::default()
                },
                now,
            )?;
            compact_changes = super::collapsed_changes(
                more.changes.as_deref().unwrap_or_default(),
                self.service.display_identity(),
            );
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
        if let Some(row) = view
            .discovery
            .assigned
            .first()
            .or_else(|| view.discovery.participated.first())
        {
            let command = format!("engram work show {}", row.work_ref);
            if !guidance.next.contains(&command) {
                guidance.next.push(command);
            }
        }
        if guidance.next.is_empty() {
            guidance.next.push("engram work add \"…\"".into());
        }
        if input.peek {
            guidance.next.insert(
                0,
                super::memory_recovery::listing_command(
                    view.peek.as_ref(),
                    view.context_generation.as_deref(),
                ),
            );
        }
        let (lines, value, guidance) = if input.verbose {
            // The direction to list memories stays first among the reminders
            // and is never shed; its command is the first next command, which
            // the loop below keeps. The backup reminder follows it and is
            // never shed either.
            let recovery = super::memory_recovery::reminder(view.peek.as_ref());
            if let Some(recovery) = &recovery {
                guidance.reminders.insert(0, recovery.clone());
            }
            let mut kept_reminders = usize::from(recovery.is_some());
            if let Some(backup) = &view.backup_reminder {
                guidance.reminders.insert(kept_reminders, backup.clone());
                kept_reminders += 1;
            }
            let mut peek_omissions: Vec<super::receipts::CompactSectionOmission> = Vec::new();
            let mut agent_omissions: Vec<super::receipts::CompactSectionOmission> = Vec::new();
            let mut evaluation_obligations = view.focus.as_ref().and_then(|focus| {
                focus
                    .evaluated_policy
                    .then(|| {
                        super::evaluation_guidance::EvaluationObligations::from_page(
                            &focus.obligation_page,
                            focus.evidence_basis,
                            focus.evaluation_obligation_rows_visible,
                        )
                    })
                    .flatten()
            });
            if evaluation_obligations.is_some() {
                guidance.reminders.retain(|reminder| {
                    !reminder.starts_with("open obligations: ")
                        && !reminder.starts_with("open obligation ")
                        && !reminder.starts_with("resolve obligations needing action, ")
                });
            }
            loop {
                let changes = compact_changes
                    .iter()
                    .map(|change| change.line.clone())
                    .collect::<Vec<_>>();
                let mut lines = super::memory_recovery::opening_lines(
                    view.peek.as_ref(),
                    view.context_generation.as_deref(),
                );
                match &view.focus {
                    Some(focus) => {
                        lines.push(format!(
                            "focus: {}",
                            item_line(&focus.status, self.holder(focus, now), now)
                        ));
                        if let Some(status) = &focus.acceptance_evaluation {
                            lines.push(format!(
                                "  evaluation: {}",
                                super::show::evaluation_summary(status)
                            ));
                        }
                    }
                    None if peek_omissions
                        .iter()
                        .any(|omission| omission.section == "focus")
                        || agent_omissions
                            .iter()
                            .any(|omission| omission.section == "focus") =>
                    {
                        lines.push("focus: omitted (byte budget)".into());
                    }
                    None => lines.push("focus: none".into()),
                }
                if let Some(advisory) = &evaluation_obligations {
                    lines.extend(advisory.reminder_lines());
                    if advisory.requires_action() {
                        lines.push("resolve obligations needing action, then request a fresh acceptance evaluation before done".into());
                    }
                }
                lines.push(format!("held by you ({}):", held.len()));
                for (item, expires_at) in &held {
                    lines.push(format!(
                        "  {}",
                        item_line(item, Holder::You(*expires_at), now)
                    ));
                }
                super::receipts::append_discovery_lines(&mut lines, &view.discovery);
                lines.push(format!("ready ({}):", ready.len()));
                for item in &ready {
                    lines.push(format!("  {}", ready_line(item)));
                }
                super::receipts::append_next_changes_lines(
                    &mut lines,
                    &changes,
                    not_delivered,
                    view.peek.as_ref(),
                );
                if let Some(memories) = &view.memories {
                    lines.push(format!(
                        "memories: {} retained{}",
                        memories.count,
                        if memories.changed { " (changed)" } else { "" }
                    ));
                }
                if input.peek {
                    super::receipts::append_peek_disclosure(
                        &mut lines,
                        view.peek.as_ref(),
                        view.context_generation.as_deref(),
                    );
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
                if let Some(advisory) = &evaluation_obligations {
                    value["evaluation_obligations"] = json!(advisory);
                }
                if input.peek {
                    value["memories_detail"] = json!(super::memory_recovery::listing_command(
                        view.peek.as_ref(),
                        view.context_generation.as_deref(),
                    ));
                    value["preview_omissions"] = json!(peek_omissions);
                    for omission in &peek_omissions {
                        lines.push(format!(
                            "  ({} {} rows omitted from this preview)",
                            omission.omitted_count, omission.section
                        ));
                    }
                } else if !agent_omissions.is_empty() {
                    value["agent_omissions"] = json!(agent_omissions);
                    for omission in &agent_omissions {
                        lines.push(format!(
                            "  ({} {} omitted to fit this receipt)",
                            omission.omitted_count, omission.section
                        ));
                    }
                }
                value["ready"] = serde_json::to_value(&ready)?;
                value["changes_by_others"] = json!(changes);
                value["held"] = serde_json::to_value(
                    held.iter()
                        .map(|(item, expires_at)| {
                            let mut row = json!({ "work": item.work, "expires_at": expires_at });
                            if let Some(status) = &item.work.current_status {
                                row["current_status"] = json!(status);
                            }
                            if let Some(peer) = &item.work.status_observation {
                                row["status_observation"] = json!(peer);
                            }
                            row
                        })
                        .collect::<Vec<_>>(),
                )?;
                let receipt =
                    Receipt::assemble(lines.clone(), guidance.clone(), value.clone(), false)
                        .with_build_identity(&view.read_cut, view.context_generation.as_deref());
                if super::receipts::agent_receipt_fits(&receipt, budget)? {
                    break (lines, value, guidance.clone());
                }
                if view.discovery.shorten_status_previews()
                    || held.iter_mut().rev().any(|(item, _)| {
                        crate::work_service::shorten_status_previews(
                            &mut item.work.current_status,
                            &mut item.work.status_observation,
                        )
                    })
                    || crate::work_service::shed_work_next_focus(&mut view)
                {
                    if view.focus.is_none() {
                        evaluation_obligations = None;
                    }
                    continue;
                }
                // Non-peek verbose delivery keeps the exact staged page. Peek
                // may omit raw rows from the preview only; it never acknowledges
                // them. Progress is monotone in remaining raw row count, not
                // rendered bytes: removing completion can reveal its longer
                // checkpoint. Re-collapse and keep shedding until it fits.
                let section = if view.discovery.shed_one() {
                    continue;
                } else if input.peek
                    && view
                        .changes
                        .as_mut()
                        .is_some_and(|rows| rows.pop().is_some())
                {
                    compact_changes = super::collapsed_changes(
                        view.changes.as_deref().unwrap_or_default(),
                        self.service.display_identity(),
                    );
                    if let Some(peek) = &mut view.peek {
                        peek.more_changes_available = true;
                    }
                    "changes"
                } else if ready.pop().is_some() {
                    "ready"
                } else if held.pop().is_some() {
                    "held"
                } else if evaluation_obligations
                    .as_mut()
                    .is_some_and(super::evaluation_guidance::EvaluationObligations::omit_one)
                {
                    continue;
                } else if guidance.reminders.len() > kept_reminders {
                    guidance.reminders.pop();
                    "reminders"
                } else if guidance.next.len() > 1 {
                    guidance.next.pop();
                    "next"
                } else if view.focus.take().is_some() {
                    evaluation_obligations = None;
                    "focus"
                } else {
                    break (lines, value, guidance.clone());
                };
                if input.peek {
                    super::receipts::record_compact_omission(&mut peek_omissions, section, 1);
                } else if section == "ready" {
                    record_verbose_next_omission(&mut view.omissions, 1);
                } else {
                    // Whole-focus removal is not a trim-step Focus count.
                    super::receipts::record_compact_omission(&mut agent_omissions, section, 1);
                }
            }
        } else {
            let claims = lists
                .claims
                .into_iter()
                .map(|(id, holder, expiry)| {
                    (
                        id,
                        (self.service.display_identity().session(&holder), expiry),
                    )
                })
                .collect();
            let compact = compact_next_receipt(
                &view,
                &held,
                &ready,
                &compact_changes,
                &claims,
                &guidance,
                lists.ready_navigation,
            )?;
            let lines = compact_next_lines(&compact);
            let value = compact_next_value(&compact);
            (lines, value, compact.guidance)
        };
        if !input.peek
            && value
                .get("memories")
                .is_some_and(|memories| !memories.is_null())
        {
            self.service.acknowledge_work_next_memories(&view, now);
        }
        Ok(Receipt::assemble(lines, guidance, value, false)
            .with_build_identity(&view.read_cut, view.context_generation.as_deref()))
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
        input.validate_listing()?;
        let command = input.list_command();
        let page = self
            .service
            .work_catalog_page(
                &crate::domain::WorkCatalogQuery {
                    search: input.search.clone(),
                    lifecycles: if input.all {
                        Vec::new()
                    } else {
                        vec![WorkLifecycle::Open]
                    },
                    blocked_only: input.blocked,
                    availabilities: if input.ready {
                        vec![WorkAvailability::Ready]
                    } else {
                        Vec::new()
                    },
                    ready_priority_order: input.ready,
                    assigned_to: input.mine.then(|| self.actor_id.clone()),
                    held_by: input.mine.then(|| self.session_id.clone()),
                    label: input.label.clone(),
                    child_requirement: if input.optional {
                        Some(ChildRequirement::Optional)
                    } else if input.required {
                        Some(ChildRequirement::Required)
                    } else {
                        None
                    },
                    limit,
                    ..crate::domain::WorkCatalogQuery::default()
                },
                input.under.as_deref(),
                input.after.as_deref(),
                now,
            )
            .map_err(|error| VerbError::for_listing(error, &command))?;
        super::listing::fit_list_receipt(input, &page, self.service.display_identity(), budget)
            .map_err(|error| VerbError::for_listing(error.error, &command))
    }

    /// `show`: one item in agent detail without changing ambient focus or
    /// claiming. Host authority and integrity fields remain on `work core
    /// focus`.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] when the reference is unknown.
    pub fn show(&self, work_ref: &str, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        let view = self
            .service
            .work_focus_for_agent(work_ref, now)
            .map_err(|error| VerbError::at(error, work_ref))?;
        fit_show_receipt(
            view,
            |view| self.render_show(view, now),
            MAX_AGENT_WORK_RESPONSE_BYTES,
        )
    }

    pub(super) fn render_show(
        &self,
        view: &WorkFocusView,
        now: DateTime<Utc>,
    ) -> Result<Receipt, VerbError> {
        if view.status.work.parent_id.is_some() && view.parent.is_none() {
            return Err(StoreError::InvalidWorkProjection(
                "safe show is missing direct parent context for a child".into(),
            )
            .into());
        }
        let holder = self.holder(view, now);
        let lines = show_lines(view, holder, self.service.display_identity(), now);
        let mut guidance = self.guidance(view, "show", now);
        if let Some(parent) = &view.parent {
            // Keep actionable recovery first; parent navigation precedes optional history.
            let command = format!("engram work show {}", parent.short_ref);
            if !guidance.next.contains(&command) {
                guidance.next.push(command);
            }
        }
        if view.history.total + view.restored_history.total > 0 {
            guidance.next.push(format!(
                "engram work show {} --history",
                view.status.work.short_ref
            ));
        }
        if super::mutation::needs_full_contract(view) {
            let command = super::mutation::full_contract(&view.status.work.short_ref);
            if !guidance.next.contains(&command) {
                guidance.next.push(command);
            }
        }
        Ok(Receipt::assemble(
            lines,
            guidance,
            serde_json::to_value(show_receipt_value(
                view,
                holder,
                self.service.display_identity(),
                now,
            ))?,
            false,
        ))
    }

    /// Like `show`, optionally returning the newest complete-note window.
    ///
    /// # Errors
    /// Returns [`VerbError`] for unknown work, invalid notes or oversized metadata.
    pub fn show_with_notes(
        &self,
        work_ref: &str,
        notes: bool,
        now: DateTime<Utc>,
    ) -> Result<Receipt, VerbError> {
        self.show_records(
            work_ref,
            &super::ShowInput {
                notes,
                ..super::ShowInput::default()
            },
            now,
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
            wording::DEFAULTED_ACCEPTANCE_REMINDER
                .spelled(self.argument_names)
                .to_owned()
        });
        let has_initial_notes = !input.notes.is_empty();
        let mut receipt = self.finish_mutation(self.add_inner(input, now)?);
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
            return Err(
                StoreError::InvalidWork(wording::OPTIONAL_NEEDS_PARENT_REFUSAL.cli.into()).into(),
            );
        }
        let evaluation_mode = parse_supplied_evaluation_mode(input.evaluation_mode.as_deref())?;
        let acceptance_bindings = parse_bindings(&input.bindings)?;
        if let Some(under) = input.under.as_deref() {
            let requirement = if input.optional {
                ChildRequirement::Optional
            } else {
                ChildRequirement::Required
            };
            return self.add_child(
                under,
                WorkChildInput {
                    external_ref: input.external,
                    notes: input.notes,
                    key: slug(&title),
                    title,
                    outcome,
                    acceptance,
                    acceptance_bindings,
                    requirement: Some(requirement),
                    kind: input.kind,
                    priority,
                    labels,
                    assigned_to,
                    deferred_until: None,
                    evaluation_mode,
                },
                now,
            );
        }
        let result = self.service.work_propose(
            WorkProposeInput::Root {
                notes: input.notes,
                external_ref: input.external,
                title,
                outcome,
                acceptance,
                acceptance_bindings,
                work_kind: input.kind,
                priority,
                labels,
                assigned_to,
                deferred_until: None,
                evaluation_mode,
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
        super::mutation::receipt(
            focus,
            "add",
            json!({"kind": "root"}),
            lines,
            guidance,
            self.holder(focus, now),
            false,
        )
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
        let peer_proposal = focus.status.work.child_requirement == ChildRequirement::Optional
            && matches!(self.holder(&parent, now), Holder::Other(..));
        let mut guidance = self.guidance(&focus, "add", now);
        if peer_proposal {
            guidance.next = vec![
                format!("engram work show {child_ref}"),
                format!("engram work show {parent_ref}"),
            ];
            guidance
                .reminders
                .retain(|reminder| reminder != "unclaimed: claim it before execution");
            let inspect = "inspect child and parent".to_owned();
            if !guidance.reminders.contains(&inspect) {
                guidance.reminders.push(inspect);
            }
        }
        let value = json!({"kind": "child", "parent_ref": parent_ref,
            "child_requirement": focus.status.work.child_requirement,
            "details_omitted": summary.details_omitted});
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
        let mut receipt = super::mutation::receipt(
            &focus,
            "add",
            value,
            lines,
            guidance,
            self.holder(&focus, now),
            false,
        )?;
        if peer_proposal {
            // `mutation::receipt` drops `show CHILD` in favor of full_detail;
            // a peer proposal still needs both inspect commands in `next`.
            let next = vec![
                format!("engram work show {child_ref}"),
                format!("engram work show {parent_ref}"),
            ];
            receipt.next.clone_from(&next);
            receipt.value["next"] = json!(next);
        }
        Ok(receipt)
    }

    /// `remember`: create one attributed project episode.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] when authorization, key, size, redaction, or
    /// revision-basis or terminal lifecycle admission fails.
    pub fn remember(&self, input: RememberInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        let retiring_target =
            parse_retiring_target(input.retires_with.as_deref(), input.clear_retires_with)?;
        let edit = super::memory_change::edit(input.append, input.section.as_deref())?;
        let receipt = self.service.revise_project_memory(
            input.text,
            input.key,
            input.revise,
            input.expected_revision,
            retiring_target,
            &edit,
            now,
        )?;
        let (changed, reads) = super::memory_change::lines(&receipt);
        let mut guidance = Guidance {
            reminders: Vec::new(),
            next: vec![format!("engram work memories {} --full", receipt.key)],
        };
        guidance.next.extend(reads);
        guidance.next.push("engram work memories".into());
        let replay = if receipt.duplicate { " (replayed)" } else { "" };
        let line = match receipt.replaced_revision {
            Some(previous) => format!(
                "revised project memory {}: revision {previous} → {}{replay}",
                receipt.key, receipt.revision
            ),
            None => format!(
                "remembered project memory {} (revision {}){replay}",
                receipt.key, receipt.revision
            ),
        };
        let lines = std::iter::once(line).chain(changed).collect();
        Ok(self.finish_mutation(Receipt::assemble(
            lines,
            guidance,
            serde_json::to_value(receipt)?,
            false,
        )))
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
        if input.revision.is_some() && !input.full {
            return Err(StoreError::InvalidProjectMemory(
                wording::REVISION_NEEDS_FULL_REFUSAL.cli.into(),
            )
            .into());
        }
        crate::storage::validate_context_generation(input.context_generation.as_deref())
            .map_err(|error| VerbError::from(error).retrying("memories"))?;
        if input.full {
            if input.after.is_some() {
                return Err(StoreError::InvalidProjectMemory(
                    wording::FULL_WITH_AFTER_REFUSAL.cli.into(),
                )
                .into());
            }
            let key = input.query.as_deref().ok_or_else(|| {
                StoreError::InvalidProjectMemory(wording::FULL_NEEDS_KEY_REFUSAL.cli.into())
            })?;
            let mut envelope = self.service.project_memory_full(key, input.revision, now)?;
            // The reminder that names the arguments keeping or clearing a
            // dropped retirement target names them as this caller passes them.
            if let Some(dropped) = &envelope.memory.retiring_target_dropped {
                let cli = crate::work_service::retiring_target_dropped_reminder(
                    dropped,
                    ArgumentNames::Cli,
                );
                let spelled = crate::work_service::retiring_target_dropped_reminder(
                    dropped,
                    self.argument_names,
                );
                for reminder in &mut envelope.reminders {
                    if *reminder == cli {
                        reminder.clone_from(&spelled);
                    }
                }
            }
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
        // Only the start of the unfiltered listing, carrying the generation a
        // peek printed, is the recovery read that peek asks for, and only once
        // it has been rendered. Every other form stays a read that records
        // nothing and reads no recorded position.
        let records = input.context_generation.is_some() && !filtered && input.after.is_none();
        let (mut result, listing) = self.service.project_memories_at_cut(
            input.query.as_deref(),
            input.after.as_deref(),
            records,
            now,
        )?;
        loop {
            let receipt = project_memory_list_receipt(&result, filtered)?;
            if super::receipts::agent_receipt_fits(&receipt, MAX_AGENT_WORK_RESPONSE_BYTES)? {
                if let (Some(generation), Some(listing)) = (&input.context_generation, listing) {
                    self.service
                        .acknowledge_project_memory_listing(listing, generation, now);
                }
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
        Ok(self.finish_mutation(Receipt::assemble(
            lines,
            Guidance {
                reminders: vec!["forget is a tombstone, not erasure".into()],
                next: vec!["engram work memories".into()],
            },
            serde_json::to_value(receipt)?,
            false,
        )))
    }

    /// `note`: record an attributed observation; only a live holder also
    /// checkpoints its execution. Non-holders need no claim on open work.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] for empty text, invalid project/lifecycle binding,
    /// or a stale holder authority basis.
    pub fn note(&self, input: &NoteInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        self.disclosing_focus(|| self.note_word(input, now))
    }

    fn note_word(&self, input: &NoteInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        let text = input.text.trim().to_owned();
        if text.is_empty() {
            return Err(StoreError::InvalidWork("note text must not be empty".into()).into());
        }
        let view = self.target_unfocused("note", input.work_ref.as_deref(), now)?;
        let work_ref = view.status.work.short_ref.clone();
        let target = view.status.work.work_id.0.to_string();
        let refs = trimmed(&input.refs);
        let result = if input.status {
            self.service
                .work_note_with_status_on(Some(&target), &text, &refs, true, now)
        } else {
            self.service.work_note_on(Some(&target), &text, &refs, now)
        }
        .map_err(|error| VerbError::at(error, &work_ref))?;
        let after = self.refreshed(&view, now)?;
        let mut guidance = self.guidance(&after, "note", now);
        // Chosen from the recorded result, not from who holds the item now:
        // a claim or a replay after the note cannot turn it into execution.
        if result.non_holder && after.status.work.lifecycle == WorkLifecycle::Open {
            observation_guidance(&mut guidance, &work_ref);
        }
        let value = super::mutation::NoteResult::from(&result);
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
        Ok(self.finish_mutation(super::mutation::receipt(
            &after,
            "note",
            value,
            lines,
            guidance,
            self.holder(&after, now),
            false,
        )?))
    }

    /// `done`: complete the held item and disclose absent criterion evidence
    /// links from its frozen seal; a typed refusal says what is owed.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] when this session does not hold the item, nothing
    /// has been noted, or a lifecycle fence moved.
    pub fn done(&self, input: DoneInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        self.disclosing_focus(|| self.done_word(input, now))
    }

    fn done_word(&self, input: DoneInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        let view = self.target("done", input.work_ref.as_deref(), now)?;
        let work_ref = view.status.work.short_ref.clone();
        let title = short(&view.status.work.title);
        let target = view.status.work.work_id.0.to_string();
        let result = self
            .service
            .work_complete_on(
                Some(&target),
                WorkCompleteInput {
                    source_fingerprint: nonempty(input.source_fingerprint),
                    landing: input.landing,
                    links: input.links,
                    link_basis: input.link_basis,
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
        let retirement_candidates = matches!(result, WorkCompleteResult::Completed(_)).then(|| {
            self.service
                .project_memory_retirement_candidates(view.status.work.work_id, now)
        });
        let after = self.refreshed(&view, now)?;
        let child_resolution = match &result {
            WorkCompleteResult::Refused(refusal) => {
                super::child_obligations::ShowChildSuccessor::for_refusal(refusal)
            }
            WorkCompleteResult::Completed(_) => None,
        };
        let (lines, guidance, owed) = match &result {
            WorkCompleteResult::Completed(completed) => {
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
                let mut lines = vec![
                    format!("done {work_ref} \"{title}\""),
                    format!(
                        "asserted {} acceptance {} satisfied; completion changed no criterion",
                        completed.acceptance_criteria_asserted,
                        if completed.acceptance_criteria_asserted == 1 {
                            "criterion"
                        } else {
                            "criteria"
                        }
                    ),
                    super::show::acceptance_provenance_line(
                        completed.acceptance_provenance.as_ref(),
                        self.service.display_identity(),
                    ),
                ];
                if completed.landing.is_some() || completed.landing_unavailable.is_some() {
                    lines.push(super::show::landing_line(
                        completed.landing.as_ref(),
                        completed.landing_unavailable,
                    ));
                }
                lines.extend(super::show::untested_change_lines(
                    &completed.obligation_page,
                ));
                lines.extend(super::show::displaced_change_lines(
                    &completed.obligation_page,
                ));
                (lines, guidance, false)
            }
            WorkCompleteResult::Refused(refusal) => {
                let mut guidance = self.guidance(&after, "done", now);
                // Which remedy a missing evaluation, or one the policy, the
                // mark or the evaluator's identity no longer admits, has
                // depends on the task's mark and what the project admits;
                // only those causes read the policy.
                let evaluation = if matches!(
                    refusal.recovery.cause,
                    crate::WorkCompletionRecoveryCause::MissingAcceptanceEvaluation { .. }
                        | crate::WorkCompletionRecoveryCause::AcceptanceEvaluationStale {
                            reason: crate::AcceptanceStaleReason::Policy
                                | crate::AcceptanceStaleReason::Identity
                        }
                ) {
                    EvaluationRemedy {
                        mark: (refusal.recovery.item.work_id == after.status.work.work_id)
                            .then_some(after.status.work.evaluation_mode)
                            .flatten(),
                        admitted: self
                            .service
                            .acceptance_evaluation_policy(now)
                            .map_err(|error| VerbError::at(error, &work_ref))?
                            .allowed_modes,
                    }
                } else {
                    EvaluationRemedy::default()
                };
                let mut reminder = completion_recovery_reminder(
                    &refusal.recovery,
                    refusal.recovery.item.work_id != after.status.work.work_id,
                    &evaluation,
                    self.argument_names,
                );
                if let Some(resolution) = &child_resolution {
                    reminder.push_str("; ");
                    reminder.push_str(&resolution.line());
                }
                guidance.reminders.push(reminder);
                guidance.next = vec![refusal.recovery.command.clone()];
                // The check an open obligation waits for follows the cause's
                // unchanged words as its own reminder, with its detail to read.
                if let (
                    Some(check),
                    crate::WorkCompletionRecoveryCause::OpenObligation { required_check, .. },
                ) = (
                    refusal.recovery.open_obligation_check.as_deref(),
                    &refusal.recovery.cause,
                ) {
                    guidance.reminders.push(
                        super::verification_assessment::open_obligation_check_line(
                            check,
                            *required_check,
                        ),
                    );
                    if let crate::domain::OpenObligationCheck::Newest { verification, .. } = check {
                        guidance.next.push(format!(
                            "engram work show {} --note {}",
                            refusal.recovery.item.short_ref,
                            verification.as_str()
                        ));
                    }
                }
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
        let value = match &result {
            WorkCompleteResult::Refused(refusal) => super::child_obligations::done_refusal_value(
                refusal,
                child_resolution,
                &self.service.display_identity(),
            )?,
            WorkCompleteResult::Completed(receipt) => {
                let mut value = json!({
                    "seal": receipt.seal,
                    "completed_at": receipt.completed_at,
                    "acceptance_criteria_asserted": receipt.acceptance_criteria_asserted,
                    "acceptance_criteria_changed": false,
                    "acceptance": super::show::acceptance_provenance_value(
                        receipt.acceptance_provenance.as_ref(),
                        self.service.display_identity(),
                    ),
                });
                if receipt.landing.is_some() || receipt.landing_unavailable.is_some() {
                    value["landing"] = super::show::landing_value(
                        receipt.landing.as_ref(),
                        receipt.landing_unavailable,
                    );
                }
                let untested = super::show::untested_changes(&receipt.obligation_page);
                if !untested.is_empty() {
                    value["untested_changes"] = serde_json::to_value(untested)?;
                }
                let omitted = super::show::untested_changes_omitted(&receipt.obligation_page);
                if omitted > 0 {
                    value["untested_changes_omitted"] = json!(omitted);
                }
                let displaced = super::show::displaced_changes(&receipt.obligation_page);
                if !displaced.is_empty() {
                    value["foreign_workspace_changes"] = serde_json::to_value(displaced)?;
                }
                let omitted = super::show::displaced_changes_omitted(&receipt.obligation_page);
                if omitted > 0 {
                    value["foreign_workspace_changes_omitted"] = json!(omitted);
                }
                value
            }
        };
        let receipt = self.finish_mutation(super::mutation::receipt(
            &after,
            "done",
            value,
            lines,
            guidance,
            self.holder(&after, now),
            owed,
        )?);
        if !owed {
            let children = self.service.remaining_optional_children(
                view.status.work.work_id,
                super::child_obligations::MAX_CHILD_OBLIGATION_REFS,
                now,
            );
            let facts = match &result {
                WorkCompleteResult::Completed(completed) => completed.acceptance_evidence.as_ref(),
                WorkCompleteResult::Refused(_) => None,
            };
            let error_class = match &result {
                WorkCompleteResult::Completed(completed) => {
                    completed.acceptance_evidence_error_class
                }
                WorkCompleteResult::Refused(_) => None,
            };
            let action = super::memory_retirement::RetirementAction::Completed;
            let reserve = retirement_candidates.as_ref().map_or(Ok(0), |candidates| {
                super::memory_retirement::reserve(
                    &receipt,
                    candidates,
                    &action,
                    self.argument_names,
                )
            })?;
            let composed = super::child_obligations::done_with_acceptance(
                &receipt,
                facts,
                error_class,
                &children,
                &work_ref,
                super::FOCUS_DISCLOSED_BUDGET.saturating_sub(reserve),
            )?;
            return match retirement_candidates.as_ref() {
                Some(candidates) => super::memory_retirement::append(
                    &composed,
                    candidates,
                    &action,
                    super::FOCUS_DISCLOSED_BUDGET,
                    self.argument_names,
                ),
                None => Ok(composed),
            };
        }
        Ok(receipt)
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
        // Refuse display labels before target binding can write focus or an
        // offer. This is a usability guard, not identity resolution or trust.
        if let HandoffAction::Offer { to, .. } = &input.action {
            let forwarded = to.trim();
            crate::storage::admit_session_id_text(forwarded)?;
            if crate::work_service::identity::is_display_label(forwarded) {
                return Err(StoreError::InvalidWork(
                    super::attribution::HANDOFF_DISPLAY_TARGET_REFUSAL.into(),
                )
                .into());
            }
        }
        // A bare handoff keeps the ambient focus without the implicit-target
        // guard: its recipient accepts an item it does not hold yet, and an
        // offer or a cancel already needs the claim, so it cannot act on an
        // item the caller did not mean.
        let view = match input.work_ref.as_deref() {
            Some(work_ref) => self.target("handoff", Some(work_ref), now)?,
            None => self.ambient_focus(now)?,
        };
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
                            .unwrap_or_else(|| "handoff offered".into()),
                        to: to.clone(),
                        ttl_seconds,
                        idempotency_key: String::new(),
                    },
                    // Raw targets no longer reach this compact surface. The
                    // hostile-target framing test covers rich verbose output.
                    format!(
                        "offered {work_ref} \"{title}\" to {}",
                        self.service
                            .display_identity()
                            .session(&SessionId(to.clone()))
                    ),
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
        Ok(self.finish_mutation(Receipt::assemble(
            vec![line],
            guidance,
            serde_json::to_value(&result)?,
            false,
        )))
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

    fn holder<'a>(&'a self, view: &'a WorkFocusView, now: DateTime<Utc>) -> Holder<'a> {
        match &view.claim {
            Some(claim) if live(claim, now) => {
                if claim.holder == self.session_id {
                    Holder::You(claim.expires_at)
                } else {
                    Holder::Other(
                        &claim.holder,
                        claim.expires_at,
                        self.service.display_identity(),
                    )
                }
            }
            _ => Holder::Nobody,
        }
    }

    fn guidance(&self, view: &WorkFocusView, word: &str, now: DateTime<Utc>) -> Guidance {
        let holder = self.holder(view, now);
        let evaluation_obligations =
            if word != "evaluate" && view.evaluated_policy && matches!(holder, Holder::You(_)) {
                super::evaluation_guidance::EvaluationObligations::from_page(
                    &view.obligation_page,
                    view.evidence_basis,
                    view.evaluation_obligation_rows_visible,
                )
            } else {
                None
            };
        let action_required = evaluation_obligations.as_ref().is_some_and(|_| {
            super::evaluation_guidance::EvaluationObligations::from_page(
                &view.obligation_page,
                view.evidence_basis,
                usize::MAX,
            )
            .is_some_and(|advisory| advisory.requires_action())
        });
        let fresh_pass = view.acceptance_evaluation.as_ref().is_some_and(|status| {
            status.stale.is_none()
                && status
                    .record
                    .verdicts
                    .iter()
                    .all(|verdict| verdict.verdict == crate::AcceptanceVerdict::Pass)
        });
        let claim_recovery_required = view
            .allowed_next
            .iter()
            .any(|action| action == WORK_UPDATE_CLAIM_RECOVERY_ACTION);
        let blockers = view
            .blockers
            .iter()
            .map(|blocker| short(&blocker.detail))
            .collect::<Vec<_>>();
        // First, so a fitter that sheds reminders from the end never reaches it.
        let mut reminders = view
            .acceptance_placeholder
            .as_deref()
            .filter(|_| matches!(word, "show" | "claim" | "done"))
            .map(placeholder_acceptance_reminder)
            .into_iter()
            .collect::<Vec<_>>();
        if let Some(advisory) = &evaluation_obligations
            && word != "show"
        {
            reminders.extend(advisory.reminder_lines());
            if action_required {
                reminders.push("resolve obligations needing action, then request a fresh acceptance evaluation before done".into());
            } else if fresh_pass {
                reminders.push(
                    "fresh passing evaluation recorded; done may handle the no-action obligations"
                        .into(),
                );
            } else {
                reminders
                    .push("request acceptance evaluation, then complete on a fresh pass".into());
            }
        }
        let parent_reminder = view
            .status
            .blocking_ancestor
            .as_ref()
            .map(super::receipts::ancestor_reminder);
        if let Some(words) = &parent_reminder {
            reminders.push(words.clone());
        }
        for reason in &view.status.why {
            if parent_reminder.as_deref() == Some(reason.trim()) {
                continue;
            }
            if let Some(words) =
                reminder_for_reason(reason, holder, &blockers, claim_recovery_required)
                && !reminders.contains(&words)
            {
                reminders.push(words);
            }
        }
        if matches!(word, "show" | "gate") && matches!(holder, Holder::You(_)) {
            let unlinked = super::acceptance::UnlinkedCriteria::from_view(view);
            reminders.extend(unlinked.and_then(|unlinked| unlinked.reminder()));
        }
        if evaluation_obligations.is_none() && !(word == "evaluate" && view.evaluated_policy) {
            for words in obligation_reminders(&view.obligation_page) {
                if !reminders.contains(&words) {
                    reminders.push(words);
                }
            }
        }
        let unblock = update::unblock_guidance(view, word);
        let mut next = next_commands(
            &view.allowed_next,
            &view.status.work.short_ref,
            word,
            unblock.as_deref(),
            view.status.work.lifecycle == WorkLifecycle::Open,
            &view.prerequisites,
        );
        if evaluation_obligations.is_some() {
            if action_required {
                next.retain(|command| !command.starts_with("engram work done "));
            }
            if action_required || !fresh_pass {
                let evidence_read = format!(
                    "engram work show {} --notes --gates",
                    view.status.work.short_ref
                );
                if !next.contains(&evidence_read) {
                    let before_done = next
                        .iter()
                        .position(|command| command.starts_with("engram work done "))
                        .unwrap_or(next.len());
                    next.insert(before_done, evidence_read);
                }
            }
        }
        if let Some(ancestor) = &view.status.blocking_ancestor {
            let command = format!("engram work show {}", ancestor.short_ref);
            if !next.contains(&command) {
                let position = next
                    .iter()
                    .position(|command| command.contains(" --history"))
                    .unwrap_or(next.len());
                next.insert(position, command);
            }
        }
        if word == "show"
            && let Some(origin) = &view.detached_from
        {
            let command = format!("engram work show {}", origin.work_ref);
            if !next.contains(&command) {
                next.push(command);
            }
        }
        Guidance { reminders, next }
    }
}

/// The smallest receipt the `evaluate` word can answer with: the
/// verdict-independent provenance (evaluation hash, replay flag, mode, exact
/// counts, work ref and revision, full-detail read) and nothing variable
/// beyond bounded identifiers and numbers. Its size is pinned by a test.
pub(super) fn minimal_evaluate_receipt(
    work_ref: &str,
    revision: i64,
    projection: &crate::work_service::WorkEvaluationProjection,
    evaluation: &crate::ObjectId,
    replayed: bool,
    advisory: Option<&super::evaluation_guidance::EvaluationObligations>,
) -> Receipt {
    let detail = super::mutation::full_contract(work_ref);
    let mut value = json!({
        "operation": "evaluate",
        "work": { "short_ref": work_ref, "revision": revision },
        "evaluation": {
            "hash": evaluation.as_str(),
            "replayed": replayed,
            "mode": projection.mode.word(),
            "verdicts_total": projection.verdicts_total,
            "verdicts_omitted": projection.verdicts_total,
            "passed": projection.passed,
            "full_detail": detail,
        },
        "full_detail": detail,
    });
    if let Some(advisory) = advisory {
        value["evaluation_obligations"] = json!(advisory.minimal());
    }
    let mut lines = vec![format!(
        "recorded {} evaluation on {work_ref}: {}/{} pass{}",
        projection.mode.word(),
        projection.passed,
        projection.verdicts_total,
        if replayed { " (replayed)" } else { "" }
    )];
    if let Some(advisory) = advisory {
        lines.push(format!(
            "{} open obligation(s), {} not shown; {}",
            advisory.open_total, advisory.open_total, advisory.timing
        ));
    }
    Receipt::assemble(
        lines,
        Guidance {
            reminders: Vec::new(),
            next: vec![detail.clone()],
        },
        value,
        false,
    )
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
        crate::PLAIN_READY_REASON => Some("unclaimed: claim it before execution".into()),
        "prior claim is recoverable" => claim_recovery_required
            .then(|| "a previous holder's claim lapsed; claiming needs a recovery reason".into()),
        "live claim has checkpointed progress" => match holder {
            Holder::Other(session, _, identity) => {
                Some(format!("held by {}", identity.session(session)))
            }
            Holder::You(_) | Holder::Nobody => None,
        },
        "live claim has not checkpointed progress" => Some(match holder {
            Holder::You(_) => "you hold this item but have not noted progress yet".into(),
            Holder::Other(session, _, identity) => format!(
                "held by {}; no progress noted yet",
                identity.session(session)
            ),
            Holder::Nobody => "held; no progress noted yet".into(),
        }),
        other => Some(other.to_owned()),
    }
}

/// How many lifecycle moves a receipt suggests before deferring to `show`.
const NEXT_LIFECYCLE_LIMIT: usize = 3;

pub(super) fn detach_command(work_ref: &str) -> String {
    format!("engram work update {work_ref} --detach \"Continue as independent work\"")
}

/// What an observation on open work offers its writer: a read first, never a
/// nudge to claim. The item's own detail read is the receipt's `full detail`
/// line, which mutation receipts keep out of `next`, so the first next
/// command is the orientation read `next --peek`. When the item can be
/// claimed, the claim follows last, with a reminder naming it as the way to
/// execute the item rather than observe it.
fn observation_guidance(guidance: &mut Guidance, work_ref: &str) {
    guidance
        .reminders
        .retain(|reminder| reminder != "unclaimed: claim it before execution");
    let is_claim = |command: &String| command.starts_with(&format!("engram work claim {work_ref}"));
    let claims = guidance
        .next
        .iter()
        .filter(|command| is_claim(command))
        .cloned()
        .collect::<Vec<_>>();
    guidance.next.retain(|command| !is_claim(command));
    let read = "engram work next --peek".to_owned();
    guidance.next.retain(|command| *command != read);
    guidance.next.insert(0, read);
    if !claims.is_empty() {
        guidance.next.extend(claims);
        guidance.reminders.push(
            "observation recorded; to execute this item rather than observe it, claim it (the last next command)"
                .into(),
        );
    }
}

/// Fixed table from `allowed_next` tags to literal commands. Only the moves
/// that change who holds the item or whether it is finished are suggested
/// (accept, claim, note, done, unblock) — at most [`NEXT_LIFECYCLE_LIMIT`] in
/// priority order — followed by `engram work show REF` for the rest. The one
/// planning exception is removing a dead prerequisite that can never
/// satisfy its edge. Other planning edits and entries the agent cannot run
/// through the agent words stay in `allowed_next` on the structured receipt.
/// `unblock` is the exact command that clears the item's only active
/// blocker, or the read that lists each of several with its own; it is
/// suggested only while the caller may unblock.
pub(super) fn next_commands(
    allowed_next: &[String],
    work_ref: &str,
    word: &str,
    unblock: Option<&str>,
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
    if has("work_update:detach") {
        push(detach_command(work_ref));
    }
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
    if let Some(command) = unblock
        && has("work_update:unblock")
    {
        push(command.to_owned());
    }
    let lifecycle = !out.is_empty();
    // Looking at a closed item again or calling `next` from `next` changes
    // nothing, so neither is suggested.
    let show = format!("engram work show {work_ref}");
    if open && has("work_focus") && word != "show" && !out.contains(&show) {
        out.push(show);
    }
    if !lifecycle && word != "next" {
        out.push("engram work next".into());
    }
    out
}

fn record_verbose_next_omission(
    omissions: &mut Vec<crate::work_service::WorkSectionOmission>,
    count: usize,
) {
    let section = WorkNextSection::Ready;
    if let Some(existing) = omissions.iter_mut().find(|entry| {
        entry.section == section
            && entry.reason == crate::work_service::WorkSectionOmissionReason::ByteBudget
    }) {
        existing.omitted_count += count;
    } else {
        omissions.push(crate::work_service::WorkSectionOmission {
            section,
            reason: crate::work_service::WorkSectionOmissionReason::ByteBudget,
            omitted_count: count,
        });
    }
}

fn project_memory_list_receipt(
    result: &crate::domain::ProjectMemoryList,
    filtered: bool,
) -> Result<Receipt, VerbError> {
    let mut lines = vec![format!("{} project memory item(s):", result.memories.len())];
    for row in &result.memories {
        lines.push(format!(
            "  {} (revision {}) — {} — by {} ({})",
            row.key,
            row.revision,
            short(&terminal_safe_multiline(&row.first_line)),
            terminal_safe_actor_label(&row.actor_id, row.actor_context.as_deref()),
            row.remembered_at.format("%Y-%m-%d %H:%M UTC")
        ));
        for line in crate::work_service::retiring_target_lines(
            row.retiring_target.as_ref(),
            row.retiring_state.as_ref(),
            row.retiring_target_dropped.as_ref(),
            true,
        ) {
            // Whole, not shortened: the display form of the target, with a
            // local item's short ref; the full read's reminder gives the
            // restore command by work id. The listing is byte-fitted.
            lines.push(format!("    {line}"));
        }
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
