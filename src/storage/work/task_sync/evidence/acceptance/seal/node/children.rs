//! Account for every required child as the root feed stood at completion.
use crate::domain::{
    ChildRequirement, CompletionSeal, RequiredChildResolution, RequiredChildWaiver, RootExecution,
    WorkEvent, WorkId, WorkLifecycle, WorkRunId, WorkTransition,
};
use crate::storage::work::task_sync::evidence::acceptance::{History, Refusal, Result, invalid};
use crate::{FeedId, ObjectId, WorkItem};
use std::collections::{BTreeMap, HashSet};

struct Child {
    revision: i64,
    lifecycle: WorkLifecycle,
    run: Option<(WorkRunId, i64)>,
}

pub(super) fn validate(
    history: &History<'_>,
    seal: &CompletionSeal,
    root: &RootExecution,
    completed_at: i64,
    seen: &mut std::collections::BTreeSet<ObjectId>,
    budget: &mut crate::task_sync::evidence::limits::StagingBudget,
    depth: usize,
) -> Result<Option<Refusal>> {
    let (children, waiver_events) = children_at(history, seal, completed_at, budget)?;
    let mut accounted = HashSet::new();
    for id in &seal.required_child_seals {
        let child: CompletionSeal = history.load(id, "completion_seal")?;
        let governing: WorkItem =
            history.load(&child.accepted_work_revision_hash, "work_item_revision")?;
        if !accounted.insert(child.work_id)
            || !root.required_child_seals.contains(id)
            || child.root_id != seal.root_id
            || child.root_execution_id != seal.root_execution_id
            || governing.parent_id != Some(seal.work_id)
            || governing.child_requirement != ChildRequirement::Required
            || children.get(&child.work_id).is_none_or(|state| {
                state.lifecycle != WorkLifecycle::Completed
                    || state.run != Some((child.run_id, child.run_generation))
            })
        {
            return Ok(Some(Refusal::IncompleteClosure));
        }
        if let Some(refusal) = super::validate(history, id, seen, budget, depth + 1)? {
            return Ok(Some(refusal));
        }
    }
    for waiver in &seal.required_child_waivers {
        if !accounted.insert(waiver.work_id)
            || !root.required_child_waivers.contains(waiver)
            || waiver.waived_by.trim().is_empty()
            || waiver.reason.trim().is_empty()
            || children.get(&waiver.work_id).is_none_or(|state| {
                !matches!(
                    state.lifecycle,
                    WorkLifecycle::Cancelled | WorkLifecycle::Superseded
                ) || state.revision != waiver.work_revision
            })
            || waiver_events.get(&waiver.work_id) != Some(waiver)
        {
            return Ok(Some(Refusal::IncompleteClosure));
        }
    }
    for resolution in &seal.required_child_resolutions {
        let RequiredChildResolution::ResolvedBySuccessor {
            work_id,
            work_revision,
            supersession,
            successor_seal,
            ..
        } = resolution;
        let event: WorkEvent = history.load(supersession, "work_event")?;
        let proof: CompletionSeal = history.load(successor_seal, "completion_seal")?;
        let accepted: WorkItem =
            history.load(&proof.accepted_work_revision_hash, "work_item_revision")?;
        if children.get(work_id).is_none_or(|state| {
            state.lifecycle != WorkLifecycle::Superseded || state.revision != *work_revision
        }) || crate::storage::work::task_sync::evidence::acceptance::movement::position(
            history,
            &FeedId::RootWork(seal.root_id),
            supersession,
        )? >= completed_at
        {
            return Ok(Some(Refusal::IncompleteClosure));
        }
        crate::storage::work::completion::validate_retained_resolution(
            seal,
            resolution,
            &event,
            &proof,
            &accepted,
            &mut accounted,
        )?;
        // The successor seal is already checked in the required child set.
        // Its second link accounts for the superseded sibling, not another run.
    }
    if accounted.len() != children.len() || children.keys().any(|id| !accounted.contains(id)) {
        return Ok(Some(Refusal::IncompleteClosure));
    }
    Ok(None)
}

type Children = BTreeMap<WorkId, Child>;
type Waivers = BTreeMap<WorkId, RequiredChildWaiver>;

fn children_at(
    history: &History<'_>,
    seal: &CompletionSeal,
    completed_at: i64,
    budget: &mut crate::task_sync::evidence::limits::StagingBudget,
) -> Result<(Children, Waivers)> {
    let feed = FeedId::RootWork(seal.root_id);
    let mut children: Children = BTreeMap::new();
    let mut runs = BTreeMap::new();
    let mut waivers = BTreeMap::new();
    if !history.walk(&feed, completed_at - 1, |_, id, kind| {
        if kind != "work_event" {
            return Ok(());
        }
        let event: WorkEvent = history.load(id, kind)?;
        if event.project_id != *history.project || event.root_id != seal.root_id {
            return Err(invalid("required-child history crosses its root feed"));
        }
        if let Some(run) = event.run {
            if !runs.contains_key(&event.work_id) {
                budget
                    .reserve(std::mem::size_of::<(WorkId, WorkRunId, i64)>())
                    .map_err(crate::task_sync::evidence::EvidenceError::from)?;
            }
            runs.insert(event.work_id, (run.run_id, run.generation));
        }
        if event.work.parent_id == Some(seal.work_id)
            && event.work.child_requirement == ChildRequirement::Required
        {
            if !children.contains_key(&event.work_id) {
                budget
                    .reserve(std::mem::size_of::<(WorkId, Child)>())
                    .map_err(crate::task_sync::evidence::EvidenceError::from)?;
            }
            children.insert(
                event.work_id,
                Child {
                    revision: event.work.revision,
                    lifecycle: event.work.lifecycle,
                    run: runs.get(&event.work_id).copied(),
                },
            );
        } else {
            children.remove(&event.work_id);
        }
        if event.work_id == seal.work_id
            && event
                .root_execution
                .as_ref()
                .is_some_and(|root| root.root_execution_id == seal.root_execution_id)
            && let WorkTransition::RequiredChildWaived {
                child_id,
                child_revision,
                reason,
            } = event.transition
        {
            let waiver = RequiredChildWaiver {
                work_id: child_id,
                work_revision: child_revision,
                waived_by: event.actor.actor_id,
                reason,
            };
            budget
                .reserve(
                    std::mem::size_of::<(WorkId, RequiredChildWaiver)>()
                        + waiver.waived_by.len()
                        + waiver.reason.len(),
                )
                .map_err(crate::task_sync::evidence::EvidenceError::from)?;
            if waivers.insert(child_id, waiver).is_some() {
                return Err(invalid("required-child waiver repeats its origin event"));
            }
        }
        Ok(())
    })? {
        return Err(invalid(
            "required-child history has no complete witnessed interval",
        ));
    }
    Ok((children, waivers))
}
