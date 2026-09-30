//! Transient source recovery selected from the evaluation's deciding snapshot.

use super::WorkRunId;
use crate::ObjectId;
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptanceSourceMismatch {
    UnconfirmedDeclaration,
    UnconfirmedEvaluatedRevision,
    CompletionMeasurementMissing,
    CompletionFingerprintMismatch,
    EvaluationSourceBasisMissing,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AcceptanceSourceRemedy {
    EndTurnReadAndRetry,
    ReadSourceAndEvaluate,
    MeasureSourceAndRetry,
    EvaluateCurrentSource,
}

/// Context only: it grants no authority and predicts no future host report.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AcceptanceSourceRecoveryCause {
    pub mismatch: AcceptanceSourceMismatch,
    pub evaluation: ObjectId,
    pub run_id: WorkRunId,
    pub evaluated_cut: i64,
    pub remedy: AcceptanceSourceRemedy,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub root_binding: Option<ObjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expected_fingerprint: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub presented_fingerprint: Option<String>,
}
