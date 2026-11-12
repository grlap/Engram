//! Bounded read summaries of selected exchange requests, never source cursors.

use serde::Serialize;

use super::{Direction, ObjectId};

/// Planning includes the request's selected task fields, notes and activity
/// profile. Evidence retains historical originals, independently of planning.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExchangeFamily {
    Planning,
    Evidence,
}

/// An anchor in one binding's canonical exchange chain. Its sequence orders
/// exchange bookkeeping only; it is not a project, root or run feed cursor.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CaptureFrontier {
    pub prepared: ObjectId,
    pub sequence: u64,
    pub operation: uuid::Uuid,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CoverageMeaning {
    PlanningApplied,
    PublicationConfirmed,
    EvidenceRetained,
}

/// Coverage of the exact selection named by `capture`, not every earlier
/// request or every record in a source feed. Evidence retention is not credit.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct CoverageFrontier {
    pub capture: CaptureFrontier,
    pub outcome: ObjectId,
    pub sequence: u64,
    pub meaning: CoverageMeaning,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FamilyRequestState {
    Pending,
    PublicationUnknown,
    Deferred,
    Refused,
    Covered,
    ResolvedBySuccessor,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FamilyProgress {
    pub family: ExchangeFamily,
    pub direction: Direction,
    pub captured: Option<CaptureFrontier>,
    pub covered: Option<CoverageFrontier>,
    pub latest_state: Option<FamilyRequestState>,
    pub captured_requests: usize,
    pub covered_requests: usize,
    /// Includes earlier deferred, refused and pending selections. A newer
    /// success cannot silently bridge them. An applied explicit successor
    /// accounts for its original deferred request without covering it again.
    pub uncovered_requests: usize,
    pub deferred_requests: usize,
    pub refused_requests: usize,
    pub pending_requests: usize,
    pub resolved_by_successor: usize,
}

impl FamilyProgress {
    pub(crate) fn empty(family: ExchangeFamily, direction: Direction) -> Self {
        Self {
            family,
            direction,
            captured: None,
            covered: None,
            latest_state: None,
            captured_requests: 0,
            covered_requests: 0,
            uncovered_requests: 0,
            deferred_requests: 0,
            refused_requests: 0,
            pending_requests: 0,
            resolved_by_successor: 0,
        }
    }
}

/// All rows are derived on one read snapshot from validated canonical receipts.
/// No aggregate "fully synced" claim is made: coverage is selection-specific,
/// and current native evidence eligibility is checked by its separate owner.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct FamilyFrontiers {
    pub binding: ObjectId,
    pub tail: ObjectId,
    pub sequence: u64,
    pub families: Vec<FamilyProgress>,
}
