//! Explicit successor enrollment after loss of installation-local custody.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::task_sync::{
    ledger::{RepositoryBasis, TargetBasis},
    progress::ExportCut,
};
use crate::{ActorContext, ObjectId, ProjectId};

pub const RECOVERY_KIND: &str = "task_sync_origin_recovery";

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryCause {
    DbOnlyClone,
    SameOriginRewind,
    MigrationImport,
}

/// A deliberate new local lineage, never adoption of the predecessor's custody.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryRequest {
    pub operation: uuid::Uuid,
    pub predecessor: ObjectId,
    pub cause: RecoveryCause,
}

/// Content comparisons of preserved private bytes, not object links or identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryCustody {
    pub enrollment_intent: Option<ObjectId>,
    pub registration: Option<ObjectId>,
    pub audit_intent: Option<ObjectId>,
    /// Reported predecessor facts, not adopted authority. These keep a witness
    /// ahead of a rewound DB visible even when its binding is absent there.
    pub witness: Option<RecoveryWitness>,
    pub registration_uninterpretable: bool,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryWitness {
    pub project: ProjectId,
    pub origin: ObjectId,
    pub enrollment_operation: uuid::Uuid,
    pub cut: ExportCut,
    pub exchange: Option<RecoveryExchangeAnchor>,
    /// A request content comparison, not an object-link or successful receipt.
    pub pending_exchange: Option<ObjectId>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryExchangeAnchor {
    pub binding: ObjectId,
    pub head: ObjectId,
    pub sequence: u64,
    pub cut: ExportCut,
}

/// Original ledger references and the target whose unresolved effects remain held.
/// Recovery neither changes the referenced stage nor settles its operation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryBinding {
    pub binding: ObjectId,
    pub head: ObjectId,
    pub sequence: u64,
    pub repository: RepositoryBasis,
    pub branch: String,
    pub target: TargetBasis,
    pub unresolved: bool,
}

/// Attributed recovery boundary. Both origin UUIDs refer to immutable Enrollment
/// objects; the containing object's separately minted UUID identifies this fact.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryRecord {
    pub schema_version: u32,
    pub project_id: ProjectId,
    pub request: RecoveryRequest,
    pub successor_origin: ObjectId,
    pub predecessor_cut: ExportCut,
    pub custody: RecoveryCustody,
    pub bindings: Vec<RecoveryBinding>,
    pub actor: ActorContext,
    pub recovered_at: DateTime<Utc>,
}

/// Canonical recovery result, not a capability or assertion of current custody.
/// Replaying it never recreates a missing installation witness.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RecoveryReceipt {
    pub recovery: ObjectId,
    pub record: RecoveryRecord,
}

impl RecoveryRecord {
    pub(crate) fn validate(&self) -> Result<(), crate::storage::StoreError> {
        use crate::task_sync::ledger::{attribution, require};
        require(
            self.schema_version == 1 && !self.request.operation.is_nil(),
            "invalid origin recovery request",
        )?;
        require(
            !self.project_id.0.trim().is_empty()
                && self.request.predecessor != self.successor_origin,
            "invalid origin recovery lineage",
        )?;
        require(
            self.predecessor_cut.position >= 0
                && (self.predecessor_cut.position == 0) == self.predecessor_cut.anchor.is_none(),
            "invalid origin recovery cut",
        )?;
        require(
            self.bindings.len() <= 64,
            "origin recovery has too many bindings",
        )?;
        let mut bindings = std::collections::BTreeSet::new();
        for binding in &self.bindings {
            require(
                binding.sequence > 0 && bindings.insert(&binding.binding),
                "invalid or repeated origin recovery binding",
            )?;
        }
        for fingerprint in [
            &self.custody.enrollment_intent,
            &self.custody.registration,
            &self.custody.audit_intent,
        ]
        .into_iter()
        .flatten()
        {
            require(
                fingerprint.as_str().len() == 64,
                "origin recovery custody requires content fingerprints",
            )?;
        }
        attribution(&self.actor)
    }
}
