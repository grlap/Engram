//! Same-snapshot activity projection; no notice is restored into runtime state.

use super::{
    Connection, ImportOrigin, ORIGIN_KIND, ObjectId, SqliteStore, StoreError, TaskTree, WorkItem,
    invalid, origin,
};
use crate::task_sync::{
    SharedActor,
    activity::{ActivityMap, ActivityNotice, ActivityState, exclusion},
    notes::NoteOmissions,
};
use crate::{WorkClaim, WorkEvent, WorkTransition};

pub(super) fn capture(
    connection: &Connection,
    item: &WorkItem,
    records: &[(ObjectId, WorkEvent)],
    machine: Option<&str>,
    comparison: bool,
    omissions: &mut NoteOmissions,
) -> Result<ActivityMap, StoreError> {
    let mut activity = origin::for_item(connection, item)?
        .map_or_else(ActivityMap::new, |origin| origin.task.activity);
    if !comparison {
        if let Some(machine) = machine {
            for (id, notice) in native(item, records, machine)? {
                let entries = activity.entry(machine.into()).or_default();
                if let Some(retained) = entries.get(&id)
                    && retained != &notice
                {
                    return Err(invalid(
                        "native activity conflicts with retained origin entry",
                    ));
                }
                entries.insert(id, notice);
            }
        }
        for entries in activity.values_mut() {
            entries.retain(|_, notice| {
                if let Some(reason) = exclusion(notice, &item.labels) {
                    omissions.omit(reason);
                    false
                } else {
                    true
                }
            });
        }
        activity.retain(|_, entries| !entries.is_empty());
    }
    Ok(activity)
}

fn native(
    item: &WorkItem,
    records: &[(ObjectId, WorkEvent)],
    machine: &str,
) -> Result<std::collections::BTreeMap<ObjectId, ActivityNotice>, StoreError> {
    let mut entries = std::collections::BTreeMap::new();
    let mut current: Option<(ObjectId, WorkClaim)> = None;
    for (id, event) in records {
        if event.project_id != item.project_id || event.work_id != item.work_id {
            return Err(invalid("activity event belongs to another task"));
        }
        match &event.transition {
            WorkTransition::Claimed { claim, .. } => {
                start(&mut entries, &mut current, id, event, claim, machine)?;
            }
            WorkTransition::HandedOff { from, claim_id, .. } => {
                if let Some((activity_id, claim)) = &current
                    && &claim.holder == from
                    && &claim.claim_id == claim_id
                    && Some(claim.run_id) == event.run_id
                {
                    close(&mut entries, activity_id, id, ActivityState::Released)?;
                }
                let claim = event
                    .claim
                    .as_ref()
                    .ok_or_else(|| invalid("activity handoff lacks claim"))?;
                start(&mut entries, &mut current, id, event, claim, machine)?;
            }
            WorkTransition::Released { claim_id, .. } => {
                if let Some((activity_id, claim)) = &current
                    && &claim.claim_id == claim_id
                    && Some(claim.run_id) == event.run_id
                {
                    close(&mut entries, activity_id, id, ActivityState::Released)?;
                    current = None;
                }
            }
            WorkTransition::Completed { .. } | WorkTransition::Disposed { .. } => {
                if let Some((activity_id, claim)) = &current
                    && Some(claim.run_id) == event.run_id
                {
                    let state = if matches!(event.transition, WorkTransition::Completed { .. }) {
                        ActivityState::Finished
                    } else {
                        ActivityState::Abandoned
                    };
                    close(&mut entries, activity_id, id, state)?;
                    current = None;
                }
            }
            // Renewal, inferred expiry, notes and checkpoints carry no shared change.
            WorkTransition::Created { .. }
            | WorkTransition::Decomposed { .. }
            | WorkTransition::Revised { .. }
            | WorkTransition::PrerequisiteAdded { .. }
            | WorkTransition::PrerequisiteRemoved { .. }
            | WorkTransition::Blocked { .. }
            | WorkTransition::Unblocked { .. }
            | WorkTransition::ClaimRenewed { .. }
            | WorkTransition::Checkpointed { .. }
            | WorkTransition::HandoffOffered { .. }
            | WorkTransition::HandoffExpired { .. }
            | WorkTransition::HandoffCancelled { .. }
            | WorkTransition::EvidenceAdded { .. }
            | WorkTransition::MemoryCaptured { .. }
            | WorkTransition::TypedEvidenceAdded { .. }
            | WorkTransition::RequiredChildWaived { .. }
            | WorkTransition::Reopened { .. } => {}
        }
    }
    Ok(entries)
}

fn start(
    entries: &mut std::collections::BTreeMap<ObjectId, ActivityNotice>,
    current: &mut Option<(ObjectId, WorkClaim)>,
    id: &ObjectId,
    event: &WorkEvent,
    claim: &WorkClaim,
    machine: &str,
) -> Result<(), StoreError> {
    if claim.work_id != event.work_id || Some(claim.run_id) != event.run_id {
        return Err(invalid("activity start claim binding differs"));
    }
    let notice = ActivityNotice {
        activity_id: id.clone(),
        task_id: event.work_id,
        origin_machine: machine.into(),
        actor: SharedActor::from_actor(&event.actor),
        state: ActivityState::Started,
        state_revision: 1,
        event_id: id.clone(),
    };
    if entries.insert(id.clone(), notice).is_some() {
        return Err(invalid("duplicate activity start event"));
    }
    // Recovery records a new start, never an inferred end to the old episode.
    *current = Some((id.clone(), claim.clone()));
    Ok(())
}

fn close(
    entries: &mut std::collections::BTreeMap<ObjectId, ActivityNotice>,
    activity_id: &ObjectId,
    event_id: &ObjectId,
    state: ActivityState,
) -> Result<(), StoreError> {
    let notice = entries
        .get_mut(activity_id)
        .ok_or_else(|| invalid("activity start missing"))?;
    notice.state = state;
    notice.state_revision += 1;
    notice.event_id.clone_from(event_id);
    Ok(())
}

/// Original ids cannot silently move across projects/tasks or change retained bodies.
pub(super) fn refuse_known_changes(
    connection: &Connection,
    tree: &TaskTree,
) -> Result<(), StoreError> {
    let incoming = tree
        .tasks
        .iter()
        .flat_map(|task| task.activity.iter())
        .flat_map(|(machine, entries)| {
            entries
                .iter()
                .map(move |(id, notice)| ((machine, id), notice))
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    if incoming.is_empty() {
        return Ok(());
    }
    let ids = connection
        .prepare("SELECT object_id FROM objects WHERE object_kind = ?1")?
        .query_map([ORIGIN_KIND], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for id in ids {
        let id = ObjectId::from_stored(id).ok_or_else(|| invalid("invalid import origin id"))?;
        let origin: ImportOrigin =
            SqliteStore::get_canonical_object_on(connection, &id, ORIGIN_KIND)?
                .ok_or_else(|| invalid("task import origin missing"))?
                .decode()?;
        for (machine, entries) in &origin.task.activity {
            for (id, notice) in entries {
                if let Some(candidate) = incoming.get(&(machine, id))
                    && (origin.project_id != tree.project.project_id || *candidate != notice)
                {
                    return Err(invalid(
                        "known activity differs from its project or shared fields",
                    ));
                }
            }
        }
    }
    Ok(())
}
