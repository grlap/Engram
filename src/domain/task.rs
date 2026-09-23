//! Host-local task bindings, context packets, and task deltas.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ObjectId;

use super::{
    ActorContext, Authority, ChangeCursor, FeedPosition, MemoryAssertionEvent, MemoryId,
    MemoryKind, MemoryStatus, MemorySummary, MemoryVersion, ProjectId, SessionId, TaskId, WorkId,
};

/// Local task lifecycle. A bound task is always active; no transition out of
/// that state exists.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Active,
}

/// Header returned with a context packet. The hash reproduces content; the
/// cursor orders later peer changes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextPacketHeader {
    pub project_id: ProjectId,
    pub task_id: Option<TaskId>,
    #[serde(default)]
    pub work_id: Option<WorkId>,
    #[serde(default)]
    pub work_feed_heads: Vec<FeedPosition>,
    /// Monotonic fence for project-visible memory that is not ordered by a
    /// task or work feed.
    #[serde(default)]
    pub project_context_revision: i64,
    /// Monotonic, owner-private fence. The revision reveals no private object
    /// identity and is scoped to the packet's project and agent.
    #[serde(default)]
    pub private_context_revision: i64,
    pub packet_hash: ObjectId,
    pub event_cursor: ChangeCursor,
    pub proposed_count: u32,
    pub stale_count: u32,
}

/// One memory included in a context packet with an auditable retrieval basis.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextItem {
    pub memory_id: MemoryId,
    pub version: ObjectId,
    pub kind: MemoryKind,
    pub authority: Authority,
    pub status: MemoryStatus,
    pub title: String,
    pub body: Option<String>,
    pub retrieval_reason: String,
}

/// Visible record excluded from a bounded packet.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextOmission {
    pub memory_id: MemoryId,
    pub version: ObjectId,
    pub reason: String,
}

/// Count of additional omitted memories after the exact manifest is bounded.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextOmissionSummary {
    pub reason: String,
    pub count: u32,
}

/// Canonical packet content stored under the hash returned in the header.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextPacketPayload {
    pub schema_version: u16,
    pub project_id: ProjectId,
    pub task_id: Option<TaskId>,
    #[serde(default)]
    pub work_id: Option<WorkId>,
    #[serde(default)]
    pub work_feed_heads: Vec<FeedPosition>,
    #[serde(default)]
    pub project_context_revision: i64,
    #[serde(default)]
    pub private_context_revision: i64,
    pub agent_id: String,
    pub event_cursor: ChangeCursor,
    pub pinned: Vec<ContextItem>,
    pub index: Vec<ContextItem>,
    pub omissions: Vec<ContextOmission>,
    #[serde(default)]
    pub omission_summaries: Vec<ContextOmissionSummary>,
    pub proposed_count: u32,
    pub stale_count: u32,
    pub created_at: DateTime<Utc>,
}

/// Context result returned by CLI and MCP.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextPacket {
    pub header: ContextPacketHeader,
    pub pinned: Vec<ContextItem>,
    pub index: Vec<ContextItem>,
    pub omissions: Vec<ContextOmission>,
    #[serde(default)]
    pub omission_summaries: Vec<ContextOmissionSummary>,
}

/// Authorized full-memory view with its initial activation event.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MemoryRecord {
    #[serde(rename = "version_hash")]
    pub version_id: ObjectId,
    #[serde(rename = "assertion_hash")]
    pub assertion_id: ObjectId,
    pub version: MemoryVersion,
    pub assertion: MemoryAssertionEvent,
}

/// One ordered task-feed item, decoded enough for an agent to act on it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeltaItem {
    pub cursor: ChangeCursor,
    pub object_kind: String,
    #[serde(rename = "object_hash")]
    pub object_id: ObjectId,
    pub memory: Option<MemorySummary>,
    pub object: Value,
}

/// Deterministic task delta; repeating the same request after restart returns
/// the same bytes while the underlying feed is unchanged.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TaskDelta {
    pub task_id: TaskId,
    pub after: ChangeCursor,
    pub cursor: ChangeCursor,
    pub changes: Vec<DeltaItem>,
}

/// Local operational task; this is a reference to, never a mirror of, an
/// external ticket.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LocalTask {
    pub schema_version: u16,
    pub project_id: ProjectId,
    pub task_id: TaskId,
    pub title: String,
    pub external_ref: Option<String>,
    pub participants: Vec<SessionId>,
    pub state: TaskState,
    pub event_cursor: ChangeCursor,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Immutable creation event for a local task bound to an external reference.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TaskStartedEvent {
    pub schema_version: u16,
    pub task_id: TaskId,
    pub project_id: ProjectId,
    pub title: String,
    pub external_ref: String,
    pub participant: SessionId,
    pub actor: ActorContext,
    pub created_at: DateTime<Utc>,
}

/// Immutable event emitted the first time another session joins a task.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TaskJoinedEvent {
    pub schema_version: u16,
    pub task_id: TaskId,
    pub participant: SessionId,
    pub actor: ActorContext,
    pub created_at: DateTime<Utc>,
}

/// Ref-bound task result shared by CLI and MCP.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TaskBindReceipt {
    pub task: LocalTask,
    pub joined: bool,
    pub cursor: ChangeCursor,
}
