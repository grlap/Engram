//! Domain records shared by storage, context assembly, and tracker adapters.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

use crate::ObjectHash;

/// Schema version understood by this release.
pub const SCHEMA_VERSION: u16 = 1;

/// Stable host-local project identity shared by every session and worktree.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ProjectId(pub String);

/// Runtime session identity asserted by the host integration.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SessionId(pub String);

/// Monotonic position in a task's durable change feed.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ChangeCursor(pub i64);

/// Stable identifier for a memory across immutable versions.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct MemoryId(pub Uuid);

impl MemoryId {
    /// Creates a time-sortable identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for MemoryId {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable identifier for a local operational task.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct TaskId(pub Uuid);

impl TaskId {
    /// Creates a time-sortable identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for TaskId {
    fn default() -> Self {
        Self::new()
    }
}

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
    Agent {
        project: ProjectId,
        task: Option<TaskId>,
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
            Self::Project { .. } | Self::Agent { task: None, .. } => None,
        }
    }

    /// Whether the scope is visible to every participant of its task.
    #[must_use]
    pub fn is_task_shared(&self) -> bool {
        matches!(self, Self::Task { .. })
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
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssuranceLevel {
    Asserted,
    Authenticated,
    Signed,
}

/// Retrieval classification applied before context is assembled.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    Public,
    Internal,
    Restricted,
    SecretRef,
}

/// How an assertion reached the actor recording it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvenanceRelation {
    AssertedBy,
    RelayedBy,
    DerivedFrom,
}

/// One retained hop in an assertion's provenance chain.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProvenanceLink {
    pub relation: ProvenanceRelation,
    pub source: String,
    pub reference: Option<String>,
}

/// Fingerprint of mutable source material as it was observed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SourceSnapshot {
    pub source_ref: String,
    pub fingerprint: String,
    pub observed_at: DateTime<Utc>,
}

/// Attribution retained on every durable object.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ActorContext {
    pub actor_id: String,
    pub actor_kind: String,
    pub assurance: AssuranceLevel,
    pub run_id: Option<String>,
    pub session_id: Option<SessionId>,
    pub source_tool: Option<String>,
    pub source_skill: Option<String>,
    #[serde(default)]
    pub provenance_chain: Vec<ProvenanceLink>,
    pub reason: String,
}

/// Immutable content of one memory version. Its object hash is stored outside
/// this payload so identity is computed over canonical content only.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MemoryVersion {
    pub schema_version: u16,
    pub memory_id: MemoryId,
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
    pub classification_reason: String,
    pub policy_reason: String,
    pub duplicate: bool,
}

/// Immutable declaration that two memory versions cannot both guide action.
/// Hash ordering is canonicalized before this object is frozen.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MemoryContradictionEvent {
    pub schema_version: u16,
    pub task_id: TaskId,
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
    pub cursor: ChangeCursor,
    pub duplicate: bool,
}

/// Compact, explainable memory view used by search and context indexes.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
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

/// Local task lifecycle. External trackers remain authoritative for the
/// organizational work item referenced by `external_ref`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Active,
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

/// Canonical packet content stored under the hash returned in the header.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextPacketPayload {
    pub schema_version: u16,
    pub project_id: ProjectId,
    pub task_id: Option<TaskId>,
    pub agent_id: String,
    pub event_cursor: ChangeCursor,
    pub pinned: Vec<ContextItem>,
    pub index: Vec<ContextItem>,
    pub omissions: Vec<ContextOmission>,
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
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct DeltaItem {
    pub cursor: ChangeCursor,
    pub object_kind: String,
    pub object_hash: ObjectHash,
    pub memory: Option<MemorySummary>,
    pub object: Value,
}

/// Deterministic task delta; repeating the same request after restart returns
/// the same bytes while the underlying feed is unchanged.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
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
