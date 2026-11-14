//! Typed object links, distinct from fingerprints and inert execution scalars.

use super::{EvidenceError, RecordEnvelope, RecordKind};
use crate::{ObjectId, domain};

mod batch;
mod consumption;
mod context;
mod observations;
mod planning;
mod records;
mod scope;
mod witnesses;

pub use batch::plan_batch;
pub use scope::check_project;

#[cfg(test)]
mod tests;

/// A required original. Empty kinds denote a polymorphic evidence citation;
/// resolution must still require a member of the closed record inventory.
#[derive(Clone, Debug, Eq, Ord, PartialEq, PartialOrd)]
pub struct RequiredObject {
    pub object_id: ObjectId,
    pub kinds: Vec<RecordKind>,
}

#[derive(Default)]
struct Links(Vec<RequiredObject>);

impl Links {
    fn add(&mut self, id: &ObjectId, kind: Option<RecordKind>) {
        self.0.push(RequiredObject {
            object_id: id.clone(),
            kinds: kind.into_iter().collect(),
        });
    }
    fn choices(&mut self, id: &ObjectId, kinds: &[RecordKind]) {
        self.0.push(RequiredObject {
            object_id: id.clone(),
            kinds: kinds.to_vec(),
        });
    }
    fn optional(&mut self, id: Option<&ObjectId>, kind: Option<RecordKind>) {
        if let Some(id) = id {
            self.add(id, kind);
        }
    }
    fn many<'a>(&mut self, ids: impl IntoIterator<Item = &'a ObjectId>, kind: Option<RecordKind>) {
        for id in ids {
            self.add(id, kind);
        }
    }
    fn decision(&mut self, value: Option<&domain::RegisteredInputDecision>) {
        if let Some(value) = value {
            self.add(&value.verification, Some(RecordKind::VerificationEvidence));
            self.add(
                &value.named_root_binding,
                Some(RecordKind::NamedRootBinding),
            );
        }
    }
    fn basis(&mut self, value: &domain::WorkObservationBasis) {
        match value {
            domain::WorkObservationBasis::NativeEvent { event } => {
                self.add(event, Some(RecordKind::WorkEvent));
            }
            domain::WorkObservationBasis::RestoredRecord { record } => {
                self.add(record, Some(RecordKind::WorkRestoredRecord));
            }
        }
    }
    fn work(&mut self, value: &domain::WorkItem) {
        self.optional(
            value.source_snapshot_id.as_ref(),
            Some(RecordKind::WorkSourceSnapshot),
        );
    }
    fn member(&mut self, value: &domain::RootExecutionMember) {
        match value {
            domain::RootExecutionMember::ChildSeal(id) => {
                self.add(id, Some(RecordKind::CompletionSeal));
            }
            domain::RootExecutionMember::Contribution(value) => self.add(&value.object, None),
            domain::RootExecutionMember::Run(_)
            | domain::RootExecutionMember::ChildWaiver(_)
            | domain::RootExecutionMember::Contributor(_)
            | domain::RootExecutionMember::Waiver(_) => {}
        }
    }
}

fn decode<T: serde::de::DeserializeOwned>(record: &RecordEnvelope) -> Result<T, EvidenceError> {
    Ok(serde_json::from_slice(record.canonical_bytes())?)
}

/// Derive dependencies from stored typed fields, never from caller-supplied
/// link lists or a recursive search for UUID-shaped strings. No bytes change.
///
/// # Errors
/// Refuses a malformed typed body or a reference to a context outside the
/// closed interchange inventory. Such a refusal never implies complete proof.
pub fn dependencies(record: &RecordEnvelope) -> Result<Vec<RequiredObject>, EvidenceError> {
    use RecordKind as K;
    // Original IDs use ObjectId's stored 32/64-character contract. Origins
    // are newly minted namespace records and retain their UUID requirement.
    if record.origin().as_str().len() != 32 {
        return Err(EvidenceError::InvalidPack(
            "evidence origins require minted record identities",
        ));
    }
    let mut links = Links::default();
    links.add(record.origin(), Some(K::ProducingOrigin));
    match record.kind() {
        K::VerificationApplicability | K::AcceptanceUse | K::ImportedConsumption => {
            consumption::collect(&mut links, record)?;
        }
        K::VerificationEvidence
        | K::ExecutionObservation
        | K::UnadmittedExecutionObservation
        | K::RemoteVerificationAssociation => observations::collect(&mut links, record)?,
        K::ProducingOrigin
        | K::AcceptanceEvaluation
        | K::MemoryVersion
        | K::MemoryAssertionEvent
        | K::ControlPolicy => context::collect(&mut links, record)?,
        K::WorkRootDelta
        | K::WorkItemRevision
        | K::WorkHandoffOffer
        | K::WorkObservation
        | K::WorkSourceProposal
        | K::WorkEvent => planning::collect(&mut links, record)?,
        K::WorkEvidence
        | K::WorkCheckpoint
        | K::WorkObligation
        | K::WorkObligationResolution
        | K::WorkRestoredEvidence
        | K::WorkRestoredRecord => records::collect(&mut links, record)?,
        K::OriginFeedSegment | K::OriginFeedCut | K::GoverningSnapshotWitness => {
            witnesses::collect(&mut links, record)?;
        }
        K::CompletionSeal => seal(&mut links, &decode(record)?),
        K::RegisteredHostRun
        | K::RemoteTerminalOrigin
        | K::EnvironmentEvidence
        | K::NamedRootBinding
        | K::WorkBlocker
        | K::ObligationRuleSet
        | K::WorkSourceSnapshot => {}
    }
    links.0.sort();
    links.0.dedup();
    Ok(links.0)
}

fn seal(links: &mut Links, value: &domain::CompletionSeal) {
    use RecordKind as K;
    // Despite its legacy name, accepted_work_revision_hash is an OBJECT id.
    links.add(
        &value.accepted_work_revision_hash,
        Some(K::WorkItemRevision),
    );
    links.add(&value.root_execution.head, Some(K::WorkRootDelta));
    links.optional(value.checkpoint.as_ref(), Some(K::WorkCheckpoint));
    links.many(&value.evidence, None);
    links.optional(
        value.acceptance_evaluation.as_ref(),
        Some(K::AcceptanceEvaluation),
    );
    for result in &value.acceptance {
        links.many(&result.evidence, None);
    }
    for obligation in &value.obligations {
        links.add(&obligation.definition, Some(K::WorkObligation));
        links.add(&obligation.resolution, Some(K::WorkObligationResolution));
    }
    links.many(
        &value.foreign_workspace_changes,
        Some(K::UnadmittedExecutionObservation),
    );
    links.many(&value.environment, Some(K::EnvironmentEvidence));
    links.many(&value.required_child_seals, Some(K::CompletionSeal));
    links.many(
        &value.restored_child_completions,
        Some(K::WorkRestoredRecord),
    );
    links.many(&value.drain.reconciled_action_outcomes, None);
    for resolution in &value.required_child_resolutions {
        let domain::RequiredChildResolution::ResolvedBySuccessor {
            supersession,
            successor_seal,
            ..
        } = resolution;
        links.add(supersession, Some(K::WorkEvent));
        links.add(successor_seal, Some(K::CompletionSeal));
    }
}

fn work_event(links: &mut Links, value: &domain::WorkEvent) {
    use RecordKind as K;
    use domain::WorkTransition as T;
    links.work(&value.work);
    if let Some(run) = &value.run {
        links.optional(run.last_checkpoint.as_ref(), Some(K::WorkCheckpoint));
        links.optional(run.completion_seal.as_ref(), Some(K::CompletionSeal));
    }
    if let Some(root) = &value.root_execution {
        links.add(&root.head, Some(K::WorkRootDelta));
    }
    if let Some(offer) = &value.handoff_offer {
        links.add(&offer.checkpoint, Some(K::WorkCheckpoint));
    }
    match &value.transition {
        T::Checkpointed { checkpoint } => links.add(checkpoint, Some(K::WorkCheckpoint)),
        T::HandoffOffered {
            checkpoint, offer, ..
        }
        | T::HandedOff {
            checkpoint, offer, ..
        } => {
            links.add(checkpoint, Some(K::WorkCheckpoint));
            links.add(offer, Some(K::WorkHandoffOffer));
        }
        T::HandoffExpired { offer, .. } | T::HandoffCancelled { offer, .. } => {
            links.add(offer, Some(K::WorkHandoffOffer));
        }
        T::EvidenceAdded { evidence } | T::TypedEvidenceAdded { evidence, .. } => {
            links.add(evidence, None);
        }
        T::MemoryCaptured { version, assertion } => {
            links.add(version, Some(K::MemoryVersion));
            links.add(assertion, Some(K::MemoryAssertionEvent));
        }
        T::Completed { seal } => links.add(seal, Some(K::CompletionSeal)),
        T::CompletionReported { record } => links.add(record, Some(K::WorkRestoredRecord)),
        // Custody belongs to the originating store. Preserve these references
        // in the original bytes without following them as evidence dependencies.
        T::SupersededByImport { .. }
        | T::ReconciliationHoldAbandoned { .. }
        | T::Created { .. }
        | T::Decomposed { .. }
        | T::Revised { .. }
        | T::PrerequisiteAdded { .. }
        | T::PrerequisiteRemoved { .. }
        | T::Blocked { .. }
        | T::Unblocked { .. }
        | T::Claimed { .. }
        | T::ClaimRenewed { .. }
        | T::Released { .. }
        | T::Disposed { .. }
        | T::RequiredChildWaived { .. }
        | T::Reopened { .. } => {}
    }
}
