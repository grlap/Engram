//! The ordinary folder export: incremental from the recorded folder head, or
//! the named bootstrap or rebuild of the complete folder.
//!
//! One export or import holds the data worktree's folder lock throughout.
//! Every route plans on one snapshot, proves the folder on disk is the
//! recorded head's, retains its exact file operations with each path's
//! preimage, records the next folder head while its predecessor is still
//! current, and only then touches the folder. An interrupted publication is
//! finished by the next export or import when its head was recorded and every
//! path it writes is still at its preimage or already published, and dropped
//! when its head was not recorded.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
    time::Duration,
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;

use super::LocalWorkService;
use crate::{
    ObjectId, ProjectId, WorkId,
    storage::{FolderDisk, FolderExportPlan, FolderHead, FolderHeadReason, FolderSource},
    task_sync::{
        self, TaskDocument, TaskSyncRefusal, TaskTree,
        data_worktree::DataWorktree,
        directory::{self, FolderWrite, INCREMENTAL_FORMAT, IncrementalBasis},
        file_lock::HeldLock,
        inventory::InventoryRoot,
        notes::NoteOmissions,
        progress::ExportProfile,
    },
};

/// The data worktree's folder lock: one export or import at a time, from
/// recovering an interrupted publication to publishing the next.
pub(super) struct FolderLock {
    _lock: HeldLock,
}

/// Take the folder lock without waiting.
///
/// # Errors
/// Refuses while another export or import holds it, and I/O failures.
pub(super) fn lock_folder(data: &DataWorktree) -> Result<FolderLock> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create(true)
        .truncate(false)
        .open(data.git_directory.join("engram-task-sync-folder.lock"))?;
    // Only contention is reported as busy; a lock the filesystem cannot take
    // fails with its own I/O error, which no retry clears.
    let lock = HeldLock::try_acquire(file).map_err(|error| -> anyhow::Error {
        match error {
            std::fs::TryLockError::WouldBlock => TaskSyncRefusal::FolderBusy
                .invalid(
                    "another task-sync export or import is running on this data worktree; run this one again when it finishes",
                )
                .into(),
            std::fs::TryLockError::Error(error) => anyhow::Error::from(error)
                .context("locking the data worktree's task-sync folder"),
        }
    })?;
    Ok(FolderLock { _lock: lock })
}

/// What one folder export did.
#[derive(Debug, Serialize)]
pub struct FolderExportReceipt {
    /// `bootstrap`, `rebuild`, `incremental` or `unchanged`.
    pub route: &'static str,
    pub notes_included: bool,
    pub inventory_root: InventoryRoot,
    pub task_count: u64,
    /// The complete folder's ordered fingerprint, written by the bootstrap and
    /// a rebuild only: the basis of a complete import.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub export_basis: Option<String>,
    /// Task files written and removed; an unchanged file is not rewritten.
    pub written: usize,
    pub removed: usize,
    /// Tasks the last import held whose other-host edit a rebuild replaced
    /// with this host's version, so that replacement is never silent.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub superseded: Vec<WorkId>,
    pub omissions: NoteOmissions,
    pub activity_omissions: NoteOmissions,
    /// The bytes the folder's task files hold once this export is written,
    /// so that the memory component is published without reading them.
    #[serde(skip)]
    pub task_bytes: u64,
}

/// The memory component an export includes: captured on the task plan's own
/// snapshot, so both components describe one store state, and admitted
/// before any file is written.
pub struct FolderMemoryExport<'a> {
    pub selected: &'a BTreeMap<String, task_sync::memories::KeyHistory>,
    pub resolutions: &'a BTreeMap<String, task_sync::memories::ImportOptions>,
    /// Runs every memory admission check on the captured component and
    /// returns its bytes, counted against the folder's size limit.
    pub admit: Box<dyn FnOnce(task_sync::memories::ExportOutcome) -> Result<u64> + 'a>,
}

/// One planned folder: its files, membership, encoded size and disk state.
struct Planned {
    route: &'static str,
    files: Files,
    root: InventoryRoot,
    count: u64,
    task_bytes: u64,
    complete: Option<TaskTree>,
    disk: FolderDisk,
    /// The task files planning read, as it read them.
    read: Preimages,
}

impl LocalWorkService {
    /// Export shared planning into the data worktree's `.engram` folder.
    ///
    /// With a recorded folder head only the tasks changed since it are
    /// written, and the folder on disk must still be the one that head
    /// records. With none, the complete folder is written into an empty folder
    /// or over the same planning; with `rebuild`, over whatever the folder
    /// holds. Held tasks keep the other host's bytes, except that a rebuild
    /// writes this host's version of one whose planning changed here too.
    ///
    /// # Errors
    /// Refuses a read-only service, a missing opt-in, another running export
    /// or import, a bootstrap over other planning (remedy: import it first, or
    /// `--rebuild`), a folder that is not the recorded one (remedy: import the
    /// data branch's new commits first, or commit or restore the folder), a
    /// moved head, the candidate limits, every refusal of the plan, and unsafe
    /// or failed writes.
    ///
    /// An export that includes `memories` captures that component on the
    /// plan's snapshot and admits it before any write; its size counts
    /// against the folder's limit, and the envelope advertises it. A
    /// task-only export (`None`) refuses a folder that advertises it.
    pub fn export_task_sync_data_folder(
        &self,
        data: &DataWorktree,
        profile: &ExportProfile,
        rebuild: bool,
        memories: Option<FolderMemoryExport<'_>>,
        now: DateTime<Utc>,
    ) -> Result<FolderExportReceipt> {
        self.require_task_sync_writer()?;
        self.require_task_sync_enabled()?;
        let _lock = lock_folder(data)?;
        // A rebuild writes this host's complete folder over whatever the
        // folder holds, so an interrupted export it cannot finish is
        // superseded rather than left to refuse every route; its journal is
        // dropped only once the rebuild is admitted and about to retain its own.
        let superseded_journal = if rebuild {
            match self.finish_interrupted_export(data, now) {
                Ok(()) => false,
                Err(error) => {
                    eprintln!("task-sync rebuild supersedes an unfinished export: {error:#}");
                    true
                }
            }
        } else {
            self.finish_interrupted_export(data, now)?;
            false
        };
        let envelope = data.checkout.join(".engram").join("project.json");
        if memories.is_none()
            && envelope.try_exists()?
            && directory::read_folder_basis(&envelope)?.memory_capable
        {
            return Err(TaskSyncRefusal::MemoryProfileRequiresOptIn
                .invalid(
                    "the data folder carries the memory component: export with --include-memories, so that the task export keeps it",
                )
                .into());
        }
        // The committed HEAD decides too, whatever the folder on disk holds:
        // a task-only export never writes onto committed memories, nor onto
        // a HEAD whose committed envelope cannot say whether it has them.
        if memories.is_none() {
            directory::require_task_only_head(data)?;
        }
        let (plan, memory_bytes) = match memories {
            Some(memories) => {
                let (plan, mut captured) = self.plan_task_sync_folder_export_with_memories(
                    profile,
                    rebuild,
                    memories.selected,
                    memories.resolutions,
                    now,
                )?;
                withhold_targets_held_out(
                    &plan,
                    data,
                    &self.project_id,
                    memories.selected,
                    &mut captured,
                )?;
                (plan, Some((memories.admit)(captured)?))
            }
            None => (
                self.plan_task_sync_folder_export(profile, rebuild, now)?,
                None,
            ),
        };
        let planned = if plan.complete {
            let (route, files, tree, task_bytes) = self.complete(&plan, data)?;
            // Every path the complete folder writes or removes, and its
            // envelope, as they are before the folder is admitted: a change
            // after this, even one the admission below reads, is never
            // overwritten.
            let folder = data.checkout.join(".engram");
            let mut read = Preimages::new();
            for relative in files
                .keys()
                .map(String::as_str)
                .chain(std::iter::once("project.json"))
            {
                read.insert(relative.to_owned(), fingerprint_path(&folder, relative)?);
            }
            if plan.head.is_none() && !rebuild {
                require_unoccupied(data, &tree, memory_bytes.is_some())?;
            }
            let (root, count) = folder_root(&tree.tasks)?;
            Planned {
                route,
                files,
                root,
                count,
                task_bytes,
                complete: Some(tree),
                disk: FolderDisk::Unbound,
                read,
            }
        } else {
            let (_, head) = plan
                .head
                .as_ref()
                .context("incremental plan without a head")?;
            let base = verify_disk(head, data, &self.project_id, memory_bytes.is_some())?;
            if plan.changes.is_empty() {
                // Nothing is written, so no head is recorded: a size the
                // folder's commits changed is recorded by the next import or
                // export that writes. A memory component still published
                // beside the folder must fit with it.
                if let Some(memory_bytes) = memory_bytes {
                    directory::admit_memories_beside(
                        &data.checkout.join(".engram"),
                        base.task_bytes,
                        memory_bytes,
                    )?;
                }
                return Ok(FolderExportReceipt {
                    route: "unchanged",
                    notes_included: plan.project.notes_included,
                    inventory_root: head.inventory_root,
                    task_count: head.task_count,
                    export_basis: None,
                    written: 0,
                    removed: 0,
                    superseded: Vec::new(),
                    omissions: plan.omissions,
                    activity_omissions: plan.activity_omissions,
                    task_bytes: base.task_bytes,
                });
            }
            let (files, root, count, task_bytes, read) =
                incremental(&plan, head, base.task_bytes, data)?;
            // Held tasks keep the other host's documents beside this host's
            // changes; together they must still form a valid folder.
            if !head.pending.is_empty() && !files.is_empty() {
                self.validate_beside_held(head, profile, data, &files, now)?;
            }
            let disk = match base.from {
                Some(from) => {
                    let mut written = base.carried;
                    for (relative, bytes) in &files {
                        written.insert(
                            task_id(relative)?,
                            bytes.as_deref().map(|bytes| fingerprint(bytes.as_bytes())),
                        );
                    }
                    FolderDisk::Written {
                        from,
                        files: written,
                    }
                }
                None => FolderDisk::Unbound,
            };
            Planned {
                route: "incremental",
                files,
                root,
                count,
                task_bytes,
                complete: None,
                disk,
                read,
            }
        };
        let Planned {
            route,
            mut files,
            root,
            count,
            task_bytes,
            complete,
            disk,
            read,
        } = planned;
        #[cfg(test)]
        if let Some(hook) = AFTER_EXPORT_PLAN.with(|hook| hook.borrow_mut().take()) {
            hook();
        }
        let previous = plan.head.as_ref();
        let next = FolderHead {
            project_id: self.project_id.clone(),
            profile: profile.clone(),
            previous: previous.map(|(id, _)| id.clone()),
            reason: match route {
                "bootstrap" => FolderHeadReason::Bootstrap,
                "rebuild" => FolderHeadReason::Rebuild,
                _ => FolderHeadReason::Export,
            },
            cut: plan.cut.clone(),
            inventory_root: root,
            task_count: count,
            task_bytes,
            source: previous.and_then(|(_, head)| head.source.clone()),
            disk,
            // A rebuild wrote this host's version of every conflicted held
            // task, so those are no longer held.
            pending: previous
                .map(|(_, head)| {
                    head.pending
                        .iter()
                        .filter(|(id, _)| !plan.conflicted.contains(id))
                        .map(|(id, fingerprint)| (*id, fingerprint.clone()))
                        .collect()
                })
                .unwrap_or_default(),
            // Every change the plan saw, passed over by an import or not, is
            // in this export, except a held task's: its held bytes stay, so
            // what changed here stays owed until the hold resolves.
            unexported: plan.held_owed.clone(),
            // A complete export marks the folder rewritten until an import;
            // incremental exports since keep that mark.
            rewritten: complete.is_some() || previous.is_some_and(|(_, head)| head.rewritten),
            exported: if complete.is_some() {
                // A complete folder is bound to no commit; the next import
                // does not read its tasks again, but reads the complete folder.
                BTreeMap::new()
            } else {
                let mut exported = previous
                    .map(|(_, head)| head.exported.clone())
                    .unwrap_or_default();
                for (relative, bytes) in &files {
                    exported.insert(
                        task_id(relative)?,
                        bytes.as_deref().map(|bytes| fingerprint(bytes.as_bytes())),
                    );
                }
                exported
            },
        };
        let basis = IncrementalBasis {
            format: INCREMENTAL_FORMAT.into(),
            inventory_root: root,
            task_count: count,
            cut: plan.cut.clone(),
        };
        // A complete folder also carries its ordered fingerprint, the basis of
        // a complete import; an incremental one never reads every task for it.
        let (envelope, export_basis) = match &complete {
            Some(tree) => {
                let (bytes, fingerprint) =
                    directory::complete_envelope(tree, profile, basis, memory_bytes.is_some())?;
                (bytes, Some(fingerprint))
            }
            None => (
                directory::incremental_envelope(
                    &plan.project,
                    profile,
                    basis,
                    memory_bytes.is_some(),
                )?,
                None,
            ),
        };
        files.insert("project.json".into(), Some(String::from_utf8(envelope)?));
        // What the readers could not take back is never written.
        let folder = data.checkout.join(".engram");
        let mut policy_bytes = 0_u64;
        for name in [".gitignore", ".gitattributes"] {
            if let Ok(meta) = std::fs::symlink_metadata(folder.join(name)) {
                policy_bytes = policy_bytes.saturating_add(meta.len());
            }
        }
        // The memory component shares the folder's size limit with the tasks.
        directory::admit_written(
            &files,
            count,
            task_bytes,
            policy_bytes.saturating_add(memory_bytes.unwrap_or(0)),
        )?;
        // A task file planning read must still be as it read it: a change
        // since, such as a merge into the folder, is never overwritten.
        let mut before = BTreeMap::new();
        for relative in files.keys() {
            let current = fingerprint_path(&folder, relative)?;
            if read
                .get(relative)
                .is_some_and(|planned| *planned != current)
            {
                return Err(changed_folder(&format!(
                    "the data folder's {relative} changed after this export was planned; nothing was written. Commit or import that change, then export again"
                )));
            }
            before.insert(relative.clone(), current);
        }
        // A file already holding its bytes is not rewritten, so not counted.
        let written = files
            .iter()
            .filter(|(path, bytes)| {
                path.as_str() != "project.json"
                    && bytes.as_deref().is_some_and(|bytes| {
                        before.get(*path).cloned().flatten() != Some(fingerprint(bytes.as_bytes()))
                    })
            })
            .count();
        let removed = files.values().filter(|bytes| bytes.is_none()).count();
        let write = FolderWrite {
            head: serde_json::to_value(&next)?,
            files,
            before,
        };
        if superseded_journal {
            FolderWrite::discard(data)?;
        }
        write.retain(data)?;
        if let Err(error) = self.record_task_sync_folder_head(&next, now) {
            FolderWrite::discard(data)?;
            return Err(error.into());
        }
        write.publish(data)?;
        Ok(FolderExportReceipt {
            route,
            notes_included: plan.project.notes_included,
            inventory_root: root,
            task_count: count,
            export_basis,
            written,
            removed,
            superseded: plan.conflicted.iter().copied().collect(),
            omissions: plan.omissions,
            activity_omissions: plan.activity_omissions,
            task_bytes,
        })
    }

    /// The complete folder: every exported task, removal of every other task
    /// file, and held tasks left with the other host's bytes. A held task
    /// whose planning changed here too gets this host's version instead. A
    /// held task whose file is gone from the folder is restored from the
    /// commit the last import recorded; one absent there too was removed by
    /// that host.
    fn complete(
        &self,
        plan: &FolderExportPlan,
        data: &DataWorktree,
    ) -> Result<(&'static str, Files, TaskTree, u64)> {
        let folder = data.checkout.join(".engram");
        let head = plan.head.as_ref().map(|(_, head)| head);
        let pending = head
            .map(|head| {
                head.pending
                    .keys()
                    .copied()
                    .filter(|id| !plan.conflicted.contains(id))
                    .collect::<BTreeSet<_>>()
            })
            .unwrap_or_default();
        let mut files = Files::new();
        let mut tasks = Vec::new();
        for (&id, document) in &plan.changes {
            if pending.contains(&id) {
                continue;
            }
            if let Some(document) = document {
                files.insert(
                    path(id),
                    Some(String::from_utf8(task_sync::pretty(document)?)?),
                );
                tasks.push(document.clone());
            }
        }
        let mut missing = Vec::new();
        // A held task's file stays as the other host wrote it, at its size.
        let mut kept_bytes = 0_u64;
        for &id in &pending {
            match on_disk(&folder, id)? {
                Some((incoming, stored)) => {
                    tasks.push(incoming);
                    kept_bytes += stored;
                }
                None => missing.push(id),
            }
        }
        if !missing.is_empty() {
            // The commit the last import read holds every held task's bytes.
            let commit = head
                .and_then(|head| head.source.as_ref())
                .map(|source| source.commit.as_str())
                .ok_or_else(|| {
                    TaskSyncRefusal::IncrementalBasisUnavailable.invalid(format!(
                        "the folder lost the file of a task the last import held, and no commit records it; restore the folder or import it again before exporting: {}",
                        missing
                            .iter()
                            .map(|id| id.0.to_string())
                            .collect::<Vec<_>>()
                            .join(", ")
                    ))
                })?;
            let restored = task_sync::git::read_tasks_at(
                &data.checkout,
                &self.project_id,
                commit,
                missing,
                GIT_DEADLINE,
            )?;
            for (id, document) in restored {
                if let Some(document) = document {
                    files.insert(
                        path(id),
                        Some(String::from_utf8(task_sync::pretty(&document)?)?),
                    );
                    tasks.push(document);
                }
            }
        }
        // Any other task file left from an earlier folder is removed. Only
        // the canonical name of a task is part of the layout.
        let kept = tasks
            .iter()
            .map(|task| task.id)
            .collect::<std::collections::BTreeSet<_>>();
        let directory = folder.join("tasks");
        if directory.try_exists()? {
            for entry in std::fs::read_dir(&directory)? {
                let name = entry?.file_name();
                let name = name.to_str().context("task file name is not UTF-8")?;
                let id = name
                    .strip_suffix(".json")
                    .and_then(|id| uuid::Uuid::parse_str(id).ok())
                    .map(WorkId)
                    .filter(|id| name == format!("{}.json", id.0))
                    .ok_or_else(|| {
                        TaskSyncRefusal::DestinationUnknownEntry
                            .invalid("the task folder holds an entry the format does not define")
                    })?;
                if !kept.contains(&id) {
                    files.insert(path(id), None);
                }
            }
        }
        tasks.sort_by_key(|task| task.id.0);
        let route = if plan.head.is_none() {
            "bootstrap"
        } else {
            "rebuild"
        };
        let mut task_bytes = kept_bytes;
        for bytes in files.values().flatten() {
            task_bytes += u64::try_from(bytes.len())?;
        }
        Ok((
            route,
            files,
            TaskTree {
                project: plan.project.clone(),
                tasks,
            },
            task_bytes,
        ))
    }

    /// Finish a publication whose head was recorded, or drop one whose head
    /// was not; the folder is never touched before its head. A recorded one
    /// is finished only when every path it writes is still at its preimage or
    /// already holds the published bytes, and, for a write onto a clean
    /// commit, over that same commit. Anything else may be a later edit, so
    /// the journal is kept and the export refused.
    ///
    /// # Errors
    /// Refuses a folder that changed since the interrupted export.
    pub(super) fn finish_interrupted_export(
        &self,
        data: &DataWorktree,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let Some(write) = FolderWrite::pending(data)? else {
            return Ok(());
        };
        let planned: FolderHead = serde_json::from_value(write.head.clone())?;
        let current = self.task_sync_folder_head(&planned.profile, now)?;
        if !current.is_some_and(|(_, head)| head == planned) {
            return FolderWrite::discard(data);
        }
        let folder = data.checkout.join(".engram");
        let mut changed = Vec::new();
        for (relative, bytes) in &write.files {
            let on_disk = fingerprint_path(&folder, relative)?;
            let published = bytes.as_deref().map(|bytes| fingerprint(bytes.as_bytes()));
            if on_disk != published && Some(&on_disk) != write.before.get(relative) {
                changed.push(relative.clone());
            }
        }
        // A write onto a clean commit finishes over that commit, or over a
        // later one whose task files are still exactly that commit's, such as
        // one setting other hosts' task changes aside.
        let moved = match &planned.disk {
            FolderDisk::Written { from, .. }
                if !same_tasks(data, &self.project_id, from, data.head_commit()) =>
            {
                Some(from.commit.clone())
            }
            _ => None,
        };
        if !changed.is_empty() || moved.is_some() {
            let restore = moved.map_or_else(String::new, |commit| {
                format!(
                    " The data branch moved past {commit}, which the export wrote onto: {}, letting this export finish first.",
                    task_sync::data_worktree::set_aside_exit(Some(&commit))
                )
            });
            return Err(TaskSyncRefusal::IncrementalBasisUnavailable
                .invalid(format!(
                    "an interrupted export was not finished, and the folder changed since it was planned{}: preserve those changes, then restore the paths to the interrupted export's starting point and export again, or run `engram task-sync export --rebuild`, which writes the complete folder over them.{restore}",
                    if changed.is_empty() {
                        String::new()
                    } else {
                        format!(" ({})", changed.join(", "))
                    }
                ))
                .into());
        }
        write.publish(data)
    }
}

/// A folder this host has no head for holds nothing, or exactly the planning
/// this export writes. Anything else is another host's planning, which only
/// an import or an explicit rebuild may replace.
fn require_unoccupied(data: &DataWorktree, tree: &TaskTree, memories: bool) -> Result<()> {
    let folder = data.checkout.join(".engram");
    let tasks = folder.join("tasks");
    let occupied = folder.join("project.json").try_exists()?
        || (tasks.try_exists()? && std::fs::read_dir(&tasks)?.next().is_some());
    if !occupied {
        return Ok(());
    }
    let same = directory::read_resolved_data_folder_profile(data, memories).is_ok_and(|existing| {
        let mut existing = existing.tree.tasks;
        existing.sort_by_key(|task| task.id.0);
        existing == tree.tasks
    });
    if same {
        return Ok(());
    }
    Err(TaskSyncRefusal::DestinationOccupied
        .invalid(
            "the data folder already holds shared planning this host has not imported: import it first (`engram task-sync import --basis OWN_EXPORT/project.json`, or `--empty-basis` for an empty store), or replace it with `engram task-sync export --rebuild`",
        )
        .into())
}

/// The clean commit this export writes onto, if the folder has one, the files
/// earlier exports already wrote onto it without a commit since, and the
/// bytes the folder's task files hold as stored.
struct Base {
    from: Option<FolderSource>,
    carried: BTreeMap<WorkId, Option<String>>,
    task_bytes: u64,
}

/// Prove the folder on disk is the one `head` recorded, reading only what
/// changed since: the files the last export wrote, and the task paths a
/// commit since changed. A folder recorded without a commit is read whole
/// once, then bound to the next clean commit. A commit since may also change
/// only how task files are formatted, so their stored size is carried
/// forward from the files it changed.
fn verify_disk(
    head: &FolderHead,
    data: &DataWorktree,
    project: &ProjectId,
    memories: bool,
) -> Result<Base> {
    let folder = data.checkout.join(".engram");
    let dirty = data.folder_changes()?;
    let current = data.head_commit().map(str::to_owned);
    let at = |commit: String, task_bytes: u64| FolderSource {
        commit,
        inventory_root: head.inventory_root,
        task_count: head.task_count,
        task_bytes,
    };
    let clean_base = |task_bytes: u64| Base {
        from: current.clone().map(|commit| at(commit, task_bytes)),
        carried: BTreeMap::new(),
        task_bytes,
    };
    match &head.disk {
        FolderDisk::Committed { commit } => {
            if !dirty.is_empty() {
                return Err(changed_folder(
                    "the data folder holds uncommitted changes the last import did not leave: commit or restore them. A memory capability that an export enabled or recovered changes only project.json; commit it, then export again",
                ));
            }
            let current = current.as_deref().ok_or_else(|| {
                changed_folder("the data branch has no commit, but the last import read one")
            })?;
            let task_bytes = if current == commit {
                head.task_bytes
            } else {
                prove(
                    data,
                    project,
                    &at(commit.clone(), head.task_bytes),
                    current,
                    head,
                )?
            };
            Ok(clean_base(task_bytes))
        }
        FolderDisk::Written { from, files } => {
            // Committed since, the folder is proved through Git from the
            // export's base commit: its own files and any other host's. A
            // commit whose task files are still exactly the base's, such as
            // one setting other hosts' changes aside, holds none of the last
            // export's files: they were discarded, and an import owes them.
            if dirty.is_empty() {
                let current = current.as_deref().ok_or_else(|| {
                    changed_folder("the data branch has no commit to compare the folder with")
                })?;
                if current != from.commit && !same_tasks(data, project, from, Some(current)) {
                    let task_bytes = prove(data, project, from, current, head)?;
                    return Ok(clean_base(task_bytes));
                }
            }
            for (&id, expected) in files {
                if fingerprint_on_disk(&folder, id)? != *expected {
                    return Err(changed_folder(&format!(
                        "task {} differs from what the last export wrote: restore it, or commit the folder as exported; if that change was discarded, run `engram task-sync import`, which owes it to the next export",
                        id.0
                    )));
                }
            }
            if dirty.is_empty() {
                return Ok(clean_base(head.task_bytes));
            }
            let owned = files
                .keys()
                .map(|id| format!(".engram/{}", path(*id)))
                .collect::<BTreeSet<_>>();
            if !same_tasks(data, project, from, current.as_deref())
                || dirty
                    .iter()
                    .any(|changed| changed != ".engram/project.json" && !owned.contains(changed))
            {
                return Err(changed_folder(
                    "the data folder changed beyond the last export's own files: commit what it wrote first, then merge and import any other host's changes before exporting",
                ));
            }
            Ok(Base {
                from: Some(from.clone()),
                carried: files.clone(),
                task_bytes: head.task_bytes,
            })
        }
        FolderDisk::Unbound => {
            let not_recorded = || {
                changed_folder(
                    "the data folder is not the one the last complete export or import recorded: import any other host's changes first, or restore the folder. `engram task-sync export --rebuild` writes the complete folder over it",
                )
            };
            let (root, count) = match current.as_deref().filter(|_| dirty.is_empty()) {
                Some(commit) => {
                    let snapshot =
                        task_sync::git::read_task_snapshot(&data.checkout, project, commit)?;
                    folder_root(&snapshot.tree.tasks)?
                }
                // A folder that is gone is not the recorded one either.
                None if !folder.try_exists()? => return Err(not_recorded()),
                None => folder_root(
                    &directory::read_resolved_data_folder_profile(data, memories)?
                        .tree
                        .tasks,
                )?,
            };
            if root != head.inventory_root || count != head.task_count {
                return Err(not_recorded());
            }
            // Read whole anyway, so the stored size is the files' own.
            Ok(clean_base(stored_task_bytes(&folder)?).filter_dirty(&dirty))
        }
    }
}

impl Base {
    /// A folder read whole with uncommitted changes has no clean base.
    fn filter_dirty(self, dirty: &BTreeSet<String>) -> Self {
        if dirty.is_empty() {
            self
        } else {
            Self {
                from: None,
                carried: BTreeMap::new(),
                task_bytes: self.task_bytes,
            }
        }
    }
}

/// The commits from `base` to `current` change the folder only to the root
/// `head` recorded, read through a rooted diff of the changed task paths.
/// Returns the bytes the task files hold at `current`: the base's, less the
/// changed files' stored size before and plus their size after.
fn prove(
    data: &DataWorktree,
    project: &ProjectId,
    base: &FolderSource,
    current: &str,
    head: &FolderHead,
) -> Result<u64> {
    let delta = task_sync::git::read_rooted_task_diff(
        &data.checkout,
        project,
        &base.commit,
        base.inventory_root,
        current,
        GIT_DEADLINE,
    )?;
    let mut count = base.task_count;
    for change in delta.changed.values() {
        match (&change.before, &change.after) {
            (None, Some(_)) => count += 1,
            (Some(_), None) => count = count.saturating_sub(1),
            _ => {}
        }
    }
    if delta.root != head.inventory_root || count != head.task_count {
        // A written head's base is the commit it wrote onto, not one that
        // holds its files; only a committed head names its own commit.
        let own = matches!(head.disk, FolderDisk::Committed { .. }).then_some(base.commit.as_str());
        return Err(changed_folder(&format!(
            "the data branch at {current} holds task changes this host has not imported: import them first (`engram task-sync import`), then export. If that import refuses because this host changed the same planning, {}. `--rebuild` would overwrite the other host's changes",
            task_sync::data_worktree::set_aside_exit(own)
        )));
    }
    Ok(base
        .task_bytes
        .saturating_sub(delta.bytes_before)
        .saturating_add(delta.bytes_after))
}

/// `current` is the commit `from` records, or a later one whose task files
/// are still exactly its own, such as one setting other hosts' task changes
/// aside. Anything unreadable counts as different.
fn same_tasks(
    data: &DataWorktree,
    project: &ProjectId,
    from: &FolderSource,
    current: Option<&str>,
) -> bool {
    current.is_some_and(|current| {
        current == from.commit
            || task_sync::git::read_rooted_task_diff(
                &data.checkout,
                project,
                &from.commit,
                from.inventory_root,
                current,
                GIT_DEADLINE,
            )
            .is_ok_and(|delta| delta.changed.is_empty())
    })
}

fn changed_folder(reason: &str) -> anyhow::Error {
    TaskSyncRefusal::IncrementalBasisUnavailable
        .invalid(reason.to_owned())
        .into()
}

fn folder_root(tasks: &[TaskDocument]) -> Result<(InventoryRoot, u64)> {
    Ok((
        tasks
            .iter()
            .try_fold(InventoryRoot::ZERO, InventoryRoot::including)?,
        u64::try_from(tasks.len())?,
    ))
}

/// A task file's content fingerprint: a comparison of bytes, not an identity.
fn fingerprint(bytes: &[u8]) -> String {
    ObjectId::from_canonical_bytes(bytes).to_string()
}

/// The fingerprint of the file an export writes for `document`.
///
/// # Errors
/// Refuses a document that cannot be encoded.
pub(super) fn written_fingerprint(document: &TaskDocument) -> Result<String> {
    Ok(fingerprint(&task_sync::pretty(document)?))
}

fn task_id(relative: &str) -> Result<WorkId> {
    relative
        .strip_prefix("tasks/")
        .and_then(|name| name.strip_suffix(".json"))
        .and_then(|id| uuid::Uuid::parse_str(id).ok())
        .map(WorkId)
        .context("folder write names a path outside the task folder layout")
}

/// One task file's content fingerprint as it is on disk, `None` if absent.
fn fingerprint_on_disk(folder: &Path, id: WorkId) -> Result<Option<String>> {
    fingerprint_path(folder, &path(id))
}

/// One folder file's content fingerprint as it is on disk, `None` if absent.
fn fingerprint_path(folder: &Path, relative: &str) -> Result<Option<String>> {
    let file = folder.join(relative);
    match std::fs::symlink_metadata(&file) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
        Ok(meta) if !meta.is_file() || meta.file_type().is_symlink() => {
            Err(TaskSyncRefusal::DestinationNotFile
                .invalid("task folder entry must be an ordinary file")
                .into())
        }
        Ok(meta) if meta.len() > directory::MAX_FILE_BYTES => {
            Err(TaskSyncRefusal::DestinationFileLimit
                .invalid("task folder file is larger than the per-file limit")
                .into())
        }
        Ok(_) => Ok(Some(fingerprint(&std::fs::read(&file)?))),
    }
}

type Files = BTreeMap<String, Option<String>>;

/// Each path's content fingerprint as planning read it, `None` if absent.
type Preimages = BTreeMap<String, Option<String>>;

const GIT_DEADLINE: Duration = Duration::from_secs(60);

#[cfg(test)]
thread_local! {
    /// Task files this thread read from the folder on disk.
    pub(super) static FILE_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// Runs once an export is planned, before it looks at the folder again.
    pub(super) static AFTER_EXPORT_PLAN: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

fn path(id: WorkId) -> String {
    format!("tasks/{}.json", id.0)
}

/// A memory key whose historical task target the last import holds while the
/// folder this export writes lacks that task's file, as when another host's
/// removal of it is held, is withheld whole: a receiving host could not
/// resolve that target. A held task this export rewrites with this host's
/// version is disclosed, and so is one a complete export restores from the
/// commit the last import read. A key the folder already holds stays in it
/// whatever this export captures, so withholding it could not keep the
/// target out of view: the export refuses, before anything is written.
fn withhold_targets_held_out(
    plan: &FolderExportPlan,
    data: &DataWorktree,
    project: &ProjectId,
    selected: &BTreeMap<String, task_sync::memories::KeyHistory>,
    memories: &mut task_sync::memories::ExportOutcome,
) -> Result<()> {
    let Some((_, head)) = &plan.head else {
        return Ok(());
    };
    let folder = data.checkout.join(".engram");
    let mut missing = Vec::new();
    for &id in head.pending.keys() {
        if !plan.conflicted.contains(&id) && !folder.join(path(id)).try_exists()? {
            missing.push(id);
        }
    }
    let mut absent = BTreeSet::new();
    match head.source.as_ref() {
        // A complete export restores a held task's missing file from the
        // commit the last import read: only one that commit lacks stays out.
        Some(source) if plan.complete && !missing.is_empty() => {
            for (id, document) in task_sync::git::read_tasks_at(
                &data.checkout,
                project,
                &source.commit,
                missing,
                GIT_DEADLINE,
            )? {
                if document.is_none() {
                    absent.insert(id);
                }
            }
        }
        _ => absent.extend(missing),
    }
    if absent.is_empty() {
        return Ok(());
    }
    let target_held_out = |history: &task_sync::memories::KeyHistory| {
        history
            .revisions
            .values()
            .find_map(|revision| match &revision.provenance.retiring_target {
                Some(crate::domain::ProjectMemoryRetiringTarget::Local { work_id, .. })
                    if absent.contains(work_id) =>
                {
                    Some(*work_id)
                }
                _ => None,
            })
    };
    if let Some((key, target)) = selected
        .iter()
        .find_map(|(key, history)| target_held_out(history).map(|target| (key, target)))
    {
        return Err(TaskSyncRefusal::RemovedPlanning
            .invalid(format!(
                "memory key {key} is already in the folder and retires with task {}, whose removal by another host the last import held, so the folder would publish the key without its target: decide that held removal first, as the import that held it names, then export memories again",
                target.0
            ))
            .into());
    }
    let withheld = memories
        .keys
        .iter()
        .filter(|(_, history)| target_held_out(history).is_some())
        .map(|(key, _)| key.clone())
        .collect::<Vec<_>>();
    for key in withheld {
        memories.keys.remove(&key);
        memories.withheld.push(task_sync::memories::WithheldKey {
            key,
            reason: "historical retirement target is held out of the folder by the last import"
                .into(),
        });
    }
    Ok(())
}

/// Only the changed tasks, against a folder already proved the recorded one
/// and holding `stored` bytes of task files: each changed task's old file is
/// read alone for the root arithmetic and the folder's stored size. Returns
/// also each written path's preimage as planning read it, so that a change
/// made to it after planning is never overwritten.
fn incremental(
    plan: &FolderExportPlan,
    head: &FolderHead,
    stored: u64,
    data: &DataWorktree,
) -> Result<(Files, InventoryRoot, u64, u64, Preimages)> {
    let folder = data.checkout.join(".engram");
    let folder = folder.as_path();
    let mut files = Files::new();
    let mut read = Preimages::new();
    let mut root = head.inventory_root;
    let mut count = head.task_count;
    let mut task_bytes = stored;
    for (&id, document) in &plan.changes {
        let old = read_on_disk(folder, id)?;
        // A file that already holds this document is not rewritten: it is
        // neither written nor recorded as written, so no later import can
        // mistake its absence from a commit's changes for a discarded write.
        if old.as_ref().map(|(old, _, _)| old) == document.as_ref() {
            continue;
        }
        read.insert(
            path(id),
            old.as_ref().map(|(_, _, fingerprint)| fingerprint.clone()),
        );
        if let Some((old, stored, _)) = old {
            root = root.excluding(&old)?;
            count = count
                .checked_sub(1)
                .context("folder task count is inconsistent")?;
            task_bytes = task_bytes.saturating_sub(stored);
        }
        let bytes = document
            .as_ref()
            .map(|document| {
                Ok::<_, anyhow::Error>(String::from_utf8(task_sync::pretty(document)?)?)
            })
            .transpose()?;
        if let Some(document) = document {
            root = root.including(document)?;
            count += 1;
        }
        if let Some(bytes) = &bytes {
            task_bytes += u64::try_from(bytes.len())?;
        }
        files.insert(path(id), bytes);
    }
    Ok((files, root, count, task_bytes, read))
}

impl LocalWorkService {
    /// The folder as this export would leave it, checked as one graph where
    /// it can differ from any admitted one: the tasks the last import held,
    /// whose documents are the other host's rather than this host's, beside
    /// the planned writes, over the connected components they touch. Only
    /// the held tasks' own files are read from the folder.
    fn validate_beside_held(
        &self,
        head: &FolderHead,
        profile: &ExportProfile,
        data: &DataWorktree,
        files: &Files,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let folder = data.checkout.join(".engram");
        let mut held_documents = BTreeMap::new();
        for &id in head.pending.keys() {
            held_documents.insert(id, on_disk(&folder, id)?.map(|(document, _)| document));
        }
        let mut written = BTreeMap::new();
        for (relative, bytes) in files {
            let document = bytes
                .as_deref()
                .map(|bytes| task_sync::parse::<TaskDocument>(bytes.as_bytes()))
                .transpose()?;
            written.insert(task_id(relative)?, document);
        }
        self.read_store_at(now)?
            .check_task_sync_folder_beside_held(&self.project_id, profile, &held_documents, &written)
            .map_err(|error| {
                TaskSyncRefusal::InvalidGraph
                    .invalid(format!(
                        "this export, together with the tasks the last import held, would publish an inconsistent task graph ({error}); resolve the held tasks first, for example by releasing their claims and importing again, or apply their held edits here and write this host's version with `engram task-sync export --rebuild`"
                    ))
                    .into()
            })
    }
}

/// One task document as it is in the folder, read alone, with the bytes its
/// file holds.
fn on_disk(folder: &Path, id: WorkId) -> Result<Option<(TaskDocument, u64)>> {
    Ok(read_on_disk(folder, id)?.map(|(task, stored, _)| (task, stored)))
}

/// One task file read once: its document, stored size and content
/// fingerprint, `None` if absent.
fn read_on_disk(folder: &Path, id: WorkId) -> Result<Option<(TaskDocument, u64, String)>> {
    let file = folder.join(path(id));
    match std::fs::symlink_metadata(&file) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
        Ok(meta) if !meta.is_file() || meta.file_type().is_symlink() => {
            return Err(TaskSyncRefusal::DestinationNotFile
                .invalid("task folder entry must be an ordinary file")
                .into());
        }
        Ok(meta) if meta.len() > directory::MAX_FILE_BYTES => {
            return Err(TaskSyncRefusal::DestinationFileLimit
                .invalid("task folder file is larger than the per-file limit")
                .into());
        }
        Ok(_) => {}
    }
    #[cfg(test)]
    FILE_READS.with(|count| count.set(count.get() + 1));
    let bytes = std::fs::read(&file)?;
    let task: TaskDocument =
        task_sync::parse(&bytes).map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
    if task.id != id {
        return Err(TaskSyncRefusal::InvalidGraph
            .invalid("task filename/id mismatch")
            .into());
    }
    Ok(Some((
        task,
        u64::try_from(bytes.len())?,
        fingerprint(&bytes),
    )))
}

/// The bytes the folder's task files hold on disk, as stored rather than as
/// re-encoded: what the readers' size limit counts.
///
/// # Errors
/// Refuses an unreadable folder and an irregular entry.
pub(super) fn stored_task_bytes(folder: &Path) -> Result<u64> {
    let tasks = folder.join("tasks");
    if !tasks.try_exists()? {
        return Ok(0);
    }
    let mut total = 0_u64;
    for entry in std::fs::read_dir(&tasks)? {
        let meta = std::fs::symlink_metadata(entry?.path())?;
        if !meta.is_file() || meta.file_type().is_symlink() {
            return Err(TaskSyncRefusal::DestinationNotFile
                .invalid("task folder entry must be an ordinary file")
                .into());
        }
        total = total.saturating_add(meta.len());
    }
    Ok(total)
}
