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
    storage::{
        FolderDisk, FolderExportConfirmation, FolderExportPlan, FolderHead, FolderHeadReason,
        FolderSource,
    },
    task_sync::{
        self, TaskDocument, TaskSyncRefusal, TaskTree,
        data_worktree::DataWorktree,
        directory::{self, ExportParent, FolderWrite, INCREMENTAL_FORMAT, IncrementalBasis},
        file_lock::HeldLock,
        git::ExportParentPaths,
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
    /// In recovery, whether the retained component matches the admitted Git
    /// basis. Check after routing the owned journal's recovery remedy.
    pub recovery_basis_matches: Option<bool>,
    /// Runs every memory admission check on the captured component and
    /// returns the admitted component without publishing it.
    pub admit: Box<
        dyn FnOnce(task_sync::memories::ExportOutcome) -> Result<FolderMemoryAdmission<'a>> + 'a,
    >,
}

/// The size and exact memory postimages admitted for combined export.
pub struct FolderMemoryAdmission<'a> {
    pub bytes: u64,
    pub files: BTreeMap<String, Option<String>>,
    /// Finish staging recovery after export admission and before the task
    /// writer publishes the replacement envelope.
    pub clear_capability_staging: Option<Box<dyn FnOnce() -> Result<()> + 'a>>,
    pub publish: Box<dyn FnOnce(u64) -> Result<()> + 'a>,
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

enum RetainedExport {
    None,
    Pending(FolderWrite),
    Replay(FolderWrite, FolderHead),
    Confirm(FolderExportConfirmation, Option<ObjectId>),
}

enum OwnedExportSlot {
    Inactive,
    RebuildRequired(anyhow::Error),
    Current(Box<(FolderWrite, ObjectId, FolderHead)>),
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
    #[allow(clippy::too_many_lines)]
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
        if !rebuild {
            FolderWrite::require_no_unowned_journal(data)?;
        }
        let dirty = data.folder_changes_admitted(true)?;
        let envelope = data.checkout.join(".engram").join("project.json");
        let recovery_basis_matches = memories
            .as_ref()
            .and_then(|memories| memories.recovery_basis_matches);
        let recovering = recovery_basis_matches.is_some();
        match &memories {
            None => {
                self.require_task_only_folder(data, "export", rebuild, now)?;
                directory::require_no_capability_staging(data)?;
            }
            Some(_) if !recovering => {
                directory::require_no_capability_staging(data)?;
                task_sync::memories::directory::require_no_pending_publication(data)?;
            }
            Some(_) => {}
        }
        // Explicit replacement plans against the admitted worktree and does
        // not adopt or replay the previous proof.
        let (previous_write, discard_unrecorded) = if rebuild {
            match self.owned_export_slot(data, now)? {
                OwnedExportSlot::Inactive => (None, false),
                OwnedExportSlot::RebuildRequired(_) => (None, true),
                OwnedExportSlot::Current(write) => (Some(*write), false),
            }
        } else {
            (None, false)
        };
        let (pending, confirmation) = match if rebuild {
            RetainedExport::None
        } else {
            self.inspect_retained_export(data, profile, &dirty, recovering, now)?
        } {
            RetainedExport::None => (None, None),
            RetainedExport::Pending(write) => (Some(write), None),
            RetainedExport::Confirm(proof, owner) => (None, Some((proof, owner))),
            RetainedExport::Replay(write, head) => {
                if memories.is_some() {
                    return Err(finish_task_only_export());
                }
                write.replay(data)?;
                return Ok(FolderExportReceipt {
                    route: "unchanged",
                    notes_included: head.profile.include_notes,
                    inventory_root: head.inventory_root,
                    task_count: head.task_count,
                    export_basis: None,
                    written: 0,
                    removed: 0,
                    superseded: Vec::new(),
                    omissions: NoteOmissions::default(),
                    activity_omissions: NoteOmissions::default(),
                    task_bytes: head.task_bytes,
                });
            }
        };
        if recovery_basis_matches == Some(false) {
            if let Some((write, _, _)) = &previous_write
                && !same_parent(data, write)
            {
                return Err(write.restore_combined_export());
            }
            return Err(TaskSyncRefusal::MemoryExportBasisChanged
                .invalid("recovery basis differs from data HEAD")
                .into());
        }
        let proof = confirmation.as_ref().map(|(proof, _)| proof);
        let (plan, memory_bytes, memory_files, clear_capability_staging, publish_memories) =
            match memories {
                Some(memories) => {
                    let (plan, mut captured) = self
                        .read_store_at(now)?
                        .plan_task_sync_folder_export_with_memory_confirmation(
                            &self.project_id,
                            profile,
                            rebuild,
                            memories.selected,
                            memories.resolutions,
                            proof,
                        )?;
                    withhold_targets_held_out(
                        &plan,
                        data,
                        &self.project_id,
                        memories.selected,
                        &mut captured,
                    )?;
                    let admitted = (memories.admit)(captured)?;
                    (
                        plan,
                        Some(admitted.bytes),
                        admitted.files,
                        admitted.clear_capability_staging,
                        Some(admitted.publish),
                    )
                }
                None => (
                    self.read_store_at(now)?
                        .plan_task_sync_folder_export_with_confirmation(
                            &self.project_id,
                            profile,
                            rebuild,
                            proof,
                        )?,
                    None,
                    BTreeMap::new(),
                    None,
                    None,
                ),
            };
        let (planned, parent_paths) = if plan.complete {
            let (route, mut files, tree, task_bytes) = self.complete(&plan, data, rebuild)?;
            if rebuild {
                let kept = tree
                    .tasks
                    .iter()
                    .map(|task| task.id)
                    .collect::<BTreeSet<_>>();
                for relative in dirty
                    .iter()
                    .filter_map(|path| path.strip_prefix(".engram/"))
                {
                    if !relative.starts_with("tasks/") || files.contains_key(relative) {
                        continue;
                    }
                    let id = task_id(relative)?;
                    if kept.contains(&id) {
                        return Err(changed_folder(
                            "a held task has an uncommitted edit: restore or commit that task's shared file before rebuilding",
                        ));
                    }
                    // The path was already absent on disk. The retained proof
                    // must still account for its removal from the Git parent.
                    files.insert(relative.to_owned(), None);
                }
            }
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
                if dirty
                    .iter()
                    .filter_map(|path| path.strip_prefix(".engram/"))
                    .any(|path| path.starts_with("tasks/") && !files.contains_key(path))
                {
                    return Err(changed_folder(
                        "the data folder has an uncommitted task deletion: restore or commit it and import the folder, or run `engram task-sync export --rebuild` to replace it",
                    ));
                }
                require_unoccupied(data, &tree, memory_bytes.is_some())?;
            }
            let (root, count) = folder_root(&tree.tasks)?;
            let from = plan
                .head
                .as_ref()
                .map(|(_, head)| self.read_store_at(now)?.task_sync_folder_import_basis(head))
                .transpose()?
                .flatten()
                .filter(|from| data.head_commit() == Some(from.commit.as_str()));
            let disk = from
                .map(|from| written_disk(from, &files))
                .transpose()?
                .unwrap_or(FolderDisk::Unbound);
            (
                Planned {
                    route,
                    files,
                    root,
                    count,
                    task_bytes,
                    complete: Some(tree),
                    disk,
                    read,
                },
                None,
            )
        } else {
            let (_, head) = plan
                .head
                .as_ref()
                .context("incremental plan without a head")?;
            let base = export_base(
                head,
                data,
                &dirty,
                pending.as_ref(),
                &memory_files,
                memory_bytes.is_some(),
                recovering,
            )?;
            let memory_only = plan.changes.is_empty() && memory_bytes.is_some();
            let enable_memories =
                memory_only && !directory::read_folder_basis(&envelope)?.memory_capable;
            let memory_write = enable_memories || !memory_files.is_empty();
            if plan.changes.is_empty()
                && (!memory_write
                    || pending.as_ref().is_some_and(|write| {
                        write.memory_files == memory_files && !enable_memories
                    }))
            {
                // An empty plan does not write task files or an envelope.
                if let Some(memory_bytes) = memory_bytes {
                    directory::admit_memories_beside(
                        &data.checkout.join(".engram"),
                        head.task_bytes,
                        memory_bytes,
                    )?;
                }
                self.finish_unchanged_export(data, confirmation.as_ref(), now, || {
                    if let Some(clear) = clear_capability_staging {
                        clear()?;
                    }
                    if let Some(publish) = publish_memories {
                        publish(head.task_bytes)?;
                    }
                    Ok(())
                })?;
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
                    task_bytes: head.task_bytes,
                });
            }
            if pending.is_some() {
                if recovering {
                    return Err(rebuild_combined_export());
                }
                return Err(commit_export_first());
            }
            if base.is_none() {
                if recovering {
                    return Err(rebuild_combined_export());
                }
                return Err(changed_folder(
                    "the last export has no retained commit proof: commit its files and import the committed folder, or export with --rebuild",
                ));
            }
            let parent_paths = ExportParentPaths::read(
                &data.checkout,
                &self.project_id,
                data.head_commit(),
                plan.changes
                    .keys()
                    .copied()
                    .map(path)
                    .chain(std::iter::once("project.json".into()))
                    .chain(memory_files.keys().cloned())
                    .collect(),
                GIT_DEADLINE,
            )?;
            let (files, root, count, task_bytes, read) =
                incremental(&plan, head, data, &parent_paths)?;
            // Held tasks keep the other host's documents beside this host's
            // changes; together they must still form a valid folder.
            if !head.pending.is_empty() && !files.is_empty() {
                self.validate_beside_held(head, profile, data, &files, now)?;
            }
            let disk = base
                .map(|from| written_disk(from, &files))
                .transpose()?
                .unwrap_or(FolderDisk::Unbound);
            (
                Planned {
                    route: if memory_only {
                        "unchanged"
                    } else {
                        "incremental"
                    },
                    files,
                    root,
                    count,
                    task_bytes,
                    complete: None,
                    disk,
                    read,
                },
                Some(parent_paths),
            )
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
                (Some(bytes), Some(fingerprint))
            }
            None if route == "unchanged" => {
                let envelope = data.checkout.join(".engram/project.json");
                (directory::memory_capability_bytes(&envelope)?, None)
            }
            None => (
                Some(directory::incremental_envelope(
                    &plan.project,
                    profile,
                    basis,
                    memory_bytes.is_some(),
                )?),
                None,
            ),
        };
        if let Some(envelope) = envelope {
            files.insert("project.json".into(), Some(String::from_utf8(envelope)?));
        } else if let Some(memory_bytes) = memory_bytes {
            directory::admit_memories_beside(
                &data.checkout.join(".engram"),
                task_bytes,
                memory_bytes,
            )?;
        }
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
            let bytes = folder_file_bytes(&folder, relative)?;
            if relative == "project.json"
                && let Some(parent_paths) = &parent_paths
            {
                require_parent_preimage(parent_paths, relative, bytes.as_deref())?;
            }
            let current = bytes.as_deref().map(fingerprint);
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
        let removed = files
            .iter()
            .filter(|(path, bytes)| {
                bytes.is_none() && before.get(*path).is_some_and(Option::is_some)
            })
            .count();
        let owner = self.folder_write_owner()?;
        let mut write = FolderWrite {
            owner: Some(owner.clone()),
            head: serde_json::to_value(&next)?,
            head_id: None,
            files,
            before,
            parent: Some(data.head_commit().map_or(ExportParent::Unborn, |commit| {
                ExportParent::Commit(commit.to_owned())
            })),
            commit_paths: None,
            published: false,
            memory_files,
        };
        let parent_paths = match parent_paths {
            Some(paths) => paths,
            None => ExportParentPaths::read(
                &data.checkout,
                &self.project_id,
                data.head_commit(),
                write
                    .files
                    .keys()
                    .chain(write.memory_files.keys())
                    .cloned()
                    .collect(),
                GIT_DEADLINE,
            )?,
        };
        let mut commit_paths = BTreeSet::new();
        for (path, bytes) in write.files.iter().chain(&write.memory_files) {
            if !parent_paths.matches(path, bytes.as_deref().map(str::as_bytes))? {
                commit_paths.insert(path.clone());
            }
        }
        let receipt = FolderExportReceipt {
            route: if commit_paths.is_empty() && written == 0 && removed == 0 {
                "unchanged"
            } else {
                route
            },
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
        };
        if commit_paths.is_empty() {
            if let Some((write, _, _)) = previous_write.as_ref()
                && !same_parent(data, write)
                && (!write.published || recovering)
            {
                return Err(if recovering {
                    write.restore_combined_export()
                } else {
                    write.moved_interrupted_export()?
                });
            }
            if !previous.is_some_and(|(_, head)| {
                head.import_basis()
                    .is_some_and(|basis| data.head_commit() == Some(basis.commit.as_str()))
            }) {
                let reason = "the planned files already match HEAD but this store has no admitted basis for it";
                if discard_unrecorded {
                    let memories = if memory_bytes.is_some() {
                        " --include-memories"
                    } else {
                        ""
                    };
                    return Err(changed_folder(&format!(
                        "{reason}; run `engram task-sync import --basis OWN_EXPORT/project.json{memories}` with your saved pre-merge envelope"
                    )));
                }
                return Err(changed_folder(&format!(
                    "{reason}; import the committed folder first"
                )));
            }
            if dirty
                .iter()
                .any(|path| !path.starts_with(".engram/memories/"))
            {
                return Err(changed_folder(
                    "restore the loose planning files to the admitted commit before exporting again",
                ));
            }
            self.finish_unchanged_export(data, confirmation.as_ref(), now, || {
                if let Some(clear) = clear_capability_staging {
                    clear()?;
                }
                if let Some(publish) = publish_memories {
                    publish(task_bytes)?;
                }
                Ok(())
            })?;
            if discard_unrecorded {
                FolderWrite::discard(data, Some(&owner))?;
            }
            return Ok(receipt);
        }
        write.commit_paths = Some(commit_paths);
        let previous_bytes = write.retained_bytes(data)?;
        let mut retained = false;
        let recorded = self
            .lock_store_at(now)?
            .record_task_sync_folder_publication(Some(next), proof, now, |head| {
                let (id, head) = head.context("new export head missing")?;
                write.head = serde_json::to_value(head)?;
                write.head_id = Some(id.clone());
                data.check_head()?;
                retained = true;
                write.retain(data)?;
                #[cfg(test)]
                if let Some(hook) = AFTER_EXPORT_RETAIN.with(std::cell::Cell::take) {
                    hook()?;
                }
                Ok(())
            });
        if let Err(error) = recorded {
            if retained {
                write
                    .restore_retained(data, previous_bytes.as_deref())
                    .with_context(|| format!("restoring the previous proof after {error:#}"))?;
            }
            return Err(error);
        }
        if let Some(clear) = clear_capability_staging {
            data.check_head()?;
            clear()?;
        }
        write.publish_with_memories(data, || match publish_memories {
            Some(publish) => publish(task_bytes),
            None => Ok(()),
        })?;
        Ok(receipt)
    }

    pub(super) fn require_task_only_folder(
        &self,
        data: &DataWorktree,
        operation: &str,
        replace_unrecorded: bool,
        now: DateTime<Utc>,
    ) -> Result<()> {
        let envelope = data.checkout.join(".engram").join("project.json");
        if envelope.try_exists()?
            && directory::read_folder_basis(&envelope)
                .with_context(|| format!("reading {}", envelope.display()))?
                .memory_capable
        {
            return Err(TaskSyncRefusal::MemoryProfileRequiresOptIn
                .invalid(format!(
                    "the data folder carries the memory component: {operation} with --include-memories"
                ))
                .into());
        }
        directory::require_task_only_head(data, operation)?;
        if let Some((write, _, _)) = self.recorded_export(data, replace_unrecorded, now)?
            && write.writes_memories()?
        {
            return Err(TaskSyncRefusal::MemoryProfileRequiresOptIn
                    .invalid(format!(
                        "the retained export carries the memory component: {operation} with --include-memories"
                    ))
                    .into());
        }
        Ok(())
    }

    /// Verify the retained write against the one commit immediately after its
    /// observed parent. No body of a Git task blob enters this path.
    fn inspect_retained_export(
        &self,
        data: &DataWorktree,
        profile: &ExportProfile,
        dirty: &BTreeSet<String>,
        recovering: bool,
        now: DateTime<Utc>,
    ) -> Result<RetainedExport> {
        let Some((write, id, head)) = self.recorded_export(data, false, now)? else {
            return Ok(RetainedExport::None);
        };
        if recovering {
            return Err(if same_parent(data, &write) {
                if !write.published && !write.writes_memories()? {
                    finish_task_only_export()
                } else {
                    rebuild_combined_export()
                }
            } else {
                write.restore_combined_export()
            });
        }
        if !write.published {
            if !same_parent(data, &write) {
                return Err(write.moved_interrupted_export()?);
            }
            if write.writes_memories()? {
                return Err(rebuild_combined_export());
            }
            return Ok(RetainedExport::Replay(write, head));
        }
        if !head.rebuild_needs_complete_import() && same_parent(data, &write) {
            if write.writes_memories()? {
                return Err(rebuild_combined_export());
            }
            if dirty.is_empty() && data.head_commit().is_some() {
                return Err(changed_folder(
                    "the data folder is clean at the export's original commit; run `engram task-sync import` to account for the discarded export",
                ));
            }
            return Err(commit_export_first());
        }
        if !head.rebuild_needs_complete_import()
            && head.profile.include_notes == profile.include_notes
            && let Some(proof) = self.prove_export_commit(data, dirty, &write, &id, &head)?
        {
            return Ok(RetainedExport::Confirm(proof, write.owner));
        }
        Ok(RetainedExport::Pending(write))
    }

    fn prove_export_commit(
        &self,
        data: &DataWorktree,
        dirty: &BTreeSet<String>,
        write: &FolderWrite,
        id: &ObjectId,
        head: &FolderHead,
    ) -> Result<Option<FolderExportConfirmation>> {
        let (Some(commit), Some(parent), Some(paths)) = (
            data.head_commit().filter(|_| dirty.is_empty()),
            &write.parent,
            &write.commit_paths,
        ) else {
            return Ok(None);
        };
        let parent = match parent {
            ExportParent::Unborn => None,
            ExportParent::Commit(commit) => Some(commit.as_str()),
        };
        let postimages = write
            .files
            .iter()
            .chain(&write.memory_files)
            .filter(|(path, _)| paths.contains(*path))
            .map(|(path, bytes)| (path.as_str(), bytes.as_deref()))
            .collect::<BTreeMap<_, _>>();
        if postimages.len() != paths.len() {
            return Err(changed_folder(
                "the retained export proof is incomplete; run `engram task-sync export --rebuild`",
            ));
        }
        if !task_sync::git::matches_export_commit(
            &data.checkout,
            &self.project_id,
            parent,
            commit,
            &postimages,
            GIT_DEADLINE,
        )? {
            return Ok(None);
        }
        let documents = write
            .files
            .iter()
            .filter(|(path, _)| path.as_str() != "project.json")
            .map(|(path, bytes)| {
                let id = task_id(path)?;
                let document = bytes
                    .as_deref()
                    .map(|bytes| task_sync::parse::<TaskDocument>(bytes.as_bytes()))
                    .transpose()?;
                anyhow::ensure!(
                    document.as_ref().is_none_or(|document| document.id == id),
                    "retained task file differs from its id"
                );
                Ok((id, document))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        data.check_head()?;
        Ok(Some(FolderExportConfirmation {
            expected: id.clone(),
            head: head.clone(),
            commit: commit.to_owned(),
            changes: documents,
        }))
    }

    /// Inspect a retained write without replaying or adopting it. An ordinary
    /// import can apply the exact proof with its other database changes.
    pub(super) fn prepare_folder_import(
        &self,
        data: &DataWorktree,
        basis: Option<&directory::DestinationBasis>,
        rebuild: bool,
        dry_run: bool,
        now: DateTime<Utc>,
    ) -> Result<FolderImportRecovery> {
        let saved_basis = matches!(basis, Some(directory::DestinationBasis::Recorded(_)));
        let complete_recovery = |owner| FolderImportRecovery {
            confirmation: None,
            owner: if dry_run { None } else { owner },
            requires_committed_basis: true,
        };
        let (write, id, head) = match self.owned_export_slot(data, now)? {
            OwnedExportSlot::Inactive => return Ok(FolderImportRecovery::default()),
            OwnedExportSlot::RebuildRequired(_) if saved_basis => {
                return Ok(complete_recovery(Some(self.folder_write_owner()?)));
            }
            OwnedExportSlot::RebuildRequired(error) => return Err(error),
            OwnedExportSlot::Current(write) => *write,
        };
        if !write.published {
            if !same_parent(data, &write) {
                if saved_basis && write.parent == Some(ExportParent::Unborn) {
                    // An explicit basis may reconcile the committed folder
                    // without replaying or adopting the interrupted write.
                    return Ok(complete_recovery(write.owner));
                }
                return Err(write.moved_interrupted_export()?);
            }
            if write.writes_memories()? {
                return Err(rebuild_combined_export());
            }
            write.validate_replay(data)?;
            return Err(interrupted_rebuild_commit_required());
        }
        if basis.is_none() && head.rebuild_needs_complete_import() {
            return Err(unbound_rebuild_basis_required(write.writes_memories()?));
        }
        // A saved basis may adopt this host's own complete rebuild, even
        // when this store's planning has advanced since the rebuild.
        let own_rebuild = if let Some(directory::DestinationBasis::Recorded(saved)) = basis
            && head.reason == FolderHeadReason::Rebuild
            && head.rewritten
        {
            saved.project_id == head.project_id
                && saved.profile == head.profile
                && saved.export_basis.is_some()
                && saved.incremental.as_ref().is_some_and(|incremental| {
                    incremental.inventory_root == head.inventory_root
                        && incremental.task_count == head.task_count
                        && incremental.cut == head.cut
                })
                && write
                    .files
                    .get("project.json")
                    .and_then(Option::as_deref)
                    .is_some_and(|envelope| {
                        directory::folder_basis_from_bytes(envelope.as_bytes())
                            .is_ok_and(|published| &published == saved.as_ref())
                    })
        } else {
            false
        };
        if !rebuild && (basis.is_none() || own_rebuild) {
            let dirty = data.folder_changes_admitted(true)?;
            if let Some(proof) = self.prove_export_commit(data, &dirty, &write, &id, &head)? {
                return Ok(FolderImportRecovery {
                    confirmation: Some(proof),
                    owner: if dry_run { None } else { write.owner },
                    requires_committed_basis: false,
                });
            }
        }
        Ok(FolderImportRecovery {
            confirmation: None,
            owner: if basis.is_none() && !rebuild && !dry_run {
                write.owner
            } else {
                None
            },
            requires_committed_basis: false,
        })
    }

    fn finish_unchanged_export(
        &self,
        data: &DataWorktree,
        confirmation: Option<&(FolderExportConfirmation, Option<ObjectId>)>,
        now: DateTime<Utc>,
        publish: impl FnOnce() -> Result<()>,
    ) -> Result<()> {
        let publish_checked = || {
            data.check_head()?;
            publish()
        };
        if let Some((proof, owner)) = confirmation {
            self.lock_store_at(now)?
                .record_task_sync_folder_publication(None, Some(proof), now, |_| {
                    publish_checked()
                })?;
            FolderWrite::discard(data, owner.as_ref())
        } else {
            publish_checked()
        }
    }

    pub(super) fn folder_write_owner(&self) -> Result<ObjectId> {
        let database = self.database.canonicalize()?;
        let store = crate::diagnostics::Fingerprint::store(&database);
        // A host-private filename fingerprint. Moving the database requires
        // import to establish a new comparison basis at its new path.
        let filename_key = serde_json::to_vec(&(store, &self.project_id))?;
        Ok(ObjectId::from_canonical_bytes(&filename_key))
    }

    fn recorded_export(
        &self,
        data: &DataWorktree,
        replace_unrecorded: bool,
        now: DateTime<Utc>,
    ) -> Result<Option<(FolderWrite, ObjectId, FolderHead)>> {
        match self.owned_export_slot(data, now)? {
            OwnedExportSlot::Inactive => Ok(None),
            OwnedExportSlot::RebuildRequired(_) if replace_unrecorded => Ok(None),
            OwnedExportSlot::RebuildRequired(error) => Err(error),
            OwnedExportSlot::Current(write) => Ok(Some(*write)),
        }
    }

    fn owned_export_slot(
        &self,
        data: &DataWorktree,
        now: DateTime<Utc>,
    ) -> Result<OwnedExportSlot> {
        let malformed = || {
            OwnedExportSlot::RebuildRequired(changed_folder(
                "preserve the malformed proof and run `engram task-sync export --rebuild`",
            ))
        };
        let owner = self.folder_write_owner()?;
        let Some(mut journal) = FolderWrite::open(data, Some(&owner))? else {
            return Ok(OwnedExportSlot::Inactive);
        };
        let store = self.read_store_at(now)?;
        if let Some((id, fingerprint)) = journal.head_key()?
            && store.task_sync_folder_head_is_displaced(&self.project_id, &id, &fingerprint)?
        {
            return Ok(OwnedExportSlot::Inactive);
        }
        let write = match journal.read() {
            Err(error) if error.downcast_ref::<serde_json::Error>().is_some() => {
                return Ok(malformed());
            }
            result => result?,
        };
        let planned: FolderHead = match serde_json::from_value(write.head.clone()) {
            Ok(head) => head,
            Err(_) => return Ok(malformed()),
        };
        if planned.project_id != self.project_id {
            return Ok(OwnedExportSlot::Inactive);
        }
        if let Some((id, head)) = store.task_sync_folder_head(&self.project_id, &planned.profile)?
            && head == planned
            && write.head_id.as_ref().is_none_or(|key| *key == id)
        {
            return Ok(OwnedExportSlot::Current(Box::new((write, id, head))));
        }
        // An explicit import may move the head while leaving its earlier
        // proof in the slot. Only a never-recorded head requires recovery.
        if let Some(id) = &write.head_id
            && store.task_sync_folder_head_was_recorded(id, &planned)?
        {
            return Ok(OwnedExportSlot::Inactive);
        }
        Ok(OwnedExportSlot::RebuildRequired(
            if write.writes_memories()? {
                rebuild_combined_export()
            } else {
                changed_folder(
                    "the retained export has no recorded head. Run `engram task-sync export --rebuild` to replace the interrupted write",
                )
            },
        ))
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
        rebuild: bool,
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
        let route = if plan.head.is_none() && (!rebuild || data.head_commit().is_none()) {
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
}

#[derive(Default)]
pub(super) struct FolderImportRecovery {
    owner: Option<ObjectId>,
    pub(super) confirmation: Option<FolderExportConfirmation>,
    pub(super) requires_committed_basis: bool,
}

impl FolderImportRecovery {
    pub(super) fn finish(&self, data: &DataWorktree) -> Result<()> {
        if let Some(owner) = &self.owner {
            FolderWrite::discard(data, Some(owner))?;
        }
        Ok(())
    }
}

fn same_parent(data: &DataWorktree, write: &FolderWrite) -> bool {
    match &write.parent {
        Some(ExportParent::Commit(commit)) => data.head_commit() == Some(commit.as_str()),
        Some(ExportParent::Unborn) => data.head_commit().is_none(),
        None => false,
    }
}

fn commit_export_first() -> anyhow::Error {
    changed_folder(
        "commit the exported folder, then run `engram task-sync export` again, or rerun with `--rebuild` to replace the uncommitted export",
    )
}

fn rebuild_combined_export() -> anyhow::Error {
    changed_folder(
        "the combined export is unfinished; run `engram task-sync export --rebuild --include-memories --memory-recovery-basis OWN_EXPORT` using the retained pre-export folder",
    )
}

fn finish_task_only_export() -> anyhow::Error {
    changed_folder(
        "finish the interrupted task-only write with `engram task-sync export` without --include-memories, then commit the folder before exporting with memories",
    )
}

pub(super) fn unbound_rebuild_basis_required(include_memories: bool) -> anyhow::Error {
    let memories = if include_memories {
        " --include-memories"
    } else {
        ""
    };
    TaskSyncRefusal::CommitContextRequired
        .invalid(format!("this rebuild has no admitted comparison basis; commit the rebuilt folder, then run `engram task-sync import --basis OWN_EXPORT/project.json{memories}` with your saved pre-merge rebuild envelope"))
        .into()
}

fn interrupted_rebuild_commit_required() -> anyhow::Error {
    TaskSyncRefusal::CommitContextRequired
        .invalid("run `engram task-sync export` to finish the interrupted write before committing and importing")
        .into()
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

/// Check the recorded parent and the scoped status without opening Git task
/// bodies. A retained write at its parent may still be awaiting commit.
fn export_base(
    head: &FolderHead,
    data: &DataWorktree,
    dirty: &BTreeSet<String>,
    pending: Option<&FolderWrite>,
    memory_files: &Files,
    include_memories: bool,
    recovering: bool,
) -> Result<Option<FolderSource>> {
    let moved = || {
        if recovering {
            return changed_folder(
                "restore the admitted tip and retry recovery, or run `engram task-sync export --rebuild --include-memories --memory-recovery-basis OWN_EXPORT` to replace your output after preserving newer changes",
            );
        }
        let expected = match pending.and_then(|write| write.parent.as_ref()) {
            Some(ExportParent::Commit(commit)) => Some(commit.as_str()),
            Some(ExportParent::Unborn) => None,
            None => match &head.disk {
                FolderDisk::Committed { commit } => Some(commit.as_str()),
                FolderDisk::Written { from, .. } => Some(from.commit.as_str()),
                FolderDisk::Unbound => None,
            },
        };
        export_basis_mismatch(data, expected, include_memories)
    };
    if head.rebuild_needs_complete_import() {
        return Err(unbound_rebuild_basis_required(include_memories));
    }
    let folder = data.checkout.join(".engram");
    if let Some(write) = pending {
        if !same_parent(data, write) {
            return Err(moved());
        }
        for (relative, bytes) in &write.files {
            let written = bytes.as_deref().map(|bytes| fingerprint(bytes.as_bytes()));
            let current = fingerprint_path(&folder, relative)?;
            if current != written {
                return Err(changed_folder(
                    "the folder no longer holds the last export's files: restore them, or run `engram task-sync import` to account for a discarded export",
                ));
            }
        }
        if dirty.iter().any(|changed| {
            changed.strip_prefix(".engram/").is_none_or(|relative| {
                !write.files.contains_key(relative) && !write.memory_files.contains_key(relative)
            })
        }) {
            return Err(changed_folder(
                "the data folder changed beyond the last export's own files: commit what it wrote first, then import any other host's changes before exporting",
            ));
        }
        return Ok(match &head.disk {
            FolderDisk::Written { from, .. } => Some(from.clone()),
            _ => None,
        });
    }
    match &head.disk {
        FolderDisk::Committed { commit } if data.head_commit() == Some(commit.as_str()) => {
            if dirty.iter().any(|path| {
                path.strip_prefix(".engram/")
                    .is_none_or(|relative| !memory_files.contains_key(relative))
            }) {
                if recovering {
                    return Err(rebuild_combined_export());
                }
                return Err(changed_folder(
                    "the data folder has uncommitted changes: commit or restore them, then import any new changes before exporting",
                ));
            }
            Ok(head.import_basis())
        }
        // The old journal can finish at the commit it was written onto.
        // There is no retained proof with which to adopt a later commit.
        FolderDisk::Written { from, files } if data.head_commit() == Some(from.commit.as_str()) => {
            for (&id, expected) in files {
                if fingerprint_on_disk(&folder, id)? != *expected {
                    return Err(moved());
                }
            }
            if dirty.iter().any(|changed| {
                changed != ".engram/project.json"
                    && changed.strip_prefix(".engram/").is_none_or(|relative| {
                        task_id(relative).map_or(true, |id| !files.contains_key(&id))
                    })
            }) {
                return Err(moved());
            }
            Ok(None)
        }
        FolderDisk::Unbound if recovering => Err(rebuild_combined_export()),
        FolderDisk::Unbound => Err(changed_folder(
            "the last import or export has no committed comparison basis: commit the shared folder and import with `--rebuild --basis BASIS/project.json`, or export with `--rebuild`",
        )),
        _ => Err(moved()),
    }
}

fn export_basis_mismatch(
    data: &DataWorktree,
    expected: Option<&str>,
    include_memories: bool,
) -> anyhow::Error {
    let observed = data.head_commit();
    let memory_flag = if include_memories {
        " --include-memories"
    } else {
        ""
    };
    let rebuild = format!("`engram task-sync export --rebuild{memory_flag}`");
    if observed == expected {
        return changed_folder(&format!(
            "the data folder differs from the last export at HEAD {}; run `engram task-sync import{memory_flag}` first, or {rebuild} to replace the shared folder",
            observed.unwrap_or("(unborn)"),
        ));
    }
    let reason = format!(
        "data HEAD {} does not match the admitted export basis {}",
        observed.unwrap_or("(unborn)"),
        expected.unwrap_or("(unborn)"),
    );
    if let (Some(expected), Some(observed)) = (expected, observed) {
        // Recommend ordinary import only when Git proves that it can start
        // from the recorded commit. This query runs only on refusal.
        if task_sync::ledger::oid(expected).is_ok()
            && task_sync::git::query(
                &data.checkout,
                &["merge-base", "--is-ancestor", expected, observed],
                false,
                None,
                GIT_DEADLINE,
            )
            .is_ok()
        {
            return changed_folder(&format!(
                "{reason}; run `engram task-sync import{memory_flag}` first, or {rebuild} to replace the shared folder"
            ));
        }
        return changed_folder(&format!(
            "{reason}; restore the data branch to {expected} and retry, or run {rebuild} to replace the shared folder"
        ));
    }
    changed_folder(&format!(
        "{reason}; run {rebuild} to replace the shared folder"
    ))
}

fn changed_folder(reason: &str) -> anyhow::Error {
    TaskSyncRefusal::IncrementalBasisUnavailable
        .invalid(reason.to_owned())
        .into()
}

fn written_disk(from: FolderSource, files: &Files) -> Result<FolderDisk> {
    let mut written = BTreeMap::new();
    for (relative, bytes) in files {
        let after = bytes.as_deref().map(|bytes| fingerprint(bytes.as_bytes()));
        written.insert(task_id(relative)?, after);
    }
    Ok(FolderDisk::Written {
        from,
        files: written,
    })
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
    Ok(folder_file_bytes(folder, relative)?
        .as_deref()
        .map(fingerprint))
}

fn folder_file_bytes(folder: &Path, relative: &str) -> Result<Option<Vec<u8>>> {
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
        Ok(_) => Ok(Some(std::fs::read(&file)?)),
    }
}

type Files = BTreeMap<String, Option<String>>;

/// Each path's content fingerprint as planning read it, `None` if absent.
type Preimages = BTreeMap<String, Option<String>>;

const GIT_DEADLINE: Duration = Duration::from_secs(60);

#[cfg(test)]
type ExportRetainHook = fn() -> Result<()>;

#[cfg(test)]
thread_local! {
    /// Task files this thread read from the folder on disk.
    pub(super) static FILE_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    pub(super) static BEFORE_EXPORT_PREIMAGES: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
    /// Runs once an export is planned, before it looks at the folder again.
    pub(super) static AFTER_EXPORT_PLAN: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
    pub(super) static AFTER_EXPORT_RETAIN: std::cell::Cell<Option<ExportRetainHook>> =
        const { std::cell::Cell::new(None) };
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
    data: &DataWorktree,
    parent_paths: &ExportParentPaths,
) -> Result<(Files, InventoryRoot, u64, u64, Preimages)> {
    #[cfg(test)]
    if let Some(hook) = BEFORE_EXPORT_PREIMAGES.with(|hook| hook.borrow_mut().take()) {
        hook();
    }
    let folder = data.checkout.join(".engram");
    let folder = folder.as_path();
    let mut files = Files::new();
    let mut read = Preimages::new();
    let mut root = head.inventory_root;
    let mut count = head.task_count;
    let mut task_bytes = head.task_bytes;
    for (&id, document) in &plan.changes {
        let old = read_on_disk(folder, id, Some(parent_paths))?;
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
    Ok(read_on_disk(folder, id, None)?.map(|(task, stored, _)| (task, stored)))
}

/// Return the size of the matching Git preimage. The caller keeps the raw
/// checkout bytes for its replay fingerprint.
fn require_parent_preimage(
    parent_paths: &ExportParentPaths,
    relative: &str,
    bytes: Option<&[u8]>,
) -> Result<usize> {
    if parent_paths.matches(relative, bytes)? {
        return Ok(bytes.map_or(0, <[u8]>::len));
    }
    if let Some(bytes) = bytes
        && let Ok(text) = std::str::from_utf8(bytes)
        && text.contains("\r\n")
    {
        let lf = text.replace("\r\n", "\n");
        if parent_paths.matches(relative, Some(lf.as_bytes()))? {
            return Ok(lf.len());
        }
    }
    Err(changed_folder(&format!(
        "the data folder's {relative} differs from the admitted commit; nothing was written. Commit and import the edit, or set it aside and restore the file from the admitted commit before exporting again"
    )))
}

/// One task file read once: its document, stored size and content
/// fingerprint, `None` if absent.
fn read_on_disk(
    folder: &Path,
    id: WorkId,
    parent_paths: Option<&ExportParentPaths>,
) -> Result<Option<(TaskDocument, u64, String)>> {
    let relative = path(id);
    let bytes = folder_file_bytes(folder, &relative)?;
    let stored = match parent_paths {
        Some(parent_paths) => require_parent_preimage(parent_paths, &relative, bytes.as_deref())?,
        None => bytes.as_deref().map_or(0, <[u8]>::len),
    };
    let Some(bytes) = bytes else { return Ok(None) };
    #[cfg(test)]
    FILE_READS.with(|count| count.set(count.get() + 1));
    let task: TaskDocument =
        task_sync::parse(&bytes).map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
    if task.id != id {
        return Err(TaskSyncRefusal::InvalidGraph
            .invalid("task filename/id mismatch")
            .into());
    }
    Ok(Some((task, u64::try_from(stored)?, fingerprint(&bytes))))
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
