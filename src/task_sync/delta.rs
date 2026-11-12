//! Additive delta payloads. These types alone expose no writable transition.
//!
//! Historical ledger and planning bodies keep their existing formats. Storage
//! must establish canonical links, graph admission, continuity and freshness;
//! validating one of these payloads proves only its internal consistency.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::PathBuf,
};

use serde::{Deserialize, Serialize};

use super::{
    TaskDocument,
    inventory::InventoryRoot,
    ledger::{Direction, RetainedCommit},
    progress::{ExportProfile, FeedAnchor},
    reconciliation::ReconciliationWorkBasis,
};
use crate::{ActorContext, FeedPosition, ObjectId, ProjectId, WorkId};

mod checkpoint;
mod successor;
pub use successor::{
    PlannedRunRetirement, PlannedSuccessorObligation, PlannedSuccessorResolution,
    PlanningSuccessorChain, PlanningSuccessorRecipe, PlanningSuccessorRequest,
};
#[cfg(test)]
mod tests;
mod validate;

#[cfg(test)]
pub(crate) mod validation_probe {
    use std::cell::Cell;

    thread_local! {
        static ADMISSIONS: Cell<usize> = const { Cell::new(0) };
    }

    pub(super) fn count() {
        ADMISSIONS.with(|count| count.set(count.get() + 1));
    }

    pub(crate) fn admissions() -> usize {
        ADMISSIONS.with(Cell::get)
    }
}

pub const DELTA_PLANNING_KIND: &str = "task_sync_delta_planning";
pub const DELTA_PAYLOAD_FORMAT: &str = "engram-task-sync-delta-v1";

/// A named local project feed qualified by its explicitly enrolled origin and
/// binding. The restore occurrence is an observation, never execution authority.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OriginQualifiedCut {
    pub binding: ObjectId,
    pub origin: ObjectId,
    pub restore_occurrence: Option<uuid::Uuid>,
    #[serde(deserialize_with = "validate::project_position")]
    pub position: FeedPosition,
    pub anchor: Option<FeedAnchor>,
}

/// Membership comparison and disclosure profile, without a claim of delivery.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct OutputMembership {
    pub inventory_root: InventoryRoot,
    pub profile: ExportProfile,
}

/// An exact retained Git snapshot, not an unverified project.json fingerprint.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct MembershipSnapshot {
    pub commit: RetainedCommit,
    pub membership: OutputMembership,
}

/// Bootstrap must be explicitly admitted; absence never means an empty source.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum IncomingSourceAnchor {
    Applied { outcome: ObjectId },
    Bootstrap { checkpoint: ObjectId },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct IncomingSourceBasis {
    pub anchor: IncomingSourceAnchor,
    pub snapshot: MembershipSnapshot,
}

/// Preparation and confirmed publication are different canonical anchors.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum PreparedExportAnchor {
    Prepared { event: ObjectId },
    Bootstrap { checkpoint: ObjectId },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedExportBasis {
    pub anchor: PreparedExportAnchor,
    pub membership: OutputMembership,
    pub cut: OriginQualifiedCut,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConfirmedExportBasis {
    pub outcome: ObjectId,
    pub prepared: ObjectId,
    pub snapshot: MembershipSnapshot,
}

/// The local destination baseline can be newer than the last incoming source.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DestinationPlanningBasis {
    pub baseline: ObjectId,
    pub membership: OutputMembership,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum DeltaBasis {
    Incoming {
        source_before: Box<IncomingSourceBasis>,
        /// The exact explicitly admitted resolved commit, not an inferred tip.
        source_after: Box<MembershipSnapshot>,
        source_changed: BTreeSet<WorkId>,
        local_before: Box<DestinationPlanningBasis>,
    },
    Outgoing {
        export_before: Box<PreparedExportBasis>,
        /// None means no publication has been confirmed, not an empty basis.
        confirmed_before: Option<Box<ConfirmedExportBasis>>,
    },
}

/// Only a descriptor enters the canonical intent. Changed document bytes are
/// retained separately and later stored by the existing planning owners.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RetainedDeltaCandidate {
    pub operation: uuid::Uuid,
    pub directory: PathBuf,
    pub content_fingerprint: ObjectId,
    pub output: OutputMembership,
    /// Changes against the local destination or preceding prepared export.
    pub changed: BTreeSet<WorkId>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeltaIntent {
    pub operation: uuid::Uuid,
    pub key: String,
    pub direction: Direction,
    pub actor: ActorContext,
    pub producer: ObjectId,
    pub expected_head: Option<String>,
    pub common_commit: Option<RetainedCommit>,
    pub local_cut: OriginQualifiedCut,
    pub basis: DeltaBasis,
    pub candidate: RetainedDeltaCandidate,
}

/// Actual retained commit selected after outgoing preparation. Its later
/// canonical event must link the original prepared request; historical
/// `LocalCommitRecorded` bodies are not extended or reinterpreted.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeltaCommitSelection {
    pub prepared: ObjectId,
    pub commit: RetainedCommit,
}

/// Retained changed bytes, never a partial `TaskTree` admitted as a project.
/// Source and destination selections share one document when their keys overlap.
/// Outgoing preparation need not invent a Git commit that does not exist yet.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeltaCandidatePayload {
    pub format: String,
    pub project_id: ProjectId,
    pub operation: uuid::Uuid,
    pub output: OutputMembership,
    /// Keys equal the union of `source_changed` and `candidate.changed`.
    pub documents: BTreeMap<WorkId, Option<TaskDocument>>,
}

/// Resolve by map key: original /task or existing planning /changes/<work id>.
/// Storage checks the kind, project, membership and body before admission.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SourceMemberRef {
    pub record_id: ObjectId,
}

/// One present bootstrap member. Storage resolves a canonical link by work key
/// and checks its project, exact body and disclosure profile. Embed a document
/// only when no suitable canonical owner exists; its checkpoint then owns it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BootstrapMember {
    Canonical { member: SourceMemberRef },
    Document { task: Box<TaskDocument> },
}

/// Complete membership, not changed members or tombstones. An initial import
/// must resolve to a complete legacy `ImportApplied` receipt, never a reconciled
/// receipt. Storage proves completeness and recomputes the membership root.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum BootstrapMembers {
    InitialImport {
        outcome: ObjectId,
    },
    Captured {
        members: BTreeMap<WorkId, BootstrapMember>,
    },
}

/// A checkpoint seeds one role. Incoming membership belongs to the exact Git
/// snapshot, independently of local planning. Export membership belongs to the
/// checkpoint's local cut and makes no claim of confirmed publication.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CheckpointSeed {
    Incoming {
        snapshot: Box<MembershipSnapshot>,
        members: BootstrapMembers,
    },
    PreparedExport {
        membership: OutputMembership,
        members: BootstrapMembers,
    },
}

/// Source-relative links are distinct from the existing local-baseline delta.
/// The containing applied event supplies its outcome identity; there is no
/// outcome link here that could make the canonical graph circular.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeltaPlanning {
    pub project_id: ProjectId,
    pub prepared: ObjectId,
    pub local_planning: ObjectId,
    pub source_changes: BTreeMap<WorkId, Option<SourceMemberRef>>,
}

/// Nested in the delta ledger outcome; canonical admission requires the atomic
/// graph writer, never a standalone record-applied API.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeltaImportReceipt {
    pub planning: ObjectId,
    pub after_cut: OriginQualifiedCut,
    pub before: Vec<ReconciliationWorkBasis>,
    pub tasks: Vec<DeltaImportedTask>,
    pub holds: Vec<ObjectId>,
    pub terminal_evidence: Vec<ObjectId>,
}

/// Local imported-head observation after one selected change. An existing
/// baseline record can own several work members, including tombstones. Native
/// work can retain no imported head. Storage checks each exact canonical owner
/// and work member; this is never a fabricated shared-version record.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeltaImportedTask {
    pub task: WorkId,
    pub previous_head: Option<ObjectId>,
    pub imported_head: Option<ObjectId>,
    pub local_record_ids: Vec<ObjectId>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointReason {
    Bootstrap,
    NamedAudit,
    NamedRebuild,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CustodyArtifactKind {
    Candidate,
    Commit,
    ImportPlan,
    PublicationWitness,
    EvidenceRecipe,
    PlanningSuccessorRecipe,
    /// A pin owned by this incoming data bootstrap checkpoint's seed.
    IncomingDataCommit,
    /// Explicit abandonment accounts for a fetched pin, never its contents.
    AbandonedFetchedCommit,
    /// Exact custody of a delta construction that never entered the ledger.
    AbandonedDeltaConstruction,
    /// Exact custody of an evidence recipe that never entered the ledger.
    AbandonedEvidenceConstruction,
    /// Exact custody of a successor recipe that never entered the ledger.
    AbandonedSuccessorConstruction,
}

/// The retained family selected for explicit noncanonical custody accounting.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RetainedConstructionKind {
    Delta,
    Evidence,
    Successor,
}

/// Comparison/provenance only; a publication witness does not imply success.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CustodyArtifactRef {
    pub operation: uuid::Uuid,
    pub kind: CustodyArtifactKind,
    pub content_fingerprint: ObjectId,
    pub record: Option<ObjectId>,
}

/// Explicit cold selection. The local destination baseline is never a source
/// seed, and custody-only audit does not establish an empty membership.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum CustodySeedSelection {
    CustodyOnly,
    InitialImport {
        outcome: ObjectId,
    },
    IncomingCommit {
        commit: String,
        prepared: ObjectId,
    },
    /// An exact pinned folder-envelope commit. The checkpoint itself owns
    /// this cold bootstrap; no legacy raw-document preparation is invented.
    IncomingDataCommit {
        commit: RetainedCommit,
    },
    /// Preserve an unrequested fetched carrier under its original binding.
    /// This is custody accounting only, with no source membership seed.
    AbandonFetchedCommit {
        binding: ObjectId,
        commit: RetainedCommit,
    },
    /// Preserve an exact construction while permanently retiring its request.
    /// The enclosing audit uses the original construction's operation and key.
    AbandonRetainedConstruction {
        binding: ObjectId,
        family: RetainedConstructionKind,
        construction_fingerprint: ObjectId,
    },
    LocalExport,
}

/// Exact cold-audit request. Attribution and time belong to the first retained
/// event, while every retry compares these selections before recapturing files.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CustodyAuditRequest {
    pub binding: ObjectId,
    pub operation: uuid::Uuid,
    pub key: String,
    pub reason: CheckpointReason,
    pub previous_checkpoint: Option<ObjectId>,
    pub expected_tail: ObjectId,
    pub expected_local_cut: OriginQualifiedCut,
    pub seed: CustodySeedSelection,
}

/// Exact checkpoint result. Acknowledgement observes local custody only;
/// it makes no publication or ordinary managed-write claim.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CustodyAuditReceipt {
    pub checkpoint: ObjectId,
    pub operation: uuid::Uuid,
    pub request_fingerprint: ObjectId,
    pub repeated: bool,
    pub acknowledged: bool,
}

/// Cold audit metadata for a later `CustodyCheckpoint` exchange-event variant.
/// The enclosing event's minted ID is its checkpoint generation. Defining this
/// payload does not provide a runnable audit, settle effects or grant authority.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ManagedCustodyCheckpoint {
    pub operation: uuid::Uuid,
    pub key: String,
    /// Compared request content, never the checkpoint's identity or a link.
    pub request_fingerprint: ObjectId,
    pub previous_checkpoint: Option<ObjectId>,
    pub local_cut: OriginQualifiedCut,
    pub reason: CheckpointReason,
    /// None audits custody only; it does not imply an empty source membership.
    pub seed: Option<CheckpointSeed>,
    pub artifacts: Vec<CustodyArtifactRef>,
}
