//! Complete old components plus new endpoints cover both sides of a delta.

use std::collections::{BTreeMap, BTreeSet};

use super::{component, graph::document_edges};
use crate::{
    ProjectId, WorkId,
    storage::{SqliteStore, StoreError},
    task_sync::{
        FORMAT, ProjectDocument, TaskDocument, TaskGraphComponent, TaskGraphOverlay,
        TaskSyncGraphView, TaskSyncRefusal, TaskTree,
    },
};

impl SqliteStore {
    /// Capture the recorded baseline components touched by a hypothetical
    /// delta, checking stored document fields and graph rules before and after
    /// the overlay with the existing complete-graph validator.
    ///
    /// Old relations are traversed both ways, including unchanged references
    /// to removed tasks. New endpoints seed their old components too. The read
    /// and both checks share one snapshot; nothing is written. The result is
    /// not import admission or a source-commit basis.
    ///
    /// # Errors
    /// Refuses unavailable or inconsistent selected baseline projections,
    /// mismatched task keys, nonmember removals and invalid resulting graphs.
    pub fn capture_task_sync_baseline_overlay(
        &self,
        project: &ProjectId,
        changes: &BTreeMap<WorkId, Option<TaskDocument>>,
    ) -> Result<TaskGraphOverlay, StoreError> {
        if project.0.trim().is_empty() {
            return Err(TaskSyncRefusal::InvalidGraph.invalid("empty project id"));
        }
        self.work_read_snapshot(|store| {
            super::super::origin::with_origin_scope(&store.connection, || {
                capture_on(&store.connection, project, changes)
            })
        })
    }
}

/// The atomic admission owner calls this inside its existing writer. Public
/// observations are never substituted for a fresh transaction-scoped capture.
pub(in crate::storage::work::task_sync) fn capture_on(
    connection: &rusqlite::Connection,
    project: &ProjectId,
    changes: &BTreeMap<WorkId, Option<TaskDocument>>,
) -> Result<TaskGraphOverlay, StoreError> {
    if project.0.trim().is_empty() {
        return Err(TaskSyncRefusal::InvalidGraph.invalid("empty project id"));
    }
    let before = component::capture(
        connection,
        project,
        TaskSyncGraphView::Baseline,
        seeds(changes)?,
    )?;
    overlay(project, before, changes)
}

fn seeds(changes: &BTreeMap<WorkId, Option<TaskDocument>>) -> Result<BTreeSet<WorkId>, StoreError> {
    let mut seeds = BTreeSet::new();
    for (&id, next) in changes {
        seeds.insert(id);
        if let Some(document) = next {
            if document.id != id {
                return Err(TaskSyncRefusal::InvalidGraph.invalid("delta task key differs"));
            }
            seeds.extend(
                document_edges(document)
                    .into_iter()
                    .map(|(_, target)| target),
            );
        }
        component::check_capacity(seeds.len())?;
    }
    Ok(seeds)
}

pub(in crate::storage::work::task_sync) fn overlay(
    project: &ProjectId,
    before: TaskGraphComponent,
    changes: &BTreeMap<WorkId, Option<TaskDocument>>,
) -> Result<TaskGraphOverlay, StoreError> {
    let validate = |documents: &BTreeMap<WorkId, TaskDocument>| {
        TaskTree {
            project: ProjectDocument {
                format: FORMAT.into(),
                project_id: project.clone(),
                notes_included: before.notes_included,
            },
            tasks: documents.values().cloned().collect(),
        }
        .validate_stored()
        .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))
    };
    // A new value must not hide invalid selected retained data.
    validate(&before.documents)?;
    let mut after = before.documents.clone();
    for (&id, next) in changes {
        match next {
            Some(document) => {
                after.insert(id, document.clone());
            }
            None => {
                if after.remove(&id).is_none() {
                    return Err(TaskSyncRefusal::InvalidGraph
                        .invalid("delta removes a task which is not a member"));
                }
            }
        }
    }
    validate(&after)?;
    Ok(TaskGraphOverlay { before, after })
}
