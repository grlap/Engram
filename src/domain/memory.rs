//! Typed memory vocabulary: kinds, authority, delivery, scope, status,
//! immutable versions, project-memory records, notes, and contradictions.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ObjectHash;

use super::{
    ActorContext, ChangeCursor, FeedPosition, MemoryId, ProjectId, SessionId, SourceSnapshot,
    TaskId, WorkId,
};

/// What a memory means independently of how it is delivered.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    Constraint,
    Decision,
    Convention,
    Fact,
    Preference,
    Episode,
}

/// Strength of the memory's instruction or assertion.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Authority {
    Hard,
    Firm,
    Soft,
}

/// Default context-delivery behavior; policy may override it with a reason.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Pinned,
    Index,
    OnDemand,
    Suppressed,
}

/// Scope supported by the local V1 backend.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Scope {
    Project {
        project: ProjectId,
    },
    Task {
        project: ProjectId,
        task: TaskId,
    },
    Work {
        project: ProjectId,
        work: WorkId,
    },
    Agent {
        project: ProjectId,
        task: Option<TaskId>,
        #[serde(default)]
        work: Option<WorkId>,
        agent: String,
    },
}

impl Scope {
    /// Returns the task whose working set this scope belongs to, if any.
    #[must_use]
    pub fn task_id(&self) -> Option<TaskId> {
        match self {
            Self::Task { task, .. }
            | Self::Agent {
                task: Some(task), ..
            } => Some(*task),
            Self::Project { .. } | Self::Work { .. } | Self::Agent { task: None, .. } => None,
        }
    }

    /// Returns the local work identity this memory belongs to, if any.
    #[must_use]
    pub fn work_id(&self) -> Option<WorkId> {
        match self {
            Self::Work { work, .. }
            | Self::Agent {
                work: Some(work), ..
            } => Some(*work),
            Self::Project { .. } | Self::Task { .. } | Self::Agent { work: None, .. } => None,
        }
    }

    /// Whether the scope is visible to every participant of its task.
    #[must_use]
    pub fn is_task_shared(&self) -> bool {
        matches!(self, Self::Task { .. })
    }

    /// Whether the scope is visible to every participant of local work.
    #[must_use]
    pub fn is_work_shared(&self) -> bool {
        matches!(self, Self::Work { .. })
    }
}

/// Lifecycle state for a memory head.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    Proposed,
    Active,
    Contested,
    Stale,
    Retracted,
    Expired,
    Tombstoned,
}

/// Assurance attached to actor and authority text supplied by the host.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssuranceLevel {
    Asserted,
    Authenticated,
    Signed,
}

/// Retrieval classification applied before context is assembled.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    Public,
    Internal,
    Restricted,
    SecretRef,
}

/// Immutable content of one memory version. Its object hash is stored outside
/// this payload so identity is computed over canonical content only.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MemoryVersion {
    pub schema_version: u16,
    pub memory_id: MemoryId,
    /// Stable project-memory key. Ordinary typed memories omit it, preserving
    /// their canonical bytes; project episodes reserve it permanently.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_key: Option<String>,
    pub parents: Vec<ObjectHash>,
    pub kind: MemoryKind,
    pub authority: Authority,
    pub delivery: Delivery,
    pub scope: Scope,
    pub title: String,
    pub body: String,
    pub structured_value: Option<Value>,
    pub tags: Vec<String>,
    pub evidence: Vec<ObjectHash>,
    pub refs: Vec<String>,
    pub source_snapshot: Option<SourceSnapshot>,
    pub confidence: Option<f64>,
    pub sensitivity: Sensitivity,
    pub classification_reason: String,
    pub delivery_override_reason: Option<String>,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_until: Option<DateTime<Utc>>,
    pub review_by: Option<DateTime<Utc>>,
    pub last_verified: Option<DateTime<Utc>>,
    pub actor: ActorContext,
    pub created_at: DateTime<Utc>,
}

/// Initial activation decision for a memory version. Status remains derived
/// from immutable events; this object is the first event in that history.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MemoryAssertionEvent {
    pub schema_version: u16,
    pub memory_id: MemoryId,
    pub version: ObjectHash,
    pub status: MemoryStatus,
    pub policy_reason: String,
    pub actor: ActorContext,
    pub created_at: DateTime<Utc>,
}

/// Maximum admitted UTF-8 body bytes for one project memory.
pub const MAX_PROJECT_MEMORY_BODY_BYTES: usize = 8 * 1024;
/// Maximum safe project-memory key length in ASCII bytes.
pub const MAX_PROJECT_MEMORY_KEY_BYTES: usize = 64;
/// Maximum raw UTF-8 bytes accepted for one project-memory search query.
pub const MAX_PROJECT_MEMORY_QUERY_BYTES: usize = 256;
/// Maximum normalized full-text tokens accepted in one project-memory query.
pub const MAX_PROJECT_MEMORY_QUERY_TOKENS: usize = 16;

/// Create request for one immutable project episode.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RememberProjectMemoryRequest {
    pub project_id: ProjectId,
    pub session_id: SessionId,
    pub key: Option<String>,
    pub body: String,
    pub actor: ActorContext,
    pub created_at: DateTime<Utc>,
}

/// Tombstone request for one permanently reserved project-memory key.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ForgetProjectMemoryRequest {
    pub project_id: ProjectId,
    pub session_id: SessionId,
    pub key: String,
    pub actor: ActorContext,
    pub created_at: DateTime<Utc>,
}

/// Model-visible mutation receipt. Canonical hashes and UUIDs stay hidden.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectMemoryMutationReceipt {
    pub key: String,
    pub remembered_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forgotten_at: Option<DateTime<Utc>>,
    pub duplicate: bool,
}

/// Compact project-memory row; the full body is available only through a
/// dedicated full read.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectMemoryListRow {
    pub key: String,
    pub first_line: String,
    pub remembered_at: DateTime<Utc>,
    pub actor_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_context: Option<String>,
}

/// Bounded listing result. Filtered queries omit continuation and report how
/// many additional matches were not returned.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectMemoryList {
    pub memories: Vec<ProjectMemoryListRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub next_after: Option<String>,
    /// Additional matches omitted from a filtered result. Unfiltered keyset
    /// listings use `next_after` instead and leave this at zero.
    pub omitted_count: usize,
    pub exhausted: bool,
}

/// Dedicated full-read envelope whose exact serialized size is checked before
/// the corresponding memory is persisted.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectMemoryFull {
    pub key: String,
    pub body: String,
    pub remembered_at: DateTime<Utc>,
    pub actor_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_context: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
}

/// Visibility override for low-friction prose capture.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteVisibility {
    #[default]
    Shared,
    Private,
}

/// Common capture request used by the CLI and MCP surface.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct NoteRequest {
    pub project_id: ProjectId,
    pub task_id: Option<TaskId>,
    #[serde(default)]
    pub work_id: Option<WorkId>,
    pub prose: String,
    #[serde(default)]
    pub visibility: NoteVisibility,
    pub kind: Option<MemoryKind>,
    pub authority: Option<Authority>,
    pub sensitivity: Option<Sensitivity>,
    pub title: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub evidence: Vec<ObjectHash>,
    #[serde(default)]
    pub refs: Vec<String>,
    pub actor: ActorContext,
    pub idempotency_key: String,
    pub created_at: DateTime<Utc>,
}

/// Explainable receipt returned after prose capture.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NoteReceipt {
    pub idempotency_key: String,
    pub memory_id: MemoryId,
    pub version: ObjectHash,
    pub assertion: ObjectHash,
    pub status: MemoryStatus,
    pub kind: MemoryKind,
    pub authority: Authority,
    pub delivery: Delivery,
    pub scope: Scope,
    pub cursor: Option<ChangeCursor>,
    #[serde(default)]
    pub work_positions: Vec<FeedPosition>,
    pub classification_reason: String,
    pub policy_reason: String,
    pub duplicate: bool,
}

/// Immutable declaration that two memory versions cannot both guide action.
/// Hash ordering is canonicalized before this object is frozen.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MemoryContradictionEvent {
    pub schema_version: u16,
    pub project_id: ProjectId,
    #[serde(default)]
    pub task_id: Option<TaskId>,
    #[serde(default)]
    pub work_root_id: Option<WorkId>,
    pub left_version: ObjectHash,
    pub right_version: ObjectHash,
    pub reason: String,
    pub actor: ActorContext,
    pub created_at: DateTime<Utc>,
}

/// Idempotent result of declaring an explicit contradiction.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MemoryContradictionReceipt {
    pub idempotency_key: String,
    pub contradiction: ObjectHash,
    pub left_version: ObjectHash,
    pub right_version: ObjectHash,
    #[serde(default)]
    pub cursor: Option<ChangeCursor>,
    #[serde(default)]
    pub work_positions: Vec<FeedPosition>,
    pub duplicate: bool,
}

/// Compact, explainable memory view used by search and context indexes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MemorySummary {
    pub memory_id: MemoryId,
    pub version: ObjectHash,
    pub status: MemoryStatus,
    pub kind: MemoryKind,
    pub authority: Authority,
    pub delivery: Delivery,
    pub scope: Scope,
    pub title: String,
    pub body: String,
    pub sensitivity: Sensitivity,
    pub created_at: DateTime<Utc>,
}
