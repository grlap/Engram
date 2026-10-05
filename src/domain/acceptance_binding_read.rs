//! A host's read of what satisfied each bound acceptance criterion of one
//! item on its active run: the binding, the obligation that answers for it,
//! and that obligation's recorded resolution, with the original verification
//! and its producer. It reads the record as stored and computes no
//! freshness: a satisfied obligation names the check that closed it, even
//! when a newer check failed or the source moved since.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::ObjectId;

use super::{
    ActorContext, BindMeasurement, BuiltinObligationRuleRef, ExecutionOutcome,
    ExecutionSourceBasis, ProjectId, VerificationKind, VerificationRequirement, VerificationResult,
    WorkId, WorkObligationId, WorkObligationState, WorkRunId,
};

/// The most criterion rows one page holds.
pub const ACCEPTANCE_BINDING_READ_PAGE_ROWS: usize = 8;

/// The most bytes one page's serialized result holds.
pub const ACCEPTANCE_BINDING_READ_PAGE_BYTES: usize = 16 * 1_024;

/// One page of the read. Rows are complete and in ascending criterion
/// order; `continuation` is `None` exactly when no row remains.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceBindingPage {
    pub basis: AcceptanceBindingReadBasis,
    /// Every authored criterion of the item at the basis revision.
    pub total: usize,
    /// Rows on earlier pages.
    pub earlier: usize,
    /// Rows on this page.
    pub shown: usize,
    /// Rows on later pages.
    pub omitted: usize,
    pub rows: Vec<AcceptanceBindingRow>,
    pub continuation: Option<String>,
}

/// What every page of one read is pinned to. The first page captures the
/// run's feed head as `run_cut`; a continuation reads at the same cut.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceBindingReadBasis {
    pub project_id: ProjectId,
    pub work_id: WorkId,
    pub work_revision: i64,
    pub run_id: WorkRunId,
    pub run_cut: i64,
}

/// One authored criterion, by its one-based position. `binding` is `None`
/// for a criterion bound to no verification requirement.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceBindingRow {
    pub criterion: usize,
    pub binding: Option<AcceptanceBindingReadBinding>,
}

/// A bound criterion's requirement and the obligation that answers for it
/// on the run. `obligation` is `None` when the run's feed holds no
/// obligation opened for this binding, which says nothing of a waiver or a
/// pass. An obligation on the feed without its projection row is a damaged
/// store, never `None`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceBindingReadBinding {
    pub requirement: VerificationRequirement,
    pub obligation: Option<AcceptanceBindingObligation>,
}

/// The obligation the binding selects: the newest one the run opened for
/// this criterion and requirement. Positions are on the basis run's feed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceBindingObligation {
    pub obligation_id: WorkObligationId,
    /// The obligation's record id.
    pub definition: ObjectId,
    /// The item revision that opened it. A revision that left the criterion
    /// and its binding unchanged keeps the older obligation.
    pub work_revision: i64,
    pub rule: BuiltinObligationRuleRef,
    pub triggering_observation: ObjectId,
    pub trigger_position: i64,
    pub definition_position: i64,
    /// The obligation's state at the basis cut.
    pub state: WorkObligationState,
    /// `None` exactly when the state is open.
    pub resolution: Option<AcceptanceBindingResolution>,
}

/// How the obligation was resolved, as recorded.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptanceBindingResolutionKind {
    Satisfied,
    Waived,
    Displaced,
}

/// The obligation's terminal record. `satisfaction` is present exactly when
/// the kind is satisfied.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceBindingResolution {
    pub record: ObjectId,
    pub position: i64,
    pub kind: AcceptanceBindingResolutionKind,
    pub satisfaction: Option<AcceptanceBindingSatisfaction>,
}

/// The original evidence that satisfied the obligation, and the run-feed
/// cut the satisfaction was judged at.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceBindingSatisfaction {
    pub evaluated_cut: i64,
    pub verification: AcceptanceBindingVerification,
}

/// The host-minted verification that satisfied the obligation, as recorded.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceBindingVerification {
    pub record: ObjectId,
    pub position: i64,
    pub check_kind: VerificationKind,
    pub check_fingerprint: ObjectId,
    pub result: VerificationResult,
    pub source_basis: ExecutionSourceBasis,
    /// For a bound record, the original's producer: its record and position
    /// are on the original's run, not this one.
    pub producer: AcceptanceBindingProducer,
    /// Present only on a record that binds a check run on another item;
    /// absent on every native record, whose shape is unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound: Option<AcceptanceBoundVerification>,
}

/// Where a bound record's check ran and how it was bound to this item. The
/// check's own time is `original_completed_at`; `bound_at` is when it was
/// credited here.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceBoundVerification {
    /// The original native verification.
    pub verification: ObjectId,
    /// The original's item.
    pub work_ref: String,
    /// The original's run, on whose feed `original_position` and the
    /// producer's position lie.
    pub run: WorkRunId,
    pub original_position: i64,
    pub original_basis: ExecutionSourceBasis,
    pub original_completed_at: DateTime<Utc>,
    pub binder: ActorContext,
    pub bound_at: DateTime<Utc>,
    /// The target run's sighting the binding stood on.
    pub sighting: ObjectId,
    pub measurement: BindMeasurement,
    /// The criterion positions the binder intended; intent, not credit.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub criteria: Vec<u32>,
}

/// The execution observation that produced the verification.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceBindingProducer {
    pub record: ObjectId,
    pub position: i64,
    pub outcome: ExecutionOutcome,
}

/// Why a read was refused. A refusal is never an empty result.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptanceBindingReadRefusal {
    /// The store holds no item with this id.
    UnknownWork,
    /// The item belongs to another project.
    WrongProject,
    /// The item's revision is not the expected one.
    WrongRevision,
    /// The run is not the item's active run.
    WrongRun,
    /// The run's feed moved past the cut a continuation is pinned to.
    StaleCut,
    /// The continuation is not one this read issued.
    InvalidCursor,
    /// The continuation was issued for another item, revision or run.
    CursorBasisMismatch,
    /// One complete row does not fit a page.
    PageTooLarge,
}

impl AcceptanceBindingReadRefusal {
    /// The host error code this refusal answers with.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::UnknownWork => "acceptance_binding_read_unknown_work",
            Self::WrongProject => "acceptance_binding_read_wrong_project",
            Self::WrongRevision => "acceptance_binding_read_wrong_revision",
            Self::WrongRun => "acceptance_binding_read_wrong_run",
            Self::StaleCut => "acceptance_binding_read_stale_cut",
            Self::InvalidCursor => "acceptance_binding_read_invalid_cursor",
            Self::CursorBasisMismatch => "acceptance_binding_read_cursor_basis_mismatch",
            Self::PageTooLarge => "acceptance_binding_read_page_too_large",
        }
    }
}
