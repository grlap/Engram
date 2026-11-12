//! Follow explicit uses without replacing their original producing namespace.

use super::{History, Refusal, Result, same_governing};
use crate::domain::{
    AcceptanceBinding, AcceptanceEvaluation, AcceptanceEvaluationPolicy, CompletionSeal,
    SCHEMA_VERSION,
};
use crate::storage::work::task_sync::evidence::verification::applicability::{
    Destination, VerificationUse, eligible_for_on,
};
use crate::task_sync::evidence::acceptance::AcceptanceUseDecision;
use crate::task_sync::evidence::consumption::{
    CONSUMPTION_KIND, ConsumedProof, ImportedEvidenceConsumption,
};
use crate::{CanonicalObject, ObjectId, source::SourceId};

pub(super) fn verification(
    history: &History<'_>,
    destination: &Destination,
    source: &SourceId,
    id: &ObjectId,
    bound: Option<&AcceptanceBinding>,
) -> Result<bool> {
    let record: ImportedEvidenceConsumption = history.load(id, CONSUMPTION_KIND)?;
    let ConsumedProof::Verification {
        origin,
        original,
        origin_cut,
        requirement,
        criterion,
        ..
    } = &record.proof
    else {
        return Ok(false);
    };
    if &record.source != source
        || !same_governing(&record.governing, &destination.item)?
        || bound.is_some_and(|bound| {
            criterion != &Some(bound.criterion) || requirement != &bound.requirement
        })
    {
        return Ok(false);
    }
    // Re-read the actual original producer and complete witnessed scope.
    super::super::verification::applicability::candidate_in_context(
        history.connection,
        history.project,
        origin,
        original,
        origin_cut,
    )?;
    let governing = CanonicalObject::freeze(&destination.item)?.key().clone();
    let context = VerificationUse {
        project: history.project,
        origin,
        original,
        binding: &history.request.binding,
        criterion: bound.map(|bound| bound.criterion),
        requirement: bound.map(|bound| &bound.requirement),
        governing: &governing,
        root: &destination.root,
        source,
    };
    for decision in &history.request.verification_decisions {
        if eligible_for_on(history.connection, decision, &context)? {
            return Ok(true);
        }
    }
    Ok(false)
}

/// A judged pass may cite any consumption its own run recorded, as native
/// admission does. The record must still belong to that run, on the governing
/// item and source in use, and what it consumed must be intact in its origin.
pub(super) fn judgment(
    history: &History<'_>,
    destination: &Destination,
    source: &SourceId,
    evaluation: &AcceptanceEvaluation,
    id: &ObjectId,
) -> Result<bool> {
    let record: ImportedEvidenceConsumption = history.load(id, CONSUMPTION_KIND)?;
    if record.schema_version != SCHEMA_VERSION
        || &record.request.consumption != id
        || record.project_id != *history.project
        || record.request.binding.run_id != evaluation.run_id
        || record.request.binding.work_id != evaluation.work_id
        || &record.source != source
        || CanonicalObject::freeze(&record.governing)?.key() != &record.governing_fingerprint
        || !same_governing(&record.governing, &destination.item)?
    {
        return Ok(false);
    }
    match &record.proof {
        ConsumedProof::Verification {
            origin,
            original,
            origin_cut,
            ..
        } => {
            super::super::verification::applicability::candidate_in_context(
                history.connection,
                history.project,
                origin,
                original,
                origin_cut,
            )?;
            Ok(true)
        }
        ConsumedProof::Acceptance { origin, proof } => {
            let decision: AcceptanceUseDecision =
                history.load(&record.request.decision, "task_sync_acceptance_use")?;
            Ok(decision.refusal.is_none()
                && &decision.request.origin == origin
                && &decision.request.proof == proof
                && decision.request.binding == record.request.binding)
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(super) fn acceptance(
    history: &History<'_>,
    destination: &Destination,
    policy: &AcceptanceEvaluationPolicy,
    source: &SourceId,
    seal: &CompletionSeal,
    id: &ObjectId,
    depth: usize,
    selected: &mut bool,
) -> Result<Option<Refusal>> {
    let record: ImportedEvidenceConsumption = history.load(id, CONSUMPTION_KIND)?;
    let ConsumedProof::Acceptance { origin, proof } = &record.proof else {
        return Ok(None);
    };
    if *selected
        || record.request.binding.work_id != seal.work_id
        || record.request.binding.run_id != seal.run_id
        || &record.source != source
        || !same_governing(&record.governing, &destination.item)?
    {
        return Ok(Some(Refusal::OriginContextDiffers));
    }
    let decision: AcceptanceUseDecision =
        history.load(&record.request.decision, "task_sync_acceptance_use")?;
    if decision.refusal.is_some()
        || &decision.request.origin != origin
        || &decision.request.proof != proof
        || decision.request.binding != record.request.binding
    {
        return Ok(Some(Refusal::IncompleteClosure));
    }
    // Only the new assessment context changes. Original proof ids, cuts and
    // origin remain the recorded immutable request; no stored bytes change.
    let mut request = decision.request;
    request.binding = history.request.binding.clone();
    request
        .verification_decisions
        .clone_from(&history.request.verification_decisions);
    let nested = History::new(history.connection, history.project, &request)?;
    if let Some(refusal) = super::inspect_at(&nested, destination, policy, source, depth + 1)? {
        return Ok(Some(refusal));
    }
    let mut results = match &request.proof {
        super::HistoricalProof::Evaluation { evaluation, .. } => {
            crate::storage::work::acceptance_evaluation::derive_acceptance_results(
                &nested.load(evaluation, "acceptance_evaluation")?,
                record.actor.assurance,
            )
        }
        super::HistoricalProof::NativeCompletion { seal, .. } => {
            nested
                .load::<CompletionSeal>(seal, "completion_seal")?
                .acceptance
        }
    };
    for result in &mut results {
        result.evidence = vec![id.clone()];
    }
    if results != seal.acceptance {
        return Ok(Some(Refusal::EvaluationShape));
    }
    *selected = true;
    Ok(None)
}
