//! The ordinary folder imports and the folder head they record.
//!
//! An incremental import reads its changed task documents from a verified
//! Git delta between the folder head's source commit and the incoming one,
//! together with the tasks the last import held, read again at the incoming
//! commit. They are admitted per task, like a complete folder import: a task
//! that cannot take its change is held or refused by name while the others
//! apply. Both routes record the next folder head in the import's own
//! transaction, with the cut read there, so nothing committed between the
//! import and its head can be mistaken for part of the folder. Local changes
//! the import passed over stay recorded for the next export.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};

use super::{
    baseline,
    closure::graph,
    folder_head::{self, FolderDisk, FolderHead, FolderHeadReason, FolderSource},
    origin, producing_origin, reconciliation, selected,
};
use crate::{
    ActorContext, ObjectId, ProjectId, WorkId,
    storage::{
        SqliteStore, StoreError, TaskSyncFolderApply, TaskSyncImportResult, TaskSyncTaskResult,
        TaskSyncTaskStatus,
    },
    task_sync::{TaskDocument, TaskSyncRefusal, inventory::InventoryRoot, progress::ExportProfile},
};

/// One complete (enrolment or rebuild) folder import and the folder head it
/// records in the same transaction.
pub struct FolderCompleteApply<'a> {
    pub apply: TaskSyncFolderApply<'a>,
    pub idempotency_key: &'a str,
    /// The folder head this import replaces, compared and set.
    pub previous: Option<&'a ObjectId>,
    /// The commit the folder holds, when it is committed.
    pub commit: Option<String>,
    pub inventory_root: InventoryRoot,
    pub task_count: u64,
    /// The encoded bytes of the folder's task files.
    pub task_bytes: u64,
    pub rebuild: bool,
    /// The selected memory component, imported after the tasks in the same
    /// transaction, with its per-key options.
    pub memories: &'a BTreeMap<String, crate::task_sync::memories::KeyHistory>,
    pub memory_options: &'a BTreeMap<String, crate::task_sync::memories::ImportOptions>,
}

/// What a folder import applied: its tasks, the head it recorded, and each
/// selected memory key's result, `memory_partial` when any key was not
/// applied.
#[derive(Debug)]
pub struct FolderImportApplied {
    pub result: TaskSyncImportResult,
    pub head: Option<FolderHead>,
    pub memories: Vec<crate::task_sync::memories::KeyImportResult>,
    pub memory_partial: bool,
}

/// One incremental folder import, read from a verified Git delta.
pub struct FolderDeltaApply<'a> {
    pub project: &'a ProjectId,
    pub actor: &'a ActorContext,
    pub profile: &'a ExportProfile,
    /// The folder head the delta was read against, compared and set.
    pub head: &'a ObjectId,
    /// The incoming commit with the root and count the delta derived for it.
    pub incoming: FolderSource,
    /// Every changed task path, with its incoming document or `None`.
    pub changes: &'a BTreeMap<WorkId, Option<TaskDocument>>,
    /// Tasks this host exported since its last import that the incoming
    /// commit holds exactly as this host wrote them: shared from now on, they
    /// join the baseline with these documents.
    pub shared: &'a BTreeMap<WorkId, TaskDocument>,
    /// The tasks whose files the last export wrote that no commit since its
    /// base holds, as when discarded before a commit.
    pub discarded: &'a BTreeSet<WorkId>,
    /// What this host last agreed for the tasks it exported since the last
    /// import, whatever the baseline, which exports leave where it was, still
    /// holds: the document a commit published from one of those exports, or,
    /// for a task whose last write was discarded, its bytes at the commit
    /// that write was written onto; `None` for a removal.
    pub written: &'a BTreeMap<WorkId, Option<TaskDocument>>,
    /// The selected memory component, imported after the tasks in the same
    /// transaction, with its per-key options.
    pub memories: &'a BTreeMap<String, crate::task_sync::memories::KeyHistory>,
    pub memory_options: &'a BTreeMap<String, crate::task_sync::memories::ImportOptions>,
    pub dry_run: bool,
    pub now: DateTime<Utc>,
}

/// What an incremental folder import applied, and the head it recorded.
#[derive(Debug)]
pub struct FolderDeltaOutcome {
    pub result: TaskSyncImportResult,
    pub head: FolderHead,
    pub memories: Vec<crate::task_sync::memories::KeyImportResult>,
    pub memory_partial: bool,
}

impl SqliteStore {
    /// Apply one incremental folder import in one transaction.
    ///
    /// # Errors
    /// Refuses a moved or missing folder head, a head with no source commit,
    /// local planning changed since the head for a task the change touches
    /// (remedy: export and commit it first), and every admission refusal.
    pub fn apply_task_sync_folder_delta(
        &mut self,
        request: &FolderDeltaApply<'_>,
    ) -> Result<FolderDeltaOutcome, StoreError> {
        self.apply_task_sync_folder_delta_with_confirmation(request, None, || Ok(()))
    }

    pub(crate) fn apply_task_sync_folder_delta_with_confirmation<E: From<StoreError>>(
        &mut self,
        request: &FolderDeltaApply<'_>,
        proof: Option<&super::folder_head::FolderExportConfirmation>,
        check_tip: impl FnOnce() -> Result<(), E>,
    ) -> Result<FolderDeltaOutcome, E> {
        super::folder::validate_memory_selection(
            request.project,
            request.memories,
            request.memory_options,
        )?;
        let transaction = self.begin_work_mutation()?;
        let confirmed;
        let confirmed_request;
        let request = if let Some(proof) = proof {
            confirmed =
                super::incremental::confirm_folder_export_on(&transaction, proof, request.now)?;
            confirmed_request = FolderDeltaApply {
                head: &confirmed,
                incoming: request.incoming.clone(),
                ..*request
            };
            &confirmed_request
        } else {
            request
        };
        let mut outcome =
            origin::with_origin_scope(&transaction, || apply_on(&transaction, request))?;
        // Memories follow the tasks they may depend on, in the same
        // transaction and under the same dry-run rollback.
        (outcome.memories, outcome.memory_partial) = super::folder::import_memories_on(
            &transaction,
            request.project,
            request.actor,
            request.now,
            request.memories,
            request.memory_options,
            request.dry_run,
        )?;
        check_tip()?;
        if request.dry_run {
            // Dropping the transaction rolls every effect back.
            return Ok(outcome);
        }
        transaction.commit().map_err(StoreError::from)?;
        Ok(outcome)
    }

    /// Apply one complete folder import and record its folder head in one
    /// transaction; none when the project still has no baseline, because
    /// nothing applied.
    ///
    /// # Errors
    /// Refuses a moved folder head and every refusal of the complete import.
    pub fn apply_task_sync_folder_complete(
        &mut self,
        request: FolderCompleteApply<'_>,
    ) -> Result<FolderImportApplied, StoreError> {
        self.apply_task_sync_folder_complete_checked(request, || Ok(()))
    }

    pub(crate) fn apply_task_sync_folder_complete_checked<E: From<StoreError>>(
        &mut self,
        request: FolderCompleteApply<'_>,
        check_tip: impl FnOnce() -> Result<(), E>,
    ) -> Result<FolderImportApplied, E> {
        super::folder::validate_memory_selection(
            request.apply.project,
            request.memories,
            request.memory_options,
        )?;
        let transaction = self.begin_work_mutation()?;
        let project = request.apply.project.clone();
        let profile = request.apply.profile.clone();
        let (actor, now) = (request.apply.actor, request.apply.now);
        // The head keeps the alias this host last exported under, never the
        // alias of the folder or basis an import read, which may be another
        // host's.
        let (carried, exported_alias) = match request.previous {
            Some(id) => {
                let (current, head) = folder_head::head_on(&transaction, &project, &profile)?
                    .ok_or_else(|| {
                        TaskSyncRefusal::BaselineHeadMoved.invalid(
                            "another export or import recorded the shared folder first; import again",
                        )
                    })?;
                if current != *id {
                    return Err(TaskSyncRefusal::BaselineHeadMoved.invalid(
                        "another export or import recorded the shared folder first; import again",
                    ).into());
                }
                (head.unexported, head.profile.origin_machine)
            }
            None => (BTreeSet::new(), None),
        };
        let incoming = request.apply.tree;
        let result = super::folder::apply_task_sync_folder_on(
            &transaction,
            request.apply,
            request.idempotency_key,
        )?;
        // Memories follow the tasks they may depend on, in the same
        // transaction.
        let (memories, memory_partial) = super::folder::import_memories_on(
            &transaction,
            &project,
            actor,
            now,
            request.memories,
            request.memory_options,
            false,
        )?;
        // A first import that applied nothing, every change held, records no
        // baseline, and an incremental import admits against one. It records
        // no head either: the next import is the same complete one.
        if super::baseline::head(&transaction, &project)?.is_none() {
            check_tip()?;
            transaction.commit().map_err(StoreError::from)?;
            return Ok(FolderImportApplied {
                result,
                head: None,
                memories,
                memory_partial,
            });
        }
        let pending = super::incremental::planning_fingerprints(
            &transaction,
            &project,
            profile.include_notes,
            result
                .tasks
                .iter()
                .filter(|task| task.status != TaskSyncTaskStatus::Applied)
                .map(|task| task.task),
        )?;
        // A local task the folder leaves out or holds differently, such as one
        // the import retained as this host's own, stays owed to the next
        // export although the cut moves past it. The complete route reads
        // every task anyway.
        let folder = incoming
            .tasks
            .iter()
            .map(|task| (task.id, task))
            .collect::<BTreeMap<_, _>>();
        let mut unexported = carried;
        for task in super::folder::folder_export_on(&transaction, &project, &profile)?
            .tree
            .tasks
        {
            if !pending.contains_key(&task.id)
                && folder.get(&task.id).is_none_or(|shared| **shared != task)
            {
                unexported.insert(task.id);
            }
        }
        // Read inside this transaction: nothing committed after the import
        // can fall inside the cut the head records.
        let cut = producing_origin::cut_on(&transaction, &project)?;
        let next = FolderHead {
            project_id: project,
            profile: ExportProfile {
                origin_machine: exported_alias,
                ..profile
            },
            previous: request.previous.cloned(),
            reason: if request.rebuild {
                FolderHeadReason::Rebuild
            } else {
                FolderHeadReason::Import
            },
            cut,
            inventory_root: request.inventory_root,
            task_count: request.task_count,
            task_bytes: request.task_bytes,
            source: request.commit.clone().map(|commit| FolderSource {
                commit,
                inventory_root: request.inventory_root,
                task_count: request.task_count,
                task_bytes: request.task_bytes,
            }),
            disk: request
                .commit
                .map_or(FolderDisk::Unbound, |commit| FolderDisk::Committed {
                    commit,
                }),
            pending,
            unexported,
            // The complete folder was read; nothing earlier is owed a reread.
            exported: BTreeMap::new(),
            rewritten: false,
        };
        folder_head::append_on(&transaction, &next)?;
        check_tip()?;
        transaction.commit().map_err(StoreError::from)?;
        Ok(FolderImportApplied {
            result,
            head: Some(next),
            memories,
            memory_partial,
        })
    }
}

fn apply_on(
    transaction: &rusqlite::Transaction<'_>,
    request: &FolderDeltaApply<'_>,
) -> Result<FolderDeltaOutcome, StoreError> {
    let project = request.project;
    let (id, head) = folder_head::head_on(transaction, project, request.profile)?.ok_or_else(|| {
        TaskSyncRefusal::IncrementalBasisUnavailable.invalid(
            "no recorded folder head; enrol with `engram task-sync import --basis` or `--empty-basis` first",
        )
    })?;
    if id != *request.head {
        return Err(TaskSyncRefusal::BaselineHeadMoved
            .invalid("another export or import recorded the shared folder first; import again"));
    }
    if folder_head::import_basis_on(transaction, &head)?.is_none() {
        return Err(TaskSyncRefusal::IncrementalBasisUnavailable.invalid(
            "the folder head records no imported commit yet: commit the folder, then import it completely once with `engram task-sync import --rebuild --basis OWN_EXPORT/project.json`",
        ));
    }
    let cut = producing_origin::cut_on(transaction, project)?;
    super::incremental::require_continuous(transaction, project, &head.cut, cut.position)?;
    // Every local change not yet in the folder: those since the head, and
    // those an earlier import passed over.
    let mut local = super::super::feed_tasks::changed_between(
        transaction,
        project,
        head.cut.position,
        cut.position,
    )?;
    local.extend(head.unexported.iter().copied());
    // The files the last export wrote reach the folder only through a commit.
    // One that no commit since holds, as when discarded, stays owed to the
    // next export; a change the commit makes to it meets that local change
    // like any other. One a commit holds was published, even when a later
    // commit changed it again.
    if matches!(head.disk, FolderDisk::Written { .. }) {
        local.extend(request.discarded.iter().copied());
    }
    // A held task whose governing planning is still what it was when held
    // changed nothing that conflicts here: its claim, release or note of this
    // host's own only adds to it. Such a change stays owed to the export once
    // the hold resolves, and never refuses the import.
    let changed_held = super::incremental::held_changed(
        transaction,
        project,
        request.profile.include_notes,
        &head.pending,
        local.iter().copied(),
    )?;
    let mut changes = request.changes.clone();
    let mut set_aside = Vec::new();
    let mut kept = BTreeMap::new();
    // A held task changed here too conflicts with the other host's bytes. It
    // stays held with its fingerprint at hold time and owed to the export,
    // which names how to resolve it; the rest of the import goes ahead.
    for &task in &changed_held {
        changes.remove(&task);
        kept.insert(task, head.pending.get(&task).cloned().flatten());
        set_aside.push(TaskSyncTaskResult {
            task,
            status: TaskSyncTaskStatus::Held,
            reason: Some("local_planning_changed".into()),
            detail: Some(
                "this host changed the task's planning while another host's edit was held: apply the held edits to this host's planning first, because `engram task-sync export --rebuild` writes this host's version over the held one and any held edit not applied here is lost".into(),
            ),
        });
    }
    let previous = baseline::entries(transaction, project, changes.keys().copied())?;
    let mut unpublished = Vec::new();
    let mut collisions = Vec::new();
    for (task, document) in changes.clone() {
        if let Some(entry) = previous.get(&task) {
            // The baseline already holds this task as removed, so removing it
            // again changes nothing, as when another host's file returned a
            // retired task, its addition was refused and held, and that file
            // left the folder again: nothing to remove, and nothing held.
            if document.is_none() && entry.document.is_none() {
                changes.remove(&task);
            }
            continue;
        }
        let owner = connection_project(transaction, task)?;
        match (document, owner) {
            // The other host removed a task this host never applied, such as
            // a held addition: nothing here to remove, and nothing held.
            (None, None) => {
                changes.remove(&task);
            }
            // Nor is there anything to remove of a task another project
            // holds, such as an addition refused for its id.
            (None, Some(owner)) if owner != project.0 => {
                changes.remove(&task);
            }
            // A task of this host's that neither the baseline nor an export
            // since the last import shares was never published from here, as
            // when its only folder file was a discarded loose edit: it stays
            // this host's own, owed to the next export.
            (None, Some(_)) if !head.exported.contains_key(&task) => {
                changes.remove(&task);
                unpublished.push(task);
            }
            // An addition whose id another project already holds.
            (Some(_), Some(owner)) if owner != project.0 => {
                collisions.push((task, owner));
            }
            _ => {}
        }
    }
    // So does a task whose parent, prerequisite or replacement is such a
    // task, as a task the last import held beside one: its link would seed
    // another project's task into this project's graph.
    let mut links = Vec::new();
    for (task, document) in &changes {
        let Some(document) = document else {
            continue;
        };
        for (_, target) in graph::document_edges(document) {
            if changes.contains_key(&target)
                || collisions.iter().any(|(colliding, _)| *colliding == target)
            {
                continue;
            }
            if let Some(owner) = connection_project(transaction, target)?
                && owner != project.0
            {
                links.push((*task, target, owner));
            }
        }
    }
    // Such a task means the folder carries another project's task, as when it
    // was bound to the wrong project. A task the last import held is read
    // again at every import, so this holds until the folder no longer carries
    // it: the incremental route takes none of the folder meanwhile, before
    // anything is written, and names both exits.
    if !collisions.is_empty() || !links.is_empty() {
        let names = collisions
            .iter()
            .map(|(task, owner)| format!("{} (held by project {owner})", task.0))
            .chain(links.iter().map(|(task, target, owner)| {
                format!(
                    "{}, which names {} (held by project {owner})",
                    task.0, target.0
                )
            }))
            .collect::<Vec<_>>()
            .join(", ");
        return Err(TaskSyncRefusal::IdCollision.invalid(format!(
            "the folder at the incoming commit holds {names}: a task whose id another project in this store already holds, or one that names such a task, so the incremental import takes none of the folder and nothing was imported. Remove those tasks, and every task that names them, from the folder and commit; until then, import with `engram task-sync import --rebuild --basis OWN_EXPORT/project.json`, whose complete import refuses each such task by name, holds the tasks that depend on it and applies the rest"
        )));
    }
    let mut seeds = changes.keys().copied().collect::<BTreeSet<_>>();
    for document in changes.values().flatten() {
        seeds.extend(
            graph::document_edges(document)
                .into_iter()
                .map(|(_, target)| target),
        );
    }
    // What this host published since the last import is what it last agreed
    // with the folder, so the baseline takes it first, as this host wrote it,
    // also for a task it already holds at older bytes: every task this host
    // published, whatever the incoming commit made of it since, and every one
    // that commit holds as this host wrote it. Admission then judges against
    // this host's last published folder, a whole graph, never a mixture of
    // those documents with older ones: another host's change of such a task
    // is judged against what this host published, one that links to such a
    // task, such as a new child of it, finds it, and every later import
    // compares this host's planning with what it published. A task the last
    // import held keeps the baseline it was held at.
    let published_documents = request
        .written
        .iter()
        .filter_map(|(id, document)| document.as_ref().map(|document| (*id, document)))
        .chain(request.shared.iter().map(|(id, document)| (*id, document)))
        .filter(|(id, _)| !head.pending.contains_key(id))
        .collect::<BTreeMap<_, _>>();
    let shared_known =
        baseline::entries(transaction, project, published_documents.keys().copied())?;
    let mut joining_shared = published_documents
        .into_iter()
        .filter(|(id, document)| {
            shared_known
                .get(id)
                .is_none_or(|entry| entry.document.as_ref() != Some(*document))
        })
        .map(|(id, document)| (id, Some(document.clone())))
        .collect::<BTreeMap<_, _>>();
    // A document whose parent or other link still names a task neither the
    // baseline nor this join holds would leave the baseline with a dangling
    // link: it joins with the recorded changes after admission, which bring
    // that task in.
    let mut deferred_shared = BTreeMap::new();
    loop {
        let linked = joining_shared
            .values()
            .flatten()
            .flat_map(graph::document_edges)
            .map(|(_, target)| target)
            .filter(|target| !joining_shared.contains_key(target))
            .collect::<BTreeSet<_>>();
        let present = baseline::entries(transaction, project, linked.iter().copied())?;
        let dangling = joining_shared
            .iter()
            .filter(|(_, document)| {
                document.as_ref().is_some_and(|document| {
                    graph::document_edges(document)
                        .into_iter()
                        .any(|(_, target)| {
                            !joining_shared.contains_key(&target)
                                && present
                                    .get(&target)
                                    .is_none_or(|entry| entry.document.is_none())
                        })
                })
            })
            .map(|(id, _)| *id)
            .collect::<Vec<_>>();
        if dangling.is_empty() {
            break;
        }
        for id in dangling {
            if let Some(document) = joining_shared.remove(&id) {
                deferred_shared.insert(id, document);
            }
        }
    }
    let already_joined = joining_shared.clone();
    if !joining_shared.is_empty() {
        let recorded = baseline::head(transaction, project)?.map(|head| head.record);
        let advanced = joining_shared.keys().copied().collect::<Vec<_>>();
        baseline::append_retiring(
            transaction,
            project,
            recorded,
            joining_shared,
            BTreeSet::new(),
            None,
            request.profile,
            request.now,
        )?;
        reconciliation::advance_on(transaction, project, advanced, &shared_known)?;
    }
    // A held task is decided again by its own rules, never as divergence:
    // one whose planning changed here is set aside above.
    let mut refusing = local
        .iter()
        .filter(|id| !head.pending.contains_key(*id))
        .copied()
        .collect::<BTreeSet<_>>();
    // Only a change to what governs a task diverges. A task the feed names for
    // a claim, its renewal or release, or a note of this host's own still
    // governs as this host last agreed, and so never refuses; nor does one
    // whose incoming change, as in a strict delta, leaves its governing
    // fields as this host holds them and only adds shared notes or activity.
    // What this host last agreed is what it published since the last import,
    // when it did, and the baseline otherwise.
    let baseline_agreed = baseline::entries(transaction, project, refusing.iter().copied())?;
    for id in refusing.clone() {
        let agreed = match request.written.get(&id) {
            Some(written) => written.as_ref(),
            None => baseline_agreed
                .get(&id)
                .and_then(|entry| entry.document.as_ref()),
        };
        let incoming = changes.get(&id).and_then(Option::as_ref);
        if agreed.is_none() && incoming.is_none() {
            continue;
        }
        let item = match super::super::query::load_work_item(transaction, id) {
            Ok(item) => item,
            Err(StoreError::WorkNotFound(_)) => continue,
            Err(error) => return Err(error),
        };
        // A comparison only: governing fields decide, so no publication
        // refusal of notes or activity may stop the import here.
        let (current, _) = super::capture::capture_document_on(
            transaction,
            item,
            request.profile.include_notes,
            request.profile.origin_machine.as_deref(),
            true,
            &mut crate::task_sync::notes::NoteOmissions::default(),
            &mut crate::task_sync::notes::NoteOmissions::default(),
        )?;
        if agreed
            .into_iter()
            .chain(incoming)
            .any(|document| crate::task_sync::shared::governing_equal(document, &current))
        {
            refusing.remove(&id);
        }
    }
    let exit = |commit: Option<&str>| {
        format!(
            ". If the data branch already holds another host's commits merged on top, {}",
            crate::task_sync::data_worktree::set_aside_exit(commit)
        )
    };
    let committed = match &head.disk {
        FolderDisk::Committed { commit } => Some(commit.as_str()),
        FolderDisk::Written { .. } | FolderDisk::Unbound => None,
    };
    // The commit the last export was written onto, when that export never
    // reached a commit: its task files are this host's last known state.
    let written_onto = match &head.disk {
        FolderDisk::Written { from, .. } => Some(from.commit.as_str()),
        FolderDisk::Committed { .. } | FolderDisk::Unbound => None,
    };
    let (_, application) = selected::admit_on(
        transaction,
        project,
        request.profile,
        request.actor,
        &changes,
        seeds,
        // A task this host exported is shared even before any import put it
        // in the baseline, so another host may remove it.
        &head.exported.keys().copied().collect(),
        |read| {
            let touched = read.intersection(&refusing).copied().collect::<Vec<_>>();
            if touched.is_empty() {
                return Ok(());
            }
            let names = touched
                .iter()
                .map(|id| id.0.to_string())
                .collect::<Vec<_>>()
                .join(", ");
            // Edits an export wrote and a commit never took: another host
            // changed these tasks after the commit that export was written
            // onto, so setting that host's changes aside restores exactly it.
            // A task touched only through a changed neighbour meets no change
            // of its own, so that wording does not fit it.
            if let Some(from) = written_onto
                && touched
                    .iter()
                    .all(|id| request.discarded.contains(id) && changes.contains_key(id))
            {
                return Err(TaskSyncRefusal::DivergentPlanning.invalid(format!(
                    "another host changed {names} after {from}, the commit this host's discarded export of them was written onto, while that edit is still owed here: nothing is overwritten. To keep both sides, set the other host's task changes aside without moving the branch back: in the data worktree run `git restore --source={from} --staged --worktree -- .engram/tasks` and commit that; import, which owes the discarded edit again; export and commit; then `git revert --no-edit` the set-aside commit, resolving the conflict on {names} as an agent merge and committing it, and import"
                )));
            }
            Err(TaskSyncRefusal::DivergentPlanning.invalid(format!(
                "local planning of {names} changed since the last export or import; export and commit it, then import again{}",
                exit(committed)
            )))
        },
        request.now,
        true,
    )?;
    let unapplied = application
        .tasks
        .iter()
        .filter(|task| task.status != TaskSyncTaskStatus::Applied)
        .map(|task| task.task)
        .collect::<BTreeSet<_>>();
    let mut pending = super::incremental::planning_fingerprints(
        transaction,
        project,
        request.profile.include_notes,
        unapplied.iter().copied(),
    )?;
    pending.extend(kept);
    let applied = changes
        .iter()
        .filter(|(id, _)| !unapplied.contains(id))
        .map(|(id, document)| (*id, document.clone()))
        .collect::<BTreeMap<_, _>>();
    let mut tasks = application.tasks;
    tasks.extend(set_aside);
    // A task this host exported is shared once a commit carries it, so this
    // import records it in the baseline, and every later import decides its
    // changes and its removal as a member's: one the incoming commit holds as
    // this host wrote it joins with that document, and one whose incoming
    // change or removal admission held or refused joins with this host's
    // document, held. The exported set clears with this import, so a task left
    // out here would read as never published to every later one. An admitted
    // removal of one retires it, so no later export, rebuild included,
    // publishes it again.
    let mut recorded_changes = applied.clone();
    let held_published = unapplied
        .iter()
        .filter(|id| head.exported.contains_key(*id) && changes.contains_key(*id))
        .copied()
        .collect::<BTreeSet<_>>();
    let removed = applied
        .iter()
        .filter(|(_, document)| document.is_none())
        .map(|(id, _)| *id)
        .collect::<BTreeSet<_>>();
    let known = baseline::entries(
        transaction,
        project,
        held_published.iter().chain(&removed).copied(),
    )?;
    let joining = held_published
        .into_iter()
        .filter(|id| !known.contains_key(id))
        .collect::<BTreeSet<_>>();
    if !joining.is_empty() {
        let captured = super::capture::capture_on(transaction, project, request.profile, joining)?;
        for (id, task) in captured.documents {
            if let crate::task_sync::CapturedTask::Present(document) = task {
                recorded_changes.insert(id, Some(*document));
            }
        }
    }
    // The shared documents held back above join now, beside the tasks they
    // name; an incoming change admission applied to one wins.
    recorded_changes.extend(
        deferred_shared
            .into_iter()
            .filter(|(id, _)| !applied.contains_key(id)),
    );
    // A change admission applied as this host published it is in the
    // baseline already.
    recorded_changes.retain(|id, document| already_joined.get(id) != Some(&*document));
    if !recorded_changes.is_empty() {
        let retired_unshared = removed
            .into_iter()
            .filter(|id| !known.contains_key(id))
            .collect();
        // What each task's baseline holds now, after the join before
        // admission, so that an imported head advances to this record even
        // when it returns a task to the bytes it held before that join.
        let prior = baseline::entries(transaction, project, recorded_changes.keys().copied())?;
        let recorded = baseline::head(transaction, project)?.map(|head| head.record);
        let advanced = recorded_changes.keys().copied().collect::<Vec<_>>();
        baseline::append_retiring(
            transaction,
            project,
            recorded,
            recorded_changes,
            retired_unshared,
            None,
            request.profile,
            request.now,
        )?;
        reconciliation::advance_on(transaction, project, advanced, &prior)?;
    }
    // A removal of a task this host never published left it its own.
    local.extend(unpublished);
    let after = producing_origin::cut_on(transaction, project)?;
    let next = FolderHead {
        project_id: project.clone(),
        // As in the complete import, the head keeps the alias this host last
        // exported under, whatever alias the import rendered with.
        profile: ExportProfile {
            origin_machine: head.profile.origin_machine.clone(),
            ..request.profile.clone()
        },
        previous: Some(id),
        reason: FolderHeadReason::Import,
        cut: after,
        // The folder on disk is the incoming commit, held tasks included:
        // the next import reads them again at its own incoming commit.
        inventory_root: request.incoming.inventory_root,
        task_count: request.incoming.task_count,
        task_bytes: request.incoming.task_bytes,
        source: Some(request.incoming.clone()),
        disk: FolderDisk::Committed {
            commit: request.incoming.commit.clone(),
        },
        pending,
        // The cut moves past the local changes this import did not touch;
        // they stay owed to the next export.
        unexported: local,
        // This import read every task the earlier exports wrote again.
        exported: BTreeMap::new(),
        // A rewritten folder is never imported incrementally.
        rewritten: false,
    };
    folder_head::append_on(transaction, &next)?;
    Ok(FolderDeltaOutcome {
        result: TaskSyncImportResult {
            task_count: usize::try_from(request.incoming.task_count).unwrap_or(usize::MAX),
            changed: !applied.is_empty(),
            dry_run: request.dry_run,
            tasks,
        },
        head: next,
        memories: Vec::new(),
        memory_partial: false,
    })
}

/// The project that holds `task`, if any project does.
fn connection_project(
    transaction: &rusqlite::Transaction<'_>,
    task: WorkId,
) -> Result<Option<String>, StoreError> {
    use rusqlite::OptionalExtension;
    Ok(transaction
        .query_row(
            "SELECT project_id FROM work_items WHERE work_id = ?1",
            [task.0.to_string()],
            |row| row.get(0),
        )
        .optional()?)
}
