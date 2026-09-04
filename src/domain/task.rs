//! Local task lifecycle, finalization barrier, context packets, task deltas,
//! and frozen report records.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::ObjectHash;

use super::{
    ActorContext, Authority, ChangeCursor, FeedPosition, MemoryAssertionEvent, MemoryId,
    MemoryKind, MemoryStatus, MemorySummary, MemoryVersion, ProjectId, SessionId, TaskId, WorkId,
};

/// Local task lifecycle. External trackers remain authoritative for the
/// organizational work item referenced by `external_ref`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Active,
    Quiescing,
    FinalizationPending,
    ReportReady,
    Publishing,
    Published,
}

/// One participant's state at the task finalization barrier.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ParticipantReadiness {
    Working {
        participant: String,
    },
    Ready {
        participant: String,
        contribution: ObjectHash,
    },
    Waived {
        participant: String,
        waived_by: String,
        reason: String,
    },
}

impl ParticipantReadiness {
    /// Returns the participant represented by this barrier entry.
    #[must_use]
    pub fn participant(&self) -> &str {
        match self {
            Self::Working { participant }
            | Self::Ready { participant, .. }
            | Self::Waived { participant, .. } => participant,
        }
    }

    /// Whether this participant is accounted for before report freeze.
    #[must_use]
    pub fn is_accounted_for(&self) -> bool {
        matches!(self, Self::Ready { .. } | Self::Waived { .. })
    }
}

/// Barrier that prevents a coordinator from freezing a report while a peer
/// is still contributing, unless that omission is explicitly waived.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FinalizationBarrier {
    pub task_id: TaskId,
    pub participants: Vec<ParticipantReadiness>,
}

impl FinalizationBarrier {
    /// Whether every expected participant contributed or was explicitly
    /// waived. An empty participant list is invalid and never ready.
    #[must_use]
    pub fn is_satisfied(&self) -> bool {
        !self.participants.is_empty()
            && self
                .participants
                .iter()
                .all(ParticipantReadiness::is_accounted_for)
    }

    /// Participants whose contribution still blocks report freeze.
    #[must_use]
    pub fn waiting_on(&self) -> Vec<&str> {
        self.participants
            .iter()
            .filter(|participant| !participant.is_accounted_for())
            .map(ParticipantReadiness::participant)
            .collect()
    }
}

/// Current exclusive execution claim. The mutable row is a coordination
/// projection; every transition also emits an immutable task event.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TaskLease {
    pub task_id: TaskId,
    pub lease_id: String,
    pub holder: SessionId,
    pub idempotency_key: String,
    /// Original requested duration. It fingerprints a retry independently of
    /// the wall-clock instant at which the transport repeats the call.
    #[serde(default)]
    pub ttl_seconds: i64,
    pub expires_at: DateTime<Utc>,
    pub revision: i64,
}

/// Immutable audit event for task ownership transitions.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TaskClaimEvent {
    pub schema_version: u16,
    pub lease: TaskLease,
    pub previous_holder: Option<SessionId>,
    pub actor: ActorContext,
    pub created_at: DateTime<Utc>,
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
    pub packet_hash: ObjectHash,
    pub event_cursor: ChangeCursor,
    pub proposed_count: u32,
    pub stale_count: u32,
}

/// One memory included in a context packet with an auditable retrieval basis.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextItem {
    pub memory_id: MemoryId,
    pub version: ObjectHash,
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
    pub version: ObjectHash,
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
    pub version_hash: ObjectHash,
    pub assertion_hash: ObjectHash,
    pub version: MemoryVersion,
    pub assertion: MemoryAssertionEvent,
}

/// One ordered task-feed item, decoded enough for an agent to act on it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeltaItem {
    pub cursor: ChangeCursor,
    pub object_kind: String,
    pub object_hash: ObjectHash,
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

/// Structured report sections frozen before any publication attempt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReportSections {
    pub outcome: String,
    pub work_performed: String,
    pub decisions: String,
    pub constraints_and_conventions: String,
    pub validation_and_evidence: String,
    pub unresolved_follow_ups: String,
    pub promotion_candidates: String,
    pub provenance: String,
}

/// Immutable report payload. Publication binds the resulting object hash to
/// exactly one idempotency key.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FrozenReport {
    pub schema_version: u16,
    pub report_id: Uuid,
    pub task_id: TaskId,
    pub supersedes: Option<ObjectHash>,
    pub source_memory_versions: Vec<ObjectHash>,
    pub participant_contributions: Vec<ObjectHash>,
    pub waived_participants: Vec<String>,
    pub sections: ReportSections,
    pub actor: ActorContext,
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(seed: &str) -> ObjectHash {
        ObjectHash::from_canonical_bytes(seed.as_bytes())
    }

    #[test]
    fn finalization_waits_for_every_participant() {
        let task_id = TaskId::new();
        let barrier = FinalizationBarrier {
            task_id,
            participants: vec![
                ParticipantReadiness::Ready {
                    participant: "codex".into(),
                    contribution: hash("codex contribution"),
                },
                ParticipantReadiness::Working {
                    participant: "fable".into(),
                },
            ],
        };

        assert!(!barrier.is_satisfied());
        assert_eq!(barrier.waiting_on(), vec!["fable"]);
    }

    #[test]
    fn explicit_waiver_satisfies_the_barrier() {
        let barrier = FinalizationBarrier {
            task_id: TaskId::new(),
            participants: vec![
                ParticipantReadiness::Ready {
                    participant: "codex".into(),
                    contribution: hash("codex contribution"),
                },
                ParticipantReadiness::Waived {
                    participant: "fable".into(),
                    waived_by: "coordinator".into(),
                    reason: "session ended before contributing".into(),
                },
            ],
        };

        assert!(barrier.is_satisfied());
        assert!(barrier.waiting_on().is_empty());
    }
}
