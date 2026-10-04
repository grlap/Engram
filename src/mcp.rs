//! MCP stdio surface for the fifteen agent-facing tools: the fourteen work
//! words plus `search`.

mod read_only;

use std::{path::PathBuf, sync::Arc};

use chrono::Utc;
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, tool::ToolCallContext, wrapper::Parameters},
    model::{CallToolRequestParams, CallToolResponse, CallToolResult},
    schemars::JsonSchema,
    service::RequestContext,
    tool, tool_handler, tool_router,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    AddInput, AgentVerbs, ClaimInput, ClaimUnderInput, DoneInput, EvaluateInput, ForgetInput,
    GateInput, HandoffAction, HandoffInput, LocalWorkService, LsInput, MemoriesInput, NextInput,
    NoteInput, ProjectId, Receipt, RememberInput, SessionId, UpdateAction, UpdateInput, VerbError,
    WorkItemKind, parse_defer_date,
    storage::{PROCESS_DEFAULT_WORK_SESSION_REUSE_REFUSAL, StoreError},
    work_service::COMPLETED_WORK_LATE_FINDING_REFUSAL,
};

/// Immutable host context asserted for one MCP connection.
#[derive(Clone, Debug)]
pub struct McpServer {
    actor_id: String,
    session_id: SessionId,
    work_service: Arc<LocalWorkService>,
    tool_router: ToolRouter<Self>,
    /// Fixed at construction: only the read words, in their reading forms.
    read_only: bool,
    /// The opt-in phase trace, when enabled at server start.
    phase_trace: Option<Arc<crate::phase_trace::PhaseTrace>>,
}

impl McpServer {
    /// Creates an MCP service with optional host-asserted actor attribution
    /// context. The context never participates in actor/session authority.
    #[must_use]
    pub fn new_with_actor_context(
        database: PathBuf,
        project_id: ProjectId,
        actor_id: String,
        session_id: SessionId,
        source_skill: Option<String>,
        actor_context: Option<String>,
    ) -> Self {
        Self::with_mode(
            database,
            project_id,
            actor_id,
            session_id,
            source_skill,
            actor_context,
            false,
        )
    }

    /// The same server in read-only mode: it lists only the read words and
    /// refuses any other call, and any writing form of a read word, as an
    /// MCP tool error with a stable code. Its service never opens the store
    /// for writing.
    #[must_use]
    pub fn new_read_only_with_actor_context(
        database: PathBuf,
        project_id: ProjectId,
        actor_id: String,
        session_id: SessionId,
        source_skill: Option<String>,
        actor_context: Option<String>,
    ) -> Self {
        Self::with_mode(
            database,
            project_id,
            actor_id,
            session_id,
            source_skill,
            actor_context,
            true,
        )
    }

    fn with_mode(
        database: PathBuf,
        project_id: ProjectId,
        actor_id: String,
        session_id: SessionId,
        source_skill: Option<String>,
        actor_context: Option<String>,
        read_only: bool,
    ) -> Self {
        // Capture before this long-lived server can outlive an executable install.
        let _ = crate::build_identity::current();
        let service = LocalWorkService::new_with_attribution(
            database,
            project_id,
            actor_id.clone(),
            session_id.clone(),
            source_skill,
            actor_context,
            crate::WorkAttributionDefaults::default(),
        );
        let work_service = Arc::new(if read_only {
            service.into_read_only()
        } else {
            service
        });
        let mut tool_router = Self::agent_tool_router();
        if read_only {
            // Every route the router holds, so a tool added later is
            // refused unless it is made a read word here.
            for tool in tool_router.list_all() {
                if !read_only::READ_TOOLS.contains(&tool.name.as_ref()) {
                    tool_router.remove_route(&tool.name);
                }
            }
        }
        Self {
            actor_id,
            session_id,
            work_service,
            tool_router,
            read_only,
            phase_trace: None,
        }
    }

    /// The same server, recording each tool call's phases into `trace`.
    #[must_use]
    pub fn with_phase_trace(mut self, trace: Option<Arc<crate::phase_trace::PhaseTrace>>) -> Self {
        self.phase_trace = trace;
        self
    }

    /// One tool call: the read-only admission, then the routed tool.
    async fn call_tool_untraced(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        // The read-only mode decides on the raw call, before any tool runs.
        if self.read_only
            && let Err(restriction) = read_only::admit(&request.name, request.arguments.as_ref())
        {
            return Ok(CallToolResult::structured_error(read_only::refusal(
                &request.name,
                restriction,
            ))
            .into());
        }
        self.tool_router
            .call(ToolCallContext::new(self, request, context))
            .await
    }

    fn verb(&self, outcome: Result<Receipt, VerbError>) -> CallToolResult {
        verb(outcome, &self.verbs())
    }

    fn verbs(&self) -> AgentVerbs {
        AgentVerbs::with_shared_service(
            Arc::clone(&self.work_service),
            self.actor_id.clone(),
            self.session_id.clone(),
        )
        .with_mcp_argument_names()
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct NextArgs {
    /// Read-only orientation: no staging, acknowledgement, focus or cursor changes.
    /// Repeated peeks repeat unacknowledged signals; use memories to read the notes.
    peek: Option<bool>,
    /// Maximum changes (default 20); compact ready candidates are capped at five.
    limit: Option<u32>,
    /// Return rich structured output, including raw identity and integrity metadata.
    /// Terse show and compact rows omit selected fields; this is not a global security boundary.
    verbose: Option<bool>,
    /// Asserted host/client context generation, a plain token; until a memories listing carries it, a peek directs the session to list them.
    #[schemars(length(max = 256))]
    context_generation: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct LsArgs {
    /// Case-insensitive text over refs, titles, outcomes, and labels.
    search: Option<String>,
    /// Only items with an active blocker or incomplete prerequisite.
    blocked: Option<bool>,
    /// Only ready candidates in priority then work-id order; excludes blocked. Inspect an item before claiming it.
    ready: Option<bool>,
    /// Only items assigned to this actor or held by this session.
    mine: Option<bool>,
    /// Include completed, cancelled, and superseded items.
    all: Option<bool>,
    /// Exact case-insensitive label.
    label: Option<String>,
    /// Direct children of this parent.
    under: Option<String>,
    /// Only optional direct children; requires under, excludes required.
    optional: Option<bool>,
    /// Only required direct children; requires under, excludes optional.
    required: Option<bool>,
    /// Continuation encoding filters and project/session context, not confidential; stale cursors refuse.
    after: Option<String>,
    /// Maximum items to return (default 20).
    limit: Option<u32>,
    /// Return rich structured output, including raw identity and integrity metadata.
    /// Terse show and compact rows omit selected fields; this is not a global security boundary.
    verbose: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ShowArgs {
    /// Short work ref or full UUID; reading changes neither focus nor claims.
    work_ref: String,
    /// Newest notes/observations, excluding gates, with exact omissions.
    notes: Option<bool>,
    /// Include gate evidence in the notes window; requires notes:true.
    gates: Option<bool>,
    /// Newest history window, using the same bounded continuation contract.
    history: Option<bool>,
    /// Item/kind-bound continuation; readable query context, not confidential.
    /// With note, continues a verification record's obligation assessment.
    after: Option<String>,
    /// Complete note body beyond the window ceiling: record id or `RECORD_ID:INDEX`.
    /// An inherited event or completion: `RECORD_ID:event-INDEX` or `RECORD_ID:completion`,
    /// returning the complete member. A verification record also shows its
    /// reconstructed obligation assessment.
    note: Option<String>,
    /// Complete stored title, outcome, and acceptance; exclusive of windows.
    full: Option<bool>,
    /// The evaluation records of the item's run in a bounded window, oldest to newest; after continues it.
    evaluations: Option<bool>,
    /// One evaluation record complete, by its full record id from the evaluations window.
    evaluation: Option<String>,
    /// The source observations of the item's run in a bounded window, oldest to newest; after continues it. Exclusive of the other windows.
    observations: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct AddArgs {
    /// Opaque external planning linkage, not an imported snapshot.
    external: Option<String>,
    /// Ordered initial notes, committed atomically with creation.
    notes: Option<Vec<String>>,
    /// Only required field.
    title: String,
    /// Defaults to the title.
    outcome: Option<String>,
    /// Acceptance criteria `done` is checked against, kept in the order given
    /// without repeats; defaults to one criterion "<title> is done".
    acceptance: Option<Vec<String>>,
    /// Bind criteria to typed host verification, as `POSITION=KIND[:FINGERPRINT]`
    /// (kind: test, build, lint, review or acceptance; positions count the
    /// acceptance list as typed; FINGERPRINT is a check's command fingerprint,
    /// and a stored record's id is refused); a bound criterion passes only on
    /// host-observed verification of that kind, never on judgment.
    bindings: Option<Vec<String>>,
    /// Add as a child of this item instead of a root.
    under: Option<String>,
    /// Make the child optional for parent completion. Requires `under`.
    optional: Option<bool>,
    /// 0 (highest) through 4. Omitted, a root or an optional child gets the
    /// project default (1) and a required child its parent's priority.
    priority: Option<i32>,
    labels: Option<Vec<String>>,
    assignee: Option<String>,
    /// task, bug, feature, epic, chore, or research.
    kind: Option<WorkItemKind>,
    /// Pin the acceptance-evaluation mode from creation: same-session, sub-agent, or independent-session.
    evaluation_mode: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WorkClaimArgs {
    /// Short work ref or full UUID; omit it with `under`.
    work_ref: Option<String>,
    /// Hold this parent's next ready child instead, chosen in the ls --ready order and claimed in the same transaction.
    under: Option<String>,
    /// Claim lifetime in seconds (default one hour).
    ttl_seconds: Option<i64>,
    /// Attributed reason for recovering a lapsed prior claim.
    recover: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum UpdateActionArg {
    Reject,
    Release,
    Blocked,
    Unblock,
    Revise,
    EvaluationMode,
    Cancel,
    After,
    DropAfter,
    Waive,
    Supersede,
    Detach,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct UpdateArgs {
    /// Replace external planning linkage; requires action revise.
    external: Option<String>,
    /// Remove external planning linkage as an audited revision.
    #[serde(default)]
    clear_external: bool,
    /// Item to act on; defaults to the focus.
    work_ref: Option<String>,
    /// `release`, `blocked`, `unblock`, `revise`, `evaluation_mode`, `cancel`,
    /// `after`, `drop_after`, `waive`, `reject`, `supersede`, or `detach`.
    action: UpdateActionArg,
    /// For the evaluation-mode action: same-session, sub-agent, or
    /// independent-session; omit to return the task to the default,
    /// independent evaluation unless the policy admits only same-session. A
    /// same-session mark set by the task's executor waives nothing.
    evaluation_mode: Option<String>,
    /// Reason for release, cancel, waive, reject, supersede, or detach. Required
    /// for all but release; a release by a session with neither a contribution
    /// nor a waiver under the item's root needs it too, as the attributed
    /// waiver of that missing contribution.
    reason: Option<String>,
    /// Why the item is blocked.
    text: Option<String>,
    /// For unblock: the blocker to clear, by the selector `show` prints
    /// beside it; omit to clear the item's only active blocker.
    blocker: Option<String>,
    title: Option<String>,
    outcome: Option<String>,
    /// Replace the whole acceptance list for revise, kept in the order given;
    /// reordering a bound criterion owes its verification again. Omission
    /// preserves it; an empty list or blank criterion is refused.
    acceptance: Option<Vec<String>>,
    /// Replace the criteria bound to typed host verification for revise, as
    /// `POSITION=KIND[:FINGERPRINT]`. Positions count the acceptance list as
    /// typed when it is replaced in the same call, otherwise the stored list
    /// as show numbers it; FINGERPRINT is a check's command fingerprint, and a
    /// stored record's id is refused. Omitted while acceptance is replaced,
    /// the bindings are cleared; omitted otherwise, they are unchanged.
    bindings: Option<Vec<String>>,
    assignee: Option<String>,
    /// 0 (highest) through 4.
    priority: Option<i32>,
    /// Defer until: RFC 3339, YYYY-MM-DD, or YYYY-MM-DDTHH:MM:SS (UTC).
    defer: Option<String>,
    /// task, bug, feature, epic, chore, or research.
    kind: Option<WorkItemKind>,
    /// Labels to add.
    labels: Option<Vec<String>>,
    /// Labels to remove.
    unlabels: Option<Vec<String>>,
    /// Prerequisite item for `after` or `drop_after`.
    prerequisite: Option<String>,
    /// Cancelled or superseded required child for `waive`; its parent and
    /// ancestors must be open. Requires `reason`.
    child: Option<String>,
    /// Replacement item for supersede.
    replacement: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct GateArgs {
    /// Item to record the gate on; defaults to the focus.
    work_ref: Option<String>,
    /// Stable gate name, normalized case-insensitively.
    name: String,
    /// Failure labels (test ids or check names). Omit only when the gate passed.
    failed: Option<Vec<String>>,
    /// Bounded opaque external-evidence reference; a path or URL by convention.
    evidence_ref: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct EvaluateArgs {
    /// Item to evaluate; defaults to the focus.
    work_ref: Option<String>,
    /// same-session, sub-agent, or independent-session; the project policy lists the allowed modes.
    mode: String,
    /// The item revision whose criteria the verdicts address, as printed by show.
    acceptance_basis: i64,
    /// The run-feed position the evaluator read through, as printed by show. A host check after it asks for a resubmission, unless it passed on the revision given as `source_fingerprint`; a source change after it voids the evaluation, unless it is to that revision.
    evidence_basis: i64,
    /// One verdict per current criterion by one-based position; a pass cites note/gate locators as `show` with notes and gates prints them, or full record ids of host-minted verification or environment evidence.
    verdicts: Vec<crate::WorkCriterionVerdictInput>,
    /// Explicit attempt key; identical resends replay, contradicting content under the same key refuses.
    attempt: Option<String>,
    /// Host-measured source fingerprint at evaluation time.
    source_fingerprint: Option<String>,
    /// PROVIDER/MODEL or PROVIDER/MODEL@VERSION, recorded as asserted metadata.
    model: Option<String>,
    /// Sub-agent mode only: the evaluator's distinct execution identity.
    execution_identity: Option<String>,
    /// Sub-agent mode only: the host-attested parent session.
    parent_session: Option<String>,
    /// Record id of the carried failing evaluation this one acknowledges, as `show` prints it in `carried_failure`. Required after the run's executor revised the criteria that evaluation failed, and then only from an evaluator that never held the run (never `same_session`); a failing evaluation that names it keeps it carried; refused when no failure is carried.
    supersedes: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct RememberArgs {
    /// Project note or observation. Never include credentials or secrets.
    #[schemars(length(max = 8192))]
    text: String,
    /// Safe permanent key; omitted to derive a slug from the first words.
    #[schemars(length(max = 64))]
    key: Option<String>,
    /// Append an attributed version to an existing key, retaining all prior versions.
    revise: Option<bool>,
    /// Optional current revision check; stale values refuse. Omit to revise the current head.
    expected_revision: Option<u64>,
    /// Review this memory when the named item retires: local:REF (an item in this
    /// project) or external:PROJECT#REFERENCE (asserted text of ASCII letters, digits
    /// and . _ - / : @ +). Omitted on revise, the current target is kept.
    retires_with: Option<String>,
    /// With revise, remove the retirement target, or acknowledge one a revision
    /// dropped, and record the clear; refused without revise or when there is neither.
    clear_retires_with: Option<bool>,
    /// With revise and `expected_revision`, append text to that revision as a
    /// paragraph instead of replacing the body.
    append: Option<bool>,
    /// With revise and `expected_revision`, replace only the interior of the
    /// section marked `<!-- engram-section NAME -->` … `<!-- /engram-section NAME -->`.
    #[schemars(length(max = 64))]
    section: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct MemoriesArgs {
    /// Search text, or the exact key when full is true.
    #[schemars(length(max = 256))]
    query: Option<String>,
    /// Continue an unfiltered key-ordered listing.
    #[schemars(length(max = 64))]
    after: Option<String>,
    /// Return one dedicated full body for the positional key.
    full: Option<bool>,
    /// With full and an exact key, read this historical revision instead of the current one.
    revision: Option<u64>,
    /// The host's context generation, as a peek printed it; the first page of an
    /// unfiltered listing records it, which records a listing, not a reading.
    /// Without it, memories records nothing.
    #[schemars(length(max = 256))]
    context_generation: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct ForgetArgs {
    /// Permanently reserved project-memory key to tombstone.
    #[schemars(length(max = 64))]
    key: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct NoteArgs {
    /// Record current coordination status; storage determines owner/peer qualification.
    status: Option<bool>,
    /// Item to note on; defaults to the focus.
    work_ref: Option<String>,
    /// What you found or decided.
    text: String,
    /// Evidence pointers such as paths or URLs.
    refs: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct DoneArgs {
    /// At most 64 explicit author links, not verification: criterion position and an existing note/gate locator.
    links: Option<Vec<crate::work_service::WorkCriterionLinkInput>>,
    /// Required with links; pass `acceptance_basis` from show. Any work revision change refuses.
    link_basis: Option<i64>,
    /// Item to complete; defaults to the focus.
    work_ref: Option<String>,
    /// What was delivered; recorded and checkpointed before sealing.
    summary: Option<String>,
    /// Shared acceptance note; does not link evidence to individual criteria.
    note: Option<String>,
    /// Host-measured source fingerprint at completion time; checked against the evaluated one when the policy requires source freshness.
    source_fingerprint: Option<String>,
    /// Where the work landed, recorded in the seal as asserted provenance: `commit`, `remote`, `branch`, `pushed_at`, and `installed_build` when a binary was installed.
    landing: Option<crate::domain::CompletionLanding>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct WorkSearchArgs {
    /// Case-insensitive text over refs, titles, outcomes, and labels.
    query: String,
    /// Maximum items to return (default 20).
    limit: Option<u32>,
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum HandoffActionArg {
    Offer,
    Accept,
    Cancel,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
struct HandoffArgs {
    /// Item to hand off; defaults to the focus.
    work_ref: Option<String>,
    /// offer (with to), accept, or cancel (with reason).
    action: HandoffActionArg,
    /// Real recipient session id supplied by the host or coordinator; at most 64 UTF-8 bytes; peer display labels are refused.
    to: Option<String>,
    /// Checkpoint summary recorded with the offer.
    summary: Option<String>,
    /// Why an outstanding offer is cancelled.
    reason: Option<String>,
    /// Offer lifetime in seconds.
    ttl_seconds: Option<i64>,
}

#[tool_router(router = agent_tool_router)]
impl McpServer {
    /// What is ready, what this session holds, and what changed.
    #[tool(
        name = "next",
        description = "What is ready, what you hold, and what changed; peek=true reads orientation without staging or advancing delivery, focus or memory advertisement"
    )]
    fn next(&self, Parameters(args): Parameters<NextArgs>) -> CallToolResult {
        self.verb(self.verbs().next(
            &NextInput {
                limit: args.limit,
                peek: args.peek.unwrap_or(false),
                verbose: args.verbose.unwrap_or(false),
                context_generation: args.context_generation,
            },
            Utc::now(),
        ))
    }

    /// List open work with flat filters.
    #[tool(
        name = "ls",
        description = "List open work; search, ready, blocked, mine, all, label, and under with optional/required narrow it; after continues the same listing"
    )]
    fn ls(&self, Parameters(args): Parameters<LsArgs>) -> CallToolResult {
        self.verb(self.verbs().ls(
            &LsInput {
                search: args.search,
                blocked: args.blocked.unwrap_or(false),
                ready: args.ready.unwrap_or(false),
                mine: args.mine.unwrap_or(false),
                all: args.all.unwrap_or(false),
                label: args.label,
                under: args.under,
                optional: args.optional.unwrap_or(false),
                required: args.required.unwrap_or(false),
                after: args.after,
                limit: args.limit,
                verbose: args.verbose.unwrap_or(false),
            },
            Utc::now(),
        ))
    }

    /// Inspect one item without changing focus or claims.
    #[tool(
        name = "show",
        description = "One item with display-only peer labels: outcome, acceptance, holder, blockers, reminders; reading changes neither focus nor claims. Rich verbose next/ls may expose raw identity and integrity metadata."
    )]
    fn show(&self, Parameters(args): Parameters<ShowArgs>) -> CallToolResult {
        self.verb(self.verbs().show_records(
            &args.work_ref,
            &crate::verbs::ShowInput {
                notes: args.notes.unwrap_or(false),
                gates: args.gates.unwrap_or(false),
                history: args.history.unwrap_or(false),
                after: args.after,
                note: args.note,
                full: args.full.unwrap_or(false),
                evaluations: args.evaluations.unwrap_or(false),
                evaluation: args.evaluation,
                observations: args.observations.unwrap_or(false),
            },
            Utc::now(),
        ))
    }

    /// Create a root or one required/optional child.
    #[tool(
        name = "add",
        description = "Create work from a title; under adds a child and optional makes it non-blocking"
    )]
    fn add(&self, Parameters(args): Parameters<AddArgs>) -> CallToolResult {
        self.verb(self.verbs().add(
            AddInput {
                external: args.external,
                notes: args.notes.unwrap_or_default(),
                title: args.title,
                outcome: args.outcome,
                acceptance: args.acceptance.unwrap_or_default(),
                bindings: args.bindings.unwrap_or_default(),
                under: args.under,
                optional: args.optional.unwrap_or(false),
                priority: args.priority,
                labels: args.labels.unwrap_or_default(),
                assignee: args.assignee,
                kind: args.kind,
                evaluation_mode: args.evaluation_mode,
            },
            Utc::now(),
        ))
    }

    /// Hold an item, or a parent's next ready child.
    #[tool(
        name = "claim",
        description = "Hold an item before changing anything; later calls default to it. With under instead of work_ref, hold that parent's next ready child, chosen in ls --ready order and claimed in the same transaction"
    )]
    fn claim(&self, Parameters(args): Parameters<WorkClaimArgs>) -> CallToolResult {
        let outcome = match (args.work_ref, args.under) {
            (None, Some(under)) => self.verbs().claim_under(
                ClaimUnderInput {
                    under,
                    ttl_seconds: args.ttl_seconds,
                    recover: args.recover,
                },
                Utc::now(),
            ),
            (Some(work_ref), None) => self.verbs().claim(
                ClaimInput {
                    work_ref,
                    ttl_seconds: args.ttl_seconds,
                    recover: args.recover,
                },
                Utc::now(),
            ),
            (None, None) => {
                Err(crate::StoreError::InvalidWork("claim needs work_ref or under".into()).into())
            }
            (Some(_), Some(_)) => Err(crate::StoreError::InvalidWork(
                "claim takes work_ref or under, not both".into(),
            )
            .into()),
        };
        self.verb(outcome)
    }

    /// Apply exactly one planning or claim action.
    #[tool(
        name = "update",
        description = "One action: release, blocked, unblock, revise, cancel, reject (required child plus reason; atomically cancels and waives), after/drop_after (prerequisite), waive (child plus reason), supersede (replacement plus reason), or detach (stranded child plus reason)"
    )]
    fn update(&self, Parameters(args): Parameters<UpdateArgs>) -> CallToolResult {
        if args.external.is_some() && !matches!(args.action, UpdateActionArg::Revise) {
            return invalid_argument("external", "external reference requires action revise");
        }
        if args.clear_external && !matches!(args.action, UpdateActionArg::Revise) {
            return invalid_argument(
                "clear_external",
                "clearing the external reference requires action revise",
            );
        }
        if args.acceptance.is_some() && !matches!(args.action, UpdateActionArg::Revise) {
            return invalid_argument(
                "acceptance",
                "acceptance replacement requires action revise",
            );
        }
        if args.bindings.is_some() && !matches!(args.action, UpdateActionArg::Revise) {
            return invalid_argument("bindings", "verification bindings require action revise");
        }
        if args.blocker.is_some() && !matches!(args.action, UpdateActionArg::Unblock) {
            return invalid_argument("blocker", "a blocker selector requires action unblock");
        }
        if args.evaluation_mode.is_some() && !matches!(args.action, UpdateActionArg::EvaluationMode)
        {
            return invalid_argument(
                "evaluation_mode",
                "an evaluation mode requires action evaluation_mode; it is ignored by no other action",
            );
        }
        let action = match args.action {
            UpdateActionArg::Release => UpdateAction::Release {
                reason: args.reason,
            },
            UpdateActionArg::Blocked => UpdateAction::Blocked {
                detail: args.text.unwrap_or_default(),
            },
            UpdateActionArg::Unblock => UpdateAction::Unblock {
                blocker: args.blocker,
            },
            UpdateActionArg::Revise => {
                let defer = match args.defer.as_deref().map(parse_defer_date).transpose() {
                    Ok(defer) => defer,
                    Err(message) => return invalid_argument("defer", &message),
                };
                UpdateAction::Revise {
                    external: args.external,
                    clear_external: args.clear_external,
                    title: args.title,
                    outcome: args.outcome,
                    acceptance: args.acceptance,
                    bindings: args.bindings,
                    assignee: args.assignee,
                    priority: args.priority,
                    defer,
                    kind: args.kind,
                    labels: args.labels.unwrap_or_default(),
                    unlabels: args.unlabels.unwrap_or_default(),
                }
            }
            UpdateActionArg::EvaluationMode => UpdateAction::EvaluationMode {
                mode: args.evaluation_mode,
            },
            UpdateActionArg::Cancel => UpdateAction::Cancel {
                reason: args.reason.unwrap_or_default(),
            },
            UpdateActionArg::Reject => UpdateAction::Reject {
                reason: args.reason.unwrap_or_default(),
            },
            UpdateActionArg::After => UpdateAction::After {
                prerequisite: args.prerequisite.unwrap_or_default(),
            },
            UpdateActionArg::DropAfter => UpdateAction::DropAfter {
                prerequisite: args.prerequisite.unwrap_or_default(),
            },
            UpdateActionArg::Waive => UpdateAction::WaiveRequiredChild {
                child: args.child.unwrap_or_default(),
                reason: args.reason.unwrap_or_default(),
            },
            UpdateActionArg::Detach => UpdateAction::Detach {
                reason: args.reason.unwrap_or_default(),
            },
            UpdateActionArg::Supersede => UpdateAction::Supersede {
                replacement: args.replacement.unwrap_or_default(),
                reason: args.reason.unwrap_or_default(),
            },
        };
        self.verb(self.verbs().update(
            UpdateInput {
                work_ref: args.work_ref,
                action,
            },
            Utc::now(),
        ))
    }

    /// Record a gate on held open work or a late finding on completed work.
    #[tool(
        name = "gate",
        description = "Record a gate on held open work or an attributed late finding on completed work; work_ref defaults to focus and evidence_ref is opaque"
    )]
    fn gate(&self, Parameters(args): Parameters<GateArgs>) -> CallToolResult {
        self.verb(self.verbs().gate(
            GateInput {
                work_ref: args.work_ref,
                name: args.name,
                failed: args.failed.unwrap_or_default(),
                evidence_ref: args.evidence_ref,
            },
            Utc::now(),
        ))
    }

    /// Record one attributed acceptance evaluation on an item's active run.
    #[tool(
        name = "evaluate",
        description = "Record one immutable acceptance evaluation on the targeted item's active run: one verdict per criterion with basis, rationale, and run-evidence citations; work_ref defaults to focus and acceptance_basis is the revision printed by show"
    )]
    fn evaluate(&self, Parameters(args): Parameters<EvaluateArgs>) -> CallToolResult {
        self.verb(self.verbs().evaluate(
            EvaluateInput {
                work_ref: args.work_ref,
                mode: args.mode,
                acceptance_basis: args.acceptance_basis,
                evidence_basis: args.evidence_basis,
                verdicts: args.verdicts,
                attempt: args.attempt,
                source_fingerprint: args.source_fingerprint,
                model: args.model,
                execution_identity: args.execution_identity,
                parent_session: args.parent_session,
                supersedes: args.supersedes,
            },
            Utc::now(),
        ))
    }

    /// Store one attributed project memory.
    #[tool(
        name = "remember",
        description = "Store an attributed project note; revise an existing key with retained history. Optional expected_revision refuses stale writes; no focus or claim changes"
    )]
    fn remember(&self, Parameters(args): Parameters<RememberArgs>) -> CallToolResult {
        self.verb(self.verbs().remember(
            RememberInput {
                text: args.text,
                key: args.key,
                revise: args.revise.unwrap_or(false),
                expected_revision: args.expected_revision,
                retires_with: args.retires_with,
                clear_retires_with: args.clear_retires_with.unwrap_or(false),
                append: args.append.unwrap_or(false),
                section: args.section,
            },
            Utc::now(),
        ))
    }

    /// List, search, or fully read project memories.
    #[tool(
        name = "memories",
        description = "List or search current project memories; full with an exact key reads one body, with optional revision for attributed history; records nothing unless context_generation is given"
    )]
    fn memories(&self, Parameters(args): Parameters<MemoriesArgs>) -> CallToolResult {
        self.verb(self.verbs().memories(
            &MemoriesInput {
                query: args.query,
                after: args.after,
                full: args.full.unwrap_or(false),
                revision: args.revision,
                context_generation: args.context_generation,
            },
            Utc::now(),
        ))
    }

    /// Permanently retire one project-memory key.
    #[tool(
        name = "forget",
        description = "Append an attributed tombstone for one project-memory key; this is not erasure"
    )]
    fn forget(&self, Parameters(args): Parameters<ForgetArgs>) -> CallToolResult {
        self.verb(
            self.verbs()
                .forget(ForgetInput { key: args.key }, Utc::now()),
        )
    }

    /// Record one finding once.
    #[tool(
        name = "note",
        description = "Record an attributed note on open or blocked work without claiming, including children of a completed parent; work_ref selects the target. Only a live holder checkpoints; completed notes never change a frozen seal"
    )]
    fn note(&self, Parameters(args): Parameters<NoteArgs>) -> CallToolResult {
        self.verb(self.verbs().note(
            &NoteInput {
                status: args.status.unwrap_or(false),
                work_ref: args.work_ref,
                text: args.text,
                refs: args.refs.unwrap_or_default(),
            },
            Utc::now(),
        ))
    }

    /// Complete the held item and disclose absent per-criterion evidence links.
    #[tool(
        name = "done",
        description = "Complete the item you hold; optional links cite existing note/gate evidence with the required link_basis from show, author linkage not verification. Success discloses criteria with no evidence linked to this criterion, without refusing completion for that absence; a refusal says what is still owed and the command that resolves it"
    )]
    fn done(&self, Parameters(args): Parameters<DoneArgs>) -> CallToolResult {
        self.verb(self.verbs().done(
            DoneInput {
                links: args.links.unwrap_or_default(),
                link_basis: args.link_basis,
                work_ref: args.work_ref,
                summary: args.summary,
                note: args.note,
                source_fingerprint: args.source_fingerprint,
                landing: args.landing,
            },
            Utc::now(),
        ))
    }

    /// Search every item by text.
    #[tool(
        name = "search",
        description = "Search every item, including closed ones, by text"
    )]
    fn search(&self, Parameters(args): Parameters<WorkSearchArgs>) -> CallToolResult {
        self.verb(self.verbs().search(&args.query, args.limit, Utc::now()))
    }

    /// Offer, accept, or cancel a transfer.
    #[tool(
        name = "handoff",
        description = "Offer the item you hold to another session, accept an offer made to you, or cancel yours"
    )]
    fn handoff(&self, Parameters(args): Parameters<HandoffArgs>) -> CallToolResult {
        let action = match args.action {
            HandoffActionArg::Offer => HandoffAction::Offer {
                to: args.to.unwrap_or_default(),
                summary: args.summary,
                ttl_seconds: args.ttl_seconds,
            },
            HandoffActionArg::Accept => HandoffAction::Accept,
            HandoffActionArg::Cancel => HandoffAction::Cancel {
                reason: args.reason.unwrap_or_default(),
            },
        };
        self.verb(self.verbs().handoff(
            HandoffInput {
                work_ref: args.work_ref,
                action,
            },
            Utc::now(),
        ))
    }
}

/// What `initialize` tells an ordinary connection.
const INSTRUCTIONS: &str = "Fourteen words: next, ls, show, add, claim, update, gate, evaluate, note, done, handoff, remember, memories, forget (plus search). add needs only a title; claim before execution; evaluate records attributed acceptance verdicts that an evaluated project policy consumes at done; note accepts project-bound non-holders on open/blocked work without granting execution authority; completed note/gate append late evidence without reopening; remember stores attributed project notes; memories is their source of truth; forget tombstones rather than erases. Every answer a word gives ends with reminders and runnable next commands; arguments a word's input schema rejects (an undeclared field, a wrong type, an unknown action) are refused before the word runs, as a text-only tool error that describes the problem, though a type error may not name the field. Keyless same-holder claim calls renew without shortening expiry. A restored completed gate always appends: inspect show before repeating an uncertain call. Other identical calls retain exact retry semantics.";

#[allow(
    clippy::unused_async_trait_impl,
    reason = "the rmcp handler macro emits required async trait methods"
)]
#[tool_handler(router = self.tool_router)]
impl ServerHandler for McpServer {
    fn get_info(&self) -> rmcp::model::ServerInfo {
        rmcp::model::ServerInfo::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
        .with_server_info(rmcp::model::Implementation::new("engram", "0.1.0"))
        .with_instructions(if self.read_only {
            read_only::INSTRUCTIONS.to_string()
        } else {
            INSTRUCTIONS.to_string()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let Some(trace) = self.phase_trace.as_ref() else {
            return self.call_tool_untraced(request, context).await;
        };
        let id = context.id.clone();
        let tool = request.name.to_string();
        let started = std::time::Instant::now();
        let (result, phases) =
            crate::phase_trace::scoped(self.call_tool_untraced(request, context)).await;
        trace.handler_settled(id, &tool, started.elapsed(), phases);
        result
    }
}

fn verb(outcome: Result<Receipt, VerbError>, words: &AgentVerbs) -> CallToolResult {
    let value = match outcome {
        Ok(receipt) => Ok(receipt.value),
        Err(error) => {
            let guidance = words.error_guidance(&error);
            let mut value = words.project_error(&error, store_error_value(&error.error));
            value["error"]["reminders"] = json!(guidance.reminders);
            value["error"]["next"] = json!(guidance.next);
            Err(value)
        }
    };
    let started = crate::phase_trace::start();
    let result = match value {
        Ok(value) => CallToolResult::structured(value),
        Err(value) => CallToolResult::structured_error(value),
    };
    crate::phase_trace::finish(crate::phase_trace::Phase::ReceiptSerialize, started);
    result
}

/// Stable structured rendering shared by MCP and native JSON/core errors.
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "one shared structured renderer keeps every CLI and MCP error surface identical"
)]
pub fn store_error_value(error: &StoreError) -> Value {
    let details = match error {
        StoreError::NoteIdempotencyConflict(key) => json!({ "idempotency_key": key }),
        StoreError::TaskAccessDenied { task, session } => json!({
            "task_id": task.0,
            "session_id": session,
        }),
        StoreError::MemoryAccessDenied(hash) | StoreError::MemoryNotFound(hash) => {
            json!({ "object_id": hash })
        }
        StoreError::ProjectMemoryExists(key) => json!({
            "key": key,
            "remedy": format!("read memories {key} --full; use remember with --key {key} --revise to retain history"),
        }),
        StoreError::ProjectMemoryRevisionConflict {
            key,
            expected,
            current,
        } => json!({
            "key": key, "expected_revision": expected, "current_revision": current,
            "remedy": format!("read memories {key} --full and reconcile before revising"),
        }),
        StoreError::ProjectMemoryRevisionNotFound {
            key,
            revision,
            current,
        } => json!({
            "key": key, "revision": revision, "current_revision": current,
            "remedy": format!("read memories {key} --full for history navigation"),
        }),
        StoreError::ProjectMemorySectionNotFound(missing) => json!({
            "key": missing.key, "revision": missing.revision, "section": missing.section,
            "sections": missing.sections,
            "remedy": format!(
                "name one of the existing sections, or add `{}` with append and its markers",
                missing.section
            ),
        }),
        StoreError::ProjectMemoryRetired(key) => json!({
            "key": key,
            "remedy": "run memories and choose a key that has never been used",
        }),
        StoreError::ProjectMemoryNotFound(key) => json!({
            "key": key,
            "remedy": "run memories to list retained project memories",
        }),
        StoreError::ProjectMemoryBindingInvalid => json!({
            "remedy": "use a non-empty asserted actor/session binding for this project",
        }),
        StoreError::InvalidProjectMemory(reason) if reason.contains("context_generation") => {
            json!({
                "reason": reason,
                "remedy": "omit context_generation or use 1 to 256 ASCII letters, digits, dots, underscores or dashes, not starting with a dash",
            })
        }
        StoreError::InvalidProjectMemory(reason) => json!({
            "reason": reason,
            "remedy": "follow the key and size bounds, or run memories for valid keys",
        }),
        StoreError::WorkNotFound(work) => missing_work_details(*work),
        StoreError::WorkReferenceAmbiguous {
            reference,
            candidates,
            more,
        } => ambiguous_work_reference_details(reference, candidates, *more),
        StoreError::WorkImplicitTargetConflict(conflict) => json!({
            "operation": conflict.operation,
            "focused_ref": conflict.focus,
            "focus_state": conflict.focus_state.as_str(),
            "held_refs": conflict.held,
            "more": conflict.more,
            "remedy": "repeat the word with the intended item named; nothing was recorded",
        }),
        StoreError::InvalidWork(message)
            if message == PROCESS_DEFAULT_WORK_SESSION_REUSE_REFUSAL =>
        {
            json!({
                "reason": message,
                "remedy": PROCESS_DEFAULT_WORK_SESSION_REUSE_REFUSAL,
            })
        }
        StoreError::InvalidWork(message) if message == COMPLETED_WORK_LATE_FINDING_REFUSAL => {
            json!({
                "reason": message,
                "remedy": "use note to record a late finding without reopening the completed item",
            })
        }
        StoreError::InvalidWork(message) if message == crate::verbs::GATE_WORK_REF_REQUIRED => {
            json!({
                "reason": message,
                "remedy": crate::verbs::GATE_WORK_REF_REQUIRED,
            })
        }
        StoreError::InvalidWork(message) if message == crate::storage::PENDING_HANDOFF_REFUSAL => {
            json!({
                "reason": message,
                "remedy": "cancel the handoff offer, or let it be accepted or expire before retrying",
            })
        }
        StoreError::WorkCatalogCursorInvalid { reason } => json!({
            "reason": reason,
            "remedy": "repeat the listing without --after, then continue from the new token",
        }),
        StoreError::WorkShowCursorInvalid { reason } => json!({
            "reason": reason,
            "remedy": "repeat show without --after, then continue from the new token",
        }),
        StoreError::WorkNoteReferenceInvalid {
            reason,
            candidates,
            more,
        } => json!({
            "reason": reason, "candidates": candidates, "more": more,
            "remedy": "use the complete locator printed beside the note",
        }),
        StoreError::WorkCriterionLinkInvalid { criterion, reason } => {
            let mut details = json!({
                "reason": reason,
                "remedy": "read show for the current acceptance basis and show --notes --gates for existing current-run evidence; an explicit author link is not verification",
            });
            if let (Some(position), Value::Object(fields)) = (criterion, &mut details) {
                fields.insert("criterion".into(), json!(position));
            }
            details
        }
        StoreError::WorkNoteTooLarge { bytes, limit } => json!({
            "bytes": bytes, "limit": limit,
            "reason": "note body exceeds the UTF-8 byte limit",
            "remedy": "carry bulk content as a reference",
        }),
        StoreError::WorkAncestorNotOpen { work, ancestor } => json!({
            "work_id": work,
            "blocking_ancestor": {"ref": ancestor.short_ref, "lifecycle": ancestor.lifecycle},
            "reason": error.to_string(),
            "remedy": "inspect the affected item and ancestor with show; follow the affected item's admitted next commands",
        }),
        StoreError::InvalidWork(message) | StoreError::InvalidWorkProjection(message) => json!({
            "reason": message,
            "remedy": "run next, then show the affected item and follow next",
        }),
        StoreError::WorkRevisionConflict {
            work,
            expected,
            current,
        } => json!({
            "work_id": work,
            "expected_revision": expected,
            "current_revision": current,
            "remedy": "run show for the affected item before retrying with a new idempotency_key",
        }),
        StoreError::WorkOperationIdempotencyConflict { operation, key } => json!({
            "operation": operation,
            "idempotency_key": key,
            "remedy": "retry the original payload exactly or use a new key for a different intent",
        }),
        StoreError::WorkDecompositionRetryConflict { parent_ref, reason } => json!({
            "parent_ref": parent_ref,
            "reason": reason,
            "remedy": crate::storage::DECOMPOSITION_RETRY_REMEDY,
        }),
        StoreError::WorkDependencyCycle => json!({
            "remedy": "remove or change the prerequisite edge that introduces the cycle",
        }),
        StoreError::WorkNotOpen(work) => json!({
            "work_id": work,
            "remedy": "run show for the affected item and follow next",
        }),
        StoreError::WorkParentNotOpen { lifecycle, .. } => json!({
            "parent_lifecycle": lifecycle,
            "remedy": crate::storage::parent_not_open_remedy(*lifecycle),
        }),
        StoreError::WorkDetachRefused {
            work_id,
            reason,
            remedy,
        } => json!({
            "work_id": work_id, "reason": reason, "remedy": remedy,
        }),
        StoreError::WorkRejectRefused {
            child_ref,
            parent_ref,
            reason,
            remedy,
        } => json!({
            "child_ref": child_ref, "parent_ref": parent_ref, "reason": reason, "remedy": remedy,
        }),
        StoreError::WorkPeerDecompositionRefused { parent } => json!({
            "work_id": parent,
            "remedy": "ask the parent holder to add required children or prerequisites; a peer may use add --under REF --optional",
        }),
        StoreError::WorkPrerequisiteAlreadySatisfied(work) => json!({
            "work_id": work,
            "remedy": "no edge is needed; run show for the prerequisite before choosing another action",
        }),
        StoreError::WorkClaimHeld {
            work,
            holder,
            expires_at,
        } => json!({
            "work_id": work,
            "holder_session_id": holder,
            "expires_at_ms": expires_at,
            "expires_at": chrono::DateTime::<Utc>::from_timestamp_millis(*expires_at)
                .map(|value| value.to_rfc3339()),
            "remedy": "wait for expiry or coordinate an explicit checkpointed handoff",
        }),
        StoreError::WorkClaimMismatch { work } => json!({
            "work_id": work,
            "remedy": "run show; claim the item again or accept its handoff before mutating",
        }),
        StoreError::WorkClaimLapsed { work, expired_at } => json!({
            "work_id": work,
            "expired_at_ms": expired_at.timestamp_millis(),
            "expired_at": expired_at.to_rfc3339(),
            "remedy": "run claim REF before mutating",
        }),
        StoreError::WorkReleaseWaiverRequired { work } => json!({
            "work_id": work,
            "remedy": "repeat the release with a nonblank reason; it is recorded as the attributed waiver of this session's missing contribution",
        }),
        StoreError::WorkCompletionRefused { work, reason } => json!({
            "work_id": work,
            "reason": reason,
            "remedy": "record evidence, checkpoint the current feed cut, and satisfy every current acceptance criterion",
        }),
        StoreError::AcceptanceEvaluationAdmissionRefused {
            work,
            reason,
            cause,
        } => json!({
            "work_id": work,
            "reason": reason,
            "cause": cause,
            "remedy": crate::work_service::evaluation_admission_remedy(cause),
        }),
        StoreError::WorkBoundVerificationRefused {
            work,
            reason,
            cause,
        } => json!({
            "work_id": work,
            "reason": reason,
            "cause": cause,
            "remedy": crate::work_service::bound_verification_remedy(cause),
        }),
        StoreError::WorkCompletionRecoveryRequired {
            work,
            cause,
            context,
        } => {
            let mut details = json!({
                "work_id": work,
                "cause": cause,
            });
            if let Some(observation) = &context.deciding_observation {
                details["deciding_observation"] = json!(observation);
            }
            if let Some(source) = &context.source {
                details["source"] = json!(source);
            }
            details
        }
        StoreError::AcceptanceCriteriaRequired { work } => json!({
            "work_id": work,
            "reason": "the item has no acceptance criteria; an acceptance evaluation needs at least one, and the host refuses to evaluate an item without criteria",
            "remedy": "add at least one criterion with `engram work update REF --accept \"criterion\"`, then have the host evaluate it, then run `engram work done REF` again",
        }),
        StoreError::AcceptanceEvaluationCarriedFailure {
            work,
            refusal,
            failed,
            ..
        } => json!({
            "work_id": work,
            "reason": refusal.word(),
            "failed_evaluation": failed,
            "remedy": refusal.remedy(),
        }),
        StoreError::AcceptanceEvaluationBasisMoved {
            work,
            moved,
            reason,
            observation,
        } => {
            let mut details = json!({
                "work_id": work,
                "reason": reason,
                "remedy": moved.remedy(),
            });
            // Added beside the unchanged fields, only when an observation
            // decided the move.
            if let Some(observation) = observation {
                details["deciding_observation"] = json!(observation);
            }
            details
        }
        _ => Value::Null,
    };
    json!({
        "error": {
            "code": error_code(error),
            "message": error.to_string(),
            "details": details,
        }
    })
}

fn missing_work_details(work: crate::WorkId) -> Value {
    json!({
        "work_id": work,
        "remedy": "run search or ls, then show a returned short_ref",
    })
}

fn ambiguous_work_reference_details(
    reference: &str,
    candidates: &[crate::WorkReferenceCandidate],
    more: usize,
) -> Value {
    json!({
        "reference": reference,
        "candidates": candidates,
        "more": more,
        "remedy": "repeat the operation with one candidate's full work_id",
    })
}

fn error_code(error: &StoreError) -> &'static str {
    match error {
        StoreError::StoreNotInitialized => "store_not_initialized",
        StoreError::NoteIdempotencyConflict(_) => "note_idempotency_conflict",
        StoreError::NoActiveTask(_) => "no_active_task",
        StoreError::TaskAccessDenied { .. } => "task_access_denied",
        StoreError::MemoryAccessDenied(_) => "memory_access_denied",
        StoreError::MemoryNotFound(_) | StoreError::ProjectMemoryNotFound(_) => "memory_not_found",
        StoreError::ProjectMemoryExists(_) => "memory_exists",
        StoreError::ProjectMemoryRevisionConflict { .. } => "memory_revision_conflict",
        StoreError::ProjectMemoryRevisionNotFound { .. } => "memory_revision_not_found",
        StoreError::ProjectMemorySectionNotFound(_) => "memory_section_not_found",
        StoreError::ProjectMemoryRetired(_) => "memory_retired",
        StoreError::ProjectMemoryBindingInvalid => "memory_binding_invalid",
        StoreError::InvalidProjectMemory(_) => "memory_invalid",
        StoreError::EmptyNote => "empty_note",
        StoreError::RedactionRefused(_) => "redaction_refused",
        StoreError::WorkNotFound(_) => "work_not_found",
        StoreError::WorkReferenceAmbiguous { .. } => "work_reference_ambiguous",
        StoreError::WorkImplicitTargetConflict(_) => "work_implicit_target_conflict",
        StoreError::InvalidWork(_) | StoreError::WorkAncestorNotOpen { .. } => "work_invalid",
        StoreError::InvalidWorkProjection(_) => "work_projection_invalid",
        StoreError::WorkRevisionConflict { .. } => "work_revision_conflict",
        StoreError::WorkOperationIdempotencyConflict { .. } => "work_idempotency_conflict",
        StoreError::WorkDecompositionRetryConflict { .. } => "work_decomposition_retry_conflict",
        StoreError::WorkDependencyCycle => "work_dependency_cycle",
        StoreError::WorkPrerequisiteAlreadySatisfied(_) => "work_prerequisite_already_satisfied",
        StoreError::WorkNotOpen(_) => "work_not_open",
        StoreError::WorkParentNotOpen { .. } => "work_parent_not_open",
        StoreError::WorkDetachRefused { .. } => "work_detach_refused",
        StoreError::WorkRejectRefused { .. } => "work_reject_refused",
        StoreError::WorkCatalogCursorInvalid { .. } => "work_catalog_cursor_invalid",
        StoreError::WorkShowCursorInvalid { .. } => "work_show_cursor_invalid",
        StoreError::WorkNoteReferenceInvalid { .. } => "work_note_reference_invalid",
        StoreError::WorkCriterionLinkInvalid { .. } => "work_criterion_link_invalid",
        StoreError::WorkNoteTooLarge { .. } => "work_note_too_large",
        StoreError::WorkPeerDecompositionRefused { .. } => "work_peer_decomposition_refused",
        StoreError::WorkClaimHeld { .. } => "work_claim_held",
        StoreError::WorkClaimMismatch { .. } => "work_claim_mismatch",
        StoreError::WorkClaimLapsed { .. } => "work_claim_lapsed",
        StoreError::WorkCompletionRefused { .. }
        | StoreError::WorkBoundVerificationRefused { .. } => "work_completion_refused",
        StoreError::WorkReleaseWaiverRequired { .. } => "work_release_waiver_required",
        StoreError::WorkCompletionRecoveryRequired { .. } => "work_completion_recovery_required",
        StoreError::AcceptanceCriteriaRequired { .. } => "acceptance_criteria_required",
        StoreError::AcceptanceEvaluationRefused { .. }
        | StoreError::AcceptanceEvaluationAdmissionRefused { .. }
        | StoreError::AcceptanceEvaluationCarriedFailure { .. } => "acceptance_evaluation_refused",
        StoreError::AcceptanceEvaluationBasisMoved { moved, .. } => {
            crate::host::evaluation_basis_move_code(*moved)
        }
        StoreError::GraphDestinationNotEmpty => "graph_destination_not_empty",
        StoreError::GraphProjectMismatch { .. } => "graph_project_mismatch",
        StoreError::GraphDifferentBuild => "different_build",
        StoreError::InvalidGraphSnapshot(_) => "graph_snapshot_corrupt",
        StoreError::Json(_)
        | StoreError::Sqlite(_)
        | StoreError::ImmutableCollision(_)
        | StoreError::ObjectKindMismatch { .. }
        | StoreError::InvalidStoredKey(_)
        | StoreError::InvalidMemoryProjection(_)
        | StoreError::InvalidTaskProjection(_)
        | StoreError::InvalidControlSession(_)
        | StoreError::NamedRootBindingRefused(_)
        | StoreError::SourceBasisTextRefused { .. }
        | StoreError::NamedRootReadRefused(_)
        | StoreError::ExecutionObservationInvalid(_)
        | StoreError::ExecutionObservationBasisMismatch(_)
        | StoreError::ExecutionObservationPolicyBasisMismatch(_)
        | StoreError::AcceptanceBindingReadRefused { .. }
        | StoreError::AcceptanceVerificationReadRefused { .. }
        | StoreError::NamedRootSightingReadRefused { .. }
        | StoreError::HostPathIdentityUnresolved
        | StoreError::ControlSessionNotBound(_)
        | StoreError::ControlSessionTokenMismatch(_)
        | StoreError::ControlConnectionSuperseded(_)
        | StoreError::ControlSessionBindConflict(_)
        | StoreError::ControlTurnIdempotencyConflict(_)
        | StoreError::ControlOperationIdempotencyConflict { .. }
        | StoreError::ControlWorkBindingStale { .. }
        | StoreError::ControlGrantScopeMismatch { .. }
        | StoreError::ControlObservationScopeMismatch { .. }
        | StoreError::VerificationProducerObservationNotFound(_)
        | StoreError::EnvironmentFingerprintMismatch
        | StoreError::EnvironmentEvidenceNotFound(_)
        | StoreError::EnvironmentBasisMismatch(_)
        | StoreError::ControlTurnGrantNotFound(_)
        | StoreError::DifferentBuildSchema
        | StoreError::InvalidControlProjection(_)
        | StoreError::ControlPolicyConflict { .. }
        | StoreError::OpenWorkObligations { .. } => "engram_store_error",
    }
}

/// An argument combination a tool refuses before it runs. Like every tool
/// error Engram itself returns, it carries `reminders` and `next`: the
/// reason, and no command, as a verb error with no specific remedy does. An
/// argument the MCP library rejects first carries text only.
fn invalid_argument(field: &str, message: &str) -> CallToolResult {
    CallToolResult::structured_error(json!({
        "error": {
            "code": "invalid_argument",
            "message": message,
            "details": { "field": field },
            "reminders": [message],
            "next": [],
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    /// The MCP `show` tool pages a verification record's obligation
    /// assessment: `note` gives the first eight, and `note` with `after`
    /// the rest.
    #[test]
    fn show_pages_a_verification_records_assessment_over_mcp() {
        let directory = crate::test_support::temp_home().expect("temp home");
        let database = directory.path().join("work.sqlite3");
        let (work_ref, records) = crate::storage::assessed_verification_fixture(
            &database,
            "mcp-assessment",
            "runner",
            9,
            1,
        );
        let record = &records[0];
        let server = McpServer::new_with_actor_context(
            database,
            ProjectId("mcp-assessment".into()),
            "runner".into(),
            SessionId("runner".into()),
            None,
            None,
        );
        let args = |after: Option<String>| ShowArgs {
            work_ref: work_ref.clone(),
            notes: None,
            gates: None,
            history: None,
            after,
            note: Some(record.as_str().to_owned()),
            full: None,
            evaluations: None,
            evaluation: None,
            observations: None,
        };
        let detail = server
            .show(Parameters(args(None)))
            .structured_content
            .expect("structured detail");
        let block = &detail["note"]["assessment"];
        assert_eq!(
            (block["total"].as_u64(), block["shown"].as_u64()),
            (Some(10), Some(8))
        );
        let token = block["continuation"]
            .as_str()
            .and_then(|command| command.split_once(" --after "))
            .map(|(_, token)| token.to_owned())
            .expect("continuation");
        let rest = server
            .show(Parameters(args(Some(token))))
            .structured_content
            .expect("structured continuation");
        assert_eq!(rest["assessment"]["shown"], 2, "{rest}");
        assert_eq!(rest["assessment"]["earlier"], 8);
    }

    /// The MCP `show` tool gives a native verification record's typed facts,
    /// in the notes window and in the record's detail, beside its summary.
    #[test]
    fn show_gives_a_verification_records_typed_facts_over_mcp() {
        use crate::domain::{ExecutionOutcome, VerificationKind, VerificationResult};
        let directory = crate::test_support::temp_home().expect("temp home");
        let database = directory.path().join("work.sqlite3");
        let (work_ref, record, _) = crate::storage::verification_note_fixture(
            &database,
            "mcp-verification",
            "runner",
            crate::storage::HostCheck {
                key: "mcp-unknown-outcome",
                kind: VerificationKind::Test,
                outcome: ExecutionOutcome::Unknown,
                result: VerificationResult::Indeterminate,
                summary: "all tests passed",
            },
        );
        let server = McpServer::new_with_actor_context(
            database,
            ProjectId("mcp-verification".into()),
            "runner".into(),
            SessionId("runner".into()),
            None,
            None,
        );
        let args = |notes: Option<bool>, note: Option<String>| ShowArgs {
            work_ref: work_ref.clone(),
            notes,
            gates: None,
            history: None,
            after: None,
            note,
            full: None,
            evaluations: None,
            evaluation: None,
            observations: None,
        };
        let window = server
            .show(Parameters(args(Some(true), None)))
            .structured_content
            .expect("structured window");
        let row = window["notes"]
            .as_array()
            .expect("rows")
            .iter()
            .find(|row| row["locator"].as_str() == Some(record.as_str()))
            .expect("the verification row")
            .clone();
        assert_eq!(row["verification"]["result"], "indeterminate");
        assert_eq!(row["verification"]["check_kind"], "test");
        assert_eq!(row["verification"]["source_revision"], "A3");
        assert_eq!(row["verification"]["producer_outcome"], "unknown");
        assert!(
            row["verification"]["meaning"]
                .as_str()
                .expect("plain words")
                .contains("cannot satisfy a passing-check requirement")
        );
        assert_eq!(row["summary"], "all tests passed");
        let detail = server
            .show(Parameters(args(None, Some(record.as_str().to_owned()))))
            .structured_content
            .expect("structured detail");
        assert_eq!(detail["note"]["verification"], row["verification"]);
    }

    #[test]
    fn record_id_descriptions_preserve_mcp_argument_names() {
        let show = serde_json::to_value(schemars::schema_for!(ShowArgs)).unwrap();
        let note = show["properties"]["note"]["description"].as_str().unwrap();
        assert!(note.contains("record id") && note.contains("RECORD_ID:INDEX"));
        assert!(!note.contains("HASH"));

        let evaluate = serde_json::to_value(schemars::schema_for!(EvaluateArgs)).unwrap();
        let verdicts = evaluate["properties"]["verdicts"]["description"]
            .as_str()
            .unwrap();
        assert!(verdicts.contains("full record ids"));
        assert!(!verdicts.contains("full hashes"));
        let verdict_definition = evaluate["properties"]["verdicts"]["items"]["$ref"]
            .as_str()
            .unwrap()
            .strip_prefix('#')
            .unwrap();
        let evidence = &evaluate.pointer(verdict_definition).unwrap()["properties"]["evidence"];
        let description = evidence["description"].as_str().unwrap();
        assert!(description.contains("full record ids"));
        assert!(!description.contains("full hashes"));
        assert_eq!(evidence["type"], "array");
        assert_eq!(evidence["items"]["type"], "string");
        assert!(evaluate["properties"].get("source_fingerprint").is_some());
    }

    #[test]
    fn different_build_refusal_preserves_mcp_wire_code_and_neutral_message() {
        let value = store_error_value(&StoreError::DifferentBuildSchema);
        assert_eq!(value["error"]["code"], "engram_store_error");
        let message = value["error"]["message"].as_str().unwrap();
        assert!(message.contains("use the Engram build that owns this store"));
        assert!(!message.contains("invalid data"));
    }

    // done's refusal for a stale evaluation names the source observation that
    // decided it beside the unchanged cause; a stale refusal for another
    // reason names none. The receipt keeps its code and words.
    #[test]
    fn done_names_the_deciding_observation_beside_the_unchanged_stale_cause() {
        for decided in [true, false] {
            let directory = crate::test_support::temp_home().expect("temporary MCP home");
            let database = directory.path().join("stale-deciding.sqlite3");
            let second = Utc::now().timestamp()
                - chrono::Utc
                    .with_ymd_and_hms(2026, 8, 27, 1, 0, 0)
                    .single()
                    .expect("epoch")
                    .timestamp()
                - 20;
            let fixture = crate::storage::stale_deciding_refusal_fixture(
                &database,
                "mcp-stale-deciding",
                "runner",
                "revision-judged",
                "C:/work/other tree",
                "revision-moved",
                decided,
                second,
            );
            let server = McpServer::new_with_actor_context(
                database,
                ProjectId("mcp-stale-deciding".into()),
                "runner".into(),
                SessionId("runner".into()),
                None,
                None,
            );
            let response = server.done(Parameters(DoneArgs {
                work_ref: Some(fixture.work.short_ref.clone()),
                summary: Some("delivered".into()),
                note: None,
                links: None,
                link_basis: None,
                source_fingerprint: None,
                landing: None,
            }));
            assert_ne!(
                response.is_error,
                Some(true),
                "an owed receipt, not an error"
            );
            let value = response.structured_content.expect("structured receipt");
            assert_eq!(value["code"], "acceptance_evaluation_stale", "{value}");
            let reason = if decided { "mutation" } else { "policy" };
            assert_eq!(
                value["recovery"]["cause"],
                json!(
                    crate::WorkCompletionRecoveryCause::AcceptanceEvaluationStale {
                        reason: if decided {
                            crate::AcceptanceStaleReason::Mutation
                        } else {
                            crate::AcceptanceStaleReason::Policy
                        }
                    }
                ),
                "{value}"
            );
            assert_eq!(value["recovery"]["cause"]["reason"], reason);
            let reminders = value["reminders"].as_array().expect("reminders");
            let named = reminders.iter().any(|reminder| {
                reminder
                    .as_str()
                    .is_some_and(|text| text.contains("The deciding source observation"))
            });
            let observation = &value["recovery"]["deciding_observation"];
            if decided {
                assert_eq!(observation["position"], fixture.position, "{value}");
                assert_eq!(observation["workspace"], "C:/work/other tree");
                assert_eq!(observation["revision"], "revision-moved");
                assert_eq!(observation["source_changed"], true);
                // Labelled as show labels sessions: the caller's own is "you".
                assert_eq!(observation["reporting_session"], "you");
                assert_eq!(observation["evaluated_revision"], "revision-judged");
                assert_eq!(observation["evaluated_revision_declared"], false);
                assert!(named, "{value}");
            } else {
                assert!(observation.is_null(), "{value}");
                assert!(!named, "{value}");
            }
        }
    }

    // B21/B22/B77/B78: storage's deciding source context selects both the
    // service remedy and word guidance, without changing the owed status. The
    // word and plain show bound host-recorded strings; service and raw error
    // details keep them whole.
    #[test]
    fn source_recovery_causes_select_the_same_service_and_mcp_guidance() {
        for case in [
            "unconfirmed",
            "missing",
            "mismatch",
            "no_basis",
            "long_unconfirmed",
            "long_mismatch",
        ] {
            let fixture: crate::storage::SourceRecoveryTransportFixture =
                crate::storage::source_recovery_transport_fixture(case, Utc::now());
            let service = crate::LocalWorkService::new(
                fixture.database.clone(),
                fixture.work.project_id.clone(),
                "runner".into(),
                SessionId("runner".into()),
                None,
            );
            let result = service
                .work_complete_on(
                    Some(&fixture.work.short_ref),
                    crate::WorkCompleteInput {
                        links: vec![],
                        link_basis: None,
                        capture: None,
                        evidence: vec![],
                        acceptance: None,
                        note: None,
                        source_fingerprint: fixture.presented.clone(),
                        landing: None,
                        idempotency_key: String::new(),
                    },
                    Utc::now(),
                )
                .expect("structured service recovery");
            let crate::WorkCompleteResult::Refused(refusal) = result else {
                panic!("{case}: completion must remain owed")
            };
            assert_eq!(refusal.code, "acceptance_evaluation_stale");
            assert_eq!(
                refusal.recovery.cause,
                crate::WorkCompletionRecoveryCause::AcceptanceEvaluationStale {
                    reason: crate::AcceptanceStaleReason::Source,
                }
            );
            let source = refusal.recovery.source.as_ref().expect("source context");
            assert_eq!(source.mismatch, fixture.mismatch);
            assert_eq!(source.evaluation, fixture.evaluation);
            assert_eq!(source.run_id, fixture.work.active_run_id.unwrap());
            assert_eq!(
                refusal.remedy,
                crate::work_service::source_recovery_remedy(source)
            );
            let server = McpServer::new_with_actor_context(
                fixture.database.clone(),
                fixture.work.project_id.clone(),
                "runner".into(),
                SessionId("runner".into()),
                None,
                None,
            );
            let response = server.done(Parameters(DoneArgs {
                work_ref: Some(fixture.work.short_ref.clone()),
                summary: Some("delivered".into()),
                note: None,
                links: None,
                link_basis: None,
                source_fingerprint: fixture.presented.clone(),
                landing: None,
            }));
            assert_ne!(response.is_error, Some(true), "{case}: an owed receipt");
            let value = response
                .structured_content
                .expect("structured word receipt");
            assert_eq!(value["code"], refusal.code);
            assert_eq!(value["recovery"]["cause"], json!(refusal.recovery.cause));
            let shown = crate::work_service::shown_source_recovery(source);
            assert_eq!(value["recovery"]["source"], json!(shown));
            let long_declared = crate::storage::long_declared_source();
            let bounded_declared = format!("{}… (129 bytes stored)", "源".repeat(42));
            if case.starts_with("long_") {
                // The service keeps the declaration whole; the word bounds it.
                assert_eq!(source.declared_revision.as_deref(), Some(&*long_declared));
                assert_eq!(
                    value["recovery"]["source"]["declared_revision"], bounded_declared,
                    "{case}"
                );
            }
            if case == "long_mismatch" {
                let long_presented = crate::storage::long_presented_source();
                assert_eq!(
                    source.expected_fingerprint.as_deref(),
                    Some(&*long_declared)
                );
                assert_eq!(
                    source.presented_fingerprint.as_deref(),
                    Some(&*long_presented)
                );
                assert_eq!(
                    value["recovery"]["source"]["expected_fingerprint"],
                    bounded_declared
                );
                assert_eq!(
                    value["recovery"]["source"]["presented_fingerprint"],
                    format!("{}… (129 bytes stored)", "m".repeat(128))
                );
            }
            // Plain show names the same remedy on its own line, from the
            // bounded projection of the source context it reads.
            let verbs = crate::verbs::AgentVerbs::new(
                fixture.database.clone(),
                fixture.work.project_id.clone(),
                "runner".into(),
                SessionId("runner".into()),
                None,
            );
            let read = verbs
                .show(&fixture.work.short_ref, Utc::now())
                .expect("show the item");
            let status = crate::SqliteStore::open(&fixture.database)
                .expect("open the store")
                .acceptance_evaluation_status(fixture.work.work_id, None)
                .expect("status read")
                .expect("newest evaluation");
            let shown_in_read = read.value["acceptance_evaluation"]["source_recovery"].clone();
            match &status.source_recovery {
                Some(at_read) => {
                    let line =
                        format!("  {}", crate::work_service::source_recovery_remedy(at_read));
                    assert!(
                        read.text().lines().any(|shown| shown == line),
                        "{case}: {}",
                        read.text()
                    );
                    assert_eq!(
                        shown_in_read,
                        json!(crate::work_service::shown_source_recovery(at_read)),
                        "{case}"
                    );
                    if case == "long_unconfirmed" {
                        assert_eq!(at_read.declared_revision.as_deref(), Some(&*long_declared));
                        assert_eq!(shown_in_read["declared_revision"], bounded_declared);
                    }
                }
                None => assert!(shown_in_read.is_null(), "{case}: {shown_in_read}"),
            }
            assert_eq!(
                status.source_recovery.is_some(),
                matches!(case, "unconfirmed" | "no_basis" | "long_unconfirmed"),
                "{case}"
            );
            assert!(value["reminders"].as_array().unwrap().iter().any(|line| {
                line.as_str()
                    .is_some_and(|text| text.contains(&refusal.remedy))
            }));
            assert!(
                value["next"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|command| command.as_str().unwrap().contains(&fixture.work.short_ref))
            );
            let raw = StoreError::WorkCompletionRecoveryRequired {
                work: fixture.work.work_id,
                cause: refusal.recovery.cause,
                context: Box::new(crate::storage::StaleRecoveryContext {
                    source: Some(source.clone()),
                    ..Default::default()
                }),
            };
            let legacy = StoreError::WorkCompletionRecoveryRequired {
                work: fixture.work.work_id,
                cause: crate::WorkCompletionRecoveryCause::AcceptanceEvaluationStale {
                    reason: crate::AcceptanceStaleReason::Source,
                },
                context: Box::default(),
            };
            assert_eq!(raw.to_string(), legacy.to_string());
            let raw_json = store_error_value(&raw);
            assert_eq!(
                raw_json["error"]["code"],
                "work_completion_recovery_required"
            );
            assert_eq!(raw_json["error"]["details"]["source"], json!(source));
            if case.starts_with("long_") {
                assert_eq!(
                    raw_json["error"]["details"]["source"]["declared_revision"],
                    long_declared
                );
            }
        }
    }

    #[test]
    fn evaluation_admission_errors_keep_status_and_deciding_causes_across_service_and_mcp() {
        for case in [
            "eligibility",
            "source_root",
            "wrong_run",
            "beyond_cut",
            "wrong_source",
            "wrong_basis",
        ] {
            let fixture: crate::storage::AdmissionTransportFixture =
                crate::storage::admission_transport_fixture(case, Utc::now());
            let service = crate::LocalWorkService::new(
                fixture.database.clone(),
                fixture.work.project_id.clone(),
                "runner".into(),
                SessionId("runner".into()),
                None,
            );
            let error = service
                .work_evaluate_on(&fixture.input, Utc::now())
                .expect_err(case);
            let shared = store_error_value(&error);
            assert_eq!(
                shared["error"]["details"]["cause"]["kind"], fixture.family,
                "{case}: {shared}"
            );
            assert_eq!(
                shared["error"]["details"]["cause"]["mismatch"], fixture.mismatch,
                "{case}: {shared}"
            );
            let server = McpServer::new_with_actor_context(
                fixture.database.clone(),
                fixture.work.project_id.clone(),
                "runner".into(),
                SessionId("runner".into()),
                None,
                None,
            );
            let input = &fixture.input;
            let response = server.evaluate(Parameters(EvaluateArgs {
                work_ref: input.work_ref.clone(),
                mode: input.mode.clone(),
                acceptance_basis: input.acceptance_basis,
                evidence_basis: input.evidence_basis,
                verdicts: input.verdicts.clone(),
                attempt: input.attempt.clone(),
                source_fingerprint: input.source_fingerprint.clone(),
                model: input.model.clone(),
                execution_identity: input.execution_identity.clone(),
                parent_session: input.parent_session.clone(),
                supersedes: input.supersedes.clone(),
            }));
            assert_eq!(response.is_error, Some(true), "{case}");
            let value = response
                .structured_content
                .expect("structured admission error");
            let error = &value["error"];
            assert_eq!(error["code"], "acceptance_evaluation_refused");
            assert_eq!(error["message"], shared["error"]["message"]);
            assert_eq!(error["details"], shared["error"]["details"]);
            let cause: crate::AcceptanceEvaluationAdmissionCause =
                serde_json::from_value(error["details"]["cause"].clone()).unwrap();
            let remedy = crate::work_service::evaluation_admission_remedy(&cause);
            assert_eq!(error["details"]["remedy"], remedy);
            if case == "wrong_basis" {
                // The basis is the fault: the valid check it cites is not
                // named, and the remedy states the admissible pass.
                assert_eq!(error["details"]["cause"]["citation"], "", "{case}");
                assert!(
                    remedy.contains("uses basis observed, and every citation of it is a passed host-minted verification"),
                    "{remedy}"
                );
            }
            assert!(
                error["reminders"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|entry| entry == &json!(remedy))
            );
            assert!(
                error["next"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|entry| entry.as_str().unwrap().contains(&fixture.work.short_ref))
            );
            let store = crate::SqliteStore::open(&fixture.database).unwrap();
            assert!(
                store
                    .acceptance_evaluation_status(fixture.work.work_id, None)
                    .unwrap()
                    .is_none()
            );
        }
    }

    #[test]
    fn done_preserves_bound_check_error_status_and_exposes_typed_guidance() {
        for result in [
            crate::VerificationResult::Passed,
            crate::VerificationResult::Failed,
            crate::VerificationResult::Indeterminate,
        ] {
            let directory = crate::test_support::temp_home().expect("temporary MCP home");
            let database = directory.path().join("bound-refusal.sqlite3");
            let second = Utc::now().timestamp()
                - chrono::Utc
                    .with_ymd_and_hms(2026, 8, 27, 1, 0, 0)
                    .single()
                    .expect("epoch")
                    .timestamp()
                - 10;
            let fixture = crate::storage::bound_verification_refusal_fixture(
                &database,
                "mcp-bound-refusal",
                "runner",
                result,
                second,
            );
            let server = McpServer::new_with_actor_context(
                database,
                ProjectId("mcp-bound-refusal".into()),
                "runner".into(),
                SessionId("runner".into()),
                None,
                None,
            );
            let response = server.done(Parameters(DoneArgs {
                work_ref: Some(fixture.work.short_ref),
                summary: Some("delivered".into()),
                note: None,
                links: None,
                link_basis: None,
                source_fingerprint: None,
                landing: None,
            }));
            assert_eq!(response.is_error, Some(true));
            let value = response.structured_content.expect("structured refusal");
            let error = &value["error"];
            assert_eq!(error["code"], "work_completion_refused");
            let details = &error["details"];
            let cause: crate::WorkBoundVerificationCause =
                serde_json::from_value(details["cause"].clone()).expect("typed cause");
            assert_eq!(cause.criterion, 1);
            assert_eq!(cause.requirement.check_kind, crate::VerificationKind::Build);
            assert_eq!(cause.verification, fixture.verification);
            assert_eq!(cause.satisfied_by, fixture.satisfied_by);
            assert_eq!(cause.result, result);
            let (mismatch, remedy) = if result == crate::VerificationResult::Passed {
                (
                    crate::VerificationEvidenceMismatch::StaleSourceRevision,
                    crate::BoundVerificationRemedy::RunCurrentCheck,
                )
            } else {
                (
                    crate::VerificationEvidenceMismatch::ResultNotPassed,
                    crate::BoundVerificationRemedy::RunPassingCheckAfter,
                )
            };
            assert_eq!(cause.mismatch, mismatch);
            assert_eq!(cause.remedy, remedy);
            // A stale check names the change it must follow, in the typed
            // cause and as one sentence after the reason; no other does.
            let reason = details["reason"].as_str().expect("reason");
            if result == crate::VerificationResult::Passed {
                let source = cause.stale_source.as_ref().expect("the deciding record");
                assert_eq!(
                    source.decider,
                    crate::domain::StaleSourceDecider::LatestChange
                );
                assert_eq!(source.source_changed, Some(true));
                assert_eq!(details["cause"]["stale_source"]["decider"], "latest_change");
                assert!(
                    error["reminders"]
                        .as_array()
                        .expect("word reminders")
                        .iter()
                        .any(|entry| entry == &json!(source.sentence())),
                    "{error}"
                );
            } else {
                assert_eq!(cause.stale_source, None);
                assert!(details["cause"].get("stale_source").is_none());
                assert!(!reason.contains("deciding source record"), "{reason}");
            }
            let legacy = StoreError::WorkCompletionRefused {
                work: fixture.work.work_id,
                reason: details["reason"].as_str().expect("reason").into(),
            };
            assert_eq!(error["message"], legacy.to_string());
            let guidance = crate::work_service::bound_verification_remedy(&cause);
            assert_eq!(details["remedy"], guidance);
            assert!(
                error["reminders"]
                    .as_array()
                    .expect("word reminders")
                    .iter()
                    .any(|entry| entry == &json!(guidance))
            );
            // Native CLI JSON uses this same formatter. No adapter extracts a
            // cause from the human message, whose bytes remain unchanged.
            let typed = StoreError::WorkBoundVerificationRefused {
                work: fixture.work.work_id,
                reason: details["reason"].as_str().unwrap().into(),
                cause: Box::new(cause),
            };
            let shared = store_error_value(&typed);
            assert_eq!(shared["error"]["code"], error["code"]);
            assert_eq!(shared["error"]["message"], error["message"]);
            assert_eq!(shared["error"]["details"], error["details"]);
        }
    }

    #[test]
    fn ambiguous_work_reference_has_a_stable_mcp_error_code() {
        let work_id = crate::WorkId::new();
        let error = StoreError::WorkReferenceAmbiguous {
            reference: "w-collision".into(),
            candidates: vec![crate::WorkReferenceCandidate {
                work_id,
                short_ref: "w-collision".into(),
                title: "Collision candidate".into(),
                lifecycle: crate::WorkLifecycle::Open,
            }],
            more: 2,
        };
        assert_eq!(error_code(&error), "work_reference_ambiguous");
        let value = store_error_value(&error);
        let details = &value["error"]["details"];
        assert_eq!(details["reference"], "w-collision");
        assert_eq!(details["candidates"][0]["work_id"], work_id.0.to_string());
        assert_eq!(details["candidates"][0]["ref"], "w-collision");
        assert_eq!(details["candidates"][0]["title"], "Collision candidate");
        assert_eq!(details["candidates"][0]["state"], "open");
        assert_eq!(details["more"], 2);
    }

    #[test]
    fn implicit_target_refusal_has_a_stable_code_and_names_both_items() {
        let error = StoreError::WorkImplicitTargetConflict(Box::new(
            crate::storage::ImplicitTargetConflict {
                operation: "note".into(),
                focus: "w-added".into(),
                focus_state: crate::storage::ImplicitFocusState::Unclaimed,
                held: vec!["w-held".into()],
                more: 2,
            },
        ));
        assert_eq!(error_code(&error), "work_implicit_target_conflict");
        let value = store_error_value(&error);
        let details = &value["error"]["details"];
        assert_eq!(details["operation"], "note");
        assert_eq!(details["focused_ref"], "w-added");
        assert_eq!(details["focus_state"], "unclaimed");
        assert_eq!(details["held_refs"], json!(["w-held"]));
        assert_eq!(details["more"], 2);
        let message = value["error"]["message"].as_str().expect("message");
        assert!(
            message.contains("w-added") && message.contains("w-held and 2 more"),
            "{message}"
        );
        assert!(message.contains("nothing was recorded"), "{message}");
    }

    #[test]
    fn satisfied_prerequisite_refusal_has_actionable_structured_details() {
        let work_id = crate::WorkId::new();
        let error = StoreError::WorkPrerequisiteAlreadySatisfied(work_id);
        assert_eq!(error_code(&error), "work_prerequisite_already_satisfied");
        let value = store_error_value(&error);
        let details = &value["error"]["details"];
        assert_eq!(details["work_id"], work_id.0.to_string());
        assert_eq!(
            details["remedy"],
            "no edge is needed; run show for the prerequisite before choosing another action"
        );
    }

    #[test]
    fn invalid_context_generation_has_a_specific_mcp_remedy() {
        let error = StoreError::InvalidProjectMemory(
            "context_generation must be 1 to 256 ASCII letters, digits, dots, underscores or dashes, and must not start with a dash".into(),
        );
        let value = store_error_value(&error);
        assert_eq!(
            value["error"]["details"]["remedy"],
            "omit context_generation or use 1 to 256 ASCII letters, digits, dots, underscores or dashes, not starting with a dash"
        );
    }

    #[test]
    fn process_default_reuse_refusal_has_a_non_looping_structured_remedy() {
        let error = StoreError::InvalidWork(PROCESS_DEFAULT_WORK_SESSION_REUSE_REFUSAL.into());
        let value = store_error_value(&error);
        assert_eq!(
            value["error"]["details"]["reason"],
            PROCESS_DEFAULT_WORK_SESSION_REUSE_REFUSAL
        );
        assert_eq!(
            value["error"]["details"]["remedy"],
            PROCESS_DEFAULT_WORK_SESSION_REUSE_REFUSAL
        );
    }

    #[test]
    fn completed_holder_word_refusal_points_to_late_note_without_reopening() {
        let error = StoreError::InvalidWork(COMPLETED_WORK_LATE_FINDING_REFUSAL.into());
        let value = store_error_value(&error);
        assert_eq!(
            value["error"]["details"]["reason"],
            COMPLETED_WORK_LATE_FINDING_REFUSAL
        );
        let remedy = value["error"]["details"]["remedy"]
            .as_str()
            .expect("late-finding remedy");
        assert_eq!(
            remedy,
            "use note to record a late finding without reopening the completed item"
        );
        assert!(!remedy.contains("next"));
    }

    #[test]
    fn retained_work_service_survives_failure_for_agent_tools() {
        let directory = crate::test_support::temp_home().expect("temporary MCP home");
        let server = McpServer::new_with_actor_context(
            directory.path().join("engram.sqlite3"),
            ProjectId("mcp-retained-service".into()),
            "agent".into(),
            SessionId("mcp-retained-session".into()),
            Some("mcp-test".into()),
            None,
        );

        server
            .verbs()
            .next(
                &NextInput {
                    limit: Some(5),
                    peek: false,
                    verbose: false,
                    context_generation: None,
                },
                Utc::now(),
            )
            .expect("agent tool initializes the retained service");

        let refused = server.verbs().add(
            AddInput {
                title: " ".into(),
                ..AddInput::default()
            },
            Utc::now(),
        );
        assert!(refused.is_err());
        assert!(format!("{:?}", server.work_service).contains("store_initialized: true"));

        server
            .verbs()
            .next(
                &NextInput {
                    limit: Some(5),
                    peek: false,
                    verbose: false,
                    context_generation: None,
                },
                Utc::now(),
            )
            .expect("agent tool remains usable after refusal");
        let cloned_handler = server.clone();
        assert!(Arc::ptr_eq(
            &server.work_service,
            &cloned_handler.work_service
        ));
    }

    #[test]
    fn mcp_add_refuses_an_unknown_argument_and_names_its_acceptance_field() {
        let refused = serde_json::from_value::<AddArgs>(json!({
            "title": "Misspelled criteria",
            "accept": ["Criterion"],
        }))
        .expect_err("an argument add does not list is refused");
        let message = refused.to_string();
        assert!(
            message.contains("unknown field `accept`, expected one of"),
            "{message}"
        );
        assert!(message.contains("`acceptance`"), "{message}");

        let directory = crate::test_support::temp_home().expect("temporary MCP home");
        let server = McpServer::new_with_actor_context(
            directory.path().join("engram.sqlite3"),
            ProjectId("mcp-acceptance-field".into()),
            "agent".into(),
            SessionId("mcp-acceptance-session".into()),
            Some("mcp-test".into()),
            None,
        );
        let added = server
            .verbs()
            .add(
                AddInput {
                    title: "Defaulted criteria".into(),
                    ..AddInput::default()
                },
                Utc::now(),
            )
            .expect("add with defaulted acceptance");
        let reminders = added.value["reminders"]
            .as_array()
            .expect("reminders")
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>();
        assert!(
            reminders.contains(&"acceptance defaulted to the title being done; set acceptance"),
            "{reminders:?}"
        );
        assert!(
            reminders.iter().all(|line| !line.contains("--accept")),
            "{reminders:?}"
        );
    }

    #[test]
    fn oversized_mcp_session_refuses_the_first_tool_without_opening_the_store() {
        let directory = crate::test_support::temp_home().expect("temporary MCP home");
        let giant = "m".repeat(65);
        let server = McpServer::new_with_actor_context(
            directory.path().join("engram.sqlite3"),
            ProjectId("mcp-oversized-session".into()),
            "agent".into(),
            SessionId(giant.clone()),
            Some("mcp-test".into()),
            None,
        );
        let refused = server.verbs().next(
            &NextInput {
                limit: Some(5),
                peek: true,
                verbose: false,
                context_generation: None,
            },
            Utc::now(),
        );
        let error = refused.expect_err("oversized MCP session");
        let message = error.to_string();
        assert!(
            message.contains(crate::SessionIdAdmissionError::TooLong.as_str()),
            "{message}"
        );
        assert!(!message.contains(&giant));
        assert!(format!("{:?}", server.work_service).contains("store_initialized: false"));
    }

    // Every update argument Engram refuses before the tool runs carries the
    // two fields every tool error Engram itself returns does: its reason, and
    // no command.
    #[test]
    fn invalid_argument_errors_carry_reminders_and_next() {
        let directory = crate::test_support::temp_home().expect("temporary MCP home");
        let server = McpServer::new_with_actor_context(
            directory.path().join("engram.sqlite3"),
            ProjectId("mcp-invalid-argument".into()),
            "agent".into(),
            SessionId("agent".into()),
            None,
            None,
        );
        for (field, arguments) in [
            (
                "external",
                json!({ "action": "cancel", "external": "planner:x" }),
            ),
            (
                "clear_external",
                json!({ "action": "cancel", "clear_external": true }),
            ),
            (
                "acceptance",
                json!({ "action": "release", "acceptance": ["x"] }),
            ),
            ("bindings", json!({ "action": "release", "bindings": [] })),
            (
                "blocker",
                json!({ "action": "release", "blocker": "w-000000000001" }),
            ),
            (
                "evaluation_mode",
                json!({ "action": "revise", "evaluation_mode": "same_session" }),
            ),
            (
                "defer",
                json!({ "action": "revise", "defer": "not a date" }),
            ),
        ] {
            let mut arguments = arguments;
            arguments["work_ref"] = json!("w-000000000001");
            let refused = server.update(Parameters(
                serde_json::from_value(arguments).expect("update arguments"),
            ));
            assert_eq!(refused.is_error, Some(true), "{field}");
            let error = &refused.structured_content.expect("structured error")["error"];
            assert_eq!(error["code"], "invalid_argument", "{field}");
            assert_eq!(error["details"]["field"], field);
            assert_eq!(error["reminders"], json!([error["message"]]), "{field}");
            assert_eq!(error["next"], json!([]), "{field}");
        }
    }
}
