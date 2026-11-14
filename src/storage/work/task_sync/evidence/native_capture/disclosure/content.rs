//! Typed text selection: source identities and record links are not prose.

use super::{ExportProfile, RecordEnvelope, RecordKind, Result, invalid, notes};
use crate::{ActorContext, domain as d, graph_snapshot as g};
use serde::de::DeserializeOwned;

fn decode<T: DeserializeOwned>(record: &RecordEnvelope) -> Result<T> {
    Ok(serde_json::from_slice(record.canonical_bytes())?)
}

struct Disclosure<'a>(&'a ExportProfile);

impl Disclosure<'_> {
    fn text(value: &str) -> Result<()> {
        if notes::prose_exclusion(value).is_some() {
            return Err(invalid("native evidence note disclosure refused"));
        }
        Ok(())
    }

    fn note(&self, value: &str) -> Result<()> {
        if !self.0.include_notes {
            return Err(invalid("native evidence note disclosure refused"));
        }
        Disclosure::text(value)
    }

    fn author(value: &str) -> Result<()> {
        if notes::author_exclusion(value).is_some() {
            return Err(invalid("native evidence actor disclosure refused"));
        }
        Ok(())
    }

    fn actor(actor: &ActorContext) -> Result<()> {
        Disclosure::author(&actor.actor_id)?;
        Disclosure::author(&actor.actor_kind)?;
        Disclosure::text(&actor.reason)?;
        for value in [&actor.source_tool, &actor.source_skill]
            .into_iter()
            .flatten()
        {
            Disclosure::text(value)?;
        }
        for link in &actor.provenance_chain {
            // Exact producer-owned markers carry attribution metadata, not paths.
            if !matches!(
                link.source.as_str(),
                d::POST_COMPLETION_EVIDENCE_PROVENANCE_SOURCE
                    | d::DEFAULTED_OS_USER_ACTOR_SOURCE
                    | d::DEFAULTED_PROCESS_ACTOR_SOURCE
                    | d::DEFAULTED_PROCESS_SESSION_SOURCE
                    | "actor_context:normalized"
            ) {
                Disclosure::text(&link.source)?;
            }
            if let Some(reference) = &link.reference {
                Disclosure::text(reference)?;
            }
        }
        Ok(())
    }

    fn refs(refs: &[String]) -> Result<()> {
        if !refs.is_empty() {
            return Err(invalid("native evidence note disclosure refused"));
        }
        Ok(())
    }

    fn gate(&self, gate: Option<&d::GateEvidenceRecord>) -> Result<()> {
        if let Some(gate) = gate {
            self.note(&gate.name)?;
            for failed in &gate.failed {
                self.note(failed)?;
            }
        }
        Ok(())
    }

    fn work(item: &d::WorkItem) -> Result<()> {
        Disclosure::planning(
            &item.title,
            &item.outcome,
            &item.acceptance,
            &item.labels,
            item.external_ref.as_deref(),
            item.assigned_to.as_deref(),
        )?;
        Disclosure::actor(&item.created_by)
    }

    fn planning(
        title: &str,
        outcome: &str,
        acceptance: &[String],
        labels: &[String],
        external: Option<&str>,
        assigned: Option<&str>,
    ) -> Result<()> {
        // Planning text is shared even with notes off, as in folder export.
        Disclosure::text(title)?;
        Disclosure::text(outcome)?;
        for value in acceptance.iter().chain(labels) {
            Disclosure::text(value)?;
        }
        if let Some(value) = external {
            Disclosure::text(value)?;
        }
        if let Some(value) = assigned {
            Disclosure::author(value)?;
        }
        Ok(())
    }

    fn waiver(&self, reason: &str, author: &str) -> Result<()> {
        self.note(reason)?;
        Disclosure::author(author)
    }

    fn json(&self, value: &serde_json::Value) -> Result<()> {
        match value {
            serde_json::Value::String(text) => self.note(text)?,
            serde_json::Value::Array(values) => {
                for value in values {
                    self.json(value)?;
                }
            }
            serde_json::Value::Object(values) => {
                for (key, value) in values {
                    self.note(key)?;
                    self.json(value)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn restored(&self, value: &g::RestoredRecord) -> Result<()> {
        let item = &value.item;
        Disclosure::planning(
            &item.title,
            &item.outcome,
            &item.acceptance,
            &item.labels,
            item.external_ref.as_deref(),
            item.assigned_to.as_deref(),
        )?;
        if let Some(reason) = &item.disposal_reason {
            self.note(reason)?;
        }
        for blocker in &value.relations.blockers {
            self.note(&blocker.detail)?;
            Disclosure::actor(&blocker.created_by)?;
        }
        for note in &value.history.notes {
            self.note(&note.summary)?;
            Disclosure::refs(&note.refs)?;
            Disclosure::actor(&note.actor)?;
            if let Some(gate) = &note.gate {
                self.note(&gate.name)?;
                for failed in &gate.failed {
                    self.note(failed)?;
                }
                if gate.evidence_ref.is_some() {
                    return Err(invalid("native evidence note disclosure refused"));
                }
            }
        }
        for event in &value.history.events {
            Disclosure::text(&event.kind)?;
            Disclosure::actor(&event.actor)?;
            if let Some(reason) = &event.reason {
                self.note(reason)?;
            }
        }
        for notice in &value.history.source_notices {
            Disclosure::actor(&notice.actor)?;
        }
        if let Some(completion) = &value.history.completion {
            self.note(&completion.summary)?;
            Disclosure::actor(&completion.actor)?;
        }
        Ok(())
    }
}

pub(super) fn validate(profile: &ExportProfile, record: &RecordEnvelope) -> Result<()> {
    use RecordKind as K;
    let check = Disclosure(profile);
    match record.kind() {
        K::ProducingOrigin => {
            Disclosure::actor(&decode::<crate::task_sync::origin::Enrollment>(record)?.actor)?;
        }
        K::VerificationEvidence => {
            let value: d::VerificationEvidence = decode(record)?;
            check.note(&value.summary)?;
            Disclosure::refs(&value.refs)?;
            Disclosure::actor(&value.actor)?;
        }
        K::RegisteredHostRun => {
            let value: d::RegisteredHostRun = decode(record)?;
            Disclosure::actor(&value.actor)?;
            check.note(&value.request.run.command)?;
            Disclosure::text(&value.request.run.mode)?;
            for stage in &value.request.checks {
                check.note(&stage.command)?;
                Disclosure::text(&stage.stage)?;
                Disclosure::refs(&stage.refs)?;
                if let Some(summary) = &stage.summary {
                    check.note(summary)?;
                }
                if let Some(counts) = &stage.counts {
                    for key in counts.keys() {
                        Disclosure::text(key)?;
                    }
                }
            }
            if let d::RegisteredVerificationPostRun::Legacy(post) = &value.request.post_run {
                Disclosure::refs(&post.dirty_paths)?;
                if let Some(reason) = &post.disposition {
                    check.note(reason)?;
                }
            }
        }
        K::RemoteTerminalOrigin => {
            let value: d::RemoteTerminalOrigin = decode(record)?;
            Disclosure::actor(&value.actor)?;
            check.note(&value.completion.result.reason)?;
        }
        K::RemoteVerificationAssociation => {
            let value: d::RemoteVerificationAssociationRecord = decode(record)?;
            Disclosure::actor(&value.actor)?;
            check.note(&value.request.completion.result.reason)?;
            if let Some(detail) = &value.qualification.detail {
                check.note(detail)?;
            }
            if let d::RemoteVerificationUse::AuthorizedTestRef { ref_name, .. } =
                &value.request.association.intended_use
            {
                Disclosure::text(ref_name)?;
            }
        }
        K::ExecutionObservation => {
            Disclosure::actor(&decode::<d::ExecutionObservation>(record)?.actor)?;
        }
        K::UnadmittedExecutionObservation => {
            let value: d::UnadmittedExecutionObservation = decode(record)?;
            Disclosure::actor(&value.observer)?;
            if let d::ObservationCausality::HostAssertion {
                claimed_actor,
                basis,
            } = &value.causality
            {
                Disclosure::actor(claimed_actor)?;
                check.note(basis)?;
            }
            for observed in value.occurrence.checks() {
                if observed.check.host_evidence_ref.is_some() {
                    return Err(invalid("native evidence note disclosure refused"));
                }
            }
        }
        K::EnvironmentEvidence => {
            let value: d::EnvironmentEvidence = decode(record)?;
            Disclosure::actor(&value.actor)?;
            if let Some(components) = &value.components {
                Disclosure::text(&components.toolchain)?;
                if let Some(sandbox) = &components.sandbox {
                    Disclosure::text(sandbox)?;
                }
            }
        }
        K::WorkEvidence => {
            let value: d::WorkEvidence = decode(record)?;
            Disclosure::actor(&value.actor)?;
            check.gate(value.gate.as_ref())?;
        }
        K::AcceptanceEvaluation => {
            let value: d::AcceptanceEvaluation = decode(record)?;
            Disclosure::actor(&value.evaluator)?;
            for criterion in &value.criteria {
                Disclosure::text(criterion)?;
            }
            for verdict in &value.verdicts {
                Disclosure::text(&verdict.criterion)?;
                check.note(&verdict.rationale)?;
            }
            if let Some(model) = &value.evaluator_model {
                Disclosure::text(&model.provider)?;
                Disclosure::text(&model.model)?;
                if let Some(version) = &model.version {
                    Disclosure::text(version)?;
                }
            }
        }
        K::WorkObligationResolution => {
            let value: d::WorkObligationResolutionEvent = decode(record)?;
            Disclosure::actor(&value.actor)?;
            if let d::WorkObligationResolution::Waived { reason, waived_by } = &value.resolution {
                check.waiver(reason, waived_by)?;
            }
        }
        K::CompletionSeal => {
            let value: d::CompletionSeal = decode(record)?;
            Disclosure::actor(&value.actor)?;
            for acceptance in &value.acceptance {
                Disclosure::text(&acceptance.criterion)?;
                check.note(&acceptance.note)?;
            }
            for waiver in &value.required_child_waivers {
                check.waiver(&waiver.reason, &waiver.waived_by)?;
            }
            if let Some(landing) = &value.landing {
                Disclosure::text(&landing.remote)?;
                Disclosure::text(&landing.branch)?;
            }
        }
        K::WorkCheckpoint => Disclosure::actor(&decode::<d::WorkCheckpoint>(record)?.actor)?,
        K::WorkRootDelta => {
            let value: d::RootExecutionDelta = decode(record)?;
            for member in value.added.iter().chain(&value.removed) {
                match member {
                    d::RootExecutionMember::Waiver(waiver) => {
                        check.waiver(&waiver.reason, &waiver.waived_by)?;
                    }
                    d::RootExecutionMember::ChildWaiver(waiver) => {
                        check.waiver(&waiver.reason, &waiver.waived_by)?;
                    }
                    d::RootExecutionMember::Run(_)
                    | d::RootExecutionMember::ChildSeal(_)
                    | d::RootExecutionMember::Contributor(_)
                    | d::RootExecutionMember::Contribution(_) => {}
                }
            }
        }
        K::NamedRootBinding => {
            Disclosure::actor(&decode::<d::NamedRootBindingEvent>(record)?.actor)?;
        }
        K::WorkObservation => Disclosure::actor(&decode::<d::WorkObservation>(record)?.actor)?,
        K::WorkRestoredEvidence => {
            let value: g::RestoredWorkEvidence = decode(record)?;
            Disclosure::actor(&value.actor)?;
            check.gate(value.gate.as_ref())?;
        }
        K::WorkRestoredRecord => check.restored(&decode(record)?)?,
        K::WorkEvent => {
            let value: d::WorkEvent = decode(record)?;
            Disclosure::actor(&value.actor)?;
            Disclosure::work(&value.work)?;
            if let Some(blocker) = &value.blocker {
                check.note(&blocker.detail)?;
                Disclosure::actor(&blocker.created_by)?;
            }
            match &value.transition {
                d::WorkTransition::Released { reason, .. }
                | d::WorkTransition::HandoffCancelled { reason, .. }
                | d::WorkTransition::Disposed { reason, .. }
                | d::WorkTransition::RequiredChildWaived { reason, .. }
                | d::WorkTransition::Reopened { reason, .. }
                | d::WorkTransition::SupersededByImport { reason, .. }
                | d::WorkTransition::ReconciliationHoldAbandoned { reason, .. } => {
                    check.note(reason)?;
                }
                d::WorkTransition::Created { .. }
                | d::WorkTransition::Decomposed { .. }
                | d::WorkTransition::Revised { .. }
                | d::WorkTransition::PrerequisiteAdded { .. }
                | d::WorkTransition::PrerequisiteRemoved { .. }
                | d::WorkTransition::Blocked { .. }
                | d::WorkTransition::Unblocked { .. }
                | d::WorkTransition::Claimed { .. }
                | d::WorkTransition::ClaimRenewed { .. }
                | d::WorkTransition::Checkpointed { .. }
                | d::WorkTransition::HandoffOffered { .. }
                | d::WorkTransition::HandoffExpired { .. }
                | d::WorkTransition::HandedOff { .. }
                | d::WorkTransition::EvidenceAdded { .. }
                | d::WorkTransition::MemoryCaptured { .. }
                | d::WorkTransition::TypedEvidenceAdded { .. }
                | d::WorkTransition::Completed { .. }
                | d::WorkTransition::CompletionReported { .. } => {}
            }
        }
        K::WorkItemRevision => Disclosure::work(&decode(record)?)?,
        K::WorkBlocker => {
            let value: d::WorkBlocker = decode(record)?;
            check.note(&value.detail)?;
            Disclosure::actor(&value.created_by)?;
        }
        K::MemoryVersion => {
            let value: d::MemoryVersion = decode(record)?;
            Disclosure::actor(&value.actor)?;
            Disclosure::text(&value.classification_reason)?;
            if let Some(reason) = &value.delivery_override_reason {
                check.note(reason)?;
            }
            for tag in &value.tags {
                Disclosure::text(tag)?;
            }
            if let Some(value) = &value.structured_value {
                check.json(value)?;
            }
            if let Some(source) = &value.source_snapshot {
                Disclosure::text(&source.source_ref)?;
            }
            if let Some(d::ProjectMemoryRetiringTarget::External { project, reference }) =
                &value.retiring_target
            {
                Disclosure::text(project)?;
                Disclosure::text(reference)?;
            }
        }
        K::MemoryAssertionEvent => {
            let value: d::MemoryAssertionEvent = decode(record)?;
            Disclosure::actor(&value.actor)?;
            Disclosure::text(&value.policy_reason)?;
        }
        K::WorkSourceSnapshot => {
            let value: d::WorkSourceSnapshot = decode(record)?;
            Disclosure::text(&value.adapter_kind)?;
            Disclosure::text(&value.canonical_ref)?;
            if let Some(url) = &value.canonical_url {
                Disclosure::text(url)?;
            }
            for text in [
                &value.projected.title,
                &value.projected.body,
                &value.projected.status,
            ]
            .into_iter()
            .flatten()
            {
                check.note(text)?;
            }
            if let Some(owner) = &value.projected.owner {
                Disclosure::author(owner)?;
            }
            for (key, value) in &value.raw {
                check.note(key)?;
                check.json(value)?;
            }
        }
        K::WorkSourceProposal => {
            Disclosure::actor(&decode::<d::WorkSourceProposal>(record)?.notice.actor)?;
        }
        K::VerificationApplicability => {
            let value: crate::task_sync::evidence::verification::VerificationApplicabilityDecision =
                decode(record)?;
            Disclosure::actor(&value.origin_actor)?;
            Disclosure::actor(&value.local_actor)?;
        }
        K::AcceptanceUse => {
            let value: crate::task_sync::evidence::acceptance::AcceptanceUseDecision =
                decode(record)?;
            Disclosure::actor(&value.local_actor)?;
        }
        K::ImportedConsumption => {
            let value: crate::task_sync::evidence::consumption::ImportedEvidenceConsumption =
                decode(record)?;
            Disclosure::actor(&value.actor)?;
            Disclosure::work(&value.governing)?;
        }
        // These kinds carry only typed identities, closed vocabulary and facts.
        K::WorkObligation
        | K::WorkHandoffOffer
        | K::ControlPolicy
        | K::ObligationRuleSet
        | K::OriginFeedSegment
        | K::OriginFeedCut
        | K::GoverningSnapshotWitness => {}
    }
    Ok(())
}
