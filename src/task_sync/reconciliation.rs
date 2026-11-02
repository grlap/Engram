//! Immutable, host-local reconciliation facts. These never enter shared task text.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{ActorContext, ObjectId, ProjectId, WorkClaimId, WorkId, WorkRunId};

use super::{TaskDocument, ledger::RetainedCommit};

pub const SHARED_VERSION_KIND: &str = "task_sync_shared_import_version";
pub const HOLD_KIND: &str = "task_sync_reconciliation_hold";

/// A retained head distinguishes an explicitly removed shared member from
/// local work that has never had an imported baseline.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ImportedBaseline {
    Present {
        head: ObjectId,
        task: Box<TaskDocument>,
    },
    Removed {
        head: ObjectId,
    },
}

/// A run's original candidate basis, not a claim or proof of host settlement.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReconciliationRunBasis {
    pub run_id: WorkRunId,
    pub generation: i64,
    pub revision: i64,
    pub claim_id: Option<WorkClaimId>,
    pub claim_fence: i64,
}

/// Exact local planning and generation membership captured for one candidate.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReconciliationWorkBasis {
    pub work_id: WorkId,
    pub revision: i64,
    pub head: Option<ObjectId>,
    pub imported_head: Option<ObjectId>,
    pub active_run_id: Option<WorkRunId>,
    pub runs: Vec<ReconciliationRunBasis>,
}

/// An explicitly retained merge base, never an assumed empty native history.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ReconciliationBase {
    KnownBase {
        commit: RetainedCommit,
    },
    ExplicitUnbasedResolution {
        local_basis: ObjectId,
        incoming_basis: ObjectId,
    },
}

/// The current imported baseline. Original creation provenance remains linked.
/// Current native edits are overlaid by the existing work exporter, not replaced.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SharedImportVersion {
    pub schema_version: u32,
    pub project_id: ProjectId,
    pub task: TaskDocument,
    pub predecessor: Option<ObjectId>,
    pub original_import_origin: Option<ObjectId>,
    pub original_restored_record: Option<ObjectId>,
    pub binding: ObjectId,
    pub prepared: ObjectId,
    pub receipt: ObjectId,
    pub base: ReconciliationBase,
    pub local_before: ReconciliationWorkBasis,
    pub shared_records: Vec<ObjectId>,
    pub actor: ActorContext,
    pub imported_at: DateTime<Utc>,
}

/// ONE immutable hold covers the candidate's complete affected scope. Multiple
/// per-run slots can reference it; non-run owners stay explicit in the scope.
/// Holding admission neither freezes files nor settles an original operation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReconciliationHold {
    pub schema_version: u32,
    pub project_id: ProjectId,
    pub binding: ObjectId,
    pub prepared: ObjectId,
    /// Seeds used to recompute the complete connected scope under the writer.
    pub seeds: Vec<WorkId>,
    pub candidate_fingerprint: ObjectId,
    pub candidate_revision: u64,
    pub hold_revision: u64,
    pub scope_revision: u64,
    pub owner_set_revision: u64,
    pub association: ObjectId,
    pub affected: Vec<ReconciliationWorkBasis>,
    pub non_run_owner_refs: Vec<ObjectId>,
    pub previous: Option<ObjectId>,
    pub actor: ActorContext,
    pub created_at: DateTime<Utc>,
}

/// Per-task facts in the one E1 reconciliation outcome; not a second receipt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ReconciledTask {
    pub task: WorkId,
    pub previous_head: Option<ObjectId>,
    pub shared_version: ObjectId,
    pub local_record_ids: Vec<ObjectId>,
}

/// Nested in E1's `ReconciledImportApplied` event. Its containing event supplies
/// identity, binding, sequence, predecessor, attribution and recorded time.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ModelAImportReceipt {
    pub operation: uuid::Uuid,
    pub operation_key: String,
    pub candidate_fingerprint: ObjectId,
    pub base: ReconciliationBase,
    pub resolved: RetainedCommit,
    /// Admitted folder-baseline delta; every versioned task appears in this delta; no second graph inventory.
    pub planning: ObjectId,
    pub before: Vec<ReconciliationWorkBasis>,
    pub after_basis: ObjectId,
    pub tasks: Vec<ReconciledTask>,
    pub holds: Vec<ObjectId>,
    pub terminal_evidence: Vec<ObjectId>,
}
