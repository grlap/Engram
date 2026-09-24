//! Prerequisite edge loading, classification into readiness state, and the
//! bounded catalog page used to show prerequisites for one work item.

use super::super::WorkPrerequisitePage;
use super::{
    Connection, ObjectId, StoreError, WorkId, WorkItem, WorkLifecycle, WorkPrerequisiteState,
    WorkTransition, load_work_item, native_work_event_optional, parse_work_id,
    require_work_item_relation_integrity, restored_record_binds_work,
};

pub(in crate::storage::work) fn classified_prerequisite_projections(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Vec<(WorkId, WorkPrerequisiteState)>, StoreError> {
    let prerequisite_ids = load_prerequisite_projection_ids(connection, work_id)?;
    let mut classified = Vec::with_capacity(prerequisite_ids.len());
    for prerequisite_id in prerequisite_ids {
        let prerequisite = load_work_item(connection, prerequisite_id)?;
        classified.push((
            prerequisite_id,
            work_prerequisite_state(connection, &prerequisite)?,
        ));
    }
    Ok(classified)
}

pub(in crate::storage::work) fn incomplete_prerequisite_projections(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Vec<WorkId>, StoreError> {
    Ok(classified_prerequisite_projections(connection, work_id)?
        .into_iter()
        .filter_map(|(work_id, state)| {
            (state != WorkPrerequisiteState::Satisfied).then_some(work_id)
        })
        .collect())
}

pub(in crate::storage) fn load_prerequisite_projection_ids(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Vec<WorkId>, StoreError> {
    let prerequisite_ids = {
        let mut statement = connection.prepare(
            "SELECT prerequisite_id, event_id FROM work_prerequisites
             WHERE work_id = ?1 ORDER BY prerequisite_id",
        )?;
        statement
            .query_map([work_id.0.to_string()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .map(|row| {
                let (prerequisite_id, event_id) = row?;
                Ok((
                    parse_work_id(&prerequisite_id)?,
                    ObjectId::from_stored(event_id.clone())
                        .ok_or(StoreError::InvalidStoredKey(event_id))?,
                ))
            })
            .collect::<Result<Vec<_>, StoreError>>()?
    };
    let mut bound = Vec::with_capacity(prerequisite_ids.len());
    for (prerequisite_id, event_id) in prerequisite_ids {
        let event_binds_edge =
            native_work_event_optional(connection, &event_id)?.is_some_and(|event| {
                event.work_id == work_id
                    && match &event.transition {
                        WorkTransition::Created { prerequisites, .. } => {
                            prerequisites.contains(&prerequisite_id)
                        }
                        WorkTransition::PrerequisiteAdded {
                            prerequisite_id: added,
                            ..
                        } => *added == prerequisite_id,
                        _ => false,
                    }
            }) || restored_record_binds_work(connection, &event_id, work_id)?;
        if !event_binds_edge {
            return Err(StoreError::InvalidWorkProjection(format!(
                "prerequisite edge {work_id:?}->{prerequisite_id:?} differs from its event binding"
            )));
        }
        bound.push(prerequisite_id);
    }
    Ok(bound)
}

pub(in crate::storage::work) fn bounded_prerequisite_projection_rows(
    connection: &Connection,
    work_id: WorkId,
    limit: usize,
) -> Result<WorkPrerequisitePage, StoreError> {
    let mut statement = connection.prepare(
        "SELECT edge.prerequisite_id, prerequisite.short_ref,
                prerequisite.lifecycle, replacement.lifecycle
         FROM work_prerequisites edge
         LEFT JOIN work_items prerequisite
           ON prerequisite.work_id = edge.prerequisite_id
         LEFT JOIN work_items replacement
           ON replacement.work_id = prerequisite.superseded_by
         WHERE edge.work_id = ?1",
    )?;
    let mut classified = statement
        .query_map([work_id.0.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?
        .map(|row| {
            let (stored_id, short_ref, lifecycle, replacement_lifecycle) = row?;
            let prerequisite_id = parse_work_id(&stored_id)?;
            let short_ref = short_ref.ok_or_else(|| {
                StoreError::InvalidWorkProjection(format!(
                    "prerequisite {prerequisite_id:?} is missing its catalog row"
                ))
            })?;
            let lifecycle = lifecycle.ok_or_else(|| {
                StoreError::InvalidWorkProjection(format!(
                    "prerequisite {prerequisite_id:?} is missing"
                ))
            })?;
            let state = projected_prerequisite_state(
                &lifecycle,
                replacement_lifecycle.as_deref(),
                prerequisite_id,
            )?;
            Ok((prerequisite_id, short_ref, state))
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    drop(statement);

    classified.sort_by(|left, right| {
        prerequisite_state_rank(left.2)
            .cmp(&prerequisite_state_rank(right.2))
            .then_with(|| left.1.cmp(&right.1))
    });
    let mut totals = [0_usize; 3];
    for (_, _, state) in &classified {
        totals[prerequisite_state_rank(*state)] += 1;
    }
    let selected = classified.into_iter().take(limit).collect::<Vec<_>>();
    let mut selected_counts = [0_usize; 3];
    let mut items = Vec::with_capacity(selected.len());
    for (prerequisite_id, _, state) in selected {
        selected_counts[prerequisite_state_rank(state)] += 1;
        items.push((load_work_item(connection, prerequisite_id)?, state));
    }
    Ok(WorkPrerequisitePage {
        items,
        omitted_by_state: std::array::from_fn(|index| totals[index] - selected_counts[index]),
    })
}

const fn prerequisite_state_rank(state: WorkPrerequisiteState) -> usize {
    match state {
        WorkPrerequisiteState::Dead => 0,
        WorkPrerequisiteState::Pending => 1,
        WorkPrerequisiteState::Satisfied => 2,
    }
}

fn work_prerequisite_state(
    connection: &Connection,
    prerequisite: &WorkItem,
) -> Result<WorkPrerequisiteState, StoreError> {
    let replacement_lifecycle = prerequisite
        .superseded_by
        .map(|replacement| load_work_item(connection, replacement))
        .transpose()?
        .map(|replacement| replacement.lifecycle);
    classify_prerequisite_state(
        prerequisite.lifecycle,
        replacement_lifecycle,
        prerequisite.work_id,
    )
}

pub(super) fn classify_prerequisite_state(
    lifecycle: WorkLifecycle,
    replacement_lifecycle: Option<WorkLifecycle>,
    prerequisite_id: WorkId,
) -> Result<WorkPrerequisiteState, StoreError> {
    match lifecycle {
        WorkLifecycle::Completed => Ok(WorkPrerequisiteState::Satisfied),
        WorkLifecycle::Cancelled => Ok(WorkPrerequisiteState::Dead),
        WorkLifecycle::Open | WorkLifecycle::Proposed => Ok(WorkPrerequisiteState::Pending),
        WorkLifecycle::Superseded => match replacement_lifecycle {
            Some(WorkLifecycle::Completed) => Ok(WorkPrerequisiteState::Satisfied),
            Some(WorkLifecycle::Open | WorkLifecycle::Proposed) => {
                Ok(WorkPrerequisiteState::Pending)
            }
            Some(WorkLifecycle::Cancelled | WorkLifecycle::Superseded) => {
                Ok(WorkPrerequisiteState::Dead)
            }
            None => Err(StoreError::InvalidWorkProjection(format!(
                "superseded prerequisite {prerequisite_id:?} has no replacement projection"
            ))),
        },
    }
}

pub(super) fn projected_prerequisite_state(
    lifecycle: &str,
    replacement_lifecycle: Option<&str>,
    prerequisite_id: WorkId,
) -> Result<WorkPrerequisiteState, StoreError> {
    let lifecycle = match lifecycle {
        "proposed" => WorkLifecycle::Proposed,
        "open" => WorkLifecycle::Open,
        "completed" => WorkLifecycle::Completed,
        "cancelled" => WorkLifecycle::Cancelled,
        "superseded" => WorkLifecycle::Superseded,
        value => {
            return Err(StoreError::InvalidWorkProjection(format!(
                "prerequisite {prerequisite_id:?} has unknown lifecycle {value:?}"
            )));
        }
    };
    let replacement_lifecycle = replacement_lifecycle
        .map(|value| match value {
            "proposed" => Ok(WorkLifecycle::Proposed),
            "open" => Ok(WorkLifecycle::Open),
            "completed" => Ok(WorkLifecycle::Completed),
            "cancelled" => Ok(WorkLifecycle::Cancelled),
            "superseded" => Ok(WorkLifecycle::Superseded),
            value => Err(StoreError::InvalidWorkProjection(format!(
                "prerequisite {prerequisite_id:?} has replacement with unknown lifecycle {value:?}"
            ))),
        })
        .transpose()?;
    classify_prerequisite_state(lifecycle, replacement_lifecycle, prerequisite_id)
}

pub(super) fn incomplete_prerequisites(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Vec<WorkId>, StoreError> {
    require_work_item_relation_integrity(connection, work_id)?;
    incomplete_prerequisite_projections(connection, work_id)
}
