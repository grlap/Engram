//! Prerequisite-edge transitions and the work-relation basis/fingerprint
//! integrity checks that admit and replay them.

use super::super::WorkRelationBlockerBasis;
use super::super::query::{load_active_blocker_projections, load_prerequisite_projection_ids};
use super::{
    CanonicalObject, ChangeWorkPrerequisiteRequest, Connection, ObjectId, OptionalExtension,
    PlanRelationBasis, PlanningValidation, SCHEMA_VERSION, StoreError, Transaction, WorkBlocker,
    WorkEventDraft, WorkId, WorkItem, WorkLifecycle, WorkRelationBasis, WorkTransition,
    active_run_snapshot, append_work_event, assert_revision, load_work_item, params,
    persist_work_item, rebase_planning_claim, validate_planning_authority, work_is_ancestor_of,
};

/// Changes one edge using the ordinary checks within a caller-owned transaction.
#[allow(
    clippy::too_many_lines,
    reason = "shared prerequisite transition keeps admission and projection writes together"
)]
pub(super) fn change_work_prerequisite_on(
    transaction: &Transaction<'_>,
    request: &ChangeWorkPrerequisiteRequest,
    add: bool,
) -> Result<WorkItem, StoreError> {
    change_work_prerequisite_with_validation_on(
        transaction,
        request,
        add,
        PlanningValidation::Immediate,
        None,
    )
}

pub(super) fn change_work_prerequisite_with_validation_on(
    transaction: &Transaction<'_>,
    request: &ChangeWorkPrerequisiteRequest,
    add: bool,
    validation: PlanningValidation,
    planned_relations: Option<&mut PlanRelationBasis>,
) -> Result<WorkItem, StoreError> {
    let mut item = load_work_item(transaction, request.work_id)?;
    let prerequisite = load_work_item(transaction, request.prerequisite_id)?;
    if let Some(relations) = planned_relations.as_ref() {
        if !matches!(validation, PlanningValidation::AtomicPlan)
            || relations.work_id != item.work_id
        {
            return Err(StoreError::InvalidWorkProjection(
                "planned relation basis has the wrong owner".into(),
            ));
        }
    } else {
        require_work_item_relation_integrity(transaction, item.work_id)?;
    }
    assert_revision(&item, request.expected_revision)?;
    validate_planning_authority(
        transaction,
        &item,
        &request.authority,
        &request.actor,
        request.changed_at,
    )?;
    if item.project_id != prerequisite.project_id {
        return Err(StoreError::InvalidWork(
            "prerequisite edges cannot cross projects".into(),
        ));
    }
    if item.lifecycle != WorkLifecycle::Open {
        return Err(StoreError::WorkNotOpen(item.work_id));
    }
    if add {
        if prerequisite.lifecycle == WorkLifecycle::Completed {
            return Err(StoreError::WorkPrerequisiteAlreadySatisfied(
                prerequisite.work_id,
            ));
        }
        if prerequisite.lifecycle != WorkLifecycle::Open {
            return Err(StoreError::WorkNotOpen(prerequisite.work_id));
        }
        if work_is_ancestor_of(transaction, prerequisite.work_id, &item)? {
            return Err(StoreError::WorkDependencyCycle);
        }
    }
    let exists: Option<String> = transaction
        .query_row(
            "SELECT event_id FROM work_prerequisites
             WHERE work_id = ?1 AND prerequisite_id = ?2",
            params![
                item.work_id.0.to_string(),
                prerequisite.work_id.0.to_string()
            ],
            |row| row.get(0),
        )
        .optional()?;
    if add == exists.is_some() {
        return Ok(item);
    }
    item.revision += 1;
    item.updated_at = request.changed_at;
    let (claim_snapshot, rebased_run) =
        rebase_planning_claim(transaction, &item, &request.authority, request.changed_at)?;
    let event = WorkEventDraft {
        schema_version: SCHEMA_VERSION,
        project_id: item.project_id.clone(),
        root_id: item.root_id,
        work_id: item.work_id,
        run_id: item.active_run_id,
        revision: item.revision,
        work: item.clone(),
        run: match rebased_run {
            Some(run) => Some(run),
            None => active_run_snapshot(transaction, &item)?,
        },
        root_execution: None,
        claim: claim_snapshot,
        handoff_offer: None,
        blocker: None,
        transition: if add {
            WorkTransition::PrerequisiteAdded {
                prerequisite_id: prerequisite.work_id,
                authority: request.authority.clone(),
            }
        } else {
            WorkTransition::PrerequisiteRemoved {
                prerequisite_id: prerequisite.work_id,
                authority: request.authority.clone(),
            }
        },
        actor: request.actor.clone(),
        created_at: request.changed_at,
    };
    let (event_id, _) = if let Some(relations) = planned_relations {
        let appended = super::super::feeds::append_planned_prerequisite_event(
            transaction,
            &event,
            &relations.basis,
        )?;
        apply_work_relation_transition(
            &mut relations.basis,
            &event.transition,
            event.blocker.as_ref(),
        )?;
        appended
    } else {
        append_work_event(transaction, &event)?
    };
    if add {
        transaction.execute(
            "INSERT INTO work_prerequisites (work_id, prerequisite_id, event_id)
             VALUES (?1, ?2, ?3)",
            params![
                item.work_id.0.to_string(),
                prerequisite.work_id.0.to_string(),
                event_id.as_str()
            ],
        )?;
        validation.check_graph(transaction, &item.project_id.0)?;
    } else {
        transaction.execute(
            "DELETE FROM work_prerequisites
             WHERE work_id = ?1 AND prerequisite_id = ?2",
            params![
                item.work_id.0.to_string(),
                prerequisite.work_id.0.to_string()
            ],
        )?;
    }
    persist_work_item(transaction, &item)?;
    Ok(item)
}

pub(in crate::storage::work) fn projected_work_relation_basis(
    connection: &Connection,
    work_id: WorkId,
) -> Result<WorkRelationBasis, StoreError> {
    let mut prerequisite_ids = load_prerequisite_projection_ids(connection, work_id)?;
    prerequisite_ids.sort_by_key(|prerequisite_id| prerequisite_id.0);
    let mut active_blockers = load_active_blocker_projections(connection, work_id)?
        .into_iter()
        .map(|blocker| {
            let blocker_id = blocker.blocker_id.clone();
            let blocker_hash = CanonicalObject::freeze(&blocker)?.key().clone();
            Ok(WorkRelationBlockerBasis {
                blocker_id,
                blocker_hash,
            })
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    active_blockers.sort_by(|left, right| left.blocker_id.cmp(&right.blocker_id));
    Ok(WorkRelationBasis {
        schema_version: SCHEMA_VERSION,
        prerequisite_ids,
        active_blockers,
    })
}

pub(in crate::storage::work) fn work_relation_fingerprint(
    basis: &WorkRelationBasis,
) -> Result<ObjectId, StoreError> {
    Ok(CanonicalObject::freeze(basis)?.key().clone())
}

pub(in crate::storage::work) fn validated_current_work_relation_basis(
    connection: &Connection,
    work_id: WorkId,
) -> Result<WorkRelationBasis, StoreError> {
    let basis = projected_work_relation_basis(connection, work_id)?;
    let actual = work_relation_fingerprint(&basis)?;
    let expected = if let Some(latest) =
        super::super::query::latest_canonical_work_event_for_item_optional(connection, work_id)?
    {
        latest.relation_fingerprint
    } else {
        let (_, restored) = super::super::query::latest_restored_record(connection, work_id)?
            .ok_or_else(|| {
                StoreError::InvalidWorkProjection(format!(
                    "relations for {work_id:?} have no canonical history anchor"
                ))
            })?;
        let mut restored_basis = WorkRelationBasis {
            schema_version: SCHEMA_VERSION,
            prerequisite_ids: restored.relations.prerequisites,
            active_blockers: restored
                .relations
                .blockers
                .into_iter()
                .map(|blocker| {
                    let blocker = WorkBlocker {
                        blocker_id: blocker.blocker_id,
                        work_id: blocker.work_id,
                        kind: blocker.kind,
                        detail: blocker.detail,
                        created_by: blocker.created_by,
                        created_at: blocker.created_at,
                    };
                    Ok(WorkRelationBlockerBasis {
                        blocker_id: blocker.blocker_id.clone(),
                        blocker_hash: CanonicalObject::freeze(&blocker)?.key().clone(),
                    })
                })
                .collect::<Result<Vec<_>, StoreError>>()?,
        };
        restored_basis.prerequisite_ids.sort_by_key(|id| id.0);
        restored_basis
            .active_blockers
            .sort_by(|left, right| left.blocker_id.cmp(&right.blocker_id));
        work_relation_fingerprint(&restored_basis)?
    };
    if actual != expected {
        return Err(StoreError::InvalidWorkProjection(format!(
            "relations for {work_id:?} differ from the latest canonical fingerprint"
        )));
    }
    Ok(basis)
}

pub(in crate::storage::work) fn apply_work_relation_transition(
    basis: &mut WorkRelationBasis,
    transition: &WorkTransition,
    blocker: Option<&WorkBlocker>,
) -> Result<(), StoreError> {
    match transition {
        WorkTransition::Created { prerequisites, .. } => {
            basis.prerequisite_ids.clone_from(prerequisites);
            basis
                .prerequisite_ids
                .sort_by_key(|prerequisite_id| prerequisite_id.0);
            basis.prerequisite_ids.dedup();
        }
        WorkTransition::PrerequisiteAdded {
            prerequisite_id, ..
        } => {
            if basis.prerequisite_ids.contains(prerequisite_id) {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "prerequisite {prerequisite_id:?} is already active"
                )));
            }
            basis.prerequisite_ids.push(*prerequisite_id);
            basis
                .prerequisite_ids
                .sort_by_key(|prerequisite_id| prerequisite_id.0);
        }
        WorkTransition::PrerequisiteRemoved {
            prerequisite_id, ..
        } => {
            let previous = basis.prerequisite_ids.len();
            basis
                .prerequisite_ids
                .retain(|candidate| candidate != prerequisite_id);
            if basis.prerequisite_ids.len() == previous {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "prerequisite {prerequisite_id:?} is not active"
                )));
            }
        }
        WorkTransition::Blocked { blocker_id } => {
            let blocker = blocker.ok_or_else(|| {
                StoreError::InvalidWorkProjection(format!(
                    "block event {blocker_id} has no blocker snapshot"
                ))
            })?;
            if blocker.blocker_id != *blocker_id
                || basis
                    .active_blockers
                    .iter()
                    .any(|candidate| candidate.blocker_id == *blocker_id)
            {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "block event {blocker_id} has an invalid relation transition"
                )));
            }
            basis.active_blockers.push(WorkRelationBlockerBasis {
                blocker_id: blocker_id.clone(),
                blocker_hash: CanonicalObject::freeze(blocker)?.key().clone(),
            });
            basis
                .active_blockers
                .sort_by(|left, right| left.blocker_id.cmp(&right.blocker_id));
        }
        WorkTransition::Unblocked { blocker_id } => {
            let previous = basis.active_blockers.len();
            basis
                .active_blockers
                .retain(|candidate| candidate.blocker_id != *blocker_id);
            if basis.active_blockers.len() == previous {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "blocker {blocker_id} is not active"
                )));
            }
        }
        _ => {}
    }
    Ok(())
}

pub(in crate::storage::work) fn require_work_item_relation_integrity(
    connection: &Connection,
    work_id: WorkId,
) -> Result<(), StoreError> {
    validated_current_work_relation_basis(connection, work_id)?;
    Ok(())
}
