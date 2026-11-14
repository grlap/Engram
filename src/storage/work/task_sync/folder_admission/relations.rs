//! Complete-folder relation admission and ordered transaction phases.

use super::{
    ActorContext, BTreeMap, Conflict, DeltaPlan, HashMap, HashSet, HeldReads, Prospective,
    StoreError, WorkId, WorkLifecycle, dependency_cycles,
};
use crate::task_sync::folder_plan::RelationChange;

fn refusal(id: WorkId, reason: &'static str, detail: String) -> Conflict {
    Conflict { id, reason, detail }
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
    if let Some((reason, detail)) = reads.eligibility(transaction, id)? {
        return Ok(Some(refusal(id, reason, detail)));
    }
    if prospective.current[&id].lifecycle != WorkLifecycle::Open
        && (!change.prerequisite_additions.is_empty()
            || !change.prerequisite_removals.is_empty()
            || !change.blocker_additions.is_empty()
            || !change.blocker_removals.is_empty())
    {
        return Ok(Some(refusal(
            id,
            "lifecycle",
            "a relation owner must be Open".into(),
        )));
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
        if let Some((reason, detail)) = reads.eligibility(transaction, ancestor)? {
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
    plan: &DeltaPlan,
    prospective: &Prospective<'_>,
    conflicts: &mut BTreeMap<uuid::Uuid, Conflict>,
    batch: &mut Vec<(&'static str, String)>,
    retained: &HashMap<WorkId, super::TaskDocument>,
) -> Result<(), StoreError> {
    let changed = plan
        .relation_changes()
        .map(|(id, _)| id)
        .collect::<HashSet<_>>();
    if changed.is_empty() {
        return Ok(());
    }
    // Retired owners remain in the durable graph. Apply only the currently
    // selected edits; a refused owner's historical edges must stay intact.
    let mut durable = retained.clone();
    for (id, _) in plan.relation_changes() {
        let task = prospective.resolved_task(id)?;
        if let Some(old) = durable.get_mut(&id) {
            old.prerequisites.clone_from(&task.prerequisites);
        }
    }
    for id in plan.additions() {
        durable.insert(id, prospective.resolved_task(id)?.clone());
    }
    let candidates = changed
        .iter()
        .copied()
        .chain(plan.additions())
        .collect::<HashSet<_>>();
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
