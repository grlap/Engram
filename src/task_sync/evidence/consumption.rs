//! Explicit destination use of retained proof, never a new execution or verdict.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::domain::{ActorContext, ControlWorkBinding, VerificationRequirement};
use crate::{ObjectId, ProjectId, SessionId};

/// A prepared local UUID makes an uncertain explicit use exactly replayable.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ConsumptionRequest {
    pub consumption: ObjectId,
    pub decision: ObjectId,
    pub binding: ControlWorkBinding,
}

/// The original producer facts and the separately admitted destination decision
/// keep their own identities. This discriminant cannot turn a note into a check.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ConsumedProof {
    Verification {
        origin: ObjectId,
        original: ObjectId,
        origin_cut: ObjectId,
        requirement: VerificationRequirement,
        criterion: Option<usize>,
        execution: Option<super::verification::ExecutionKey>,
        contributors: Vec<ObjectId>,
    },
    Acceptance {
        origin: ObjectId,
        proof: super::acceptance::HistoricalProof,
    },
}

/// Canonical local consumption at an actual destination cut. No complete source
/// value is printed by Debug or by the bounded public receipt.
#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ImportedEvidenceConsumption {
    pub schema_version: u16,
    pub project_id: ProjectId,
    pub request: ConsumptionRequest,
    pub session_id: SessionId,
    pub actor: ActorContext,
    pub generation: i64,
    pub named_root: ObjectId,
    pub governing_fingerprint: ObjectId,
    pub governing: crate::WorkItem,
    pub policy: crate::domain::AcceptanceEvaluationPolicy,
    pub source: crate::source::SourceId,
    pub destination_cut: i64,
    pub proof: ConsumedProof,
    pub recorded_at: DateTime<Utc>,
}

impl std::fmt::Debug for ImportedEvidenceConsumption {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ImportedEvidenceConsumption")
            .field("decision", &self.request.decision)
            .field("binding", &self.request.binding)
            .field("source", &self.source.summary())
            .finish_non_exhaustive()
    }
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ConsumptionReceipt {
    pub consumption: ObjectId,
    pub decision: ObjectId,
    pub replayed: bool,
}

pub const CONSUMPTION_KIND: &str = "imported_evidence_consumption";
