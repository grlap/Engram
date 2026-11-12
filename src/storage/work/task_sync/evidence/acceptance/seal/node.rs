use crate::domain::{
    AcceptanceEvaluation, COMPLETION_ENVIRONMENT_SCHEMA_VERSION,
    COMPLETION_OBLIGATION_SCHEMA_VERSION, CompletionSeal, RootExecution, SCHEMA_VERSION,
    WorkCheckpoint, WorkEvent, WorkTransition,
};
use crate::storage::work::task_sync::evidence::acceptance::{History, Refusal, Result, invalid};
use crate::storage::work::task_sync::evidence::{store_error, verification};
use crate::{FeedId, ObjectId, WorkItem};

mod children;
mod obligations;

pub(super) fn validate(
    history: &History<'_>,
    id: &ObjectId,
    seen: &mut std::collections::BTreeSet<ObjectId>,
    budget: &mut crate::task_sync::evidence::limits::StagingBudget,
    depth: usize,
) -> Result<Option<Refusal>> {
    if depth > 1_024 || !seen.insert(id.clone()) {
        return Ok(Some(Refusal::IncompleteClosure));
    }
    let result = validate_node(history, id, seen, budget, depth);
    seen.remove(id);
    result
}

fn validate_node(
    history: &History<'_>,
    id: &ObjectId,
    seen: &mut std::collections::BTreeSet<ObjectId>,
    budget: &mut crate::task_sync::evidence::limits::StagingBudget,
    depth: usize,
) -> Result<Option<Refusal>> {
    let bytes = history.bytes(id, "completion_seal")?;
    budget
        .reserve(bytes.len())
        .map_err(crate::task_sync::evidence::EvidenceError::from)?;
    let seal: CompletionSeal = serde_json::from_slice(&bytes)?;
    if seal.restored || !seal.restored_child_completions.is_empty() {
        return Ok(Some(Refusal::ReportedCompletion));
    }
    if seal.schema_version != SCHEMA_VERSION
        || seal.obligation_schema_version != COMPLETION_OBLIGATION_SCHEMA_VERSION
        || seal.environment_schema_version != COMPLETION_ENVIRONMENT_SCHEMA_VERSION
        || !seal.drain.released_resource_leases.is_empty()
    {
        return Ok(Some(Refusal::IncompleteClosure));
    }
    let governing: WorkItem =
        history.load(&seal.accepted_work_revision_hash, "work_item_revision")?;
    // Planning may have been imported before this native run. Completion
    // provenance comes from the seal and its exact native event/root proof.
    if governing.project_id != *history.project
        || governing.work_id != seal.work_id
        || governing.root_id != seal.root_id
        || governing.revision != seal.accepted_work_revision
        || governing.acceptance.len() != seal.acceptance.len()
        || governing
            .acceptance
            .iter()
            .zip(&seal.acceptance)
            .any(|(criterion, result)| criterion != &result.criterion || !result.satisfied)
    {
        return Ok(Some(Refusal::GoverningContentDiffers));
    }
    let feed = FeedId::RunExecution(seal.run_id);
    let Some(cut) = history.cut(&feed) else {
        return Ok(Some(Refusal::OriginContextUnavailable));
    };
    if seal.completion_cut.feed != feed || seal.completion_cut.position > cut.through {
        return Ok(Some(Refusal::OriginContextDiffers));
    }
    // The native writer stores the seal as a linked object. Its Completed
    // event, validated below against the exact root predecessor, is the feed
    // member; the seal itself has no run-feed position.
    if let Some(refusal) = obligations::validate(history, &seal, &feed, budget)? {
        return Ok(Some(refusal));
    }
    if let Some(refusal) = proof_links(history, &seal, &feed)? {
        return Ok(Some(refusal));
    }
    if history.cut(&FeedId::RootWork(seal.root_id)).is_none() {
        return Ok(Some(Refusal::OriginContextUnavailable));
    }
    let Some((root, completed_at)) = root_at_completion(history, id, &seal, budget)? else {
        return Ok(Some(Refusal::IncompleteClosure));
    };
    children::validate(history, &seal, &root, completed_at, seen, budget, depth)
}

fn proof_links(
    history: &History<'_>,
    seal: &CompletionSeal,
    feed: &FeedId,
) -> Result<Option<Refusal>> {
    for evidence in &seal.evidence {
        let (kind, _) = verification::applicability::qualified_bytes(
            history.connection,
            history.project,
            &history.request.origin,
            evidence,
        )?;
        if !history.member(feed, evidence, &kind, seal.completion_cut.position)? {
            return Ok(Some(Refusal::IncompleteClosure));
        }
    }
    for result in &seal.acceptance {
        if !result.satisfied
            || result
                .evidence
                .iter()
                .any(|evidence| !seal.evidence.contains(evidence))
        {
            return Ok(Some(Refusal::BlockingVerdict));
        }
    }
    if let Some(checkpoint) = &seal.checkpoint {
        let _: WorkCheckpoint = history.load(checkpoint, "work_checkpoint")?;
    }
    if let Some(evaluation) = &seal.acceptance_evaluation {
        let _: AcceptanceEvaluation = history.load(evaluation, "acceptance_evaluation")?;
    }
    for action in &seal.drain.reconciled_action_outcomes {
        verification::applicability::qualified_bytes(
            history.connection,
            history.project,
            &history.request.origin,
            action,
        )?;
    }
    Ok(None)
}

fn root_at_completion(
    history: &History<'_>,
    id: &ObjectId,
    seal: &CompletionSeal,
    budget: &mut crate::task_sync::evidence::limits::StagingBudget,
) -> Result<Option<(RootExecution, i64)>> {
    let root_feed = FeedId::RootWork(seal.root_id);
    let Some(root_cut) = history.cut(&root_feed) else {
        return Ok(None);
    };
    let mut completion = None;
    if !history.walk(&root_feed, root_cut.through, |position, event_id, kind| {
        if kind == "work_event" {
            let event: WorkEvent = history.load(event_id, kind)?;
            if matches!(&event.transition,WorkTransition::Completed {seal} if seal==id) {
                if completion.is_some() {
                    return Err(invalid("foreign completion repeats its native event"));
                }
                completion = Some((event, position));
            }
        }
        Ok(())
    })? {
        return Ok(None);
    }
    let Some((event, position)) = completion else {
        return Ok(None);
    };
    crate::storage::work::completion::validate_seal_event_fields(seal, id, &event)?;
    let after = event
        .root_execution
        .as_ref()
        .ok_or_else(|| invalid("foreign completed root is unavailable"))?;
    crate::storage::work::root_state::verify_retained_completion_predecessor(
        &seal.root_execution,
        after,
        &history.load(&seal.root_execution.head, "work_root_delta")?,
        &history.load(&after.head, "work_root_delta")?,
    )?;
    let root = crate::storage::work::root_state::resolve_retained(&seal.root_execution, |id| {
        let bytes = history.bytes(id, "work_root_delta").map_err(store_error)?;
        budget
            .reserve(bytes.len())
            .map_err(crate::task_sync::evidence::EvidenceError::from)
            .map_err(|error| store_error(error.into()))?;
        Ok(serde_json::from_slice(&bytes)?)
    })?;
    if !root.run_ids.contains(&seal.run_id)
        || seal.actor.session_id.as_ref().is_none_or(|holder| {
            !root.expected_contributors.contains(holder)
                || !root.contributions.iter().any(|contribution| {
                    &contribution.participant == holder
                        && seal.checkpoint.as_ref() == Some(&contribution.object)
                })
        })
        || (seal.work_id == seal.root_id
            && crate::storage::work::planning::first_unaccounted_root_contributor(&root).is_some())
    {
        return Ok(None);
    }
    for contribution in &root.contributions {
        verification::applicability::qualified_bytes(
            history.connection,
            history.project,
            &history.request.origin,
            &contribution.object,
        )?;
    }
    Ok(Some((root, position)))
}
