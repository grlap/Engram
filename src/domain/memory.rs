//! Typed memory vocabulary: kinds, authority, delivery, scope, status,
//! immutable versions, project-memory records, and notes.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ObjectId;

use super::{
    ActorContext, ChangeCursor, FeedPosition, MemoryId, ProjectId, SessionId, SourceSnapshot,
    TaskId, WorkId, WorkLifecycle,
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

/// Immutable content of one memory version. Its independently minted object id
/// is stored outside this payload and is not derived from its canonical content.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MemoryVersion {
    pub schema_version: u16,
    pub memory_id: MemoryId,
    /// Stable project-memory key. Ordinary typed memories omit it, preserving
    /// their canonical bytes; project episodes reserve it permanently.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub project_key: Option<String>,
    /// The item whose resolution makes this project memory worth reviewing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retiring_target: Option<ProjectMemoryRetiringTarget>,
    /// Set only by an explicit clear of the previous version's target, so a
    /// deliberate removal is never confused with a target a revision dropped
    /// without knowing about it (such as one written by an older build).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub retiring_target_cleared: bool,
    pub parents: Vec<ObjectId>,
    pub kind: MemoryKind,
    pub authority: Authority,
    pub delivery: Delivery,
    pub scope: Scope,
    pub title: String,
    pub body: String,
    pub structured_value: Option<Value>,
    pub tags: Vec<String>,
    pub evidence: Vec<ObjectId>,
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
    pub version: ObjectId,
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

/// A work item whose resolution makes a project memory worth reviewing.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProjectMemoryRetiringTarget {
    Local { work_id: WorkId, work_ref: String },
    External { project: String, reference: String },
}

/// Asserted input; local references are resolved within the bound project on write.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProjectMemoryRetiringTargetInput {
    Local { work_ref: String },
    External { project: String, reference: String },
}

/// What a `remember` does to the retiring target of the version it writes.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "change", rename_all = "snake_case")]
pub enum ProjectMemoryRetiringTargetChange {
    /// Keep the target of the version a revise builds on; a new key has none.
    #[default]
    Keep,
    /// Name this target, replacing any current one.
    Set {
        target: ProjectMemoryRetiringTargetInput,
    },
    /// Remove the current target and record the clear on the new version.
    Clear,
}

/// A local retiring target's lifecycle, read when the memory is read.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectMemoryRetiringState {
    pub lifecycle: WorkLifecycle,
    pub updated_at: DateTime<Utc>,
}

/// A retiring target that a later revision left out without an explicit
/// clear: `revision` is the first version without it, and `target` is the
/// one it had, which a revise can restore.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectMemoryRetiringTargetDropped {
    pub revision: u64,
    pub target: ProjectMemoryRetiringTarget,
}

/// Active memory keys whose current version names one local item as its
/// retiring target, bounded, with the exact total.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectMemoryRetirementCandidates {
    pub total: usize,
    pub omitted: usize,
    pub keys: Vec<String>,
}

/// Create request for one immutable project episode.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RememberProjectMemoryRequest {
    pub project_id: ProjectId,
    pub session_id: SessionId,
    pub key: Option<String>,
    #[serde(default)]
    pub revise: bool,
    pub expected_revision: Option<u64>,
    pub body: String,
    #[serde(default)]
    pub retiring_target: ProjectMemoryRetiringTargetChange,
    pub actor: ActorContext,
    pub created_at: DateTime<Utc>,
}

/// How a revise builds the new body from the revision it names: the text
/// replaces the whole body, is appended as a paragraph, or replaces the
/// interior of one marked section,
/// `<!-- engram-section NAME -->` … `<!-- /engram-section NAME -->`.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProjectMemoryEdit {
    #[default]
    Whole,
    Append,
    Section {
        name: String,
    },
}

impl ProjectMemoryEdit {
    #[must_use]
    pub fn word(&self) -> &'static str {
        match self {
            Self::Whole => "whole",
            Self::Append => "append",
            Self::Section { .. } => "section",
        }
    }
}

/// What a revision changed relative to the one it replaced: the one span
/// that differs, as bounded excerpts of what was removed and what was added,
/// with exact byte counts and the bytes each excerpt leaves out.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectMemoryChange {
    pub edit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub section: Option<String>,
    pub before_bytes: usize,
    pub after_bytes: usize,
    /// Byte offset where the changed span starts, in both bodies.
    pub span_start: usize,
    pub removed: String,
    pub removed_bytes: usize,
    pub removed_omitted_bytes: usize,
    pub added: String,
    pub added_bytes: usize,
    pub added_omitted_bytes: usize,
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
    pub revision: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub replaced_revision: Option<u64>,
    pub remembered_at: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub forgotten_at: Option<DateTime<Utc>>,
    pub duplicate: bool,
    /// What a revise changed relative to `replaced_revision`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub change: Option<ProjectMemoryChange>,
}

/// Compact project-memory row; the full body is available only through a
/// dedicated full read.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectMemoryListRow {
    pub key: String,
    pub revision: u64,
    pub first_line: String,
    pub remembered_at: DateTime<Utc>,
    pub actor_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_context: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retiring_target: Option<ProjectMemoryRetiringTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retiring_state: Option<ProjectMemoryRetiringState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retiring_target_dropped: Option<ProjectMemoryRetiringTargetDropped>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workaround: Option<bool>,
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
    pub revision: u64,
    pub current_revision: u64,
    pub body: String,
    pub remembered_at: DateTime<Utc>,
    pub actor_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_context: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retiring_target: Option<ProjectMemoryRetiringTarget>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retiring_state: Option<ProjectMemoryRetiringState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retiring_target_dropped: Option<ProjectMemoryRetiringTargetDropped>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workaround: Option<bool>,
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
    pub evidence: Vec<ObjectId>,
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
    pub version: ObjectId,
    pub assertion: ObjectId,
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

/// Compact, explainable memory view used by search and context indexes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MemorySummary {
    pub memory_id: MemoryId,
    pub version: ObjectId,
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
