//! Binding one native passed check to several held items of a changeset.
//!
//! A check ran once, on one item's run; the host asks to credit it on other
//! items it holds whose named roots hold the same content. Each target gets
//! one verification record on its own run whose producer is the original
//! execution and whose typed `bound_from` says where the check ran and who
//! bound it when. No observation, turn or focus change is created.

use serde::{Deserialize, Serialize};

use super::{BindMeasurement, ControlWorkBinding, VerificationRequirement, WorkId, WorkRunId};
use crate::ObjectId;

/// The most items one request may bind.
pub const MAX_VERIFICATION_BIND_TARGETS: usize = 16;

/// One explicit request to bind `original` to every target.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationBindInput {
    pub idempotency_key: String,
    /// The original native verification record.
    pub original: ObjectId,
    /// The host's measurement of the shared root, taken to bind.
    pub measurement: BindMeasurement,
    pub targets: Vec<VerificationBindTarget>,
}

/// One target: its claim binding as the host holds it, its existing newest
/// sighting, and the criteria the binder intends the check for.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationBindTarget {
    pub binding: ControlWorkBinding,
    pub sighting: VerificationBindSighting,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub criteria: Vec<u32>,
}

/// A target run's existing newest sighting, as the host read it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationBindSighting {
    pub observation: ObjectId,
    pub source_revision: String,
}

/// What a committed bind wrote, per target in request order.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationBindReceipt {
    pub original: ObjectId,
    /// Whether this answer replays a request committed earlier.
    pub replayed: bool,
    pub bound: Vec<BoundVerificationTarget>,
}

/// One target's bound record.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BoundVerificationTarget {
    pub work_id: WorkId,
    pub work_ref: String,
    pub run_id: WorkRunId,
    pub verification: ObjectId,
    pub run_position: i64,
    /// Rule ids of the obligations the record satisfied, in trigger order.
    pub obligations_satisfied: Vec<String>,
    pub criteria: Vec<BoundCriterionEligibility>,
}

/// Whether the bound check suits one intended criterion's binding. It is
/// never a verdict on the criterion.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BoundCriterionEligibility {
    pub position: u32,
    /// The criterion's bound requirement, absent when it binds none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub binding: Option<VerificationRequirement>,
    pub eligible: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
}

/// Why a bind request was refused; nothing was written.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationBindRefusal {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub original: Option<VerificationBindOriginalRefusal>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request: Option<VerificationBindRequestRefusal>,
    pub targets: Vec<VerificationBindTargetRefusal>,
}

impl VerificationBindRefusal {
    /// One line naming every refused part, for the error message.
    #[must_use]
    pub fn summary(&self) -> String {
        let mut parts = Vec::new();
        if let Some(original) = &self.original {
            parts.push(format!("original {}", original.code()));
        }
        if let Some(request) = &self.request {
            parts.push(format!("request {}", request.code()));
        }
        for target in &self.targets {
            parts.push(format!(
                "target {} {}",
                target.work_ref,
                target.reason.code()
            ));
        }
        parts.join("; ")
    }
}

/// Why the original cannot be bound.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationBindOriginalRefusal {
    NotFound,
    NotVerification,
    IsBound,
    NotPassed,
    ProducerNotSucceeded,
    Rootless,
    ClaimNotHeld,
    RootNotCurrent,
}

impl VerificationBindOriginalRefusal {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::NotVerification => "not_verification",
            Self::IsBound => "is_bound",
            Self::NotPassed => "not_passed",
            Self::ProducerNotSucceeded => "producer_not_succeeded",
            Self::Rootless => "rootless",
            Self::ClaimNotHeld => "claim_not_held",
            Self::RootNotCurrent => "root_not_current",
        }
    }
}

/// Why the request as a whole cannot be bound.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationBindRequestRefusal {
    NoTargets,
    TooManyTargets,
    MoreTargetsThanHeldClaims,
    InvalidIdempotencyKey,
    MeasurementWorkspaceDiffers,
    MeasurementRevisionDiffers,
}

impl VerificationBindRequestRefusal {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::NoTargets => "no_targets",
            Self::TooManyTargets => "too_many_targets",
            Self::MoreTargetsThanHeldClaims => "more_targets_than_held_claims",
            Self::InvalidIdempotencyKey => "invalid_idempotency_key",
            Self::MeasurementWorkspaceDiffers => "measurement_workspace_differs",
            Self::MeasurementRevisionDiffers => "measurement_revision_differs",
        }
    }
}

/// One refused target, with the expected and actual values where a value
/// differed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationBindTargetRefusal {
    pub work_id: WorkId,
    pub work_ref: String,
    pub reason: VerificationBindTargetReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actual: Option<String>,
    /// What obtains the missing fact, for a sighting refusal.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub remedy: Option<String>,
}

/// Why one target cannot be bound.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationBindTargetReason {
    NotFound,
    DuplicateTarget,
    IsOriginalItem,
    ClaimNotHeld,
    ClaimFenceMoved,
    WorkRevisionMoved,
    RootExecutionMoved,
    WorkNotOpen,
    RunNotActive,
    NoNamedRoot,
    WorkspaceDiffers,
    SightingMissing,
    SightingNotNewest,
    SightingScopeDiffers,
    SightingRevisionDiffers,
    UnresolvedSourceAfterSighting,
    /// The check completed before an accounted unadmitted change on the
    /// target's run was recorded, so on that run it could satisfy nothing.
    CheckPredatesUnadmittedChange,
    /// The criteria are not strictly increasing positions, so a position
    /// repeats or the list is out of order.
    CriteriaNotIncreasing,
    CriterionOutOfRange,
    AlreadyBound,
}

impl VerificationBindTargetReason {
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::NotFound => "not_found",
            Self::DuplicateTarget => "duplicate_target",
            Self::IsOriginalItem => "is_original_item",
            Self::ClaimNotHeld => "claim_not_held",
            Self::ClaimFenceMoved => "claim_fence_moved",
            Self::WorkRevisionMoved => "work_revision_moved",
            Self::RootExecutionMoved => "root_execution_moved",
            Self::WorkNotOpen => "work_not_open",
            Self::RunNotActive => "run_not_active",
            Self::NoNamedRoot => "no_named_root",
            Self::WorkspaceDiffers => "workspace_differs",
            Self::SightingMissing => "sighting_missing",
            Self::SightingNotNewest => "sighting_not_newest",
            Self::SightingScopeDiffers => "sighting_scope_differs",
            Self::SightingRevisionDiffers => "sighting_revision_differs",
            Self::UnresolvedSourceAfterSighting => "unresolved_source_after_sighting",
            Self::CheckPredatesUnadmittedChange => "check_predates_unadmitted_change",
            Self::CriteriaNotIncreasing => "criteria_not_increasing",
            Self::CriterionOutOfRange => "criterion_out_of_range",
            Self::AlreadyBound => "already_bound",
        }
    }
}
