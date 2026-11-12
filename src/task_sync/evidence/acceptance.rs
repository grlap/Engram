//! Explicit use of historical proof, separate from retaining or completing work.

use serde::{Deserialize, Serialize};

use crate::domain::ControlWorkBinding;
use crate::{ObjectId, source::SourceIdSummary};

/// The original identity is a link; an evaluation's governing hash is not.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum HistoricalProof {
    Evaluation {
        evaluation: ObjectId,
        governing_witness: ObjectId,
    },
    NativeCompletion {
        seal: ObjectId,
        governing_witness: Option<ObjectId>,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct AcceptanceUseRequest {
    pub decision: ObjectId,
    pub origin: ObjectId,
    pub proof: HistoricalProof,
    /// Exact original feed cuts, not destination positions or delivery cursors.
    pub origin_cuts: Vec<ObjectId>,
    pub binding: ControlWorkBinding,
    /// Previously prepared B decisions. Importing does not prepare these.
    pub verification_decisions: Vec<ObjectId>,
}

/// Stable reasons are structural and contain no complete source identity.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptanceUseRefusal {
    GoverningSnapshotUnavailable,
    GoverningContentDiffers,
    OriginContextUnavailable,
    OriginContextDiffers,
    IncompleteClosure,
    ReportedCompletion,
    EvaluationShape,
    EvaluationBasisMoved,
    EvaluationPolicy,
    EvaluatorAffiliated,
    SupersededEvaluation,
    BlockingVerdict,
    SourceDiffers,
    CitationUnavailable,
    CitationIneligible,
    LocalGenerationDiffers,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AcceptanceUseSummary {
    pub decision: ObjectId,
    pub proof: HistoricalProof,
    pub recorded_eligible: bool,
    /// Rechecked against actual local authority and current retained facts.
    pub currently_eligible: bool,
    pub refusal: Option<AcceptanceUseRefusal>,
    pub source: SourceIdSummary,
}

/// The retained destination decision is separate from the original verdict.
/// Full identities remain internal; public summaries use `SourceIdSummary`.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct AcceptanceUseDecision {
    pub(crate) project: crate::ProjectId,
    pub(crate) request: AcceptanceUseRequest,
    pub(crate) local_actor: crate::ActorContext,
    pub(crate) local_generation: i64,
    pub(crate) governing_fingerprint: ObjectId,
    pub(crate) named_root: ObjectId,
    pub(crate) source: crate::source::SourceId,
    pub(crate) destination_cut: i64,
    pub(crate) policy: crate::domain::AcceptanceEvaluationPolicy,
    pub(crate) refusal: Option<AcceptanceUseRefusal>,
    pub(crate) decided_at: chrono::DateTime<chrono::Utc>,
}
