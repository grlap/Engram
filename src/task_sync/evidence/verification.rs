//! Terminal facts are compared independently of recorder and association envelopes.

use serde::{Deserialize, Serialize};

use crate::domain::{
    REMOTE_COMPLETION_SCHEMA_VERSION, RemoteRunCompletion, RemoteTerminalOrigin,
    RemoteVerificationAssociationRecord, RemoteVerificationReason, SCHEMA_VERSION,
};
use crate::{ObjectId, ProjectId};

use super::{EvidenceError, RecordEnvelope, RecordKind};

/// A runner's execution identity, regardless of which store retained its result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionKey {
    pub project: ProjectId,
    pub host: String,
    pub run: String,
}

/// Frozen local association intent. The UUID identifies only this new decision.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ApplicabilityRequest {
    pub original: ObjectId,
    pub origin: ObjectId,
    pub origin_cut: ObjectId,
    pub binding: crate::domain::ControlWorkBinding,
    pub requirement: crate::domain::VerificationRequirement,
    pub criterion: Option<usize>,
    pub decision: ObjectId,
}

/// Bounded presentation of a historical decision and its current known union.
/// Source currency must still be checked when a completion consumes evidence.
#[derive(Clone, Debug, Serialize)]
pub struct ApplicabilitySummary {
    pub decision: ObjectId,
    pub original: ObjectId,
    pub recorded_eligible: bool,
    pub terminal_conflicted_since: bool,
    pub source: crate::source::SourceIdSummary,
    pub equivalent_contributors_at_decision: usize,
}

/// Original assurance stays separate from this local applicability decision.
/// Its stored shape is unchanged when carried as an inert typed original.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VerificationApplicabilityDecision {
    pub(crate) project_id: ProjectId,
    pub(crate) original: ObjectId,
    pub(crate) origin: ObjectId,
    pub(crate) origin_cut: ObjectId,
    pub(crate) binding: crate::domain::ControlWorkBinding,
    pub(crate) requirement: crate::domain::VerificationRequirement,
    pub(crate) criterion: Option<usize>,
    pub(crate) governing_fingerprint: ObjectId,
    pub(crate) named_root: ObjectId,
    pub(crate) source: crate::source::SourceId,
    pub(crate) run_cut: i64,
    pub(crate) known_import: Option<ObjectId>,
    pub(crate) execution: Option<ExecutionKey>,
    pub(crate) contributors: Vec<ObjectId>,
    pub(crate) refusal: Option<ApplicabilityRefusal>,
    pub(crate) origin_actor: crate::ActorContext,
    pub(crate) local_actor: crate::ActorContext,
    pub(crate) decided_at: chrono::DateTime<chrono::Utc>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ApplicabilityRefusal {
    SourceDiffers,
    TerminalConflict,
    Mismatch(crate::domain::VerificationEvidenceMismatch),
}

impl ExecutionKey {
    pub fn new(project: ProjectId, completion: &RemoteRunCompletion) -> Self {
        Self {
            project,
            host: completion.origin.host_id.clone(),
            run: completion.run_id.clone(),
        }
    }
}

/// Internal comparison input. Debug intentionally omits the complete source input.
#[derive(Clone, Eq, PartialEq)]
pub struct TerminalFact {
    pub record: ObjectId,
    pub key: ExecutionKey,
    completion: RemoteRunCompletion,
}

impl std::fmt::Debug for TerminalFact {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TerminalFact")
            .field("record", &self.record)
            .field("key", &self.key)
            .finish_non_exhaustive()
    }
}

fn supported(completion: &RemoteRunCompletion) -> bool {
    completion.schema_version == REMOTE_COMPLETION_SCHEMA_VERSION
        && completion.is_authoritative_terminal()
        && completion.validate().is_ok()
        && match &completion.input {
            crate::domain::RemoteRunInput::SourceId(input) => {
                [&input.source_id_before, &input.source_id_after]
                    .into_iter()
                    .flatten()
                    .all(|source| source.validate().is_ok())
            }
            crate::domain::RemoteRunInput::Legacy(_) => true,
        }
}

impl TerminalFact {
    /// Read either terminal-bearing record without following an association's
    /// origin in place of its own completion. Unsupported/provisional bodies
    /// stay retained but contribute no authoritative outcome.
    ///
    /// # Errors
    /// Returns a structural decode error, without the supplied body.
    pub fn from_record(record: &RecordEnvelope) -> Result<Option<Self>, EvidenceError> {
        Self::from_bytes(
            record.object_id().clone(),
            record.kind(),
            record.canonical_bytes(),
        )
    }

    /// Decode an already retained native or imported original.
    ///
    /// # Errors
    /// Returns a structural decode error for a terminal-bearing kind.
    pub fn from_bytes(
        record: ObjectId,
        kind: RecordKind,
        bytes: &[u8],
    ) -> Result<Option<Self>, EvidenceError> {
        let (project, completion) = match kind {
            RecordKind::RemoteTerminalOrigin => {
                let origin: RemoteTerminalOrigin = serde_json::from_slice(bytes)?;
                if origin.schema_version != SCHEMA_VERSION
                    || origin.actor.session_id.as_ref() != Some(&origin.session_id)
                {
                    return Ok(None);
                }
                (origin.project_id, origin.completion)
            }
            RecordKind::RemoteVerificationAssociation => {
                let association: RemoteVerificationAssociationRecord =
                    serde_json::from_slice(bytes)?;
                if association.schema_version != SCHEMA_VERSION
                    || association.actor.session_id.as_ref() != Some(&association.session_id)
                    || association.request.validate().is_err()
                    || association.qualification.credited != association.qualification.reason.is_none()
                    || association.qualification.reason == Some(RemoteVerificationReason::ProducerUnsupported)
                    // Historical provisional deliveries did not settle an origin.
                    || association.origin.is_none()
                {
                    return Ok(None);
                }
                (association.project_id, association.request.completion)
            }
            _ => return Ok(None),
        };
        if project.0.trim().is_empty() || !supported(&completion) {
            return Ok(None);
        }
        Ok(Some(Self {
            record,
            key: ExecutionKey::new(project, &completion),
            completion,
        }))
    }

    pub fn same_terminal(&self, other: &Self) -> bool {
        self.key == other.key && self.completion == other.completion
    }

    /// Fixed field names only, never values from either source input.
    pub fn differing_fields(&self, other: &Self) -> Vec<&'static str> {
        let mut fields = Vec::new();
        let a = &self.completion;
        let b = &other.completion;
        for (different, field) in [
            (a.schema_version != b.schema_version, "schema_version"),
            (a.origin != b.origin, "origin"),
            (a.run_id != b.run_id, "run_id"),
            (a.mode != b.mode, "mode"),
            (a.result != b.result, "result"),
            (a.input != b.input, "input"),
            (a.finished_at != b.finished_at, "finished_at"),
        ] {
            if different {
                fields.push(field);
            }
        }
        fields
    }
}
