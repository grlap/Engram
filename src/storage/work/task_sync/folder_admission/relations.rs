//! Complete-folder relation admission and ordered transaction phases.

use super::{
    ActorContext, BTreeMap, Conflict, DeltaPlan, HashMap, HashSet, HeldReads, Prospective,
    StoreError, TaskDelta, WorkId, WorkLifecycle, count_retained_validation, dependency_cycles,
    locate_fault,
};
use crate::task_sync::folder_plan::RelationChange;

fn refusal(id: WorkId, reason: &'static str, detail: String) -> Conflict {
    Conflict { id, reason, detail }
}

fn eligibility(
    transaction: &rusqlite::Transaction<'_>,
    reads: &mut HeldReads,
    id: WorkId,
) -> Result<Option<(&'static str, String)>, StoreError> {
    if let Some(found) = reads.relation_eligibility.get(&id) {
        return Ok(found.clone());
    }
    let mut found = reads.eligibility(transaction, id)?;
    if found.is_none() {
        let prior: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM work_runs WHERE work_id = ?1)",
            [id.0.to_string()],
            |row| row.get(0),
        )?;
        if prior {
            found = Some(("execution_history", "a persisted run exists".into()));
        }
    }
    reads.relation_eligibility.insert(id, found.clone());
    Ok(found)
}

pub(super) fn admit_owner(
    transaction: &rusqlite::Transaction<'_>,
    prospective: &Prospective<'_>,
    reads: &mut HeldReads,
    id: WorkId,
    change: &RelationChange,
) -> Result<Option<Conflict>, StoreError> {
    if let Some(blocker) = change.blocker_rewrites.first() {
        return Ok(Some(refusal(
            id,
            "immutable_blocker",
            format!("blocker {blocker} has different immutable content"),
        )));
    }
    for blocker in &change.blocker_additions {
        let held: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM work_blockers WHERE blocker_id = ?1)",
            [&blocker.blocker_id],
            |row| row.get(0),
        )?;
        if held {
            return Ok(Some(refusal(
                id,
                "id_collision",
                format!(
                    "blocker {} is already held, including cleared identities",
                    blocker.blocker_id
                ),
            )));
        }
    }
    if let Some((reason, detail)) = eligibility(transaction, reads, id)? {
        return Ok(Some(refusal(id, reason, detail)));
    }
    let task = prospective.resolved_task(id)?;
    if !change.prerequisite_additions.is_empty()
        && task.prerequisites.len() > super::super::super::MAX_WORK_PREREQUISITES_PER_ITEM
    {
        return Ok(Some(refusal(
            id,
            "prerequisite_bound",
            "the resulting prerequisite set exceeds the per-item bound".into(),
        )));
    }
    for target in &change.prerequisite_additions {
        let endpoint = prospective.resolved_task(*target)?;
        if endpoint.lifecycle != WorkLifecycle::Open {
            let reason = if endpoint.lifecycle == WorkLifecycle::Completed {
                "already_satisfied"
            } else {
                "prerequisite_lifecycle"
            };
            return Ok(Some(refusal(
                id,
                reason,
                format!("prerequisite {} is {:?}", target.0, endpoint.lifecycle),
            )));
        }
        if *target == id || prospective.ancestors(id).contains(target) {
            return Ok(Some(refusal(
                id,
                "dependency_cycle",
                format!("prerequisite {} is the owner or its ancestor", target.0),
            )));
        }
    }
    Ok(None)
}

pub(super) fn ancestor_binding(
    transaction: &rusqlite::Transaction<'_>,
    prospective: &Prospective<'_>,
    reads: &mut HeldReads,
    id: WorkId,
) -> Result<Option<Conflict>, StoreError> {
    for ancestor in prospective.ancestors(id) {
        if let Some((reason, detail)) = eligibility(transaction, reads, ancestor)? {
            return Ok(Some(refusal(
                id,
                "ancestor_binding",
                format!("ancestor {}: {reason} ({detail})", ancestor.0),
            )));
        }
    }
    let root = prospective.root(id)?;
    if let Some(execution) = reads.execution(transaction, root)? {
        return Ok(Some(refusal(
            id,
            "ancestor_binding",
            format!("root {} has active root execution {execution}", root.0),
        )));
    }
    Ok(None)
}

/// Patches the mutable relation view only; imported origin documents and
/// historical retired owners keep their original relations.
pub(super) fn graphs(
    transaction: &rusqlite::Transaction<'_>,
    plan: &DeltaPlan,
    prospective: &Prospective<'_>,
    conflicts: &mut BTreeMap<uuid::Uuid, Conflict>,
    batch: &mut Vec<(&'static str, String)>,
) -> Result<(), StoreError> {
    let changed = plan
        .relation_changes()
        .map(|(id, _)| id)
        .collect::<HashSet<_>>();
    if changed.is_empty() {
        return Ok(());
    }
    let removed = plan
        .tasks
        .iter()
        .filter_map(|task| matches!(task.delta, TaskDelta::Deleted).then_some(task.id))
        .collect::<HashSet<_>>();
    let mut exported = prospective
        .current
        .iter()
        .filter(|(id, _)| !removed.contains(id))
        .map(|(&id, &task)| (id, task.clone()))
        .collect::<HashMap<_, _>>();
    exported.extend(
        prospective
            .resolved
            .iter()
            .map(|(&id, &task)| (id, task.clone())),
    );
    let all = super::super::export_on(transaction, prospective.project)?;
    let mut durable = all
        .tasks
        .into_iter()
        .map(|task| (task.id, task))
        .collect::<HashMap<_, _>>();
    for (&id, &task) in &prospective.resolved {
        if let Some(old) = durable.get_mut(&id) {
            if changed.contains(&id) {
                old.prerequisites.clone_from(&task.prerequisites);
            }
        } else {
            durable.insert(id, task.clone());
        }
    }
    let candidates = changed
        .iter()
        .copied()
        .chain(plan.additions())
        .collect::<HashSet<_>>();
    if let Some(fault) = locate_fault(
        prospective.project,
        prospective.notes_included,
        exported.into_values().collect(),
        &prospective.roots,
        count_retained_validation,
    )? {
        let mut named = false;
        if let Some(located) = &fault.located {
            for task in &plan.tasks {
                if candidates.contains(&task.id) && located.groups.contains_key(&task.id) {
                    named = true;
                    conflicts.entry(task.id.0).or_insert_with(|| {
                        refusal(task.id, "retained_graph", fault.diagnostic.clone())
                    });
                }
            }
        }
        if !named {
            batch.push(("retained_graph", fault.diagnostic));
        }
    }
    let cycles = dependency_cycles(&durable.into_values().collect::<Vec<_>>());
    if !cycles.is_empty() {
        let mut named = false;
        for task in &plan.tasks {
            if candidates.contains(&task.id) && cycles.contains_key(&task.id) {
                named = true;
                conflicts.entry(task.id.0).or_insert_with(|| {
                    refusal(
                        task.id,
                        "durable_graph",
                        "the final retained dependency graph contains a cycle".into(),
                    )
                });
            }
        }
        if !named {
            batch.push((
                "durable_graph",
                "the final retained dependency graph contains a cycle".into(),
            ));
        }
    }
    Ok(())
}

pub(super) fn apply_removals(
    transaction: &rusqlite::Transaction<'_>,
    plan: &DeltaPlan,
    actor: &ActorContext,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(), StoreError> {
    for (id, change) in plan.relation_changes() {
        for &target in &change.prerequisite_removals {
            super::super::super::planning::apply_folder_prerequisite_on(
                transaction,
                id,
                target,
                false,
                actor,
                now,
            )?;
        }
        for blocker in &change.blocker_removals {
            super::super::super::planning::apply_folder_blocker_on(
                transaction,
                id,
                blocker,
                None,
                actor,
                now,
            )?;
        }
    }
    Ok(())
}

pub(super) fn apply_additions(
    transaction: &rusqlite::Transaction<'_>,
    plan: &DeltaPlan,
    actor: &ActorContext,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(), StoreError> {
    for (id, change) in plan.relation_changes() {
        for &target in &change.prerequisite_additions {
            super::super::super::planning::apply_folder_prerequisite_on(
                transaction,
                id,
                target,
                true,
                actor,
                now,
            )?;
        }
        for blocker in &change.blocker_additions {
            super::super::super::planning::apply_folder_blocker_on(
                transaction,
                id,
                &blocker.blocker_id,
                Some(blocker),
                actor,
                now,
            )?;
        }
    }
    Ok(())
}
