//! What an earlier build's `verification_bind` operation stored.
//!
//! A check ran once, on one item's run, and the host credited it on other
//! items it held whose named roots held the same content. Each target got one
//! verification record on its own run whose producer is the original
//! execution and whose typed `bound_from` says where the check ran and who
//! bound it when. The operation is gone; these shapes decode what it stored.

use serde::{Deserialize, Serialize};

use super::{ControlWorkBinding, VerificationRequirement, WorkId, WorkRunId};
use crate::ObjectId;

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
