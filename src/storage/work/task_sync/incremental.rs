//! One-cut typed inventory and dense-feed continuity for local preparation.

use super::{SqliteStore, StoreError, export_on_snapshot, with_origin_scope};
use crate::task_sync::{
    TaskSyncRefusal,
    notes::NoteOmissions,
    progress::{ExportCapture, ExportCut, ExportProfile, FeedAnchor, TaskChangeCapture},
};
use crate::{
    ProjectId, RestoredWorkEvidence, WorkEvent, WorkEvidence, WorkId,
    domain::{FeedId, WorkObservation},
};
use std::collections::{BTreeMap, BTreeSet};

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
        self.plan_task_sync_folder_export_with_confirmation(project, profile, rebuild, None)
    }

    pub(crate) fn plan_task_sync_folder_export_with_confirmation(
        &self,
        project: &ProjectId,
        profile: &ExportProfile,
        rebuild: bool,
        confirmation: Option<&super::folder_head::FolderExportConfirmation>,
    ) -> Result<super::folder_head::FolderExportPlan, StoreError> {
        profile.options().validate()?;
        self.work_read_snapshot(|store| {
            with_origin_scope(&store.connection, || {
                plan_folder_export(store, project, profile, rebuild, confirmation)
            })
        })
    }

    /// [`Self::plan_task_sync_folder_export`] and the memory component's
    /// capture on the same snapshot, so that both components describe one
    /// store state.
    ///
    /// # Errors
    /// Every refusal of the plan and of the memory capture.
    pub fn plan_task_sync_folder_export_with_memories(
        &self,
        project: &ProjectId,
        profile: &ExportProfile,
        rebuild: bool,
        selected: &BTreeMap<String, crate::task_sync::memories::KeyHistory>,
        resolutions: &BTreeMap<String, crate::task_sync::memories::ImportOptions>,
    ) -> Result<
        (
            super::folder_head::FolderExportPlan,
            crate::task_sync::memories::ExportOutcome,
        ),
        StoreError,
    > {
        self.plan_task_sync_folder_export_with_memory_confirmation(
            project,
            profile,
            rebuild,
            selected,
            resolutions,
            None,
        )
    }

    pub(crate) fn plan_task_sync_folder_export_with_memory_confirmation(
        &self,
        project: &ProjectId,
        profile: &ExportProfile,
        rebuild: bool,
        selected: &BTreeMap<String, crate::task_sync::memories::KeyHistory>,
        resolutions: &BTreeMap<String, crate::task_sync::memories::ImportOptions>,
        confirmation: Option<&super::folder_head::FolderExportConfirmation>,
    ) -> Result<
        (
            super::folder_head::FolderExportPlan,
            crate::task_sync::memories::ExportOutcome,
        ),
        StoreError,
    > {
        // Both reads join this one read transaction.
        self.work_read_snapshot(|store| {
            Ok((
                store.plan_task_sync_folder_export_with_confirmation(
                    project,
                    profile,
                    rebuild,
                    confirmation,
                )?,
                store.export_task_sync_memories(project, profile, selected, resolutions)?,
            ))
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

    /// Admit the new head and any preceding exact confirmation together. The
    /// service retains the corresponding write only after the database admits
    /// both records.
    pub(crate) fn record_task_sync_folder_publication(
        &mut self,
        mut head: Option<super::folder_head::FolderHead>,
        confirmation: Option<&super::folder_head::FolderExportConfirmation>,
        now: chrono::DateTime<chrono::Utc>,
        publish: impl FnOnce(
            Option<(&crate::ObjectId, &super::folder_head::FolderHead)>,
        ) -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let transaction = self.begin_work_mutation()?;
        if let Some(proof) = confirmation {
            let previous = confirm_folder_export_on(&transaction, proof, now)?;
            if let Some(head) = head.as_mut() {
                head.previous = Some(previous);
            }
        }
        let id = head
            .as_ref()
            .map(|head| super::folder_head::append_on(&transaction, head))
            .transpose()?;
        publish(id.as_ref().zip(head.as_ref()))?;
        transaction.commit().map_err(StoreError::from)?;
        Ok(())
    }

    /// Record a verified own export commit and the documents it published as
    /// one comparison basis. Documents of held tasks stay at their last
    /// admitted value until an import applies them.
    ///
    /// # Errors
    /// Refuses a moved or discontinuous folder head and invalid records.
    pub(crate) fn confirm_task_sync_folder_export(
        &mut self,
        proof: &super::folder_head::FolderExportConfirmation,
        dry_run: bool,
        now: chrono::DateTime<chrono::Utc>,
        check_tip: impl FnOnce() -> anyhow::Result<()>,
    ) -> anyhow::Result<()> {
        let transaction = self.begin_work_mutation()?;
        confirm_folder_export_on(&transaction, proof, now)?;
        check_tip()?;
        if !dry_run {
            transaction.commit().map_err(StoreError::from)?;
        }
        Ok(())
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

    /// A journal may name an earlier record after an explicit import. Do
    /// not grant recovery authority from the key alone: it only lets an
    /// ordinary operation skip an already displaced payload.
    pub(crate) fn task_sync_folder_head_is_displaced(
        &self,
        project: &ProjectId,
        id: &crate::ObjectId,
        fingerprint: &str,
    ) -> Result<bool, StoreError> {
        use super::folder_head::{FOLDER_HEAD_KIND, FolderHead};
        self.work_read_snapshot(|store| {
            let active: bool = store.connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM work_task_sync_folder_heads
                 WHERE project_id = ?1 AND record_id = ?2)",
                rusqlite::params![project.0, id.as_str()],
                |row| row.get(0),
            )?;
            if active {
                return Ok(false);
            }
            let Some(record) =
                SqliteStore::get_canonical_object_on(&store.connection, id, FOLDER_HEAD_KIND)?
            else {
                return Ok(false);
            };
            Ok(
                crate::ObjectId::from_canonical_bytes(record.bytes()).as_str() == fingerprint
                    && record.decode::<FolderHead>()?.project_id == *project,
            )
        })
    }

    pub(crate) fn task_sync_folder_head_was_recorded(
        &self,
        id: &crate::ObjectId,
        planned: &super::folder_head::FolderHead,
    ) -> Result<bool, StoreError> {
        use super::folder_head::{FOLDER_HEAD_KIND, FolderHead};
        self.work_read_snapshot(|store| {
            Ok(SqliteStore::get_typed_object_on::<FolderHead>(
                &store.connection,
                id,
                FOLDER_HEAD_KIND,
            )?
            .as_ref()
                == Some(planned))
        })
    }

    /// A rebuild can reuse a proved Git commit only if its saved SQLite
    /// cut still belongs to this feed. Confirmation rechecks in its transaction.
    pub(crate) fn task_sync_folder_cut_is_continuous(
        &self,
        project: &ProjectId,
        previous: &ExportCut,
    ) -> Result<bool, StoreError> {
        self.work_read_snapshot(|store| {
            let head = super::producing_origin::cut_on(&store.connection, project)?;
            cut_is_continuous(&store.connection, project, previous, head.position)
        })
    }

    pub(crate) fn task_sync_folder_import_basis(
        &self,
        head: &super::folder_head::FolderHead,
    ) -> Result<Option<super::folder_head::FolderSource>, StoreError> {
        self.work_read_snapshot(|store| {
            super::folder_head::import_basis_on(&store.connection, head)
        })
    }
}

pub(super) fn confirm_folder_export_on(
    transaction: &rusqlite::Transaction<'_>,
    proof: &super::folder_head::FolderExportConfirmation,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<crate::ObjectId, StoreError> {
    use super::{baseline, folder_head, producing_origin, reconciliation};
    crate::task_sync::ledger::oid(&proof.commit)?;
    let mut head = proof.comparison_head();
    let project = &head.project_id;
    let current = folder_head::head_on(transaction, project, &head.profile)?;
    if !current.is_some_and(|(id, current)| id == proof.expected && current == proof.head) {
        return Err(TaskSyncRefusal::BaselineHeadMoved
            .invalid("another export or import recorded the shared folder first; export again"));
    }

    let cut = producing_origin::cut_on(transaction, project)?;
    require_continuous(transaction, project, &head.cut, cut.position)?;
    head.unexported
        .extend(super::super::feed_tasks::changed_between(
            transaction,
            project,
            head.cut.position,
            cut.position,
        )?);

    let eligible = proof
        .changes
        .iter()
        .filter(|(id, _)| !head.pending.contains_key(*id));
    let prior = baseline::entries(transaction, project, eligible.clone().map(|(id, _)| *id))?;
    let changes = eligible
        .filter(|(id, document)| {
            prior.get(*id).and_then(|entry| entry.document.as_ref()) != document.as_ref()
        })
        .map(|(id, document)| (*id, document.clone()))
        .collect::<BTreeMap<_, _>>();
    if !changes.is_empty() {
        let previous = baseline::head(transaction, project)?.map(|head| head.record);
        let advanced = changes.keys().copied().collect::<Vec<_>>();
        baseline::append(
            transaction,
            project,
            previous,
            changes,
            None,
            &head.profile,
            now,
        )?;
        reconciliation::advance_on(transaction, project, advanced, &prior)?;
    }

    head.cut = producing_origin::cut_on(transaction, project)?;
    head.previous = Some(proof.expected.clone());
    head.reason = folder_head::FolderHeadReason::Export;
    folder_head::append_on(transaction, &head)
}

fn plan_folder_export(
    store: &SqliteStore,
    project: &ProjectId,
    profile: &ExportProfile,
    rebuild: bool,
    confirmation: Option<&super::folder_head::FolderExportConfirmation>,
) -> Result<super::folder_head::FolderExportPlan, StoreError> {
    let connection = &store.connection;
    let feed = FeedId::Project(project.clone());
    let cut = current_cut(store, &feed)?;
    let mut recorded = super::folder_head::head_on(connection, project, profile)?;
    if let Some(proof) = confirmation {
        if !recorded
            .as_ref()
            .is_some_and(|(id, head)| id == &proof.expected && head == &proof.head)
        {
            return Err(TaskSyncRefusal::BaselineHeadMoved.invalid(
                "another export or import recorded the shared folder first; export again",
            ));
        }
        recorded = Some((proof.expected.clone(), proof.comparison_head()));
    }
    // The bootstrap and an explicit, named rebuild export every task; the
    // rebuild keeps the recorded head as the predecessor it replaces.
    if recorded.is_none() || rebuild {
        let full = super::folder::folder_export_on(connection, project, profile)?;
        let conflicted = match &recorded {
            Some((_, head)) => held_changed(
                connection,
                project,
                head.profile.include_notes,
                &head.pending,
                head.pending.keys().copied(),
            )?,
            None => BTreeSet::new(),
        };
        // A held task the rebuild keeps with the other host's bytes stays
        // owed, whatever changed here meanwhile that does not conflict.
        let held_owed = recorded.as_ref().map_or_else(BTreeSet::new, |(_, head)| {
            head.pending
                .keys()
                .filter(|id| !conflicted.contains(*id))
                .copied()
                .collect()
        });
        return Ok(super::folder_head::FolderExportPlan {
            conflicted,
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
            held_owed,
        });
    }
    let Some((id, head)) = recorded else {
        return Err(TaskSyncRefusal::IncrementalBasisUnavailable.invalid("no recorded folder head"));
    };
    // The note selection is shared by every host; the origin-machine alias is
    // this host's own and may differ from the one an enrolment recorded.
    if head.profile.include_notes != profile.include_notes {
        return Err(TaskSyncRefusal::DivergentPlanning.invalid(
            "this export's note selection differs from the one the shared folder was recorded with; run `engram task-sync export --rebuild` to write the complete folder under it",
        ));
    }
    // The alias this host last exported under is written into its notes and
    // activity; an incremental export under another one would leave the
    // unchanged tasks under the old alias.
    if let Some(previous) = &head.profile.origin_machine
        && profile.origin_machine.as_ref() != Some(previous)
    {
        return Err(TaskSyncRefusal::DivergentPlanning.invalid(format!(
            "this export's origin-machine alias ({}) differs from the one this host exported the shared folder under ({previous}); export with `--origin-machine {previous}`, or run `engram task-sync export --rebuild` to write the complete folder under the new alias",
            profile.origin_machine.as_deref().unwrap_or("none")
        )));
    }
    if let Err(reason) = anchored(store, &feed, &head.cut, cut.position) {
        return Err(TaskSyncRefusal::IncrementalBasisUnavailable.invalid(format!(
            "{}; run `engram task-sync export --rebuild` to write the complete folder and record it afresh",
            reason?
        )));
    }
    super::baseline::validate_note_selection(connection, project, profile.include_notes)?;
    // The changes since the head, and the local changes an import passed over.
    let mut changed = super::super::feed_tasks::changed_between(
        connection,
        project,
        head.cut.position,
        cut.position,
    )?;
    changed.extend(head.unexported.iter().copied());
    // A held task keeps the other host's bytes. One whose local planning
    // changed since it was held conflicts with them; a claim or its release
    // changes no planning.
    let pending_tasks = head.pending.keys().copied().collect::<BTreeSet<_>>();
    let conflicting = held_changed(
        connection,
        project,
        head.profile.include_notes,
        &head.pending,
        changed.intersection(&pending_tasks).copied(),
    )?
    .into_iter()
    .map(|id| id.0.to_string())
    .collect::<Vec<_>>();
    if !conflicting.is_empty() {
        return Err(TaskSyncRefusal::ImportConflict.invalid(format!(
            "a task held by the last import changed here too, so this host's version and the other host's held one conflict: {}. Apply the held edits from the folder to this host's planning first, then `engram task-sync export --rebuild` writes this host's version over the held one and lists it as superseded; a held edit not applied here is lost. To keep the other host's version as it is, undo the local change and import again",
            conflicting.join(", ")
        )));
    }
    // Keyed reads for the changed tasks and, below, their edge endpoints:
    // never the project's whole removal history.
    let retired = super::baseline::retired_among(
        connection,
        project,
        changed.difference(&pending_tasks).copied(),
    )?;
    let mut omissions = NoteOmissions::default();
    let mut activity_omissions = NoteOmissions::default();
    let mut exported = std::collections::BTreeMap::new();
    for &work in changed.difference(&pending_tasks) {
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
        if let Some(target) = super::baseline::retired_among(
            connection,
            project,
            super::closure::graph::document_edges(&document)
                .into_iter()
                .map(|(_, target)| target),
        )?
        .into_iter()
        .next()
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
        conflicted: BTreeSet::new(),
        // A held task keeps the other host's bytes; what changed here
        // without conflicting stays owed until the hold resolves.
        held_owed: changed.intersection(&pending_tasks).copied().collect(),
    })
}

const FINGERPRINT_ALIAS: &str = "fingerprint";

/// The content fingerprint of each task's local planning document, `None`
/// for a task this host does not hold or the exchange retired. Only the
/// governing fields count: shared notes and activity only ever add to a
/// task, so a note, a claim or its release leaves the fingerprint unchanged
/// and never conflicts with another host's held edit.
pub(super) fn planning_fingerprints(
    connection: &rusqlite::Connection,
    project: &ProjectId,
    include_notes: bool,
    ids: impl IntoIterator<Item = WorkId>,
) -> Result<BTreeMap<WorkId, Option<String>>, StoreError> {
    let ids = ids.into_iter().collect::<BTreeSet<_>>();
    let retired = super::baseline::retired_among(connection, project, ids.iter().copied())?;
    let mut fingerprints = BTreeMap::new();
    for id in ids {
        let member: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM work_items WHERE work_id = ?1 AND project_id = ?2)",
            rusqlite::params![id.0.to_string(), project.0],
            |row| row.get(0),
        )?;
        if !member || retired.contains(&id) {
            fingerprints.insert(id, None);
            continue;
        }
        let item = super::super::query::load_work_item(connection, id)
            .map_err(super::origin::classify_binding)?;
        // Shared notes carry an origin alias. The fingerprint compares this
        // host's planning only with itself, so one fixed alias, independent
        // of any export profile, keeps every fingerprint comparable; a note
        // already retained keeps its own alias in this comparison capture.
        let (mut document, _) = super::capture::capture_document_on(
            connection,
            item,
            include_notes,
            Some(FINGERPRINT_ALIAS),
            true,
            &mut NoteOmissions::default(),
            &mut NoteOmissions::default(),
        )?;
        document.notes.clear();
        document.activity.clear();
        let bytes = serde_json::to_vec(&document)
            .map_err(|error| StoreError::InvalidWorkProjection(error.to_string()))?;
        fingerprints.insert(
            id,
            Some(crate::ObjectId::from_canonical_bytes(&bytes).to_string()),
        );
    }
    Ok(fingerprints)
}

/// The held tasks among `ids` whose local planning document no longer has
/// the fingerprint it had when the last import held them.
pub(super) fn held_changed(
    connection: &rusqlite::Connection,
    project: &ProjectId,
    include_notes: bool,
    pending: &BTreeMap<WorkId, Option<String>>,
    ids: impl IntoIterator<Item = WorkId>,
) -> Result<BTreeSet<WorkId>, StoreError> {
    let current = planning_fingerprints(
        connection,
        project,
        include_notes,
        ids.into_iter().filter(|id| pending.contains_key(id)),
    )?;
    Ok(current
        .into_iter()
        .filter(|(id, fingerprint)| pending.get(id) != Some(fingerprint))
        .map(|(id, _)| id)
        .collect())
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

/// The import's form of [`anchored`], inside its write transaction: the
/// folder head's cut must lie on this project's feed, at or behind `head`,
/// with the object it names still at its position, or the interval read from
/// it would be meaningless.
///
/// # Errors
/// Refuses a cut off the feed by name, naming the complete route.
pub(super) fn require_continuous(
    connection: &rusqlite::Connection,
    project: &ProjectId,
    previous: &ExportCut,
    head: i64,
) -> Result<(), StoreError> {
    if cut_is_continuous(connection, project, previous, head)? {
        Ok(())
    } else {
        Err(TaskSyncRefusal::IncrementalBasisUnavailable.invalid(
            "the folder head's cut is not on this store's project feed; import the complete folder with `engram task-sync import --rebuild --basis OWN_EXPORT/project.json`",
        ))
    }
}

fn cut_is_continuous(
    connection: &rusqlite::Connection,
    project: &ProjectId,
    previous: &ExportCut,
    head: i64,
) -> Result<bool, StoreError> {
    Ok(previous.position >= 0
        && previous.position <= head
        && match &previous.anchor {
            None => previous.position == 0,
            Some(anchor) => {
                previous.position > 0
                    && connection.query_row(
                        "SELECT EXISTS(SELECT 1 FROM work_feed_entries
                         WHERE feed_kind = 'project' AND feed_id = ?1 AND position = ?2
                           AND object_kind = ?3 AND object_id = ?4)",
                        rusqlite::params![
                            project.0,
                            previous.position,
                            anchor.object_kind,
                            anchor.object_id.as_str()
                        ],
                        |row| row.get::<_, bool>(0),
                    )?
            }
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
