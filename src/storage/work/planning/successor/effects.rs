//! One mapping from resolved task documents to native planning transitions.

use super::{
    AddWorkBlockerRequest, ChangeWorkPrerequisiteRequest, CheckedPlanningSuccessor,
    ClearWorkBlockerRequest, Connection, ObjectId, ReviseWorkRequest, StoreError, WorkBlocker,
    WorkId, WorkItem, WorkLifecycle, WorkPlanningAuthority, add_blocker_successor_on,
    change_prerequisite_successor_on, clear_blocker_successor_on, load_work_item,
    plan_successor_revision_obligations_on, revise_successor_on,
};
use crate::task_sync::{
    TaskDocument,
    delta::{DeltaCandidatePayload, PlanningSuccessorRecipe},
    notes::NoteOmissions,
};
use std::collections::{BTreeMap, BTreeSet};

enum Effect {
    Reopen(WorkId),
    Revise(WorkId, Box<crate::WorkRevisionPatch>),
    RemovePrerequisite(WorkId, WorkId),
    ClearBlocker(WorkId, String),
    AddPrerequisite(WorkId, WorkId),
    AddBlocker(WorkId, Box<WorkBlocker>),
}

impl Effect {
    fn work(&self) -> WorkId {
        match self {
            Self::Reopen(work)
            | Self::Revise(work, _)
            | Self::RemovePrerequisite(work, _)
            | Self::ClearBlocker(work, _)
            | Self::AddPrerequisite(work, _)
            | Self::AddBlocker(work, _) => *work,
        }
    }

    fn phase(&self) -> u8 {
        match self {
            Self::Reopen(_) => 0,
            Self::Revise(_, _) => 1,
            Self::RemovePrerequisite(_, _) => 2,
            Self::ClearBlocker(_, _) => 3,
            Self::AddPrerequisite(_, _) => 4,
            Self::AddBlocker(_, _) => 5,
        }
    }
}

fn unsupported(reason: &str) -> StoreError {
    StoreError::InvalidWork(format!(
        "native planning successor does not support {reason}"
    ))
}

fn depth_on(connection: &Connection, item: &WorkItem) -> Result<usize, StoreError> {
    let mut seen = BTreeSet::from([item.work_id]);
    let mut parent = item.parent_id;
    let mut depth = 0;
    while let Some(id) = parent {
        if !seen.insert(id) {
            return Err(StoreError::WorkDependencyCycle);
        }
        let ancestor = load_work_item(connection, id)?;
        if ancestor.project_id != item.project_id {
            return Err(unsupported("cross-project ancestry"));
        }
        parent = ancestor.parent_id;
        depth += 1;
    }
    Ok(depth)
}

fn scalar_patch(item: &WorkItem, document: &TaskDocument) -> crate::WorkRevisionPatch {
    crate::WorkRevisionPatch {
        title: (item.title != document.title).then(|| document.title.clone()),
        outcome: (item.outcome != document.outcome).then(|| document.outcome.clone()),
        acceptance: (item.acceptance != document.acceptance).then(|| document.acceptance.clone()),
        // A replacement list always carries its authored binding positions.
        acceptance_bindings: (item.acceptance != document.acceptance
            || item.acceptance_bindings != document.acceptance_bindings)
            .then(|| document.acceptance_bindings.clone()),
        kind: (item.kind != document.kind).then_some(document.kind),
        priority: (item.priority != document.priority).then_some(document.priority),
        labels: (item.labels != document.labels).then(|| document.labels.clone()),
        external_ref: (item.external_ref != document.external_ref)
            .then(|| document.external_ref.clone())
            .flatten(),
        clear_external: item.external_ref.is_some() && document.external_ref.is_none(),
        assigned_to: (item.assigned_to != document.assigned_to)
            .then(|| document.assigned_to.clone())
            .flatten(),
        clear_assignment: item.assigned_to.is_some() && document.assigned_to.is_none(),
        deferred_until: (item.deferred_until != document.deferred_until)
            .then_some(document.deferred_until)
            .flatten(),
        clear_deferral: item.deferred_until.is_some() && document.deferred_until.is_none(),
        evaluation_mode: (item.evaluation_mode != document.evaluation_mode)
            .then_some(document.evaluation_mode)
            .flatten(),
        clear_evaluation_mode: item.evaluation_mode.is_some() && document.evaluation_mode.is_none(),
        ..Default::default()
    }
}

fn derive_on(
    connection: &Connection,
    project: &crate::ProjectId,
    payload: &DeltaCandidatePayload,
    selected: &BTreeSet<WorkId>,
) -> Result<Vec<Effect>, StoreError> {
    super::super::super::query::on_one_snapshot(connection, |connection| {
        derive_in_snapshot(connection, project, payload, selected)
    })
}

fn derive_in_snapshot(
    connection: &Connection,
    project: &crate::ProjectId,
    payload: &DeltaCandidatePayload,
    selected: &BTreeSet<WorkId>,
) -> Result<Vec<Effect>, StoreError> {
    if &payload.project_id != project {
        return Err(unsupported("another project"));
    }
    let mut effects = Vec::new();
    let mut depths = BTreeMap::new();
    for &work in selected {
        let document = payload.documents.get(&work).ok_or_else(|| {
            StoreError::InvalidWorkProjection(
                "successor selected native document is missing".into(),
            )
        })?;
        let document = document
            .as_ref()
            .ok_or_else(|| unsupported("task removal"))?;
        if document.id != work {
            return Err(unsupported("a mismatched task identity"));
        }
        let item = match load_work_item(connection, work) {
            Err(StoreError::WorkNotFound(_)) => return Err(unsupported("task creation")),
            result => result?,
        };
        if &item.project_id != project {
            return Err(unsupported("cross-project task selection"));
        }
        if item.parent_id != document.parent_id
            || item.child_requirement != document.child_requirement
        {
            return Err(unsupported("parent or required-child changes"));
        }
        depths.insert(work, depth_on(connection, &item)?);
        let mut omissions = NoteOmissions::default();
        let mut activity_omissions = NoteOmissions::default();
        let (current, _) = super::super::super::task_sync::capture::capture_document_on(
            connection,
            item.clone(),
            payload.output.profile.include_notes,
            payload.output.profile.origin_machine.as_deref(),
            false,
            &mut omissions,
            &mut activity_omissions,
        )?;
        if current.notes != document.notes || current.activity != document.activity {
            return Err(unsupported("note or activity changes"));
        }
        let reopen = document.lifecycle == WorkLifecycle::Open
            && (item.lifecycle == WorkLifecycle::Completed
                || super::super::super::completion::is_import_retired_on(connection, &item)?);
        if reopen {
            effects.push(Effect::Reopen(work));
            if document.completion.is_some()
                || document.disposal_reason.is_some()
                || document.superseded_by.is_some()
            {
                return Err(unsupported("terminal metadata on reopened work"));
            }
        } else if item.lifecycle != document.lifecycle
            || current.completion != document.completion
            || current.disposal_reason != document.disposal_reason
            || current.superseded_by != document.superseded_by
        {
            return Err(unsupported("terminal lifecycle or completion changes"));
        }
        let patch = scalar_patch(&item, document);
        let changed_scalars = patch != crate::WorkRevisionPatch::default();
        let current_edges = current
            .prerequisites
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let desired_edges = document
            .prerequisites
            .iter()
            .copied()
            .collect::<BTreeSet<_>>();
        let current_blockers = current
            .blockers
            .iter()
            .map(|b| (&b.blocker_id, b))
            .collect::<BTreeMap<_, _>>();
        let desired_blockers = document
            .blockers
            .iter()
            .map(|b| (&b.blocker_id, b))
            .collect::<BTreeMap<_, _>>();
        let relations_changed =
            current_edges != desired_edges || current_blockers != desired_blockers;
        if !reopen
            && item.lifecycle != WorkLifecycle::Open
            && (changed_scalars || relations_changed)
        {
            return Err(unsupported(
                "planning changes to terminal work without reopening",
            ));
        }
        if changed_scalars {
            effects.push(Effect::Revise(work, Box::new(patch)));
        }
        for &prerequisite in current_edges.difference(&desired_edges) {
            effects.push(Effect::RemovePrerequisite(work, prerequisite));
        }
        for &prerequisite in desired_edges.difference(&current_edges) {
            effects.push(Effect::AddPrerequisite(work, prerequisite));
        }
        for (id, blocker) in &current_blockers {
            match desired_blockers.get(id) {
                Some(desired) if *desired != *blocker => {
                    return Err(unsupported("rewriting an existing blocker identity"));
                }
                None => effects.push(Effect::ClearBlocker(work, (*id).clone())),
                _ => {}
            }
        }
        for (id, blocker) in &desired_blockers {
            if !current_blockers.contains_key(id) {
                effects.push(Effect::AddBlocker(
                    work,
                    Box::new(WorkBlocker {
                        blocker_id: blocker.blocker_id.clone(),
                        work_id: work,
                        kind: blocker.kind,
                        detail: blocker.detail.clone(),
                        created_by: blocker.created_by.reported(),
                        created_at: blocker.created_at,
                    }),
                ));
            }
        }
    }
    // Remove every selected old edge before adding any new edge. This admits
    // a resolved acyclic graph without a transient cycle from iteration order.
    effects.sort_by_key(|effect| (effect.phase(), depths[&effect.work()], effect.work()));
    Ok(effects)
}

/// Mirror which native reopen transitions initialize a root aggregate. Several
/// selected children can require the same root without reopening the root task.
fn required_roots_on(
    connection: &Connection,
    reopened: impl Iterator<Item = WorkId>,
) -> Result<BTreeSet<WorkId>, StoreError> {
    let mut roots = BTreeSet::new();
    for work in reopened {
        let item = load_work_item(connection, work)?;
        let restored =
            super::super::super::query::work_completed_by_restored_record_on(connection, &item)?;
        let needs_root = if restored {
            super::super::super::query::active_root_execution_optional(connection, item.root_id)?
                .is_none()
        } else {
            item.work_id == item.root_id
        };
        if needs_root {
            roots.insert(item.root_id);
        }
    }
    Ok(roots)
}

/// Match the complete root demand before any native transition changes it.
pub(in crate::storage::work) fn require_successor_roots_on(
    connection: &Connection,
    recipe: &PlanningSuccessorRecipe,
) -> Result<(), StoreError> {
    let expected = required_roots_on(connection, recipe.new_runs.keys().copied())?;
    if recipe
        .new_root_executions
        .keys()
        .copied()
        .collect::<BTreeSet<_>>()
        != expected
    {
        return Err(StoreError::InvalidWorkProjection(
            "successor retained root selection differs from native execution demand".into(),
        ));
    }
    Ok(())
}

/// Select once while constructing a new retained recipe. This writes no SQL;
/// replays load that recipe rather than minting another construction.
pub(in crate::storage::work) fn select_successor_effects_on(
    connection: &Connection,
    project: &crate::ProjectId,
    payload: &DeltaCandidatePayload,
    recipe: &mut PlanningSuccessorRecipe,
) -> Result<(), StoreError> {
    let selected = &recipe.request.intent.candidate.changed;
    let effects = derive_on(connection, project, payload, selected)?;
    let mut counts = BTreeMap::<WorkId, usize>::new();
    for effect in &effects {
        *counts.entry(effect.work()).or_default() += 1;
    }
    for &work in selected {
        let count = counts.get(&work).copied().unwrap_or(0);
        let mut ids = recipe.task_events.remove(&work).unwrap_or_default();
        ids.resize_with(count, ObjectId::mint);
        recipe.task_events.insert(work, ids);
    }
    for effect in &effects {
        match effect {
            Effect::Reopen(work) => {
                recipe.plan_reopen(*work, false)?;
            }
            Effect::AddBlocker(work, blocker) => {
                recipe
                    .blocker_records
                    .entry(*work)
                    .or_default()
                    .insert(blocker.blocker_id.clone(), ObjectId::mint());
            }
            Effect::Revise(work, patch) => {
                let mut item = load_work_item(connection, *work)?;
                if let Some(run) = recipe.new_runs.get(work) {
                    item.lifecycle = WorkLifecycle::Open;
                    item.active_run_id = Some(*run);
                    item.revision += 1;
                }
                let (definitions, resolutions) =
                    plan_successor_revision_obligations_on(connection, &item, patch)?;
                recipe.obligation_definitions.insert(*work, definitions);
                recipe.obligation_resolutions.insert(*work, resolutions);
            }
            _ => {}
        }
    }
    for root in required_roots_on(connection, recipe.new_runs.keys().copied())? {
        recipe
            .new_root_executions
            .insert(root, crate::RootExecutionId::new());
    }
    recipe.validate()
}

fn require_exact_construction(
    connection: &Connection,
    effects: &[Effect],
    selected: &BTreeSet<WorkId>,
    recipe: &PlanningSuccessorRecipe,
) -> Result<(), StoreError> {
    require_successor_roots_on(connection, recipe)?;
    let mut counts = selected
        .iter()
        .map(|&work| (work, 0_usize))
        .collect::<BTreeMap<_, _>>();
    let mut reopened = BTreeSet::new();
    let mut revised = BTreeSet::new();
    let mut blockers = BTreeMap::<WorkId, BTreeSet<String>>::new();
    for effect in effects {
        *counts.entry(effect.work()).or_default() += 1;
        match effect {
            Effect::Reopen(work) => {
                reopened.insert(*work);
            }
            Effect::Revise(work, _) => {
                revised.insert(*work);
            }
            Effect::AddBlocker(work, blocker) => {
                blockers
                    .entry(*work)
                    .or_default()
                    .insert(blocker.blocker_id.clone());
            }
            _ => {}
        }
    }
    let actual = recipe
        .task_events
        .iter()
        .map(|(&work, ids)| (work, ids.len()))
        .collect::<BTreeMap<_, _>>();
    let actual_blockers = recipe
        .blocker_records
        .iter()
        .map(|(&work, ids)| (work, ids.keys().cloned().collect::<BTreeSet<_>>()))
        .collect::<BTreeMap<_, _>>();
    if actual != counts
        || recipe.new_runs.keys().copied().collect::<BTreeSet<_>>() != reopened
        || actual_blockers != blockers
        || recipe
            .obligation_definitions
            .keys()
            .any(|work| !revised.contains(work))
        || recipe
            .obligation_resolutions
            .keys()
            .any(|work| !revised.contains(work))
    {
        return Err(StoreError::InvalidWorkProjection(
            "successor retained construction differs from native effects".into(),
        ));
    }
    Ok(())
}

/// Apply the same operation mapping after the parent admits its real hold and
/// fresh readset. Native owners still decide authority, lifecycle and graph
/// eligibility; this function neither commits nor resolves a hold.
pub(in crate::storage::work) fn apply_successor_effects_on(
    checked: &CheckedPlanningSuccessor<'_, '_, '_>,
    payload: &DeltaCandidatePayload,
) -> Result<(), StoreError> {
    let recipe = checked.recipe();
    let selected = &recipe.request.intent.candidate.changed;
    let effects = derive_on(checked.transaction(), checked.project(), payload, selected)?;
    require_exact_construction(checked.transaction(), &effects, selected, recipe)?;
    let at = recipe
        .chain
        .as_ref()
        .ok_or_else(|| unsupported("an unretained event time"))?
        .recorded_at;
    let actor = &recipe.request.intent.actor;
    let mut offsets = BTreeMap::<WorkId, usize>::new();
    for effect in effects {
        let work = effect.work();
        let offset = offsets.entry(work).or_default();
        let event = &recipe.task_events[&work][*offset];
        *offset += 1;
        let item = load_work_item(checked.transaction(), work)?;
        let key = format!("{}:native:{work:?}:{offset}", recipe.request.intent.key);
        match effect {
            Effect::Reopen(_) => {
                super::super::super::completion::reopen_successor_on(
                    checked,
                    &crate::ReopenWorkRequest {
                        work_id: work,
                        expected_work_revision: item.revision,
                        reason: "resolved task planning starts a new local generation".into(),
                        actor: actor.clone(),
                        idempotency_key: key,
                        reopened_at: at,
                    },
                    event,
                )?;
            }
            Effect::Revise(_, patch) => {
                revise_successor_on(
                    checked,
                    &ReviseWorkRequest {
                        work_id: work,
                        expected_revision: item.revision,
                        patch: *patch,
                        authority: WorkPlanningAuthority::Project,
                        actor: actor.clone(),
                        idempotency_key: key,
                        updated_at: at,
                    },
                    event,
                )?;
            }
            Effect::RemovePrerequisite(_, prerequisite)
            | Effect::AddPrerequisite(_, prerequisite) => {
                let add = matches!(effect, Effect::AddPrerequisite(_, _));
                change_prerequisite_successor_on(
                    checked,
                    &ChangeWorkPrerequisiteRequest {
                        work_id: work,
                        prerequisite_id: prerequisite,
                        expected_revision: item.revision,
                        authority: WorkPlanningAuthority::Project,
                        actor: actor.clone(),
                        idempotency_key: key,
                        changed_at: at,
                    },
                    add,
                    event,
                )?;
            }
            Effect::ClearBlocker(_, blocker_id) => {
                clear_blocker_successor_on(
                    checked,
                    &ClearWorkBlockerRequest {
                        work_id: work,
                        expected_work_revision: item.revision,
                        blocker_id,
                        authority: WorkPlanningAuthority::Project,
                        actor: actor.clone(),
                        idempotency_key: key,
                        cleared_at: at,
                    },
                    event,
                )?;
            }
            Effect::AddBlocker(_, blocker) => {
                add_blocker_successor_on(
                    checked,
                    &AddWorkBlockerRequest {
                        work_id: work,
                        expected_work_revision: item.revision,
                        kind: blocker.kind,
                        detail: blocker.detail.clone(),
                        authority: WorkPlanningAuthority::Project,
                        actor: actor.clone(),
                        idempotency_key: key,
                        blocked_at: at,
                    },
                    &blocker,
                    &recipe.blocker_records[&work][&blocker.blocker_id],
                    event,
                )?;
            }
        }
    }
    Ok(())
}
