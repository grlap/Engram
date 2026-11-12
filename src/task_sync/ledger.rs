//! Host-local exchange bookkeeping. None of these records is shared task text.

use std::path::PathBuf;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{ActorContext, ObjectId, ProjectId, WorkId};

use super::progress::{ExportCut, ExportProfile};

mod coverage;
mod evidence;
mod publication;
pub use coverage::{
    CaptureFrontier, CoverageFrontier, CoverageMeaning, ExchangeFamily, FamilyFrontiers,
    FamilyProgress, FamilyRequestState,
};
pub use evidence::{
    EvidenceImportRecipe, EvidenceIntent, EvidencePackSelection, EvidenceReceipt, EvidenceRefusal,
    EvidenceStop,
};
pub(crate) use publication::DeltaResolutionRecipe;
pub use publication::{OutgoingFailure, OutgoingReconciliation};

pub const BINDING_KIND: &str = "task_sync_binding";
pub const EVENT_KIND: &str = "task_sync_exchange_event";

/// Observations of the bound store. The database path and restore occurrence
/// are compared, never treated as the store's identity or as proof of
/// continuity. The store's one logical identity is its optional sync origin
/// record, absent until the store is explicitly enrolled.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct StoreBasis {
    pub database: PathBuf,
    pub origin: Option<ObjectId>,
    pub restore_occurrence: Option<uuid::Uuid>,
    pub cut: ExportCut,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RepositoryBasis {
    pub enrollment: uuid::Uuid,
    /// Historical wire name. New exchange bindings select the dedicated data
    /// worktree here; existing records retain their original bytes and paths.
    pub main_checkout: PathBuf,
    pub git_directory: PathBuf,
}

/// Compares an already captured target/configuration; grants no transport authority.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct TargetBasis {
    pub remote: String,
    pub reference: String,
    pub configuration_fingerprint: ObjectId,
}

/// Immutable binding body. Its identity is the containing object's minted id.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Binding {
    pub schema_version: u32,
    pub project: ProjectId,
    pub store: StoreBasis,
    pub repository: RepositoryBasis,
    pub branch: String,
    pub target: TargetBasis,
    pub profile: ExportProfile,
    pub supersedes: Option<ObjectId>,
    pub superseded_tail: Option<ObjectId>,
    pub actor: ActorContext,
    pub created_at: DateTime<Utc>,
}

/// Raw candidate retained independently of preparation's current/predecessor journal.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedCandidate {
    pub operation: uuid::Uuid,
    pub directory: PathBuf,
    pub documents_fingerprint: ObjectId,
    pub planning_fingerprint: ObjectId,
    pub source_cut: ExportCut,
}

/// Exact Git object retention includes every byte of its original commit object.
/// OIDs identify Git objects, never Engram records.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedCommit {
    pub oid: String,
    pub tree: String,
    pub parents: Vec<String>,
    pub retention_ref: String,
    pub raw_bytes_fingerprint: ObjectId,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Direction {
    Incoming,
    Outgoing,
}

/// One canonical request; a retry compares the entire body including attribution.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Intent {
    pub operation: uuid::Uuid,
    pub key: String,
    pub direction: Direction,
    pub actor: ActorContext,
    pub producer: ObjectId,
    pub expected_head: Option<String>,
    pub common_commit: Option<RetainedCommit>,
    pub candidate: RetainedCandidate,
    pub commit: RetainedCommit,
    pub local_planning_basis: ObjectId,
}

/// Read-only historical selection. Equality covers the entire original intent,
/// including attribution; possession of this request grants no mutation authority.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletedExchangeRequest {
    pub binding: ObjectId,
    pub intent: Intent,
}

/// Original canonical outcome, not a re-created receipt or current graph check.
/// An outgoing outcome is complete only after confirmed publication.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CompletedExchange {
    pub prepared: ObjectId,
    pub outcome: ObjectId,
    pub event: Event,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ImportDisposition {
    Created,
    ExactRepeat,
    Reconciled,
}

/// References the retained source tree and original records, without copying history.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImportedTask {
    pub task: WorkId,
    pub source_task_path: String,
    pub before_basis: Option<ObjectId>,
    pub after_basis: ObjectId,
    pub local_record_ids: Vec<ObjectId>,
    pub disposition: ImportDisposition,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImportReceipt {
    pub source_commit: String,
    pub source_tree: String,
    pub before_basis: ObjectId,
    pub after_basis: ObjectId,
    pub notes_included: bool,
    pub tasks: Vec<ImportedTask>,
}

/// An identity-stable construction recipe retained outside the SQLite rollback
/// domain. Candidate bytes supply the documents; encoder and body comparisons
/// refuse reinterpretation after a build change.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImportPlan {
    pub schema_version: u32,
    pub encoder: ObjectId,
    pub binding: ObjectId,
    pub operation: uuid::Uuid,
    pub request_fingerprint: ObjectId,
    pub prepared: ObjectId,
    pub prepared_predecessor: ObjectId,
    pub predecessor: ObjectId,
    pub sequence: u64,
    pub actor: ActorContext,
    pub recorded_at: DateTime<Utc>,
    pub before_basis: ObjectId,
    pub after_basis: ObjectId,
    pub receipt_id: ObjectId,
    pub receipt_fingerprint: ObjectId,
    pub disposition: ImportDisposition,
    pub tasks: Vec<PlannedImportTask>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PlannedImportTask {
    pub task: WorkId,
    pub restored_id: ObjectId,
    pub origin_id: ObjectId,
    pub restored_fingerprint: ObjectId,
    pub origin_fingerprint: ObjectId,
    pub item_fingerprint: ObjectId,
}

/// A negative observation cannot establish that an earlier push never applied.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum PublicationObservation {
    Confirmed { observed_tip: String },
    Unknown { observed_tip: Option<String> },
}

/// Bookkeeping for a caller-owned outgoing operation. Recording a step never
/// publishes a branch, starts transport or grants permission for either act.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum OutgoingProgress {
    LocalCommitRecorded,
    PushStarted,
    /// The remote completed an exact non-fast-forward refusal of that start.
    /// Recorded before the replacement tip is fetched, so a lost fetch or
    /// response stays a known refusal; it never permits another push.
    PushRejected {
        push_started: ObjectId,
    },
    Observed(PublicationObservation),
    Reconciled {
        prepared: ObjectId,
        reconciliation: Box<OutgoingReconciliation>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "transition", rename_all = "snake_case", deny_unknown_fields)]
pub enum Transition {
    Activated,
    CustodyCheckpoint {
        checkpoint: Box<super::delta::ManagedCustodyCheckpoint>,
    },
    Prepared {
        intent: Box<Intent>,
    },
    DeltaPrepared {
        intent: Box<super::delta::DeltaIntent>,
        payload: Box<super::delta::DeltaCandidatePayload>,
    },
    PlanningSuccessorPrepared {
        recipe: Box<super::delta::PlanningSuccessorRecipe>,
        payload: Box<super::delta::DeltaCandidatePayload>,
    },
    PlanningSuccessorApplied {
        prepared: ObjectId,
        receipt: Box<super::delta::DeltaImportReceipt>,
    },
    EvidencePrepared {
        intent: Box<EvidenceIntent>,
    },
    EvidenceImportApplied {
        prepared: ObjectId,
        receipt: EvidenceReceipt,
    },
    EvidenceStopped {
        prepared: ObjectId,
        outcome: EvidenceStop,
    },
    DeltaLocalCommitRecorded {
        prepared: ObjectId,
        commit: RetainedCommit,
    },
    DeltaImportApplied {
        prepared: ObjectId,
        receipt: Box<super::delta::DeltaImportReceipt>,
    },
    /// Settles this planning request without applying its candidate. The
    /// existing reconciliation hold continues to fence the affected scope.
    PlanningDeferred {
        prepared: ObjectId,
        hold: ObjectId,
        candidate_fingerprint: ObjectId,
        blocking_claims: Vec<ObjectId>,
        /// Local bookkeeping capture, never an applied planning frontier.
        after_cut: super::delta::OriginQualifiedCut,
    },
    LocalCommitRecorded {
        prepared: ObjectId,
    },
    ImportApplied {
        prepared: ObjectId,
        receipt: ImportReceipt,
    },
    ReconciledImportApplied {
        prepared: ObjectId,
        receipt: Box<super::reconciliation::ModelAImportReceipt>,
    },
    PushStarted {
        prepared: ObjectId,
    },
    /// An exact non-fast-forward refusal of the named start: the original
    /// `PushStarted` or the retry's reconciliation start. No publication credit.
    PushRejected {
        prepared: ObjectId,
        push_started: ObjectId,
    },
    PushObserved {
        prepared: ObjectId,
        observation: PublicationObservation,
    },
    OutgoingReconciled {
        prepared: ObjectId,
        reconciliation: Box<OutgoingReconciliation>,
    },
}

/// Sequence and predecessor order one binding's ledger, never a work feed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Event {
    pub schema_version: u32,
    pub binding: ObjectId,
    pub sequence: u64,
    pub predecessor: Option<ObjectId>,
    pub actor: ActorContext,
    pub recorded_at: DateTime<Utc>,
    pub transition: Transition,
}

impl Binding {
    pub(crate) fn validate(&self) -> Result<(), crate::storage::StoreError> {
        require(
            self.schema_version == 1,
            "unsupported exchange binding schema",
        )?;
        text(&self.project.0, 4096)?;
        require(
            self.supersedes.is_some() == self.superseded_tail.is_some(),
            "superseded binding requires its exact ledger tail",
        )?;
        require(
            !self.repository.enrollment.is_nil(),
            "missing repository enrollment",
        )?;
        absolute(&self.store.database)?;
        absolute(&self.repository.main_checkout)?;
        absolute(&self.repository.git_directory)?;
        cut(&self.store.cut)?;
        reference(&self.branch, "refs/heads/")?;
        reference(&self.target.reference, "refs/heads/")?;
        text(&self.target.remote, 256)?;
        fingerprint(&self.target.configuration_fingerprint)?;
        self.profile
            .validate()
            .map_err(|_| super::invalid("invalid exchange profile"))?;
        attribution(&self.actor)
    }
}

impl Intent {
    pub(crate) fn validate(&self) -> Result<(), crate::storage::StoreError> {
        require(!self.operation.is_nil(), "missing exchange operation")?;
        text(&self.key, 4096)?;
        attribution(&self.actor)?;
        fingerprint(&self.producer)?;
        fingerprint(&self.local_planning_basis)?;
        fingerprint(&self.candidate.documents_fingerprint)?;
        fingerprint(&self.candidate.planning_fingerprint)?;
        absolute(&self.candidate.directory)?;
        require(
            !self.candidate.operation.is_nil(),
            "missing retained candidate operation",
        )?;
        cut(&self.candidate.source_cut)?;
        self.commit.validate()?;
        if let Some(commit) = &self.common_commit {
            commit.validate()?;
        }
        if let Some(head) = &self.expected_head {
            oid(head)?;
        }
        Ok(())
    }
}

impl RetainedCommit {
    pub(crate) fn validate(&self) -> Result<(), crate::storage::StoreError> {
        oid(&self.oid)?;
        fingerprint(&self.raw_bytes_fingerprint)?;
        oid(&self.tree)?;
        require(
            self.tree.len() == self.oid.len(),
            "mixed Git object formats",
        )?;
        require(self.parents.len() <= 64, "too many retained commit parents")?;
        for parent in &self.parents {
            oid(parent)?;
            require(parent.len() == self.oid.len(), "mixed Git parent format")?;
        }
        reference(&self.retention_ref, "refs/engram/task-sync/exchange/")
    }
}

pub(crate) fn attribution(actor: &ActorContext) -> Result<(), crate::storage::StoreError> {
    actor
        .validate_attribution_context()
        .map_err(super::invalid)?;
    text(&actor.actor_id, 4096)?;
    text(&actor.actor_kind, 256)?;
    let session = actor
        .session_id
        .as_ref()
        .ok_or_else(|| super::invalid("exchange attribution requires a session"))?;
    text(&session.0, 4096)
}

pub(crate) fn oid(value: &str) -> Result<(), crate::storage::StoreError> {
    require(
        matches!(value.len(), 40 | 64)
            && value
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
            && value.bytes().any(|byte| byte != b'0'),
        "invalid retained Git object id",
    )
}

pub(super) fn fingerprint(value: &ObjectId) -> Result<(), crate::storage::StoreError> {
    require(
        value.as_str().len() == 64,
        "exchange comparison requires a content fingerprint",
    )
}

fn reference(value: &str, prefix: &str) -> Result<(), crate::storage::StoreError> {
    require(
        value.len() <= 1024
            && value.starts_with(prefix)
            && value.len() > prefix.len()
            && !value.ends_with('/')
            && !value.contains("//")
            && !value.contains("..")
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || b"/._-".contains(&byte))
            && value.split('/').all(|part| {
                !part.starts_with('.')
                    && !part.ends_with('.')
                    && !part.as_bytes().ends_with(b".lock")
            }),
        "invalid exchange Git reference",
    )
}

pub(super) fn cut(value: &ExportCut) -> Result<(), crate::storage::StoreError> {
    require(
        value.position >= 0 && (value.position == 0) == value.anchor.is_none(),
        "invalid exchange feed cut",
    )?;
    if let Some(anchor) = &value.anchor {
        text(&anchor.object_kind, 256)?;
    }
    Ok(())
}

pub(super) fn absolute(value: &std::path::Path) -> Result<(), crate::storage::StoreError> {
    require(
        value.is_absolute()
            && !value
                .components()
                .any(|part| matches!(part, std::path::Component::ParentDir)),
        "exchange path must be absolute and normalized",
    )
}

pub(super) fn text(value: &str, limit: usize) -> Result<(), crate::storage::StoreError> {
    require(
        !value.is_empty()
            && value.trim() == value
            && value.len() <= limit
            && !value.chars().any(char::is_control),
        "invalid exchange text field",
    )
}

pub(crate) fn require(condition: bool, message: &str) -> Result<(), crate::storage::StoreError> {
    if condition {
        Ok(())
    } else {
        Err(super::invalid(message))
    }
}
