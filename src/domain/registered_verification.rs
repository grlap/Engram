//! A registered host run is a verification producer, without turn authority.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{
    ControlWorkBinding, GitTreeId, ProjectId, SessionId, SourceRootState, VerificationKind,
};
use crate::ActorContext;
use crate::ObjectId;

/// The input actually selected before execution. A staged tree's base is
/// provenance, whereas an ordinary commit is the input's identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum RegisteredVerificationInput {
    Commit {
        commit: String,
    },
    StagedTree {
        tree: GitTreeId,
        base_commit: String,
    },
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RegisteredVerificationRoot {
    pub workspace_id: String,
    pub generation: i64,
    pub state: SourceRootState,
}

/// The embedding host's assertion of its durable registration. Engram checks
/// the association it owns; this is not independent execution attestation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RegisteredVerificationAssociation {
    pub host_run_id: String,
    pub request_identity: String,
    pub registered_at: DateTime<Utc>,
    pub initiating_session_id: SessionId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owning_session_id: Option<String>,
    pub binding: ControlWorkBinding,
    pub root: RegisteredVerificationRoot,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RegisteredVerificationOutcome {
    Passed,
    Failed,
    Indeterminate,
    NotRun,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RegisteredVerificationRun {
    pub mode: String,
    pub command: String,
    pub started_at: DateTime<Utc>,
    pub finished_at: DateTime<Utc>,
    pub outcome: RegisteredVerificationOutcome,
}

/// Planned, unrun stages remain explicit but mint no verification record.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RegisteredVerificationCheck {
    pub stage: String,
    pub check_kind: VerificationKind,
    pub command: String,
    pub check_fingerprint: ObjectId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub started_at: Option<DateTime<Utc>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub completed_at: Option<DateTime<Utc>>,
    pub outcome: RegisteredVerificationOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub counts: Option<std::collections::BTreeMap<String, u64>>,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RegisteredVerificationInputOutcome {
    Clean,
    DebrisPendingDisposition,
    InputChanged,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RegisteredVerificationPostRun {
    pub outcome: RegisteredVerificationInputOutcome,
    pub clean_before: bool,
    pub clean_after: bool,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub dirty_paths: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub disposition: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RegisteredVerificationRequest {
    pub operation_key: String,
    pub registration: RegisteredVerificationAssociation,
    pub input: RegisteredVerificationInput,
    pub run: RegisteredVerificationRun,
    pub checks: Vec<RegisteredVerificationCheck>,
    pub post_run: RegisteredVerificationPostRun,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RegisteredVerificationRecord {
    pub stage: String,
    pub record_id: ObjectId,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RegisteredVerificationReceipt {
    pub operation: ObjectId,
    pub producer: ObjectId,
    pub verifications: Vec<RegisteredVerificationRecord>,
    pub run_outcome: RegisteredVerificationOutcome,
}

/// Immutable producer facts for all checks of one registered run. This is
/// deliberately not an execution observation and conveys no turn authority.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RegisteredHostRun {
    pub schema_version: u16,
    pub project_id: ProjectId,
    pub session_id: SessionId,
    pub request: RegisteredVerificationRequest,
    pub actor: ActorContext,
    pub recorded_at: DateTime<Utc>,
}

impl RegisteredVerificationInput {
    #[must_use]
    pub fn revision(&self) -> String {
        match self {
            Self::Commit { commit } => format!("git-commit:{commit}"),
            Self::StagedTree { tree, .. } => format!("git-tree:{}", tree.as_str()),
        }
    }
}

/// The facts the verification matcher consumes, shared by grant-backed
/// observations and registered host runs. This view creates no observation.
#[derive(Clone, Debug)]
pub struct VerificationProducerFacts {
    pub project_id: ProjectId,
    pub binding: ControlWorkBinding,
    pub session_id: SessionId,
    pub action_fingerprint: ObjectId,
    pub outcome: super::ExecutionOutcome,
    /// The registered run's aggregate result, when this is a host-run
    /// producer. A passing subcheck never makes a failed run a passed gate.
    pub run_outcome: Option<super::ExecutionOutcome>,
    pub source_basis: Option<super::ExecutionSourceBasis>,
    pub observed_at: Option<DateTime<Utc>>,
    pub actor: ActorContext,
    pub recorded_at: DateTime<Utc>,
}

/// A verification producer supplies facts without requiring turn authority.
pub trait VerificationProducer {
    fn verification_facts(&self) -> VerificationProducerFacts;
}

impl VerificationProducer for VerificationProducerFacts {
    fn verification_facts(&self) -> VerificationProducerFacts {
        self.clone()
    }
}

impl VerificationProducer for super::ExecutionObservation {
    fn verification_facts(&self) -> VerificationProducerFacts {
        VerificationProducerFacts {
            project_id: self.project_id.clone(),
            binding: self.binding.clone(),
            session_id: self.session_id.clone(),
            action_fingerprint: self.action_fingerprint.clone(),
            outcome: self.outcome,
            run_outcome: None,
            source_basis: self.source_basis.clone(),
            observed_at: self.observed_at,
            actor: self.actor.clone(),
            recorded_at: self.recorded_at,
        }
    }
}
