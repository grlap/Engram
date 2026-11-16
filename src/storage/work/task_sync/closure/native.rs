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

    /// Check the task graph a folder export leaves beside tasks an import
    /// held: the folder documents of every connected component the held and
    /// written tasks touch, with each held task as the folder holds it (`None`
    /// when its file is gone) and each written task as written (`None` when
    /// removed). Every other folder document is this host's own export of
    /// its planning, so these components are the only part of the folder no
    /// store has admitted; tasks outside them are not read.
    ///
    /// # Errors
    /// Refuses with [`TaskSyncRefusal::InvalidGraph`] a resulting graph that
    /// is inconsistent, and capture failures.
    pub fn check_task_sync_folder_beside_held(
        &self,
        project: &ProjectId,
        profile: &ExportProfile,
        held: &BTreeMap<WorkId, Option<crate::task_sync::TaskDocument>>,
        written: &BTreeMap<WorkId, Option<crate::task_sync::TaskDocument>>,
    ) -> Result<(), StoreError> {
        let mut seeds = BTreeSet::new();
        for (id, document) in held.iter().chain(written) {
            seeds.insert(*id);
            if let Some(document) = document {
                seeds.extend(
                    document_edges(document)
                        .into_iter()
                        .map(|(_, target)| target),
                );
            }
        }
        let mut documents = self.work_read_snapshot(|store| {
            origin::with_origin_scope(&store.connection, || {
                let component = capture_on(&store.connection, project, profile, seeds)?;
                // A task retired from exchange membership is in no folder.
                let entries = super::super::baseline::entries(
                    &store.connection,
                    project,
                    component.documents.keys().copied(),
                )?;
                Ok(component
                    .documents
                    .into_iter()
                    .filter(|(id, _)| entries.get(id).is_none_or(|entry| entry.document.is_some()))
                    .collect::<BTreeMap<_, _>>())
            })
        })?;
        for (&id, document) in written.iter().chain(held) {
            match document {
                Some(document) => {
                    documents.insert(id, document.clone());
                }
                None => {
                    documents.remove(&id);
                }
            }
        }
        crate::task_sync::TaskTree {
            project: crate::task_sync::ProjectDocument {
                format: crate::task_sync::FORMAT.into(),
                project_id: project.clone(),
                notes_included: profile.include_notes,
            },
            tasks: documents.into_values().collect(),
        }
        .validate_stored()
        .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))
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
