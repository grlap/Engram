//! Explicit caller-selected steps over the existing exchange owners.
//! These requests neither dispatch transport nor grant Git authority.

use serde::{Deserialize, Serialize};
use std::path::PathBuf;

use super::{
    delta::{DeltaCandidatePayload, DeltaCommitSelection, DeltaIntent, PlanningSuccessorRequest},
    ledger::{Binding, EvidenceIntent, OutgoingProgress, OutgoingReconciliation, RetainedCommit},
};
use crate::ObjectId;

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct DeltaRequest {
    pub binding: ObjectId,
    pub intent: DeltaIntent,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct CommitRequest {
    pub delta: DeltaRequest,
    pub selection: DeltaCommitSelection,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ResolutionRequest {
    pub original: CommitRequest,
    pub recipe: OutgoingReconciliation,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceRequest {
    pub binding: ObjectId,
    pub intent: EvidenceIntent,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SuccessorRequest {
    pub binding: ObjectId,
    pub request: PlanningSuccessorRequest,
    pub payload: DeltaCandidatePayload,
}

/// Every writing action is a separate explicit invocation. In particular a
/// publication start is not a push, and an old start never permits redispatch.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case", deny_unknown_fields)]
pub enum StepRequest {
    QualifyVerification {
        request: Box<super::evidence::verification::ApplicabilityRequest>,
    },
    VerificationSummary {
        decision: ObjectId,
    },
    QualifyAcceptance {
        request: Box<super::evidence::acceptance::AcceptanceUseRequest>,
    },
    AcceptanceSummary {
        decision: ObjectId,
    },
    ConsumeVerification {
        request: Box<super::evidence::consumption::ConsumptionRequest>,
    },
    ConsumeAcceptance {
        request: Box<super::evidence::consumption::ConsumptionRequest>,
    },
    CaptureNative {
        binding: ObjectId,
        request: Box<crate::storage::NativeEvidenceCaptureRequest>,
    },
    ExportNative {
        binding: ObjectId,
        project_file: PathBuf,
        branch: String,
        cursor: crate::storage::NativeEvidenceCursor,
        maximum_packs: usize,
    },
    InspectTarget {
        project_file: PathBuf,
        branch: String,
        remote: String,
        reference: String,
    },
    RetainData {
        project_file: PathBuf,
        branch: String,
        operation: uuid::Uuid,
        commit: String,
    },
    Activate {
        project_file: PathBuf,
        binding: Box<Binding>,
    },
    ReadBinding {
        binding: ObjectId,
    },
    PrepareDelta {
        request: Box<DeltaRequest>,
    },
    RecordExportCommit {
        request: Box<CommitRequest>,
    },
    RecordPublication {
        request: Box<CommitRequest>,
        progress: Box<OutgoingProgress>,
    },
    ApplyDelta {
        request: Box<DeltaRequest>,
    },
    RetainIncomingDelta {
        request: Box<DeltaRequest>,
    },
    ApplyEvidence {
        request: Box<EvidenceRequest>,
    },
    ApplySuccessor {
        request: Box<SuccessorRequest>,
    },
    RecordResolution {
        request: Box<ResolutionRequest>,
    },
    RecordRetryProgress {
        request: Box<ResolutionRequest>,
        progress: Box<OutgoingProgress>,
    },
    CompletedDelta {
        request: Box<DeltaRequest>,
    },
    ReadDeltaProgress {
        request: Box<DeltaRequest>,
    },
    CompletedEvidence {
        request: Box<EvidenceRequest>,
    },
    CompletedSuccessor {
        request: Box<SuccessorRequest>,
    },
    AdvanceData {
        project_file: PathBuf,
        branch: String,
        retained: Box<RetainedCommit>,
        expected_head: Option<String>,
    },
}

impl StepRequest {
    #[must_use]
    pub fn is_read_only(&self) -> bool {
        matches!(
            self,
            Self::VerificationSummary { .. }
                | Self::AcceptanceSummary { .. }
                | Self::ReadBinding { .. }
                | Self::CompletedDelta { .. }
                | Self::ReadDeltaProgress { .. }
                | Self::CompletedEvidence { .. }
                | Self::CompletedSuccessor { .. }
        )
    }
}

/// Original canonical ids and bounded outcome meaning, never a copied source
/// identity, task tree, pack body, credential response or invented success.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Outcome {
    pub prepared: ObjectId,
    pub outcome: ObjectId,
    pub transition: String,
}

#[derive(Clone, Debug, Serialize)]
pub struct EventSummary {
    pub event: ObjectId,
    pub sequence: u64,
    pub transition: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(tag = "result", rename_all = "snake_case")]
pub enum StepReceipt {
    Qualified {
        decision: ObjectId,
    },
    Verification {
        summary: Box<super::evidence::verification::ApplicabilitySummary>,
    },
    Acceptance {
        summary: Box<super::evidence::acceptance::AcceptanceUseSummary>,
    },
    Consumed {
        receipt: super::evidence::consumption::ConsumptionReceipt,
    },
    NativeCapture {
        capture: Box<crate::storage::NativeEvidenceCapture>,
    },
    ExportedNative {
        files: Vec<super::data_worktree::PackFile>,
        next: Option<crate::storage::NativeEvidenceCursor>,
    },
    Target {
        checkout: PathBuf,
        git_directory: PathBuf,
        branch: String,
        head: Option<String>,
        configuration_fingerprint: ObjectId,
    },
    Retained {
        commit: Box<RetainedCommit>,
    },
    DeltaRetained {
        candidate: Box<super::delta::RetainedDeltaCandidate>,
    },
    Activated {
        binding: ObjectId,
    },
    Binding {
        binding: ObjectId,
        record: Box<Binding>,
    },
    Recorded {
        event: ObjectId,
        #[serde(skip_serializing_if = "Option::is_none")]
        newly_recorded: Option<bool>,
        start_recorded: bool,
    },
    Progress {
        events: Vec<EventSummary>,
    },
    Outcome {
        outcome: Option<Outcome>,
    },
    Advanced {
        reference: String,
        head: Option<String>,
    },
}
