//! One-cut typed inventory and dense-feed continuity for local preparation.

use super::{SqliteStore, StoreError, export_on_snapshot, with_origin_scope};
use crate::task_sync::{
    TaskSyncRefusal,
    notes::NoteOmissions,
    progress::{ExportCapture, ExportCut, ExportProfile, FeedAnchor, TaskChangeCapture},
};
use crate::{
    ProjectId, RestoredWorkEvidence, WorkEvent, WorkEvidence,
    domain::{FeedId, WorkObservation},
};

impl SqliteStore {
    /// Capture the complete typed shared inventory and the feed interval on one cut.
    /// The inventory includes feedless restored imports and notes without feed work ids.
    /// Only changed task documents are serialized by the preparation layer.
    ///
    /// # Errors
    /// Refuses invalid source graphs, canonical records and I/O failures.
    pub fn capture_task_prepare(
        &self,
        project: &ProjectId,
        profile: &ExportProfile,
        previous: Option<&ExportCut>,
    ) -> Result<ExportCapture, StoreError> {
        profile.options().validate()?;
        self.work_read_snapshot(|store| {
            with_origin_scope(&store.connection, || {
                let feed = FeedId::Project(project.clone());
                let cut = current_cut(store, &feed)?;
                let head = cut.position;
                let full_reason = previous
                    .and_then(|previous| continuity(store, &feed, previous, head).err())
                    .transpose()?;
                let mut omissions = NoteOmissions::default();
                let mut activity_omissions = NoteOmissions::default();
                // Typed shared fields ARE the inventory, not whole WorkItem revisions
                // or its runtime latest-event id. This scans but never serializes tasks.
                let tree = export_on_snapshot(
                    &store.connection,
                    project,
                    profile.include_notes,
                    profile.origin_machine.as_deref(),
                    false,
                    &mut omissions,
                    &mut activity_omissions,
                )?;
                Ok(ExportCapture {
                    tree,
                    omissions,
                    activity_omissions,
                    cut,
                    full_reason,
                })
            })
        })
    }

    /// The current feed cut and the tasks changed since `previous`, from the
    /// changed-task index alone: no task is captured and no record decoded,
    /// whatever the interval's length. `previous` must lie on the feed; a
    /// basis ahead of the head or whose anchor moved is refused by name,
    /// never answered with a full scan.
    ///
    /// # Errors
    /// Refuses a discontinuous basis with
    /// [`TaskSyncRefusal::IncrementalBasisUnavailable`]; returns other errors
    /// when the feed or the index cannot be read.
    pub fn capture_task_changes(
        &self,
        project: &ProjectId,
        previous: &ExportCut,
    ) -> Result<TaskChangeCapture, StoreError> {
        self.work_read_snapshot(|store| {
            let feed = FeedId::Project(project.clone());
            let cut = current_cut(store, &feed)?;
            // The anchor and the head bound alone: the interval itself is
            // read through the index, so its entries are never fetched here.
            if let Err(reason) = anchored(store, &feed, previous, cut.position) {
                return Err(TaskSyncRefusal::IncrementalBasisUnavailable.invalid(reason?));
            }
            // A first capture names every task from the feed; a task written
            // without a feed entry, as a graph load writes them, would be
            // silently missing, so that capture refuses instead.
            if previous.position == 0
                && super::super::feed_tasks::has_unindexed_task(&store.connection, project)?
            {
                return Err(TaskSyncRefusal::IncrementalBasisUnavailable.invalid(
                    "the project holds tasks that no project-feed entry names, so a first capture cannot name them",
                ));
            }
            let changed = super::super::feed_tasks::changed_between(
                &store.connection,
                project,
                previous.position,
                cut.position,
            )?;
            Ok(TaskChangeCapture { cut, changed })
        })
    }
}

impl SqliteStore {
    /// Plan one folder export of `project` under `profile` on one snapshot.
    ///
    /// With no recorded folder head this is the named bootstrap: the complete
    /// export. Otherwise only the tasks the changed-task index names since the
    /// head's cut are captured, through the same serializer as the complete
    /// export; a task that left the project or the exchange is a removal.
    /// Held tasks keep the other host's bytes and are never captured, and a
    /// held task that also changed here refuses as a conflict.
    ///
    /// # Errors
    /// Refuses a discontinuous head cut, an index gap, a changed held task,
    /// note inclusion the profile omits, a reference to a retired task, and
    /// every refusal of the complete export.
    pub fn plan_task_sync_folder_export(
        &self,
        project: &ProjectId,
        profile: &ExportProfile,
        rebuild: bool,
    ) -> Result<super::folder_head::FolderExportPlan, StoreError> {
        profile.options().validate()?;
        self.work_read_snapshot(|store| {
            with_origin_scope(&store.connection, || {
                plan_folder_export(store, project, profile, rebuild)
            })
        })
    }

    /// Record the next folder head if its predecessor is still current.
    ///
    /// # Errors
    /// Refuses a moved head with
    /// [`TaskSyncRefusal::BaselineHeadMoved`] and an invalid record.
    pub fn record_task_sync_folder_head(
        &mut self,
        head: &super::folder_head::FolderHead,
    ) -> Result<crate::ObjectId, StoreError> {
        let transaction = self.begin_work_mutation()?;
        let id = super::folder_head::append_on(&transaction, head)?;
        transaction.commit()?;
        Ok(id)
    }

    /// The project feed's current cut.
    ///
    /// # Errors
    /// Refuses an unreadable feed.
    pub fn task_sync_project_cut(&self, project: &ProjectId) -> Result<ExportCut, StoreError> {
        self.work_read_snapshot(|store| current_cut(store, &FeedId::Project(project.clone())))
    }

    /// The recorded folder head of `project` under `profile`, if any.
    ///
    /// # Errors
    /// Refuses a damaged projected head; repair is explicit.
    pub fn task_sync_folder_head(
        &self,
        project: &ProjectId,
        profile: &ExportProfile,
    ) -> Result<Option<(crate::ObjectId, super::folder_head::FolderHead)>, StoreError> {
        self.work_read_snapshot(|store| {
            super::folder_head::head_on(&store.connection, project, profile)
        })
    }
}

fn plan_folder_export(
    store: &SqliteStore,
    project: &ProjectId,
    profile: &ExportProfile,
    rebuild: bool,
) -> Result<super::folder_head::FolderExportPlan, StoreError> {
    let connection = &store.connection;
    let feed = FeedId::Project(project.clone());
    let cut = current_cut(store, &feed)?;
    let recorded = super::folder_head::head_on(connection, project, profile)?;
    // The bootstrap and an explicit, named rebuild export every task; the
    // rebuild keeps the recorded head as the predecessor it replaces.
    if recorded.is_none() || rebuild {
        let full = super::folder::folder_export_on(connection, project, profile)?;
        return Ok(super::folder_head::FolderExportPlan {
            head: recorded,
            cut,
            project: full.tree.project.clone(),
            changes: full
                .tree
                .tasks
                .into_iter()
                .map(|task| (task.id, Some(task)))
                .collect(),
            omissions: full.omissions,
            activity_omissions: full.activity_omissions,
            complete: true,
        });
    }
    let Some((id, head)) = recorded else {
        return Err(TaskSyncRefusal::IncrementalBasisUnavailable.invalid("no recorded folder head"));
    };
    if head.profile != *profile {
        return Err(TaskSyncRefusal::DivergentPlanning.invalid(
            "this export profile differs from the one the shared folder was recorded with; run `engram task-sync export --rebuild` to write the complete folder under it",
        ));
    }
    if let Err(reason) = anchored(store, &feed, &head.cut, cut.position) {
        return Err(TaskSyncRefusal::IncrementalBasisUnavailable.invalid(reason?));
    }
    super::baseline::validate_note_selection(connection, project, profile.include_notes)?;
    let changed = super::super::feed_tasks::changed_between(
        connection,
        project,
        head.cut.position,
        cut.position,
    )?;
    let conflicting = changed
        .intersection(&head.pending)
        .map(|id| id.0.to_string())
        .collect::<Vec<_>>();
    if !conflicting.is_empty() {
        return Err(TaskSyncRefusal::ImportConflict.invalid(format!(
            "a task held by the last import changed here too; resolve it before exporting: {}",
            conflicting.join(", ")
        )));
    }
    let retired = super::baseline::retired(connection, project)?;
    let mut omissions = NoteOmissions::default();
    let mut activity_omissions = NoteOmissions::default();
    let mut exported = std::collections::BTreeMap::new();
    for &work in changed.difference(&head.pending) {
        let member: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM work_items WHERE work_id = ?1 AND project_id = ?2)",
            rusqlite::params![work.0.to_string(), project.0],
            |row| row.get(0),
        )?;
        if !member || retired.contains(&work) {
            exported.insert(work, None);
            continue;
        }
        let item = super::super::query::load_work_item(connection, work)
            .map_err(super::origin::classify_binding)?;
        let (document, stored_inclusion) = super::capture::capture_document_on(
            connection,
            item,
            profile.include_notes,
            profile.origin_machine.as_deref(),
            false,
            &mut omissions,
            &mut activity_omissions,
        )?;
        if stored_inclusion && !profile.include_notes {
            return Err(TaskSyncRefusal::DivergentPlanning.invalid(
                "a changed task retains shared notes this export profile omits; rebuild the folder explicitly",
            ));
        }
        if let Some(target) = super::closure::graph::document_edges(&document)
            .into_iter()
            .map(|(_, target)| target)
            .find(|target| retired.contains(target))
        {
            return Err(TaskSyncRefusal::DivergentPlanning.invalid(format!(
                "task folder export would retain a reference to the exchange-retired task {}",
                target.0
            )));
        }
        exported.insert(work, Some(document));
    }
    Ok(super::folder_head::FolderExportPlan {
        head: Some((id, head)),
        cut,
        project: crate::task_sync::ProjectDocument {
            format: crate::task_sync::FORMAT.into(),
            project_id: project.clone(),
            notes_included: profile.include_notes,
        },
        changes: exported,
        omissions,
        activity_omissions,
        complete: false,
    })
}

/// The project feed's head and the object at it.
fn current_cut(store: &SqliteStore, feed: &FeedId) -> Result<ExportCut, StoreError> {
    let head = store.work_feed_head(feed)?;
    let anchor = if head == 0 {
        None
    } else {
        let entry = store.work_feed_between(feed, head - 1, head)?;
        Some(FeedAnchor {
            object_kind: entry[0].object_kind.clone(),
            object_id: entry[0].object_id.clone(),
        })
    };
    Ok(ExportCut {
        position: head,
        anchor,
    })
}

/// Whether `previous` lies on the feed: at or behind the head, with the
/// object it names still at its position. Reads one entry, whatever the
/// interval's length.
fn anchored(
    store: &SqliteStore,
    feed: &FeedId,
    previous: &ExportCut,
    head: i64,
) -> Result<(), Result<String, StoreError>> {
    if previous.position < 0 || previous.position > head {
        return Err(Ok("feed head moved behind prepared cut".into()));
    }
    if previous.position > 0 {
        let entries = interval(store, feed, previous.position - 1, previous.position)?;
        let entry = &entries[0];
        if previous.anchor.as_ref().is_none_or(|anchor| {
            anchor.object_kind != entry.object_kind || anchor.object_id != entry.object_id
        }) {
            return Err(Ok("prepared feed anchor differs".into()));
        }
    } else if previous.anchor.is_some() {
        return Err(Ok("invalid zero feed anchor".into()));
    }
    Ok(())
}

/// The full preparation's continuity: the anchor, then every entry of the
/// interval fetched and its canonical family decoded.
fn continuity(
    store: &SqliteStore,
    feed: &FeedId,
    previous: &ExportCut,
    head: i64,
) -> Result<(), Result<String, StoreError>> {
    anchored(store, feed, previous, head)?;
    let mut cursor = previous.position;
    while cursor < head {
        let through = cursor.saturating_add(1000).min(head);
        for entry in interval(store, feed, cursor, through)? {
            // Decode canonical families to validate continuity, including rows
            // without work_id. The complete inventory decides byte reuse and
            // also detects imports that add no project-feed row at all.
            let object = SqliteStore::get_canonical_object_on(
                &store.connection,
                &entry.object_id,
                &entry.object_kind,
            )
            .map_err(Err)?
            .ok_or_else(|| Ok("prepared feed object is missing".into()))?;
            match entry.object_kind.as_str() {
                crate::task_sync::origin::recovery::RECOVERY_KIND => {
                    let recovery: crate::task_sync::origin::recovery::RecoveryRecord =
                        object.decode().map_err(Err)?;
                    recovery.validate().map_err(Err)?;
                }
                crate::task_sync::origin::PRODUCING_ORIGIN_KIND => {
                    let _: crate::task_sync::origin::Enrollment = object.decode().map_err(Err)?;
                }
                "work_event" => {
                    let _: WorkEvent = object.decode().map_err(Err)?;
                }
                "work_evidence" => {
                    let _: WorkEvidence = object.decode().map_err(Err)?;
                }
                "work_restored_evidence" => {
                    let _: RestoredWorkEvidence = object.decode().map_err(Err)?;
                }
                "work_observation" => {
                    let _: WorkObservation = object.decode().map_err(Err)?;
                }
                _ => {}
            }
        }
        cursor = through;
    }
    Ok(())
}

fn interval(
    store: &SqliteStore,
    feed: &FeedId,
    after: i64,
    through: i64,
) -> Result<Vec<crate::domain::WorkFeedEntry>, Result<String, StoreError>> {
    store
        .work_feed_between(feed, after, through)
        .map_err(|error| match error {
            StoreError::InvalidWorkProjection(_) | StoreError::InvalidStoredKey(_) => {
                Ok("prepared feed interval is missing or not dense".into())
            }
            other => Err(other),
        })
}
