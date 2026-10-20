//! Transaction-local relation writers for a preflighted complete folder.
//! The caller owns replay, whole-graph admission, final audit and commit.

use super::{
    CanonicalObject, ChangeWorkPrerequisiteRequest, DateTime, PlanningValidation, SCHEMA_VERSION,
    SqliteStore, StoreError, Transaction, Utc, WorkBlocker, WorkEventDraft, WorkId, WorkItem,
    WorkLifecycle, WorkPlanningAuthority, WorkTransition, append_work_event,
    change_work_prerequisite_with_validation_on, load_work_item, params, persist_work_item,
    refresh_work_catalog_projection, require_work_item_relation_integrity,
};

fn run_free_item(transaction: &Transaction<'_>, id: WorkId) -> Result<WorkItem, StoreError> {
    let item = load_work_item(transaction, id)?;
    if item.lifecycle != WorkLifecycle::Open || item.active_run_id.is_some() || !item.restored {
        return Err(StoreError::InvalidWorkProjection(
            "a folder relation owner lost run-free admission".into(),
        ));
    }
    Ok(item)
}

pub(in crate::storage::work) fn apply_folder_prerequisite_on(
    transaction: &Transaction<'_>,
    id: WorkId,
    prerequisite: WorkId,
    add: bool,
    actor: &crate::ActorContext,
    at: DateTime<Utc>,
) -> Result<(), StoreError> {
    let item = run_free_item(transaction, id)?;
    // Reuse the existing transaction-internal transition and checked event
    // basis. Only the repeated whole-project graph scan is deferred, as in
    // an atomic plan; folder admission checks the final durable graph and
    // the folder caller audits it again before its one commit.
    change_work_prerequisite_with_validation_on(
        transaction,
        &ChangeWorkPrerequisiteRequest {
            work_id: id,
            prerequisite_id: prerequisite,
            expected_revision: item.revision,
            authority: WorkPlanningAuthority::Project,
            actor: actor.clone(),
            idempotency_key: String::new(),
            changed_at: at,
        },
        add,
        PlanningValidation::AtomicPlan,
        None,
    )?;
    Ok(())
}

/// Adds an incoming shared identity, or clears an existing active identity.
/// A clear retains the original JSON and its creation anchor.
pub(in crate::storage::work) fn apply_folder_blocker_on(
    transaction: &Transaction<'_>,
    id: WorkId,
    blocker_id: &str,
    added: Option<&crate::task_sync::SharedBlocker>,
    actor: &crate::ActorContext,
    at: DateTime<Utc>,
) -> Result<(), StoreError> {
    let mut item = run_free_item(transaction, id)?;
    require_work_item_relation_integrity(transaction, id)?;
    let blocker = if let Some(incoming) = added {
        if incoming.blocker_id != blocker_id {
            return Err(StoreError::InvalidWorkProjection(
                "a folder blocker has the wrong identity".into(),
            ));
        }
        let held: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM work_blockers WHERE blocker_id = ?1)",
            [blocker_id],
            |row| row.get(0),
        )?;
        if held {
            return Err(StoreError::InvalidWorkProjection(
                "a folder blocker identity lost admission".into(),
            ));
        }
        let blocker = WorkBlocker {
            blocker_id: incoming.blocker_id.clone(),
            work_id: id,
            kind: incoming.kind,
            detail: incoming.detail.clone(),
            created_by: incoming.created_by.reported(),
            created_at: incoming.created_at,
        };
        let object = CanonicalObject::mint(&blocker)?;
        SqliteStore::insert_object(transaction, "work_blocker", &object)?;
        Some(blocker)
    } else {
        let held: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM work_blockers WHERE blocker_id = ?1 AND work_id = ?2 AND state = 'active')",
            params![blocker_id, id.0.to_string()], |row| row.get(0),
        )?;
        if !held {
            return Err(StoreError::InvalidWorkProjection(
                "a folder blocker clear lost admission".into(),
            ));
        }
        None
    };
    item.revision += 1;
    item.updated_at = at;
    persist_work_item(transaction, &item)?;
    // Append against the old live relation basis, then change that relation.
    let (event, _) = append_work_event(
        transaction,
        &WorkEventDraft {
            schema_version: SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            root_id: item.root_id,
            work_id: id,
            revision: item.revision,
            work: item.clone(),
            run_id: None,
            run: None,
            root_execution: None,
            claim: None,
            handoff_offer: None,
            blocker: blocker.clone(),
            transition: if blocker.is_some() {
                WorkTransition::Blocked {
                    blocker_id: blocker_id.into(),
                }
            } else {
                WorkTransition::Unblocked {
                    blocker_id: blocker_id.into(),
                }
            },
            actor: actor.clone(),
            created_at: at,
        },
    )?;
    if let Some(blocker) = blocker {
        transaction.execute(
            "INSERT INTO work_blockers (blocker_id, work_id, state, blocker_json, created_event_id) VALUES (?1, ?2, 'active', ?3, ?4)",
            params![blocker_id, id.0.to_string(), serde_json::to_vec(&blocker)?, event.as_str()],
        )?;
    } else {
        transaction.execute("UPDATE work_blockers SET state = 'cleared', cleared_event_id = ?2 WHERE blocker_id = ?1",
            params![blocker_id, event.as_str()])?;
    }
    refresh_work_catalog_projection(transaction, &item)?;
    Ok(())
}
