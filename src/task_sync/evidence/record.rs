use std::fmt;

use serde::{Deserialize, Serialize, de::DeserializeOwned};

use crate::{ObjectId, domain, graph_snapshot};

use super::{EvidenceError, limits};

macro_rules! record_kinds {
    ($($variant:ident => $name:literal : $body:ty),+ $(,)?) => {
        /// Closed stored-record inventory. Decoding never invokes a live writer.
        #[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
        pub enum RecordKind {
            $(#[serde(rename = $name)] $variant,)+
        }

        impl RecordKind {
            /// Resolve a stored kind without guessing another representation.
            ///
            /// # Errors
            /// Refuses kinds outside the closed evidence inventory.
            pub fn parse(name: &str) -> Result<Self, EvidenceError> {
                match name {
                    $($name => Ok(Self::$variant),)+
                    _ => Err(EvidenceError::UnknownKind(safe_kind_label(name))),
                }
            }

            pub fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $name,)+ }
            }

            fn validate_body(self, bytes: &[u8]) -> Result<(), EvidenceError> {
                match self { $(Self::$variant => decode::<$body>(bytes),)+ }
            }
        }
    };
}

record_kinds! {
    ProducingOrigin => "task_sync_producing_origin": crate::task_sync::origin::Enrollment,
    RegisteredHostRun => "registered_host_run": domain::RegisteredHostRun,
    VerificationEvidence => "verification_evidence": domain::VerificationEvidence,
    RemoteTerminalOrigin => "remote_terminal_origin": domain::RemoteTerminalOrigin,
    RemoteVerificationAssociation => "remote_verification_association": domain::RemoteVerificationAssociationRecord,
    ExecutionObservation => "execution_observation": domain::ExecutionObservation,
    UnadmittedExecutionObservation => "unadmitted_execution_observation": domain::UnadmittedExecutionObservation,
    EnvironmentEvidence => "environment_evidence": domain::EnvironmentEvidence,
    WorkEvidence => "work_evidence": domain::WorkEvidence,
    AcceptanceEvaluation => "acceptance_evaluation": domain::AcceptanceEvaluation,
    WorkObligation => "work_obligation": domain::WorkObligation,
    WorkObligationResolution => "work_obligation_resolution": domain::WorkObligationResolutionEvent,
    CompletionSeal => "completion_seal": domain::CompletionSeal,
    WorkCheckpoint => "work_checkpoint": domain::WorkCheckpoint,
    WorkRootDelta => "work_root_delta": domain::RootExecutionDelta,
    NamedRootBinding => "named_root_binding": domain::NamedRootBindingEvent,
    WorkObservation => "work_observation": domain::WorkObservation,
    WorkRestoredEvidence => "work_restored_evidence": graph_snapshot::RestoredWorkEvidence,
    WorkRestoredRecord => "work_restored_record": graph_snapshot::RestoredRecord,
    WorkEvent => "work_event": domain::WorkEvent,
    WorkItemRevision => "work_item_revision": domain::WorkItem,
    WorkHandoffOffer => "work_handoff_offer": domain::WorkHandoffOffer,
    WorkBlocker => "work_blocker": domain::WorkBlocker,
    ControlPolicy => "control_policy": domain::ControlPolicy,
    ObligationRuleSet => "obligation_rule_set": domain::ObligationRuleSet,
    MemoryVersion => "memory_version": domain::MemoryVersion,
    MemoryAssertionEvent => "memory_assertion_event": domain::MemoryAssertionEvent,
    WorkSourceSnapshot => "work_source_snapshot": domain::WorkSourceSnapshot,
    WorkSourceProposal => "work_source_proposal": domain::WorkSourceProposal,
    OriginFeedSegment => "evidence_origin_feed_segment": super::witnesses::OriginFeedSegment,
    OriginFeedCut => "evidence_origin_feed_cut": super::witnesses::OriginFeedCut,
    GoverningSnapshotWitness => "evidence_governing_snapshot": super::witnesses::GoverningSnapshotWitness,
    VerificationApplicability => "task_sync_verification_applicability": super::verification::VerificationApplicabilityDecision,
    AcceptanceUse => "task_sync_acceptance_use": super::acceptance::AcceptanceUseDecision,
    ImportedConsumption => "imported_evidence_consumption": super::consumption::ImportedEvidenceConsumption,
}

impl<'de> Deserialize<'de> for RecordKind {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let name = String::deserialize(deserializer)?;
        Self::parse(&name).map_err(serde::de::Error::custom)
    }
}

fn safe_kind_label(name: &str) -> String {
    if !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte == b'_')
    {
        name.to_owned()
    } else {
        "<invalid kind label>".into()
    }
}

fn decode<T: DeserializeOwned>(bytes: &[u8]) -> Result<(), EvidenceError> {
    serde_json::from_slice::<T>(bytes)?;
    Ok(())
}

/// Original record bytes, validated independently from their typed view.
/// The JSON text is carried as a string so optional fields are never synthesized
/// by serializing a decoded Rust record. Origin is an assertion, not execution host.
#[derive(Clone, Eq, PartialEq, Serialize)]
pub struct RecordEnvelope {
    // Keep this order canonical for the bounded streaming pack writer.
    canonical_body: String,
    object_id: ObjectId,
    object_kind: RecordKind,
    origin: ObjectId,
}

impl fmt::Debug for RecordEnvelope {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RecordEnvelope")
            .field("object_id", &self.object_id)
            .field("object_kind", &self.object_kind)
            .field("origin", &self.origin)
            .field("canonical_bytes", &self.canonical_body.len())
            .finish()
    }
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WireRecord {
    pub canonical_body: String,
    pub object_id: ObjectId,
    pub object_kind: String,
    pub origin: ObjectId,
}

impl TryFrom<WireRecord> for RecordEnvelope {
    type Error = EvidenceError;

    fn try_from(wire: WireRecord) -> Result<Self, Self::Error> {
        Self::from_canonical(
            wire.origin,
            wire.object_id,
            &wire.object_kind,
            wire.canonical_body.into_bytes(),
        )
    }
}

impl RecordEnvelope {
    /// Validate stored shape while retaining the original canonical bytes.
    ///
    /// # Errors
    /// Refuses oversized, noncanonical, unsupported or malformed records.
    pub fn from_canonical(
        origin: ObjectId,
        object_id: ObjectId,
        object_kind: &str,
        bytes: Vec<u8>,
    ) -> Result<Self, EvidenceError> {
        // ObjectId already validates stored 32- and 64-character identities.
        // Historical originals travel unchanged; only new origins require UUIDs.
        if origin.as_str().len() != 32 {
            return Err(EvidenceError::InvalidPack(
                "origin id must be a minted UUID",
            ));
        }
        limits::enforce(
            limits::LimitKind::RecordTooLarge,
            Some(&object_id),
            limits::Dimension::Bytes,
            bytes.len(),
            limits::MAX_RECORD_BYTES,
        )?;
        let kind = RecordKind::parse(object_kind)?;
        let value: serde_json::Value = serde_json::from_slice(&bytes)?;
        let canonical = serde_json_canonicalizer::to_vec(&value)?;
        if canonical != bytes {
            return Err(EvidenceError::NonCanonical(object_id));
        }
        drop(canonical);
        drop(value);
        kind.validate_body(&bytes)?;
        // Parsing JSON has already established UTF-8 validity.
        let canonical_body = String::from_utf8(bytes)
            .map_err(|_| EvidenceError::InvalidPack("record body is not UTF-8"))?;
        Ok(Self {
            canonical_body,
            object_id,
            object_kind: kind,
            origin,
        })
    }

    pub fn object_id(&self) -> &ObjectId {
        &self.object_id
    }

    pub fn kind(&self) -> RecordKind {
        self.object_kind
    }

    pub fn origin(&self) -> &ObjectId {
        &self.origin
    }

    pub fn canonical_bytes(&self) -> &[u8] {
        self.canonical_body.as_bytes()
    }

    /// Original identity collision comparison deliberately excludes recorder metadata.
    pub fn same_record(&self, other: &Self) -> bool {
        self.object_id == other.object_id
            && self.object_kind == other.object_kind
            && self.canonical_body == other.canonical_body
    }
}
