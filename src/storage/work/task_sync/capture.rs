//! Snapshot-scoped capture of selected task documents, without graph admission.

use std::collections::{BTreeMap, BTreeSet};

use super::{
    Connection, ProjectId, SharedActor, SharedBlocker, SharedCompletion, SqliteStore, StoreError,
    TaskDocument, TaskSyncRefusal, WorkId, WorkItem, activity, baseline, graph_snapshot, notes,
    origin, query, with_origin_scope,
};
use crate::task_sync::{
    CapturedTask, TaskDocumentCapture, notes::NoteOmissions, progress::ExportProfile,
};

impl SqliteStore {
    /// Captures exactly the selected task identities in one read snapshot.
    ///
    /// Duplicate IDs are captured once. Exchange-retired and absent identities
    /// are explicit, and unrelated task documents are not read. References may
    /// name unselected tasks: this is typed capture, not partial-graph admission.
    /// The caller owns document-shape and graph validation before applying or
    /// publishing a change.
    ///
    /// # Errors
    /// Refuses invalid profiles, changed baseline note selection, a task from
    /// another project, and selected read or capture errors. Returned documents
    /// have not passed task-tree admission. No full-export fallback is used.
    pub fn capture_task_documents(
        &self,
        project: &ProjectId,
        profile: &ExportProfile,
        ids: impl IntoIterator<Item = WorkId>,
    ) -> Result<TaskDocumentCapture, StoreError> {
        profile.options().validate()?;
        if project.0.trim().is_empty() {
            return Err(TaskSyncRefusal::InvalidGraph.invalid("empty project id"));
        }
        let ids = ids.into_iter().collect::<BTreeSet<_>>();
        self.work_read_snapshot(|store| {
            with_origin_scope(&store.connection, || {
                baseline::validate_note_selection(
                    &store.connection,
                    project,
                    profile.include_notes,
                )?;
                let entries = baseline::entries(&store.connection, project, ids.iter().copied())?;
                let mut documents = BTreeMap::new();
                let mut omissions = NoteOmissions::default();
                let mut activity_omissions = NoteOmissions::default();
                for id in ids {
                    let captured = if entries
                        .get(&id)
                        .is_some_and(|entry| entry.document.is_none())
                    {
                        CapturedTask::Retired
                    } else {
                        match query::load_work_item(&store.connection, id) {
                            Ok(item) => {
                                if item.project_id != *project {
                                    return Err(TaskSyncRefusal::OriginMismatch
                                        .invalid("selected task belongs to another project"));
                                }
                                let (document, _) = capture_document_on(
                                    &store.connection,
                                    item,
                                    profile.include_notes,
                                    profile.origin_machine.as_deref(),
                                    false,
                                    &mut omissions,
                                    &mut activity_omissions,
                                )?;
                                CapturedTask::Present(Box::new(document))
                            }
                            Err(StoreError::WorkNotFound(_)) => CapturedTask::Absent,
                            Err(error) => return Err(super::origin::classify_binding(error)),
                        }
                    };
                    documents.insert(id, captured);
                }
                Ok(TaskDocumentCapture {
                    documents,
                    omissions,
                    activity_omissions,
                })
            })
        })
    }
}

pub(super) fn capture_document_on(
    connection: &Connection,
    item: WorkItem,
    include_notes: bool,
    machine: Option<&str>,
    comparison: bool,
    omissions: &mut NoteOmissions,
    activity_omissions: &mut NoteOmissions,
) -> Result<(TaskDocument, bool), StoreError> {
    let id = item.work_id;
    if item.origin == crate::WorkOrigin::Imported || item.source_snapshot_id.is_some() {
        return Err(TaskSyncRefusal::OriginMismatch.invalid(
            "task export refuses external imported origin and source snapshot; use graph export to retain that provenance",
        ));
    }
    let event_records = query::canonical_work_event_records_for_item(connection, id)?;
    let shared_activity = activity::capture(
        connection,
        &item,
        &event_records,
        machine,
        comparison,
        activity_omissions,
    )
    .map_err(origin::classify_binding)?;
    let events = event_records
        .into_iter()
        .map(|(_, event)| event)
        .collect::<Vec<_>>();
    let records = graph_snapshot::restored_records_on(connection, id)?;
    let prerequisites = query::load_prerequisite_projection_ids(connection, id)?;
    let snapshot = graph_snapshot::snapshot_item(&item, prerequisites.clone(), &events, &records)?;
    let completion = graph_snapshot::completion_from_events(connection, &item, &events, &records)?
        .map(|completion| SharedCompletion {
            summary: completion.summary,
            completed_at: completion.completed_at,
            actor: SharedActor::from_actor(&completion.actor),
        });
    let mut blockers = query::load_active_blocker_projections(connection, id)?
        .into_iter()
        .map(|blocker| SharedBlocker {
            blocker_id: blocker.blocker_id,
            kind: blocker.kind,
            detail: blocker.detail,
            created_by: SharedActor::from_actor(&blocker.created_by),
            created_at: blocker.created_at,
        })
        .collect::<Vec<_>>();
    blockers.sort_by(|a, b| a.blocker_id.cmp(&b.blocker_id));
    let mut stored_notes_included = false;
    let shared_notes = if include_notes || comparison {
        let (notes, stored_inclusion) = notes::capture(
            connection,
            &item,
            machine,
            include_notes,
            comparison,
            omissions,
        )
        .map_err(origin::classify_binding)?;
        stored_notes_included = stored_inclusion;
        notes
    } else {
        Vec::new()
    };
    Ok((
        TaskDocument {
            id,
            external_ref: item.external_ref,
            title: item.title,
            outcome: item.outcome,
            acceptance: item.acceptance,
            acceptance_bindings: item.acceptance_bindings,
            evaluation_mode: item.evaluation_mode,
            kind: item.kind,
            priority: item.priority,
            labels: item.labels,
            assigned_to: item.assigned_to,
            deferred_until: item.deferred_until,
            lifecycle: item.lifecycle,
            parent_id: item.parent_id,
            child_requirement: item.child_requirement,
            prerequisites,
            blockers,
            superseded_by: item.superseded_by,
            disposal_reason: snapshot.disposal_reason,
            completion,
            notes: shared_notes,
            activity: shared_activity,
        },
        stored_notes_included,
    ))
}
