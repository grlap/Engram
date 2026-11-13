//! Immutable native endpoints of references retained in import origins.

use rusqlite::Connection;

use super::super::query;
use crate::{
    ObjectId, ProjectId, StoreError, WorkId, WorkTransition,
    task_sync::{SharedActor, SharedCompletion, TaskDocument},
};

pub(in crate::storage::work::task_sync) struct NativeEndpoint {
    pub record: ObjectId,
    pub root: WorkId,
    pub task: TaskDocument,
}

pub(in crate::storage::work::task_sync) fn native_endpoints(
    connection: &Connection,
    project: &ProjectId,
    documents: &[TaskDocument],
) -> Result<Vec<NativeEndpoint>, StoreError> {
    let mut seen = documents
        .iter()
        .map(|task| task.id)
        .collect::<std::collections::HashSet<_>>();
    let mut pending = documents
        .iter()
        .flat_map(references)
        .collect::<std::collections::BTreeSet<_>>();
    let mut endpoints = Vec::new();
    while let Some(id) = pending.pop_first() {
        if !seen.insert(id) {
            continue;
        }
        if let Some(endpoint) = native_endpoint(connection, project, id)? {
            pending.extend(references(&endpoint.task));
            endpoints.push(endpoint);
        }
    }
    Ok(endpoints)
}

pub(in crate::storage::work::task_sync) fn references(
    task: &TaskDocument,
) -> impl Iterator<Item = WorkId> + '_ {
    task.parent_id
        .into_iter()
        .chain(task.prerequisites.iter().copied())
        .chain(task.superseded_by)
}

pub(in crate::storage::work::task_sync) fn native_endpoint(
    connection: &Connection,
    project: &ProjectId,
    id: WorkId,
) -> Result<Option<NativeEndpoint>, StoreError> {
    let item = match query::load_work_item(connection, id) {
        Ok(item) => item,
        Err(StoreError::WorkNotFound(_)) => return Ok(None),
        Err(error) => return Err(error),
    };
    if item.project_id != *project {
        return Err(StoreError::InvalidWorkProjection(
            "native origin endpoint crosses projects".into(),
        ));
    }
    if item.created_by.source_tool.as_deref() == Some(super::super::IMPORT_TOOL) {
        return Ok(None);
    }
    for (record, event) in query::canonical_work_event_records_for_item(connection, id)? {
        if let WorkTransition::Created { prerequisites } = event.transition {
            let initial = event.work;
            return Ok(Some(NativeEndpoint {
                record,
                root: initial.root_id,
                task: TaskDocument {
                    id,
                    external_ref: initial.external_ref,
                    title: initial.title,
                    outcome: initial.outcome,
                    acceptance: initial.acceptance,
                    acceptance_bindings: initial.acceptance_bindings,
                    evaluation_mode: initial.evaluation_mode,
                    kind: initial.kind,
                    priority: initial.priority,
                    labels: initial.labels,
                    assigned_to: initial.assigned_to,
                    deferred_until: initial.deferred_until,
                    lifecycle: initial.lifecycle,
                    parent_id: initial.parent_id,
                    child_requirement: initial.child_requirement,
                    prerequisites,
                    blockers: Vec::new(),
                    superseded_by: initial.superseded_by,
                    disposal_reason: None,
                    completion: None,
                    notes: Vec::new(),
                    activity: std::collections::BTreeMap::default(),
                },
            }));
        }
    }
    if let Some((record, original)) = query::restored_records_with_id_for_item(connection, id)?
        .into_iter()
        .next()
    {
        let initial = original.item;
        return Ok(Some(NativeEndpoint {
            record,
            root: initial.root_id,
            task: TaskDocument {
                id,
                external_ref: initial.external_ref,
                title: initial.title,
                outcome: initial.outcome,
                acceptance: initial.acceptance,
                acceptance_bindings: Vec::new(),
                evaluation_mode: initial.evaluation_mode,
                kind: initial.kind,
                priority: initial.priority,
                labels: initial.labels,
                assigned_to: initial.assigned_to,
                deferred_until: initial.deferred_until,
                lifecycle: initial.lifecycle,
                parent_id: initial.parent_id,
                child_requirement: initial.child_requirement,
                prerequisites: original.relations.prerequisites,
                blockers: Vec::new(),
                superseded_by: initial.superseded_by,
                disposal_reason: initial.disposal_reason,
                completion: original
                    .history
                    .completion
                    .map(|completion| SharedCompletion {
                        summary: completion.summary,
                        completed_at: completion.completed_at,
                        actor: SharedActor::from_actor(&completion.actor),
                    }),
                notes: Vec::new(),
                activity: std::collections::BTreeMap::default(),
            },
        }));
    }
    Err(StoreError::InvalidWorkProjection(
        "native origin endpoint has no immutable creation basis".into(),
    ))
}
