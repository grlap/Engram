//! MCP stdio surface: the eight-word agent tools by default, and the legacy
//! task/memory/work tools only when the host opts in.

use std::{path::PathBuf, str::FromStr, sync::Arc};

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
    ActorContext, AddInput, AgentVerbs, Authority, ChangeCursor, ClaimInput,
    DevelopmentNoopRedactor, DoneInput, HandoffAction, HandoffInput, LocalWorkService, LsInput,
    MemoryKind, NextInput, NoteInput, NoteRequest, NoteVisibility, ObjectHash, ProjectId, Receipt,
    Sensitivity, SessionId, SqliteStore, TaskId, UpdateAction, UpdateInput, VerbError,
    WorkAvailability, WorkCompleteInput, WorkHandoffInput, WorkItemKind, WorkLifecycle,
    WorkNextQuery, WorkNextSection, WorkProposeInput, WorkUpdateInput,
    domain::{AssuranceLevel, ProvenanceLink, ProvenanceRelation},
    parse_defer_date,
    storage::StoreError,
};

/// Immutable host context asserted for one MCP connection.
#[derive(Clone, Debug)]
pub struct McpServer {
    database: PathBuf,
    project_id: ProjectId,
    actor_id: String,
    session_id: SessionId,
    source_skill: Option<String>,
    work_service: Arc<LocalWorkService>,
    tool_router: ToolRouter<Self>,
}

impl McpServer {
    /// Creates a tools-only MCP service exposing the eight-word agent tools.
    /// Task binding itself is stored in SQLite, so a new server process
    /// resumes the session's active task.
    #[must_use]
    pub fn new(
        database: PathBuf,
        project_id: ProjectId,
        actor_id: String,
        session_id: SessionId,
        source_skill: Option<String>,
    ) -> Self {
        let work_service = Arc::new(LocalWorkService::new(
            database.clone(),
            project_id.clone(),
            actor_id.clone(),
            session_id.clone(),
            source_skill.clone(),
            None,
        ));
        Self {
            database,
            project_id,
            actor_id,
            session_id,
            source_skill,
            work_service,
            tool_router: Self::agent_tool_router(),
        }
    }

    /// Binds one host-selected grant to this connection's work mutations.
    #[must_use]
    pub fn with_work_authority_grant(mut self, grant: Option<ObjectHash>) -> Self {
        self.work_service = Arc::new(LocalWorkService::new(
            self.database.clone(),
            self.project_id.clone(),
            self.actor_id.clone(),
            self.session_id.clone(),
            self.source_skill.clone(),
            grant,
        ));
        self
    }

    /// Also registers the legacy `task_*`, `memory_*`, `context_explain`, and
    /// `work_*` tools beside the eight words.
    #[must_use]
    pub fn with_legacy_tools(mut self, enabled: bool) -> Self {
        self.tool_router = if enabled {
            Self::agent_tool_router() + Self::legacy_tool_router()
        } else {
            Self::agent_tool_router()
        };
        self
    }

    fn verbs(&self) -> AgentVerbs {
        AgentVerbs::with_shared_service(
            Arc::clone(&self.work_service),
            self.actor_id.clone(),
            self.session_id.clone(),
        )
    }

    fn store(&self) -> Result<SqliteStore, StoreError> {
        SqliteStore::open_unresolved(&self.database)
    }

    fn actor(&self, tool_name: &str, reason: &str) -> ActorContext {
        ActorContext {
            actor_id: self.actor_id.clone(),
            actor_kind: "agent".into(),
            assurance: AssuranceLevel::Asserted,
            run_id: None,
            session_id: Some(self.session_id.clone()),
            source_tool: Some(format!("mcp:{tool_name}")),
            source_skill: self.source_skill.clone(),
            provenance_chain: vec![ProvenanceLink {
                relation: ProvenanceRelation::AssertedBy,
                source: self.actor_id.clone(),
                reference: Some(self.session_id.0.clone()),
            }],
            reason: reason.into(),
        }
    }

    fn bound_task(&self, store: &SqliteStore) -> Result<TaskId, StoreError> {
        store.bound_task(&self.project_id, &self.session_id)
    }

    fn work_service(&self) -> &LocalWorkService {
        self.work_service.as_ref()
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TaskStartArgs {
    /// Organizational ticket reference used by every session to rendezvous.
    external_ref: String,
    /// Short local execution title; the external tracker remains authoritative.
    title: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TaskJoinArgs {
    /// Organizational ticket reference; no Engram UUID is required.
    external_ref: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct MemoryNoteArgs {
    /// Natural-language note. Labels such as `Decision:`, `Fact:`, and
    /// `Constraint:` plus rule cues are optional hints.
    prose: String,
    /// Caller-stable key required for safe lost-response retry.
    idempotency_key: String,
    /// Required when both a legacy task and local work focus are active.
    target: Option<MemoryNoteTarget>,
    /// Keep this note private to the asserted agent rather than task-shared.
    private: Option<bool>,
    /// Optional explicit kind override; normal capture should omit it.
    kind: Option<String>,
    /// Optional explicit authority override; normal capture should omit it.
    authority: Option<String>,
    /// Optional external evidence or source references.
    refs: Option<Vec<String>>,
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum MemoryNoteTarget {
    Task,
    Work,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct DeltaArgs {
    /// Last processed task cursor; only strictly newer changes are returned.
    after: i64,
    /// Maximum changes to return (default 100, maximum 1000).
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct SearchArgs {
    /// Full-text query over visible memory titles and bodies.
    query: String,
    /// Maximum matches to return (default 20, maximum 1000).
    limit: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct HashArgs {
    /// Lowercase SHA-256 version hash from a memory write receipt.
    hash: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ContradictionArgs {
    /// First memory version hash returned by `memory_note` or `memory_show`.
    first_version: String,
    /// Second memory version hash that cannot safely guide action with the first.
    second_version: String,
    /// Attributed explanation of the concrete conflict.
    reason: String,
    /// Stable key for safe retry. Omit for a new declaration.
    idempotency_key: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct ClaimArgs {
    /// Lease duration in seconds (1..86400).
    ttl_seconds: i64,
    /// Stable key for a safe acquisition retry; omit for a new attempt.
    idempotency_key: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WorkNextArgs {
    /// Maximum ready candidates and feed changes to return (default 20, max 1000).
    limit: Option<u32>,
    /// Exact `delivered_through` value from the prior successfully received
    /// page. Omit to replay any unacknowledged page. Before changing focus,
    /// replay an unknown pending page first, then acknowledge the cursor that
    /// was actually received while selecting sections that exclude `changes`.
    acknowledge_through: Option<i64>,
    /// Opaque `delivery_token` returned with the same successfully received
    /// page. Required to advance a pending delivery cursor.
    acknowledge_token: Option<String>,
    /// Response sections to include: focus, ready, catalog, and/or changes.
    /// Omit for all sections. Excluding changes never stages a delivery page.
    #[serde(default)]
    sections: Vec<String>,
    /// Case-insensitive text search over refs, titles, outcomes, labels, and blockers.
    search: Option<String>,
    /// Lifecycle filters such as open, completed, cancelled, or superseded.
    #[serde(default)]
    lifecycles: Vec<String>,
    /// Availability filters such as ready, claimed, active, blocked, deferred, or closed.
    #[serde(default)]
    availabilities: Vec<String>,
    /// Return only work with an active graph/manual blocker.
    #[serde(default)]
    blocked_only: bool,
    /// Exact case-insensitive assignee filter.
    assigned_to: Option<String>,
    /// Exact case-insensitive label filter.
    label: Option<String>,
    /// Continue catalog pagination after this short ref or full UUID.
    catalog_after: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WorkFocusArgs {
    /// Short Engram work ref or full UUID; subsequent mutations use this focus.
    work_ref: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WorkProposeArgs {
    /// Short ref or UUID of the parent to decompose; selects focus in the same call.
    work_ref: Option<String>,
    /// Root creation or atomic decomposition operation.
    #[schemars(with = "WorkProposeInput")]
    input: Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WorkUpdateArgs {
    /// Short ref or UUID to act on; selects focus in the same call.
    work_ref: Option<String>,
    /// Typed mutation against ambient focused work.
    #[schemars(with = "WorkUpdateInput")]
    input: Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WorkCompleteArgs {
    /// Short ref or UUID to complete; selects focus in the same call.
    work_ref: Option<String>,
    /// Evidence-backed acceptance against ambient focused work.
    #[schemars(with = "WorkCompleteInput")]
    input: Value,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct WorkHandoffArgs {
    /// Short ref or UUID to hand off; selects focus in the same call.
    work_ref: Option<String>,
    /// Checkpoint-coupled offer, accept, or cancellation.
    #[schemars(with = "WorkHandoffInput")]
    input: Value,
}

#[tool_router(router = legacy_tool_router)]
impl McpServer {
    /// Start a local execution task or bind to the existing task with this ref.
    #[tool(
        name = "task_start",
        description = "Start or ref-idempotently bind a local Engram task"
    )]
    fn task_start(&self, Parameters(args): Parameters<TaskStartArgs>) -> CallToolResult {
        let mut store = match self.store() {
            Ok(store) => store,
            Err(error) => return store_error(&error),
        };
        result(
            store.start_task(
                &self.project_id,
                &args.external_ref,
                &args.title,
                &self.session_id,
                self.actor("task_start", "bind this session to local task execution"),
                Utc::now(),
            ),
            "task_start",
        )
    }

    /// Join a local task using only the external tracker reference.
    #[tool(
        name = "task_join",
        description = "Join an existing task by external reference only"
    )]
    fn task_join(&self, Parameters(args): Parameters<TaskJoinArgs>) -> CallToolResult {
        let mut store = match self.store() {
            Ok(store) => store,
            Err(error) => return store_error(&error),
        };
        result(
            store.join_task(
                &self.project_id,
                &args.external_ref,
                &self.session_id,
                self.actor(
                    "task_join",
                    "join shared execution memory by external reference",
                ),
                Utc::now(),
            ),
            "task_join",
        )
    }

    /// Capture one prose note into the active task's canonical working set.
    #[tool(
        name = "memory_note",
        description = "Capture one classified task/work note; target is required when both contexts are active. Optional cues: Decision:, Fact:, Constraint:, Never, Always, Must, Do not, Only"
    )]
    fn memory_note(&self, Parameters(args): Parameters<MemoryNoteArgs>) -> CallToolResult {
        let mut store = match self.store() {
            Ok(store) => store,
            Err(error) => return store_error(&error),
        };
        let work_id = match store.work_session_state(&self.project_id, &self.session_id, Utc::now())
        {
            Ok(state) => state.focused_work_id,
            Err(error) => return store_error(&error),
        };
        let task_id = match self.bound_task(&store) {
            Ok(task_id) => Some(task_id),
            Err(StoreError::NoActiveTask(_)) => None,
            Err(error) => return store_error(&error),
        };
        let (task_id, work_id) = match (args.target, task_id, work_id) {
            (Some(MemoryNoteTarget::Task), Some(task_id), _) | (None, Some(task_id), None) => {
                (Some(task_id), None)
            }
            (Some(MemoryNoteTarget::Task), None, _) => {
                return invalid_argument("target", "no legacy task is active");
            }
            (Some(MemoryNoteTarget::Work), _, Some(work_id)) | (None, None, Some(work_id)) => {
                (None, Some(work_id))
            }
            (Some(MemoryNoteTarget::Work), _, None) => {
                return invalid_argument("target", "no local work item is focused");
            }
            (None, Some(_), Some(_)) => {
                return invalid_argument(
                    "target",
                    "choose task or work when both a legacy task and local work focus are active",
                );
            }
            (None, None, None) => {
                return store_error(&StoreError::NoActiveTask(self.session_id.0.clone()));
            }
        };
        let kind = match args.kind.as_deref().map(parse_kind).transpose() {
            Ok(kind) => kind,
            Err(error) => return invalid_argument("kind", error),
        };
        let authority = match args.authority.as_deref().map(parse_authority).transpose() {
            Ok(authority) => authority,
            Err(error) => return invalid_argument("authority", error),
        };
        let request = NoteRequest {
            project_id: self.project_id.clone(),
            task_id,
            work_id,
            prose: args.prose,
            visibility: if args.private.unwrap_or(false) {
                NoteVisibility::Private
            } else {
                NoteVisibility::Shared
            },
            kind,
            authority,
            sensitivity: Some(Sensitivity::Internal),
            title: None,
            tags: Vec::new(),
            evidence: Vec::new(),
            refs: args.refs.unwrap_or_default(),
            actor: self.actor(
                "memory_note",
                "record once for context, peer delta, handoff, and final report inputs",
            ),
            idempotency_key: args.idempotency_key,
            created_at: Utc::now(),
        };
        result(
            store.capture_note(&request, &DevelopmentNoopRedactor),
            "memory_note",
        )
    }

    /// Mark two visible task/work shared memory versions as contradictory.
    #[tool(
        name = "memory_contradict",
        description = "Declare an attributed contradiction between two visible version hashes"
    )]
    fn memory_contradict(&self, Parameters(args): Parameters<ContradictionArgs>) -> CallToolResult {
        let first = match ObjectHash::from_str(&args.first_version) {
            Ok(hash) => hash,
            Err(message) => return invalid_argument("first_version", message),
        };
        let second = match ObjectHash::from_str(&args.second_version) {
            Ok(hash) => hash,
            Err(message) => return invalid_argument("second_version", message),
        };
        let mut store = match self.store() {
            Ok(store) => store,
            Err(error) => return store_error(&error),
        };
        let task_id = match self.bound_task(&store) {
            Ok(task_id) => Some(task_id),
            Err(StoreError::NoActiveTask(_)) => None,
            Err(error) => return store_error(&error),
        };
        let work_id = match store.work_session_state(&self.project_id, &self.session_id, Utc::now())
        {
            Ok(state) => state.focused_work_id,
            Err(error) => return store_error(&error),
        };
        if task_id.is_none() && work_id.is_none() {
            return store_error(&StoreError::NoActiveTask(self.session_id.0.clone()));
        }
        result(
            store.record_memory_contradiction(
                &self.project_id,
                task_id,
                work_id,
                &self.session_id,
                &self.actor_id,
                &first,
                &second,
                &args.reason,
                &args
                    .idempotency_key
                    .unwrap_or_else(|| uuid::Uuid::now_v7().to_string()),
                self.actor(
                    "memory_contradict",
                    "surface incompatible guidance before a peer acts",
                ),
                Utc::now(),
            ),
            "memory_contradict",
        )
    }

    /// Build the bounded task/work context for this session and remember its packet.
    #[tool(
        name = "memory_context",
        description = "Build a bounded context packet for the active task and focused local work"
    )]
    fn memory_context(&self) -> CallToolResult {
        let mut store = match self.store() {
            Ok(store) => store,
            Err(error) => return store_error(&error),
        };
        let task_id = match self.bound_task(&store) {
            Ok(task_id) => Some(task_id),
            Err(StoreError::NoActiveTask(_)) => None,
            Err(error) => return store_error(&error),
        };
        let work_id = match store.work_session_state(&self.project_id, &self.session_id, Utc::now())
        {
            Ok(state) => state.focused_work_id,
            Err(error) => return store_error(&error),
        };
        if task_id.is_none() && work_id.is_none() {
            return store_error(&StoreError::NoActiveTask(self.session_id.0.clone()));
        }
        result(
            store.build_context(
                &self.project_id,
                task_id,
                &self.session_id,
                &self.actor_id,
                Utc::now(),
            ),
            "memory_context",
        )
    }

    /// Return only task-shared changes after a processed cursor.
    #[tool(
        name = "memory_delta",
        description = "Read the authoritative task delta after a cursor"
    )]
    fn memory_delta(&self, Parameters(args): Parameters<DeltaArgs>) -> CallToolResult {
        let store = match self.store() {
            Ok(store) => store,
            Err(error) => return store_error(&error),
        };
        let task_id = match self.bound_task(&store) {
            Ok(task_id) => task_id,
            Err(error) => return store_error(&error),
        };
        result(
            store.task_delta(
                &self.project_id,
                task_id,
                &self.session_id,
                &self.actor_id,
                ChangeCursor(args.after.max(0)),
                args.limit.unwrap_or(100),
            ),
            "memory_delta",
        )
    }

    /// Search only memory visible to this session's active task and work focus.
    #[tool(
        name = "memory_search",
        description = "Full-text search visible memory without crossing private scope"
    )]
    fn memory_search(&self, Parameters(args): Parameters<SearchArgs>) -> CallToolResult {
        let store = match self.store() {
            Ok(store) => store,
            Err(error) => return store_error(&error),
        };
        let task_id = match self.bound_task(&store) {
            Ok(task_id) => Some(task_id),
            Err(StoreError::NoActiveTask(_)) => None,
            Err(error) => return store_error(&error),
        };
        let work_id = match store.work_session_state(&self.project_id, &self.session_id, Utc::now())
        {
            Ok(state) => state.focused_work_id,
            Err(error) => return store_error(&error),
        };
        if task_id.is_none() && work_id.is_none() {
            return store_error(&StoreError::NoActiveTask(self.session_id.0.clone()));
        }
        result(
            store.search_memories(
                &self.project_id,
                task_id,
                work_id,
                &self.session_id,
                &self.actor_id,
                Some(&args.query),
                args.limit.unwrap_or(20),
            ),
            "memory_search",
        )
    }

    /// Inspect a complete memory through the same scope checks as retrieval.
    #[tool(
        name = "memory_show",
        description = "Verify and inspect an authorized memory by receipt hash"
    )]
    fn memory_show(&self, Parameters(args): Parameters<HashArgs>) -> CallToolResult {
        let hash = match ObjectHash::from_str(&args.hash) {
            Ok(hash) => hash,
            Err(message) => return invalid_argument("hash", message),
        };
        let store = match self.store() {
            Ok(store) => store,
            Err(error) => return store_error(&error),
        };
        let work_id = match store.work_session_state(&self.project_id, &self.session_id, Utc::now())
        {
            Ok(state) => state.focused_work_id,
            Err(error) => return store_error(&error),
        };
        let task_id = match self.bound_task(&store) {
            Ok(task_id) => Some(task_id),
            Err(StoreError::NoActiveTask(_)) => None,
            Err(error) => return store_error(&error),
        };
        result(
            store.show_memory(
                &hash,
                &self.project_id,
                task_id,
                work_id,
                &self.session_id,
                &self.actor_id,
            ),
            "memory_show",
        )
    }

    /// Explain all inclusion and omission decisions in a context packet.
    #[tool(
        name = "context_explain",
        description = "Explain a context packet by its receipt hash"
    )]
    fn context_explain(&self, Parameters(args): Parameters<HashArgs>) -> CallToolResult {
        let hash = match ObjectHash::from_str(&args.hash) {
            Ok(hash) => hash,
            Err(message) => return invalid_argument("hash", message),
        };
        let store = match self.store() {
            Ok(store) => store,
            Err(error) => return store_error(&error),
        };
        result(
            store.explain_context(&hash, &self.project_id, &self.session_id, &self.actor_id),
            "context_explain",
        )
    }

    /// Atomically claim the active task under a short, expiring lease.
    #[tool(
        name = "task_claim",
        description = "Acquire an expiring task lease with typed conflict details"
    )]
    fn task_claim(&self, Parameters(args): Parameters<ClaimArgs>) -> CallToolResult {
        if !(1..=86_400).contains(&args.ttl_seconds) {
            return invalid_argument("ttl_seconds", "expected a value from 1 through 86400");
        }
        let mut store = match self.store() {
            Ok(store) => store,
            Err(error) => return store_error(&error),
        };
        let task_id = match self.bound_task(&store) {
            Ok(task_id) => task_id,
            Err(error) => return store_error(&error),
        };
        let now = Utc::now();
        result(
            store.claim_task(
                task_id,
                &self.session_id,
                &args
                    .idempotency_key
                    .unwrap_or_else(|| uuid::Uuid::now_v7().to_string()),
                now,
                args.ttl_seconds,
                self.actor("task_claim", "acquire exclusive execution ownership"),
            ),
            "task_claim",
        )
    }

    /// Return ambient focus, obligations, ready candidates, and project changes.
    #[tool(
        name = "work_next",
        description = "Return selected bounded focus, ready, catalog, and project-change sections. Each call returns the changes since this session's previous call; the previous page counts as delivered"
    )]
    fn work_next(&self, Parameters(args): Parameters<WorkNextArgs>) -> CallToolResult {
        let sections = match parse_enum_values::<WorkNextSection>(&args.sections) {
            Ok(values) => values,
            Err(message) => return invalid_argument("sections", &message),
        };
        let lifecycles = match parse_enum_values::<WorkLifecycle>(&args.lifecycles) {
            Ok(values) => values,
            Err(message) => return invalid_argument("lifecycles", &message),
        };
        let availabilities = match parse_enum_values::<WorkAvailability>(&args.availabilities) {
            Ok(values) => values,
            Err(message) => return invalid_argument("availabilities", &message),
        };
        result(
            self.work_service().work_next_with_delivery_token(
                args.limit.unwrap_or(20),
                args.acknowledge_through,
                args.acknowledge_token.as_deref(),
                WorkNextQuery {
                    sections,
                    search: args.search,
                    lifecycles,
                    availabilities,
                    blocked_only: args.blocked_only,
                    assigned_to: args.assigned_to,
                    label: args.label,
                    after: args.catalog_after,
                },
                Utc::now(),
            ),
            "work_next",
        )
    }

    /// Select and fully inspect ambient work without claiming it.
    #[tool(
        name = "work_focus",
        description = "Select work by short ref or UUID as ambient focus and return acceptance, graph, claim, handoff, evidence, and allowed-next state; never claims implicitly"
    )]
    fn work_focus(&self, Parameters(args): Parameters<WorkFocusArgs>) -> CallToolResult {
        result(
            self.work_service().work_focus(&args.work_ref, Utc::now()),
            "work_focus",
        )
    }

    /// Create a root or atomically decompose ambient work.
    #[tool(
        name = "work_propose",
        description = "Create a local root or atomically decompose work. Optional work_ref selects the parent in the same call; idempotency_key may be omitted, in which case an identical call replays"
    )]
    fn work_propose(&self, Parameters(args): Parameters<WorkProposeArgs>) -> CallToolResult {
        let input = match serde_json::from_value::<WorkProposeInput>(args.input) {
            Ok(input) => input,
            Err(error) => return invalid_argument("input", &error.to_string()),
        };
        result(
            self.work_service()
                .work_propose_on(args.work_ref.as_deref(), input, Utc::now()),
            "work_propose",
        )
    }

    /// Apply a typed mutation to ambient focused work.
    #[tool(
        name = "work_update",
        description = "Apply one typed mutation to work. Optional work_ref selects the item in the same call; revision, run, claim, and fence are inferred; idempotency_key may be omitted, in which case an identical call replays"
    )]
    fn work_update(&self, Parameters(args): Parameters<WorkUpdateArgs>) -> CallToolResult {
        let input = match serde_json::from_value::<WorkUpdateInput>(args.input) {
            Ok(input) => input,
            Err(error) => return invalid_argument("input", &error.to_string()),
        };
        result(
            self.work_service()
                .work_update_on(args.work_ref.as_deref(), input, Utc::now()),
            "work_update",
        )
    }

    /// Complete ambient work under inferred fences and explicit acceptance.
    #[tool(
        name = "work_complete",
        description = "Complete work. Optional work_ref selects the item in the same call; omitted acceptance asserts every current criterion; fences, assurance, and host grant are inferred; idempotency_key may be omitted, in which case an identical call replays"
    )]
    fn work_complete(&self, Parameters(args): Parameters<WorkCompleteArgs>) -> CallToolResult {
        let input = match serde_json::from_value::<WorkCompleteInput>(args.input) {
            Ok(input) => input,
            Err(error) => return invalid_argument("input", &error.to_string()),
        };
        result(
            self.work_service()
                .work_complete_on(args.work_ref.as_deref(), input, Utc::now()),
            "work_complete",
        )
    }

    /// Offer, accept, or cancel a checkpoint-coupled ambient claim handoff.
    #[tool(
        name = "work_handoff",
        description = "Offer, accept, or cancel a handoff without lifecycle ids. Optional work_ref selects the item in the same call; the unique matching offer and claim fence are inferred; idempotency_key may be omitted, in which case an identical call replays"
    )]
    fn work_handoff(&self, Parameters(args): Parameters<WorkHandoffArgs>) -> CallToolResult {
        let input = match serde_json::from_value::<WorkHandoffInput>(args.input) {
            Ok(input) => input,
            Err(error) => return invalid_argument("input", &error.to_string()),
        };
        result(
            self.work_service()
                .work_handoff_on(args.work_ref.as_deref(), input, Utc::now()),
            "work_handoff",
        )
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct NextArgs {
    /// Maximum ready items and changes to return (default 20).
    limit: Option<u32>,
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
    /// Add as a required child of this item instead of a root.
    under: Option<String>,
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
}

#[derive(Debug, Deserialize, JsonSchema)]
struct UpdateArgs {
    /// Item to act on; defaults to the focus.
    work_ref: Option<String>,
    /// release, blocked (with text), unblock, revise (with fields), or cancel (with reason).
    action: UpdateActionArg,
    /// Reason for release (optional) or cancel (required).
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
        verb(
            self.verbs()
                .next(&NextInput { limit: args.limit }, Utc::now()),
        )
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

    /// Create a root or one required child.
    #[tool(
        name = "add",
        description = "Create work from a title; outcome and acceptance are welcome; under adds a required child"
    )]
    fn add(&self, Parameters(args): Parameters<AddArgs>) -> CallToolResult {
        verb(self.verbs().add(
            AddInput {
                title: args.title,
                outcome: args.outcome,
                acceptance: args.acceptance.unwrap_or_default(),
                under: args.under,
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
        description = "One action: release, blocked (text), unblock, revise (title, outcome, assignee, priority, defer), or cancel (reason)"
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
                }
            }
            UpdateActionArg::Cancel => UpdateAction::Cancel {
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
    instructions = "Eight words: next, ls, show, add, claim, update, note, done (plus search and handoff). add needs only a title; claim before you change anything; note findings once; done completes what you hold. Every answer ends with reminders (what is owed) and next (commands you can run now). Identical calls are safe to repeat."
)]
impl ServerHandler for McpServer {}

fn verb(outcome: Result<Receipt, VerbError>) -> CallToolResult {
    match outcome {
        Ok(receipt) => CallToolResult::structured(receipt.value),
        Err(error) => {
            let guidance = error.guidance();
            let mut value = error_value(&error.error);
            value["error"]["reminders"] = json!(guidance.reminders);
            value["error"]["next"] = json!(guidance.next);
            CallToolResult::structured_error(value)
        }
    }
}

fn result<T: serde::Serialize>(value: Result<T, StoreError>, operation: &str) -> CallToolResult {
    match value {
        Ok(value) => match serde_json::to_value(value) {
            Ok(value) => CallToolResult::structured(value),
            Err(error) => CallToolResult::structured_error(json!({
                "error": {
                    "code": "serialization_failed",
                    "operation": operation,
                    "message": error.to_string(),
                }
            })),
        },
        Err(error) => store_error(&error),
    }
}

fn store_error(error: &StoreError) -> CallToolResult {
    CallToolResult::structured_error(error_value(error))
}

fn error_value(error: &StoreError) -> Value {
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
        StoreError::WorkNotFound(work) => json!({
            "work_id": work,
            "remedy": "call work_next to search/list work, then work_focus using a returned short_ref",
        }),
        StoreError::InvalidWork(message) | StoreError::InvalidWorkProjection(message) => json!({
            "reason": message,
            "remedy": "refresh with work_focus/work_next and follow allowed_next",
        }),
        StoreError::WorkRevisionConflict {
            work,
            expected,
            current,
        } => json!({
            "work_id": work,
            "expected_revision": expected,
            "current_revision": current,
            "remedy": "refresh the ambient item with work_focus before retrying with a new idempotency_key",
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
            "remedy": "refresh with work_focus and follow allowed_next",
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
            "remedy": "refresh with work_focus; reclaim or accept a handoff before mutating",
        }),
        StoreError::WorkCompletionRefused { work, reason } => json!({
            "work_id": work,
            "reason": reason,
            "remedy": "record evidence, checkpoint the current feed cut, and satisfy every current acceptance criterion",
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
        StoreError::MemoryNotFound(_) => "memory_not_found",
        StoreError::PacketAccessDenied(_) => "packet_access_denied",
        StoreError::PinnedBudgetExceeded { .. } => "pinned_budget_exceeded",
        StoreError::EmptyNote => "empty_note",
        StoreError::RedactionRefused(_) => "redaction_refused",
        StoreError::WorkNotFound(_) => "work_not_found",
        StoreError::InvalidWork(_) => "work_invalid",
        StoreError::InvalidWorkProjection(_) => "work_projection_invalid",
        StoreError::WorkRevisionConflict { .. } => "work_revision_conflict",
        StoreError::WorkOperationIdempotencyConflict { .. } => "work_idempotency_conflict",
        StoreError::WorkDependencyCycle => "work_dependency_cycle",
        StoreError::WorkNotOpen(_) => "work_not_open",
        StoreError::WorkClaimHeld { .. } => "work_claim_held",
        StoreError::WorkClaimMismatch { .. } => "work_claim_mismatch",
        StoreError::WorkCompletionRefused { .. } => "work_completion_refused",
        _ => "engram_store_error",
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

fn parse_enum_values<T>(values: &[String]) -> Result<Vec<T>, String>
where
    T: serde::de::DeserializeOwned,
{
    values
        .iter()
        .map(|value| {
            serde_json::from_value(Value::String(value.trim().to_lowercase()))
                .map_err(|error| error.to_string())
        })
        .collect()
}

fn parse_kind(value: &str) -> Result<MemoryKind, &'static str> {
    match value {
        "constraint" => Ok(MemoryKind::Constraint),
        "decision" => Ok(MemoryKind::Decision),
        "convention" => Ok(MemoryKind::Convention),
        "fact" => Ok(MemoryKind::Fact),
        "preference" => Ok(MemoryKind::Preference),
        "episode" => Ok(MemoryKind::Episode),
        _ => Err("expected constraint, decision, convention, fact, preference, or episode"),
    }
}

fn parse_authority(value: &str) -> Result<Authority, &'static str> {
    match value {
        "hard" => Ok(Authority::Hard),
        "firm" => Ok(Authority::Firm),
        "soft" => Ok(Authority::Soft),
        _ => Err("expected hard, firm, or soft"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retained_work_service_survives_failure_and_serves_both_tool_sets() {
        let directory = tempfile::tempdir().expect("temporary MCP home");
        let server = McpServer::new(
            directory.path().join("engram.sqlite3"),
            ProjectId("mcp-retained-service".into()),
            "agent".into(),
            SessionId("mcp-retained-session".into()),
            Some("mcp-test".into()),
        )
        .with_legacy_tools(true);

        let refused = server.verbs().add(
            AddInput {
                title: "requires host authority".into(),
                ..AddInput::default()
            },
            Utc::now(),
        );
        assert!(refused.is_err());
        assert!(format!("{:?}", server.work_service).contains("store_initialized: true"));

        server
            .verbs()
            .next(&NextInput { limit: Some(5) }, Utc::now())
            .expect("agent tool remains usable after refusal");
        server
            .work_service()
            .work_next(
                5,
                WorkNextQuery {
                    sections: vec![WorkNextSection::Catalog],
                    ..WorkNextQuery::default()
                },
                Utc::now(),
            )
            .expect("legacy work tool shares the usable retained service");

        let cloned_handler = server.clone();
        assert!(Arc::ptr_eq(
            &server.work_service,
            &cloned_handler.work_service
        ));
    }
}
