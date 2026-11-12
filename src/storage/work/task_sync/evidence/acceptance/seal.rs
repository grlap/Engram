use super::super::verification::applicability::Destination;
use crate::domain::{
    AcceptanceEvaluationPolicy, CompletionSeal, NamedRootBindingEvent, NamedRootBindingKind,
    VerificationSourceBasis,
};
use crate::storage::work::task_sync::evidence::acceptance::{
    History, Refusal, Result, same_governing,
};
use crate::{CanonicalObject, FeedId, ObjectId, WorkItem};

mod node;

pub(super) fn inspect(
    history: &History<'_>,
    destination: &Destination,
    policy: &AcceptanceEvaluationPolicy,
    source: &crate::source::SourceId,
    id: &ObjectId,
    witness: Option<&ObjectId>,
    depth: usize,
) -> Result<Option<Refusal>> {
    let (kind, _) = super::super::verification::applicability::qualified_bytes(
        history.connection,
        history.project,
        &history.request.origin,
        id,
    )?;
    if kind != "completion_seal" {
        return Ok(Some(Refusal::ReportedCompletion));
    }
    let seal: CompletionSeal = history.load(id, "completion_seal")?;
    if seal.restored || !seal.restored_child_completions.is_empty() {
        return Ok(Some(Refusal::ReportedCompletion));
    }
    let governing: WorkItem =
        history.load(&seal.accepted_work_revision_hash, "work_item_revision")?;
    if governing.revision != seal.accepted_work_revision
        || governing.work_id != seal.work_id
        || governing.root_id != seal.root_id
        || !same_governing(&governing, &destination.item)?
    {
        return Ok(Some(Refusal::GoverningContentDiffers));
    }
    if let Some(evaluation) = &seal.acceptance_evaluation {
        let Some(witness) = witness else {
            return Ok(Some(Refusal::GoverningSnapshotUnavailable));
        };
        if let Some(refusal) =
            super::evaluation::inspect(history, destination, policy, source, evaluation, witness)?
        {
            return Ok(Some(refusal));
        }
    }
    let mut imported_acceptance = false;
    for evidence in &seal.evidence {
        let (kind, _) = super::super::verification::applicability::qualified_bytes(
            history.connection,
            history.project,
            &history.request.origin,
            evidence,
        )?;
        if kind == crate::task_sync::evidence::consumption::CONSUMPTION_KIND
            && let Some(refusal) = super::imported::acceptance(
                history,
                destination,
                policy,
                source,
                &seal,
                evidence,
                depth,
                &mut imported_acceptance,
            )?
        {
            return Ok(Some(refusal));
        }
    }
    if policy.is_evaluated() && seal.acceptance_evaluation.is_none() && !imported_acceptance {
        return Ok(Some(Refusal::EvaluationPolicy));
    }
    if seal.acceptance.len() != destination.item.acceptance.len()
        || seal
            .acceptance
            .iter()
            .zip(&destination.item.acceptance)
            .any(|(result, criterion)| !result.satisfied || &result.criterion != criterion)
    {
        return Ok(Some(Refusal::BlockingVerdict));
    }
    let feed = FeedId::RunExecution(seal.run_id);
    let mut named_root = None;
    if !history.walk(&feed, seal.completion_cut.position, |_, id, kind| {
        if kind == "named_root_binding" {
            let binding: NamedRootBindingEvent = history.load(id, kind)?;
            named_root = Some(binding);
        }
        Ok(())
    })? {
        return Ok(Some(Refusal::OriginContextUnavailable));
    }
    let Some(root) = named_root.filter(|root| {
        root.kind == NamedRootBindingKind::Bound
            && root.run_id == seal.run_id
            && root.work_id == seal.work_id
    }) else {
        return Ok(Some(Refusal::OriginContextUnavailable));
    };
    if history
        .source_at(&feed, seal.completion_cut.position, &root)?
        .as_ref()
        .and_then(VerificationSourceBasis::source_id)
        != Some(source)
    {
        return Ok(Some(Refusal::SourceDiffers));
    }
    let governing = CanonicalObject::freeze(&destination.item)?.key().clone();
    for binding in &destination.item.acceptance_bindings {
        let Some(result) = seal.acceptance.get(binding.criterion.saturating_sub(1)) else {
            return Ok(Some(Refusal::CitationIneligible));
        };
        let mut eligible = false;
        for evidence in &result.evidence {
            let (kind, _) = super::super::verification::applicability::qualified_bytes(
                history.connection,
                history.project,
                &history.request.origin,
                evidence,
            )?;
            if kind == crate::task_sync::evidence::consumption::CONSUMPTION_KIND {
                eligible |= imported_acceptance
                    || super::imported::verification(
                        history,
                        destination,
                        source,
                        evidence,
                        Some(binding),
                    )?;
                continue;
            }
            let context = super::super::verification::applicability::VerificationUse {
                project: history.project,
                origin: &history.request.origin,
                binding: &history.request.binding,
                original: evidence,
                criterion: Some(binding.criterion),
                requirement: Some(&binding.requirement),
                governing: &governing,
                root: &destination.root,
                source,
            };
            for decision in &history.request.verification_decisions {
                eligible |= super::super::verification::applicability::eligible_for_on(
                    history.connection,
                    decision,
                    &context,
                )?;
            }
        }
        if !eligible {
            return Ok(Some(Refusal::CitationIneligible));
        }
    }
    let mut seen = std::collections::BTreeSet::new();
    let mut budget = crate::task_sync::evidence::limits::StagingBudget::default();
    node::validate(history, id, &mut seen, &mut budget, 0)
}
