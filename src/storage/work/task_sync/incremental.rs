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
