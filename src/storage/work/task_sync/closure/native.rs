//! Durable destination components use native adjacency, including retired work.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::Connection;

use super::super::{capture, origin, query};
use super::graph::document_edges;
use crate::{
    ProjectId, WorkId,
    storage::{SqliteStore, StoreError},
    task_sync::{
        TaskGraphComponent, TaskSyncRefusal, notes::NoteOmissions, progress::ExportProfile,
    },
};

#[cfg(test)]
thread_local! {
    pub(in crate::storage::work::task_sync) static NODE_LOOKUPS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(in crate::storage::work::task_sync) static DOCUMENT_CAPTURES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(in crate::storage::work::task_sync) static INCOMING_EDGES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

impl SqliteStore {
    /// Capture the durable destination's connected components on one snapshot.
    /// This includes work retired from exchange membership. Each reached task
    /// uses the complete exporter's per-task reader and its own canonical
    /// history; disconnected tasks and their histories are not read.
    ///
    /// The output uses the ordinary notes-off, no-machine export profile and
    /// remains unadmitted. It grants no application authority. Native root
    /// bindings are returned for the caller's graph validation.
    ///
    /// # Errors
    /// Refuses foreign-project selections and selected read/capture failures.
    /// Absent endpoints remain explicit. There is no full-inventory fallback.
    pub fn capture_task_sync_native_component(
        &self,
        project: &ProjectId,
        seeds: impl IntoIterator<Item = WorkId>,
    ) -> Result<TaskGraphComponent, StoreError> {
        self.capture_task_sync_native_component_with_profile(
            project,
            &ExportProfile {
                include_notes: false,
                origin_machine: None,
            },
            seeds,
        )
    }

    /// Capture durable connected work with an explicit disclosure profile.
    /// Retired exchange members remain part of the native graph. This read
    /// shares the exporter's per-task owner and grants no import admission.
    ///
    /// # Errors
    /// Refuses invalid profiles, foreign-project selections, unsafe selected
    /// records and a connected scope exceeding the supported capacity.
    pub fn capture_task_sync_native_component_with_profile(
        &self,
        project: &ProjectId,
        profile: &ExportProfile,
        seeds: impl IntoIterator<Item = WorkId>,
    ) -> Result<TaskGraphComponent, StoreError> {
        if project.0.trim().is_empty() {
            return Err(TaskSyncRefusal::InvalidGraph.invalid("empty project id"));
        }
        let seeds = seeds.into_iter().collect();
        self.work_read_snapshot(|store| {
            origin::with_origin_scope(&store.connection, || {
                capture_on(&store.connection, project, profile, seeds)
            })
        })
    }
}

pub(in crate::storage::work::task_sync) fn capture_on(
    connection: &Connection,
    project: &ProjectId,
    profile: &ExportProfile,
    mut pending: BTreeSet<WorkId>,
) -> Result<TaskGraphComponent, StoreError> {
    if project.0.trim().is_empty() {
        return Err(TaskSyncRefusal::InvalidGraph.invalid("empty project id"));
    }
    profile
        .validate()
        .map_err(|error| TaskSyncRefusal::InvalidGraph.invalid(error.to_string()))?;
    super::component::check_capacity(pending.len())?;
    let mut result = TaskGraphComponent {
        documents: BTreeMap::new(),
        missing: BTreeSet::new(),
        members: BTreeMap::new(),
        expected_roots: BTreeMap::new(),
        notes_included: profile.include_notes,
        baseline: None,
    };
    let mut seen = BTreeSet::new();
    while let Some(id) = pending.pop_first() {
        if !seen.insert(id) {
            continue;
        }
        super::component::check_capacity(seen.len())?;
        #[cfg(test)]
        NODE_LOOKUPS.with(|count| count.set(count.get() + 1));
        let item = match query::load_work_item(connection, id).map_err(origin::classify_binding) {
            Ok(item) => Some(item),
            Err(StoreError::WorkNotFound(_)) => None,
            Err(error) => return Err(error),
        };
        if let Some(item) = item {
            if item.project_id != *project {
                return Err(TaskSyncRefusal::OriginMismatch
                    .invalid("selected task belongs to another project"));
            }
            result.expected_roots.insert(id, item.root_id);
            #[cfg(test)]
            DOCUMENT_CAPTURES.with(|count| count.set(count.get() + 1));
            let (document, _) = capture::capture_document_on(
                connection,
                item,
                profile.include_notes,
                profile.origin_machine.as_deref(),
                false,
                &mut NoteOmissions::default(),
                &mut NoteOmissions::default(),
            )?;
            pending.extend(
                document_edges(&document)
                    .into_iter()
                    .map(|(_, target)| target),
            );
            result.documents.insert(id, document);
        } else {
            result.missing.insert(id);
        }
        // Each branch is keyed on its existing reverse index. Retired owners
        // remain references: exchange membership is not durable graph truth.
        for sql in [
            "SELECT work_id FROM work_items INDEXED BY work_items_parent WHERE parent_id=?1",
            "SELECT work_id FROM work_prerequisites INDEXED BY work_prerequisites_reverse WHERE prerequisite_id=?1",
            "SELECT work_id FROM work_items INDEXED BY work_items_replacement_reverse WHERE superseded_by=?1 AND superseded_by IS NOT NULL",
        ] {
            for owner in connection
                .prepare(sql)?
                .query_map([id.0.to_string()], |row| row.get::<_, String>(0))?
            {
                #[cfg(test)]
                INCOMING_EDGES.with(|count| count.set(count.get() + 1));
                pending.insert(super::component::work_id(&owner?)?);
            }
        }
    }
    Ok(result)
}
