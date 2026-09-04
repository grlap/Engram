//! MCP stdio surface for the fourteen agent-facing work tools.

use std::{path::PathBuf, sync::Arc};

use chrono::Utc;
use rmcp::{
    ServerHandler,
    handler::server::{router::tool::ToolRouter, wrapper::Parameters},
    model::CallToolResult,
    schemars::JsonSchema,
    tool, tool_handler, tool_router,
};
use serde::Deserialize;
use serde_json::{Value, json};

use crate::{
    AddInput, AgentVerbs, ClaimInput, DoneInput, ForgetInput, GateInput, HandoffAction,
    HandoffInput, LocalWorkService, LsInput, MemoriesInput, NextInput, NoteInput, ProjectId,
    Receipt, RememberInput, SessionId, UpdateAction, UpdateInput, VerbError, WorkItemKind,
    parse_defer_date,
    storage::{PROCESS_DEFAULT_WORK_SESSION_REUSE_REFUSAL, StoreError},
};

/// Immutable host context asserted for one MCP connection.
#[derive(Clone, Debug)]
pub struct McpServer {
    actor_id: String,
    session_id: SessionId,
    work_service: Arc<LocalWorkService>,
    tool_router: ToolRouter<Self>,
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
        let work_service = Arc::new(LocalWorkService::new_with_attribution(
            database,
            project_id,
            actor_id.clone(),
            session_id.clone(),
            source_skill,
            actor_context,
            crate::WorkAttributionDefaults::default(),
        ));
        Self {
            actor_id,
            session_id,
            work_service,
            tool_router: Self::agent_tool_router(),
        }
    }

    fn verbs(&self) -> AgentVerbs {
        AgentVerbs::with_shared_service(
            Arc::clone(&self.work_service),
            self.actor_id.clone(),
            self.session_id.clone(),
        )
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct NextArgs {
    /// Maximum ready items and changes to return (default 20).
    limit: Option<u32>,
    /// Return the full structured projection instead of compact rows.
    verbose: Option<bool>,
    /// Asserted host/client context generation; a new value may reannounce project memories.
    #[schemars(length(max = 256))]
    context_generation: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct LsArgs {
    /// Case-insensitive text over refs, titles, outcomes, and labels.
    search: Option<String>,
    /// Only items with an active blocker or incomplete prerequisite.
    blocked: Option<bool>,
    /// Only items assigned to this actor or held by this session.
    mine: Option<bool>,
    /// Include completed, cancelled, and superseded items.
    all: Option<bool>,
    /// Exact case-insensitive label.
    label: Option<String>,
    /// Maximum items to return (default 20).
    limit: Option<u32>,
    /// Return the full structured projection instead of compact rows.
    verbose: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ShowArgs {
    /// Short work ref or full UUID; becomes the focus for later calls.
    work_ref: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct AddArgs {
    /// Only required field.
    title: String,
    /// Defaults to the title.
    outcome: Option<String>,
    /// Acceptance criteria `done` is checked against; defaults to one
    /// criterion "<title> is done".
    acceptance: Option<Vec<String>>,
    /// Add as a child of this item instead of a root.
    under: Option<String>,
    /// Make the child optional for parent completion. Requires `under`.
    optional: Option<bool>,
    /// 0 (highest) through 4.
    priority: Option<i32>,
    labels: Option<Vec<String>>,
    assignee: Option<String>,
    /// task, bug, feature, epic, chore, or research.
    kind: Option<WorkItemKind>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WorkClaimArgs {
    /// Short work ref or full UUID.
    work_ref: String,
    /// Claim lifetime in seconds (default one hour).
    ttl_seconds: Option<i64>,
    /// Attributed reason for recovering a lapsed prior claim.
    recover: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum UpdateActionArg {
    Release,
    Blocked,
    Unblock,
    Revise,
    Cancel,
    After,
    DropAfter,
    Waive,
    Supersede,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct UpdateArgs {
    /// Item to act on; defaults to the focus.
    work_ref: Option<String>,
    /// `release`, `blocked`, `unblock`, `revise`, `cancel`, `after`,
    /// `drop_after`, `waive`, or `supersede`.
    action: UpdateActionArg,
    /// Reason for release (optional), cancel, waive, or supersede (required).
    reason: Option<String>,
    /// Why the item is blocked.
    text: Option<String>,
    title: Option<String>,
    outcome: Option<String>,
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
    /// Cancelled or superseded required child for `waive`.
    child: Option<String>,
    /// Replacement item for supersede.
    replacement: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GateArgs {
    /// Stable gate name, normalized case-insensitively.
    name: String,
    /// Failure labels (test ids or check names). Omit only when the gate passed.
    failed: Option<Vec<String>>,
    /// Bounded opaque external-evidence reference; a path or URL by convention.
    evidence_ref: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RememberArgs {
    /// Project note or observation. Never include credentials or secrets.
    #[schemars(length(max = 8192))]
    text: String,
    /// Safe permanent key; omitted to derive a slug from the first words.
    #[schemars(length(max = 64))]
    key: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct MemoriesArgs {
    /// Search text, or the exact key when full is true.
    #[schemars(length(max = 256))]
    query: Option<String>,
    /// Continue an unfiltered key-ordered listing.
    #[schemars(length(max = 64))]
    after: Option<String>,
    /// Return one dedicated full body for the positional key.
    full: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ForgetArgs {
    /// Permanently reserved project-memory key to tombstone.
    #[schemars(length(max = 64))]
    key: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct NoteArgs {
    /// Item to note on; defaults to the focus.
    work_ref: Option<String>,
    /// What you found or decided.
    text: String,
    /// Evidence pointers such as paths or URLs.
    refs: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DoneArgs {
    /// Item to complete; defaults to the focus.
    work_ref: Option<String>,
    /// What was delivered; recorded and checkpointed before sealing.
    summary: Option<String>,
    /// Acceptance note recorded against every criterion.
    note: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
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
struct HandoffArgs {
    /// Item to hand off; defaults to the focus.
    work_ref: Option<String>,
    /// offer (with to), accept, or cancel (with reason).
    action: HandoffActionArg,
    /// Session that receives the item.
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
        description = "What is ready, what you hold, and what changed since your last call"
    )]
    fn next(&self, Parameters(args): Parameters<NextArgs>) -> CallToolResult {
        verb(self.verbs().next(
            &NextInput {
                limit: args.limit,
                verbose: args.verbose.unwrap_or(false),
                context_generation: args.context_generation,
            },
            Utc::now(),
        ))
    }

    /// List open work with flat filters.
    #[tool(
        name = "ls",
        description = "List open work; search, blocked, mine, all, and label narrow it"
    )]
    fn ls(&self, Parameters(args): Parameters<LsArgs>) -> CallToolResult {
        verb(self.verbs().ls(
            &LsInput {
                search: args.search,
                blocked: args.blocked.unwrap_or(false),
                mine: args.mine.unwrap_or(false),
                all: args.all.unwrap_or(false),
                label: args.label,
                limit: args.limit,
                verbose: args.verbose.unwrap_or(false),
            },
            Utc::now(),
        ))
    }

    /// Inspect one item and make it the focus.
    #[tool(
        name = "show",
        description = "One item: outcome, acceptance, holder, blockers, reminders; later calls default to it"
    )]
    fn show(&self, Parameters(args): Parameters<ShowArgs>) -> CallToolResult {
        verb(self.verbs().show(&args.work_ref, Utc::now()))
    }

    /// Create a root or one required/optional child.
    #[tool(
        name = "add",
        description = "Create work from a title; under adds a child and optional makes it non-blocking"
    )]
    fn add(&self, Parameters(args): Parameters<AddArgs>) -> CallToolResult {
        verb(self.verbs().add(
            AddInput {
                title: args.title,
                outcome: args.outcome,
                acceptance: args.acceptance.unwrap_or_default(),
                under: args.under,
                optional: args.optional.unwrap_or(false),
                priority: args.priority,
                labels: args.labels.unwrap_or_default(),
                assignee: args.assignee,
                kind: args.kind,
            },
            Utc::now(),
        ))
    }

    /// Hold an item.
    #[tool(
        name = "claim",
        description = "Hold an item before changing anything; later calls default to it"
    )]
    fn claim(&self, Parameters(args): Parameters<WorkClaimArgs>) -> CallToolResult {
        verb(self.verbs().claim(
            ClaimInput {
                work_ref: args.work_ref,
                ttl_seconds: args.ttl_seconds,
                recover: args.recover,
            },
            Utc::now(),
        ))
    }

    /// Apply exactly one planning or claim action.
    #[tool(
        name = "update",
        description = "One action: release, blocked, unblock, revise, cancel, after/drop_after (prerequisite), waive (child plus reason), or supersede (replacement plus reason)"
    )]
    fn update(&self, Parameters(args): Parameters<UpdateArgs>) -> CallToolResult {
        let action = match args.action {
            UpdateActionArg::Release => UpdateAction::Release {
                reason: args.reason,
            },
            UpdateActionArg::Blocked => UpdateAction::Blocked {
                detail: args.text.unwrap_or_default(),
            },
            UpdateActionArg::Unblock => UpdateAction::Unblock,
            UpdateActionArg::Revise => {
                let defer = match args.defer.as_deref().map(parse_defer_date).transpose() {
                    Ok(defer) => defer,
                    Err(message) => return invalid_argument("defer", &message),
                };
                UpdateAction::Revise {
                    title: args.title,
                    outcome: args.outcome,
                    assignee: args.assignee,
                    priority: args.priority,
                    defer,
                    kind: args.kind,
                    labels: args.labels.unwrap_or_default(),
                    unlabels: args.unlabels.unwrap_or_default(),
                }
            }
            UpdateActionArg::Cancel => UpdateAction::Cancel {
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
            UpdateActionArg::Supersede => UpdateAction::Supersede {
                replacement: args.replacement.unwrap_or_default(),
                reason: args.reason.unwrap_or_default(),
            },
        };
        verb(self.verbs().update(
            UpdateInput {
                work_ref: args.work_ref,
                action,
            },
            Utc::now(),
        ))
    }

    /// Record one bounded gate observation on the focused item you hold.
    #[tool(
        name = "gate",
        description = "Record a gate pass or bounded failure-label list on the item you hold; evidence_ref is opaque (a path or URL by convention) and never ingested"
    )]
    fn gate(&self, Parameters(args): Parameters<GateArgs>) -> CallToolResult {
        verb(self.verbs().gate(
            GateInput {
                name: args.name,
                failed: args.failed.unwrap_or_default(),
                evidence_ref: args.evidence_ref,
            },
            Utc::now(),
        ))
    }

    /// Store one attributed project memory.
    #[tool(
        name = "remember",
        description = "Store one attributed project note under a safe permanent key; no work focus or claim changes"
    )]
    fn remember(&self, Parameters(args): Parameters<RememberArgs>) -> CallToolResult {
        verb(self.verbs().remember(
            RememberInput {
                text: args.text,
                key: args.key,
            },
            Utc::now(),
        ))
    }

    /// List, search, or fully read project memories.
    #[tool(
        name = "memories",
        description = "List compact project-memory rows, search them, or set full with an exact key to read one body"
    )]
    fn memories(&self, Parameters(args): Parameters<MemoriesArgs>) -> CallToolResult {
        verb(self.verbs().memories(
            &MemoriesInput {
                query: args.query,
                after: args.after,
                full: args.full.unwrap_or(false),
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
        verb(
            self.verbs()
                .forget(ForgetInput { key: args.key }, Utc::now()),
        )
    }

    /// Record one finding once.
    #[tool(
        name = "note",
        description = "Record one finding, decision, or evidence pointer on the item you hold; it feeds peers, handoff, and the final report"
    )]
    fn note(&self, Parameters(args): Parameters<NoteArgs>) -> CallToolResult {
        verb(self.verbs().note(
            &NoteInput {
                work_ref: args.work_ref,
                text: args.text,
                refs: args.refs.unwrap_or_default(),
            },
            Utc::now(),
        ))
    }

    /// Complete the held item.
    #[tool(
        name = "done",
        description = "Complete the item you hold; a refusal says what is still owed and the command that resolves it"
    )]
    fn done(&self, Parameters(args): Parameters<DoneArgs>) -> CallToolResult {
        verb(self.verbs().done(
            DoneInput {
                work_ref: args.work_ref,
                summary: args.summary,
                note: args.note,
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
        verb(self.verbs().search(&args.query, args.limit, Utc::now()))
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
        verb(self.verbs().handoff(
            HandoffInput {
                work_ref: args.work_ref,
                action,
            },
            Utc::now(),
        ))
    }
}

#[allow(
    clippy::unused_async_trait_impl,
    reason = "the rmcp handler macro emits required async trait methods"
)]
#[tool_handler(
    router = self.tool_router,
    name = "engram",
    version = "0.1.0",
    instructions = "Thirteen words: next, ls, show, add, claim, update, gate, note, done, handoff, remember, memories, forget (plus search). add needs only a title; claim before changing work; gate records bounded pass/fail evidence; remember stores attributed project notes; memories is their source of truth; forget tombstones rather than erases. Every answer ends with reminders and runnable next commands. Identical calls are safe to repeat."
)]
impl ServerHandler for McpServer {}

fn verb(outcome: Result<Receipt, VerbError>) -> CallToolResult {
    match outcome {
        Ok(receipt) => CallToolResult::structured(receipt.value),
        Err(error) => {
            let guidance = error.guidance();
            let mut value = store_error_value(&error.error);
            value["error"]["reminders"] = json!(guidance.reminders);
            value["error"]["next"] = json!(guidance.next);
            CallToolResult::structured_error(value)
        }
    }
}

/// Stable structured rendering shared by MCP and native JSON/core errors.
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "one shared structured renderer keeps every CLI and MCP error surface identical"
)]
pub fn store_error_value(error: &StoreError) -> Value {
    let details = match error {
        StoreError::TaskClaimHeld { holder, expires_at } => json!({
            "holder": holder,
            "expires_at_ms": expires_at,
            "expires_at": chrono::DateTime::<Utc>::from_timestamp_millis(*expires_at)
                .map(|value| value.to_rfc3339()),
        }),
        StoreError::NoteIdempotencyConflict(key)
        | StoreError::ClaimIdempotencyConflict(key)
        | StoreError::ContradictionIdempotencyConflict(key) => {
            json!({ "idempotency_key": key })
        }
        StoreError::ContradictionAlreadyRecorded(hash) => {
            json!({ "contradiction_hash": hash })
        }
        StoreError::PinnedContradiction {
            contradiction,
            left,
            right,
        } => json!({
            "contradiction_hash": contradiction,
            "left_version": left,
            "right_version": right,
        }),
        StoreError::TaskAccessDenied { task, session } => json!({
            "task_id": task.0,
            "session_id": session,
        }),
        StoreError::MemoryAccessDenied(hash)
        | StoreError::MemoryNotFound(hash)
        | StoreError::PacketAccessDenied(hash) => json!({ "object_hash": hash }),
        StoreError::ProjectMemoryExists(key) => json!({
            "key": key,
            "remedy": format!("run memories {key} --full or choose another --key"),
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
                "remedy": "omit context_generation or use at most 256 bytes without control characters",
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
        StoreError::InvalidWork(message)
            if message == PROCESS_DEFAULT_WORK_SESSION_REUSE_REFUSAL =>
        {
            json!({
                "reason": message,
                "remedy": PROCESS_DEFAULT_WORK_SESSION_REUSE_REFUSAL,
            })
        }
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
        StoreError::WorkDependencyCycle => json!({
            "remedy": "remove or change the prerequisite edge that introduces the cycle",
        }),
        StoreError::WorkNotOpen(work) => json!({
            "work_id": work,
            "remedy": "run show for the affected item and follow next",
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
        StoreError::WorkCompletionRefused { work, reason } => json!({
            "work_id": work,
            "reason": reason,
            "remedy": "record evidence, checkpoint the current feed cut, and satisfy every current acceptance criterion",
        }),
        StoreError::WorkCompletionRecoveryRequired { work, cause } => json!({
            "work_id": work,
            "cause": cause,
        }),
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
        StoreError::TaskClaimHeld { .. } => "task_claim_held",
        StoreError::NoteIdempotencyConflict(_) => "note_idempotency_conflict",
        StoreError::ClaimIdempotencyConflict(_) => "claim_idempotency_conflict",
        StoreError::ContradictionIdempotencyConflict(_) => "contradiction_idempotency_conflict",
        StoreError::ContradictionAlreadyRecorded(_) => "contradiction_already_recorded",
        StoreError::InvalidContradiction(_) => "invalid_contradiction",
        StoreError::PinnedContradiction { .. } => "pinned_contradiction",
        StoreError::NoActiveTask(_) => "no_active_task",
        StoreError::TaskReferenceNotFound(_) => "task_reference_not_found",
        StoreError::TaskAccessDenied { .. } => "task_access_denied",
        StoreError::MemoryAccessDenied(_) => "memory_access_denied",
        StoreError::MemoryNotFound(_) | StoreError::ProjectMemoryNotFound(_) => "memory_not_found",
        StoreError::ProjectMemoryExists(_) => "memory_exists",
        StoreError::ProjectMemoryRetired(_) => "memory_retired",
        StoreError::ProjectMemoryBindingInvalid => "memory_binding_invalid",
        StoreError::InvalidProjectMemory(_) => "memory_invalid",
        StoreError::PacketAccessDenied(_) => "packet_access_denied",
        StoreError::PinnedBudgetExceeded { .. } => "pinned_budget_exceeded",
        StoreError::EmptyNote => "empty_note",
        StoreError::RedactionRefused(_) => "redaction_refused",
        StoreError::WorkNotFound(_) => "work_not_found",
        StoreError::WorkReferenceAmbiguous { .. } => "work_reference_ambiguous",
        StoreError::InvalidWork(_) => "work_invalid",
        StoreError::InvalidWorkProjection(_) => "work_projection_invalid",
        StoreError::WorkRevisionConflict { .. } => "work_revision_conflict",
        StoreError::WorkOperationIdempotencyConflict { .. } => "work_idempotency_conflict",
        StoreError::WorkDependencyCycle => "work_dependency_cycle",
        StoreError::WorkPrerequisiteAlreadySatisfied(_) => "work_prerequisite_already_satisfied",
        StoreError::WorkNotOpen(_) => "work_not_open",
        StoreError::WorkClaimHeld { .. } => "work_claim_held",
        StoreError::WorkClaimMismatch { .. } => "work_claim_mismatch",
        StoreError::WorkClaimLapsed { .. } => "work_claim_lapsed",
        StoreError::WorkCompletionRefused { .. } => "work_completion_refused",
        StoreError::WorkCompletionRecoveryRequired { .. } => "work_completion_recovery_required",
        StoreError::Json(_)
        | StoreError::Sqlite(_)
        | StoreError::NonCanonicalObject(_)
        | StoreError::HashMismatch { .. }
        | StoreError::ImmutableCollision(_)
        | StoreError::ObjectKindMismatch { .. }
        | StoreError::InvalidStoredHash(_)
        | StoreError::InvalidStoredClaim(_)
        | StoreError::InvalidMemoryProjection(_)
        | StoreError::InvalidTaskBinding
        | StoreError::InvalidTaskProjection(_)
        | StoreError::TurnObservationIdempotencyConflict(_)
        | StoreError::InvalidControlObservation(_)
        | StoreError::InvalidControlSession(_)
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
        | StoreError::WorkLeaseNotFound(_)
        | StoreError::WorkLeaseNotHeld { .. }
        | StoreError::WorkLeaseExpired { .. }
        | StoreError::InvalidControlProjection(_)
        | StoreError::ControlPolicyConflict { .. }
        | StoreError::OpenWorkObligations { .. } => "engram_store_error",
    }
}

fn invalid_argument(field: &str, message: &str) -> CallToolResult {
    CallToolResult::structured_error(json!({
        "error": {
            "code": "invalid_argument",
            "message": message,
            "details": { "field": field },
        }
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

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
            "context_generation must be at most 256 bytes without control characters".into(),
        );
        let value = store_error_value(&error);
        assert_eq!(
            value["error"]["details"]["remedy"],
            "omit context_generation or use at most 256 bytes without control characters"
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
    fn retained_work_service_survives_failure_for_agent_tools() {
        let directory = tempfile::tempdir().expect("temporary MCP home");
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
}
