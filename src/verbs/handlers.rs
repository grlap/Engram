use crate::domain::normalize_gate_evidence_input;

pub(super) use super::obligation_reminders::obligation_reminders;
use super::{
    Arc, ChildRequirement, DEFAULT_LIMIT, DateTime, Deserialize, Guidance, Holder,
    LocalWorkService, MAX_AGENT_WORK_RESPONSE_BYTES, MAX_COMPACT_CHANGE_ITEMS, MAX_NEXT_PAGES,
    PathBuf, ProjectId, Receipt, Serialize, SessionId, StoreError, Utc, VerbError,
    WORK_UPDATE_CLAIM_ACTION, WORK_UPDATE_CLAIM_RECOVERY_ACTION, WorkAttributionDefaults,
    WorkAvailability, WorkBlockerKind, WorkChildInput, WorkCompleteInput, WorkCompleteResult,
    WorkCompletionCaptureInput, WorkFocusView, WorkHandoffInput, WorkItemKind, WorkLifecycle,
    WorkNextQuery, WorkNextSection, WorkPrerequisiteState, WorkProposeInput, WorkProposeResult,
    WorkRevisionPatch, WorkUpdateInput, changes_not_delivered, held_suffix, item_line, json,
    lifecycle_word, nonempty,
    receipts::{compact_next_lines, compact_next_receipt, compact_next_value, ready_line},
    section_word, short,
    show::{fit_show_receipt, live, show_lines, show_receipt_value},
    slug, terminal_safe_actor_label, terminal_safe_multiline, trimmed, validate_priority,
};

mod claims;
mod completion;
mod completion_remedy;
mod creation;
mod gates;
mod handoff;
mod memories;
mod notes;
mod orientation;
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
    /// selects open work only, so this adds nothing to it; a blocked listing
    /// excludes ended items even when this is set.
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

    pub(super) fn finish_mutation(&self, receipt: Receipt) -> Receipt {
        match &self.fit_effective_session {
            Some(session) => receipt.with_effective_session_id(session),
            None => receipt,
        }
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

    /// The session's ambient focus, read without staging or acknowledging
    /// deliveries and without registering a session.
    fn focused(&self, now: DateTime<Utc>) -> Result<Option<WorkFocusView>, VerbError> {
        Ok(self.service.ambient_focus_view(now)?)
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
/// verdict-independent provenance (evaluation record id, replay flag, mode, exact
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
            "evaluation": evaluation.as_str(),
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
/// (accept, cancel an offer, claim, note, done, unblock) — at most [`NEXT_LIFECYCLE_LIMIT`] in
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
    if has("work_handoff:cancel") {
        push(format!("engram work handoff {work_ref} --cancel \"…\""));
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
