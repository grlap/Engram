//! Validates the required-child completion barriers of a `CompletionSeal`
//! (child seals, restored-child completions, waivers, and successor
//! resolutions) and the ancestor / root-execution admission checks that gate
//! whether a work item may execute, reopen, or complete.

use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use super::{
    ChildRequirement, CompletionSeal, FeedId, ObjectId, RequiredChildWaiver, RestoredRecord,
    RootExecution, RootExecutionId, SCHEMA_VERSION, StoreError, WorkClaimState, WorkEvent,
    WorkHandoffState, WorkId, WorkItem, WorkLifecycle, WorkRun, WorkTransition,
    active_root_execution_optional, feed_parts, load_handoff_offer_projection,
    load_typed_work_object, load_work_claim_optional, load_work_item, load_work_run, parse_work_id,
    validate_completion_seal_environment_basis_on, validate_completion_seal_obligation_basis_on,
    validate_stored_seal_root, work_completed_by_restored_record_on,
};

pub(in crate::storage::work) fn validate_completion_seal_children_on(
    connection: &Connection,
    seal: &CompletionSeal,
    depth: usize,
) -> Result<(), StoreError> {
    if depth > 1_024 {
        return Err(StoreError::InvalidWorkProjection(
            "completion-seal child graph exceeds the corruption guard".into(),
        ));
    }
    let mut seen = HashSet::new();
    let mut seen_children = HashSet::new();
    let mut transitively_restored = false;
    for child_hash in &seal.required_child_seals {
        if !seen.insert(child_hash.clone()) {
            return Err(StoreError::InvalidWorkProjection(format!(
                "completion seal for run {} repeats child seal {child_hash}",
                seal.run_id.0
            )));
        }
        let child_seal: CompletionSeal =
            load_typed_work_object(connection, child_hash, "completion_seal")?;
        validate_stored_seal_root(connection, &child_seal, child_hash)?;
        let child = load_work_item(connection, child_seal.work_id)?;
        if !seen_children.insert(child.work_id) {
            return Err(StoreError::InvalidWorkProjection(format!(
                "completion seal for run {} repeats child work {:?}",
                seal.run_id.0, child.work_id
            )));
        }
        if child.parent_id != Some(seal.work_id)
            || child.child_requirement != ChildRequirement::Required
            || child_seal.root_id != seal.root_id
            || child_seal.root_execution_id != seal.root_execution_id
        {
            return Err(StoreError::InvalidWorkProjection(format!(
                "completion seal for run {} cites unrelated child seal {child_hash}",
                seal.run_id.0
            )));
        }
        validate_completion_seal_obligation_basis_on(connection, &child_seal)?;
        validate_completion_seal_environment_basis_on(connection, &child_seal)?;
        validate_completion_seal_children_on(connection, &child_seal, depth + 1)?;
        transitively_restored |= child_seal.restored;
    }
    for record_id in &seal.restored_child_completions {
        if !seen.insert(record_id.clone()) {
            return Err(StoreError::InvalidWorkProjection(format!(
                "completion seal for run {} repeats restored child record {record_id}",
                seal.run_id.0
            )));
        }
        let record: RestoredRecord =
            load_typed_work_object(connection, record_id, "work_restored_record")?;
        let child = load_work_item(connection, record.work_id)?;
        let latest = super::super::query::latest_restored_record_hash(connection, child.work_id)?;
        if latest.as_ref() != Some(record_id)
            || record.history.completion.is_none()
            || !seen_children.insert(child.work_id)
            || child.parent_id != Some(seal.work_id)
            || child.root_id != seal.root_id
            || child.child_requirement != ChildRequirement::Required
            || child.lifecycle != WorkLifecycle::Completed
        {
            return Err(StoreError::InvalidWorkProjection(format!(
                "completion seal for run {} cites unrelated restored child record {record_id}",
                seal.run_id.0
            )));
        }
        transitively_restored = true;
    }
    super::child_resolutions::validate_resolutions_on(connection, seal, &mut seen_children)?;
    if seal.restored != transitively_restored {
        return Err(StoreError::InvalidWorkProjection(format!(
            "completion seal for run {} has an invalid restored marker",
            seal.run_id.0
        )));
    }
    Ok(())
}

pub(super) fn required_restored_child_completions(
    connection: &Connection,
    parent_id: WorkId,
) -> Result<Vec<ObjectId>, StoreError> {
    let child_ids = connection
        .prepare(
            "SELECT child.work_id FROM work_items child
             WHERE child.parent_id = ?1
               AND child.child_requirement = 'required'
               AND child.lifecycle = 'completed'
             ORDER BY child.work_id",
        )?
        .query_map([parent_id.0.to_string()], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut records = Vec::with_capacity(child_ids.len());
    for child_id in child_ids {
        let child_id = parse_work_id(&child_id)?;
        let child = load_work_item(connection, child_id)?;
        if !work_completed_by_restored_record_on(connection, &child)? {
            continue;
        }
        let Some((hash, record)) =
            super::super::query::latest_restored_record(connection, child_id)?
        else {
            return Err(StoreError::InvalidWorkProjection(format!(
                "completed restored child {child_id:?} has no restored completion record"
            )));
        };
        if record.history.completion.is_none() {
            return Err(StoreError::InvalidWorkProjection(format!(
                "completed restored child {child_id:?} has no completion proof"
            )));
        }
        records.push(hash);
    }
    Ok(records)
}

pub(super) fn required_child_seals(
    connection: &Connection,
    parent_id: WorkId,
    root_execution_id: RootExecutionId,
) -> Result<Vec<ObjectId>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT child.work_id, run.run_id, seals.seal_id
         FROM work_items child
         JOIN work_runs run ON run.work_id = child.work_id
         JOIN work_completion_seals seals ON seals.run_id = run.run_id
         WHERE child.parent_id = ?1
           AND run.root_execution_id = ?2
           AND child.child_requirement = 'required'
           AND child.lifecycle = 'completed'
           AND run.state = 'completed'
           AND run.generation = (
               SELECT MAX(latest.generation) FROM work_runs latest
               WHERE latest.work_id = child.work_id
           )
         ORDER BY child.work_id",
    )?;
    let rows = statement
        .query_map(
            params![parent_id.0.to_string(), root_execution_id.0.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let mut hashes = Vec::with_capacity(rows.len());
    for (child_work, child_run, stored_hash) in rows {
        let hash = ObjectId::from_stored(stored_hash.clone())
            .ok_or(StoreError::InvalidStoredKey(stored_hash))?;
        let seal: CompletionSeal = load_typed_work_object(connection, &hash, "completion_seal")?;
        validate_stored_seal_root(connection, &seal, &hash)?;
        if seal.work_id.0.to_string() != child_work
            || seal.run_id.0.to_string() != child_run
            || seal.root_execution_id != root_execution_id
        {
            return Err(StoreError::InvalidWorkProjection(format!(
                "required child seal {hash} does not match its child run"
            )));
        }
        validate_completion_seal_obligation_basis_on(connection, &seal)?;
        validate_completion_seal_environment_basis_on(connection, &seal)?;
        validate_completion_seal_children_on(connection, &seal, 0)?;
        hashes.push(hash);
    }
    Ok(hashes)
}

pub(in crate::storage::work) fn validated_required_child_waivers(
    connection: &Connection,
    parent_id: WorkId,
    execution: &RootExecution,
) -> Result<Vec<RequiredChildWaiver>, StoreError> {
    required_child_waivers_on(connection, parent_id, execution, WaiverValidation::Live)
}

#[derive(Clone, Copy)]
enum WaiverValidation {
    Live,
    Audit,
}

fn required_child_waivers_on(
    connection: &Connection,
    parent_id: WorkId,
    execution: &RootExecution,
    validation: WaiverValidation,
) -> Result<Vec<RequiredChildWaiver>, StoreError> {
    // The root-execution projection is already bound to the latest canonical
    // event. An empty projected waiver set cannot authorize completion, so it
    // is safe to avoid replaying the retained root history here. Doctor still
    // performs the exhaustive comparison below for nonempty projected sets.
    if execution.required_child_waivers.is_empty() {
        return Ok(Vec::new());
    }
    let mut statement = connection.prepare(
        "SELECT object_id FROM work_feed_entries
         WHERE feed_kind = 'root_work' AND feed_id = ?1
           AND object_kind = 'work_event'
         ORDER BY position",
    )?;
    let hashes = statement
        .query_map([execution.root_id.0.to_string()], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);

    let mut events = HashMap::new();
    let mut witnesses = Vec::new();
    for stored_hash in hashes {
        let hash = ObjectId::from_stored(stored_hash.clone())
            .ok_or(StoreError::InvalidStoredKey(stored_hash))?;
        let event: WorkEvent = load_typed_work_object(connection, &hash, "work_event")?;
        let Some(event_execution) = event.root_execution.as_ref() else {
            continue;
        };
        if event_execution.root_execution_id != execution.root_execution_id {
            continue;
        }
        // Live completion needs only this parent's barriers. Exhaustive audit
        // still checks the complete generation, including other parents.
        if matches!(validation, WaiverValidation::Live) && event.work_id != parent_id {
            continue;
        }
        let WorkTransition::RequiredChildWaived {
            child_id,
            child_revision,
            reason,
        } = &event.transition
        else {
            continue;
        };
        let child = load_work_item(connection, *child_id)?;
        let waiver = RequiredChildWaiver {
            work_id: *child_id,
            work_revision: *child_revision,
            waived_by: event.actor.actor_id.clone(),
            reason: reason.clone(),
        };
        let event_contains_exact_waiver = match validation {
            WaiverValidation::Audit => {
                super::super::root_state::resolve(connection, event_execution)?
                    .required_child_waivers
                    .iter()
                    .filter(|candidate| *candidate == &waiver)
                    .count()
                    == 1
            }
            WaiverValidation::Live => {
                witnesses.push((event_execution.clone(), waiver.clone()));
                true // The shared fact/ancestry proof below must succeed.
            }
        };
        let valid = event.schema_version == SCHEMA_VERSION
            && event.project_id == execution.project_id
            && event.root_id == execution.root_id
            && event.work_id == child.parent_id.unwrap_or(event.work_id)
            && child.parent_id == Some(event.work_id)
            && child.root_id == execution.root_id
            && child.child_requirement == ChildRequirement::Required
            && matches!(
                child.lifecycle,
                WorkLifecycle::Cancelled | WorkLifecycle::Superseded
            )
            && child.revision == *child_revision
            && event_contains_exact_waiver;
        if !valid || events.insert(*child_id, waiver).is_some() {
            return Err(StoreError::InvalidWorkProjection(format!(
                "required-child waiver event {hash} is not uniquely bound"
            )));
        }
    }

    let mut projected = HashMap::new();
    for waiver in &execution.required_child_waivers {
        if matches!(validation, WaiverValidation::Live)
            && load_work_item(connection, waiver.work_id)?.parent_id != Some(parent_id)
        {
            continue;
        }
        if projected.insert(waiver.work_id, waiver.clone()).is_some() {
            return Err(StoreError::InvalidWorkProjection(format!(
                "root execution {:?} duplicates a required-child waiver for {:?}",
                execution.root_execution_id, waiver.work_id
            )));
        }
    }
    if projected != events {
        return Err(StoreError::InvalidWorkProjection(format!(
            "root execution {:?} required-child waivers do not match canonical events",
            execution.root_execution_id
        )));
    }
    if matches!(validation, WaiverValidation::Live) {
        super::super::root_state::verify_waiver_witnesses(connection, execution, &witnesses)?;
    }

    let mut direct = Vec::new();
    for waiver in projected.into_values() {
        let child = load_work_item(connection, waiver.work_id)?;
        if child.parent_id == Some(parent_id) {
            direct.push(waiver);
        }
    }
    direct.sort_by(super::super::root_state::compare_child_waivers);
    Ok(direct)
}

pub(super) fn verify_required_child_waiver_bindings(
    connection: &Connection,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let mut statement = connection.prepare(
        "SELECT root_execution_id
         FROM work_root_executions ORDER BY root_execution_id",
    )?;
    let rows = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    for root_execution_id in rows {
        *checked += 1;
        let valid = super::super::query::parse_root_execution_id(&root_execution_id)
            .and_then(|id| super::super::root_state::projected(connection, id))
            .is_ok_and(|(execution, _)| {
                required_child_waivers_on(
                    connection,
                    execution.root_id,
                    &execution,
                    WaiverValidation::Audit,
                )
                .is_ok()
            });
        if !valid {
            invalid.push(format!(
                "work_root_execution:{root_execution_id}:invalid_required_child_waivers"
            ));
        }
    }
    Ok(())
}

pub(super) fn unfinished_optional_children(
    connection: &Connection,
    parent_id: WorkId,
) -> Result<Vec<WorkId>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT work_id FROM work_items
         WHERE parent_id = ?1 AND child_requirement = 'optional'
           AND lifecycle != 'completed'
         ORDER BY work_id",
    )?;
    statement
        .query_map([parent_id.0.to_string()], |row| row.get::<_, String>(0))?
        .map(|row| parse_work_id(&row?))
        .collect()
}

pub(super) fn live_descendant_execution_authority(
    connection: &Connection,
    root_id: WorkId,
    now: DateTime<Utc>,
) -> Result<bool, StoreError> {
    let descendant_ids = {
        let mut statement = connection.prepare(
            "WITH RECURSIVE descendants(work_id) AS (
                 SELECT work_id FROM work_items WHERE parent_id = ?1
                 UNION
                 SELECT child.work_id FROM work_items child
                 JOIN descendants parent ON child.parent_id = parent.work_id
             )
             SELECT work_id FROM descendants ORDER BY work_id",
        )?;
        statement
            .query_map([root_id.0.to_string()], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?
    };
    for stored_id in descendant_ids {
        let item = load_work_item(connection, parse_work_id(&stored_id)?)?;
        let Some(run_id) = item.active_run_id else {
            continue;
        };
        let run = load_work_run(connection, run_id)?;
        if run.work_id != item.work_id {
            return Err(StoreError::InvalidWorkProjection(format!(
                "active run {run_id:?} belongs to a different work item"
            )));
        }
        if load_work_claim_optional(connection, run_id)?
            .is_some_and(|claim| claim.state == WorkClaimState::Active && claim.expires_at > now)
        {
            return Ok(true);
        }
        let offers = {
            let mut statement = connection.prepare(
                "SELECT offer_object_id, offer_json FROM work_handoff_offers
                 WHERE run_id = ?1 ORDER BY offer_id",
            )?;
            statement
                .query_map([run_id.0.to_string()], |row| {
                    Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Vec<u8>>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        for row in offers {
            let offer = load_handoff_offer_projection(connection, row)?;
            if offer.state == WorkHandoffState::Offered && offer.expires_at > now {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

pub(in crate::storage::work) fn ancestors_admit_execution(
    connection: &Connection,
    item: &WorkItem,
) -> Result<bool, StoreError> {
    let mut parent_id = item.parent_id;
    let mut visited = HashSet::new();
    let mut reached_root = item.work_id == item.root_id;
    while let Some(parent) = parent_id {
        if !visited.insert(parent) || visited.len() > 1_024 {
            return Err(StoreError::InvalidWorkProjection(
                "work hierarchy is cyclic or exceeds the corruption guard".into(),
            ));
        }
        let ancestor = load_work_item(connection, parent)?;
        if ancestor.project_id != item.project_id || ancestor.root_id != item.root_id {
            return Err(StoreError::InvalidWorkProjection(format!(
                "work ancestor {:?} crosses its project or root boundary",
                ancestor.work_id
            )));
        }
        if ancestor.lifecycle != WorkLifecycle::Open {
            return Ok(false);
        }
        reached_root |= ancestor.work_id == item.root_id;
        parent_id = ancestor.parent_id;
    }
    if !reached_root {
        return Err(StoreError::InvalidWorkProjection(format!(
            "work {:?} does not reach its declared root {:?}",
            item.work_id, item.root_id
        )));
    }
    Ok(true)
}

pub(in crate::storage::work) fn work_run_uses_active_root_execution(
    connection: &Connection,
    item: &WorkItem,
) -> Result<bool, StoreError> {
    let run_id = item.active_run_id.ok_or_else(|| {
        StoreError::InvalidWorkProjection(format!("open work {:?} has no active run", item.work_id))
    })?;
    let run = load_work_run(connection, run_id)?;
    run_uses_active_root_execution(connection, item, &run)
}

pub(in crate::storage::work) fn run_uses_active_root_execution(
    connection: &Connection,
    item: &WorkItem,
    run: &WorkRun,
) -> Result<bool, StoreError> {
    if run.work_id != item.work_id {
        return Err(StoreError::InvalidWorkProjection(format!(
            "run {:?} does not belong to work {:?}",
            run.run_id, item.work_id
        )));
    }
    let Some(execution) = active_root_execution_optional(connection, item.root_id)? else {
        return Ok(false);
    };
    if execution.project_id != item.project_id || execution.root_id != item.root_id {
        return Err(StoreError::InvalidWorkProjection(format!(
            "root execution {:?} crosses the work project or root boundary",
            execution.root_execution_id
        )));
    }
    Ok(execution.root_execution_id == run.root_execution_id)
}

pub(in crate::storage::work) fn feed_head(
    connection: &Connection,
    feed: &FeedId,
) -> Result<i64, StoreError> {
    let (feed_kind, feed_id) = feed_parts(feed);
    Ok(connection
        .query_row(
            "SELECT position FROM work_feed_heads WHERE feed_kind = ?1 AND feed_id = ?2",
            params![feed_kind, feed_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0))
}

pub(super) fn refuse_completed_ancestor(
    connection: &Connection,
    item: &WorkItem,
) -> Result<(), StoreError> {
    let mut parent_id = item.parent_id;
    let mut depth = 0_u16;
    while let Some(parent) = parent_id {
        depth += 1;
        if depth > 1_024 {
            return Err(StoreError::InvalidWorkProjection(
                "work hierarchy depth exceeds the corruption guard".into(),
            ));
        }
        let ancestor = load_work_item(connection, parent)?;
        if ancestor.lifecycle == WorkLifecycle::Completed {
            return Err(StoreError::InvalidWork(format!(
                "cannot reopen child work while completed ancestor {:?} consumes its seal",
                ancestor.work_id
            )));
        }
        parent_id = ancestor.parent_id;
    }
    Ok(())
}

pub(in crate::storage::work) fn work_is_ancestor_of(
    connection: &Connection,
    candidate: WorkId,
    descendant: &WorkItem,
) -> Result<bool, StoreError> {
    let mut parent_id = descendant.parent_id;
    let mut depth = 0_u16;
    while let Some(parent) = parent_id {
        depth += 1;
        if depth > 1_024 {
            return Err(StoreError::InvalidWorkProjection(
                "work hierarchy depth exceeds the corruption guard".into(),
            ));
        }
        let ancestor = load_work_item(connection, parent)?;
        if ancestor.project_id != descendant.project_id || ancestor.root_id != descendant.root_id {
            return Err(StoreError::InvalidWorkProjection(format!(
                "work ancestor {:?} crosses its project or root boundary",
                ancestor.work_id
            )));
        }
        if ancestor.work_id == candidate {
            return Ok(true);
        }
        parent_id = ancestor.parent_id;
    }
    Ok(false)
}
