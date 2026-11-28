//! The ordinary folder import: incremental from the recorded folder head's
//! source commit, or the named complete route (enrolment or rebuild) that
//! records a fresh head in its own transaction.
//!
//! An import holds the data worktree's folder lock, as an export does, and
//! first finishes or refuses an export interrupted after recording its head.

use std::{
    collections::{BTreeMap, BTreeSet},
    time::Duration,
};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;

use super::{
    LocalWorkService,
    task_sync_folder::CompleteHead,
    task_sync_folder_export::{lock_folder, unbound_rebuild_basis_required},
};
use crate::{
    WorkId,
    storage::{
        FolderDeltaApply, FolderDisk, FolderExportConfirmation, FolderHead, FolderSource,
        TaskSyncImportResult,
    },
    task_sync::{
        TaskSyncRefusal,
        data_worktree::DataWorktree,
        directory::{self, DestinationBasis},
        inventory::InventoryRoot,
        memories::{
            ImportOptions, ImportOutcome, KeyHistory, KeyImportResult, directory::PublicationLock,
        },
        progress::ExportProfile,
    },
};

fn rebuilt_commit_required() -> anyhow::Error {
    TaskSyncRefusal::CommitContextRequired
        .invalid("commit the rebuilt folder in the data worktree before importing it")
        .into()
}

fn committed_recovery_required(include_memories: bool) -> anyhow::Error {
    let memories = if include_memories {
        " --include-memories"
    } else {
        ""
    };
    TaskSyncRefusal::CommitContextRequired
        .invalid(format!("recovery requires the clean committed task and memory folder; commit or set aside loose files, then retry `engram task-sync import --basis OWN_EXPORT/project.json{memories}` with your saved envelope"))
        .into()
}

/// What one folder import did, and by which route.
#[derive(Debug, Serialize)]
pub struct FolderImportReceipt {
    /// `incremental`, `unchanged`, `enrol`, `complete` or `rebuild`.
    pub route: &'static str,
    #[serde(flatten)]
    pub result: TaskSyncImportResult,
    /// Each selected memory key's result, imported after the tasks in the
    /// same transaction.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub memories: Vec<KeyImportResult>,
    /// Whether any selected memory key was not applied.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub memory_partial: bool,
}

/// The memory component an import includes: its selected histories, read
/// under the memory publication fence the caller holds, and per-key options.
pub struct MemoryImport<'a> {
    pub fence: &'a PublicationLock,
    pub selected: &'a BTreeMap<String, KeyHistory>,
    pub options: &'a BTreeMap<String, ImportOptions>,
    /// The encoded bytes of the selected memory files, counted against the
    /// folder's size limit.
    pub bytes: u64,
    /// Called for each new retirement after the database commits and before
    /// the recovery journal is discarded.
    pub on_retirement: Option<&'a dyn Fn(&ImportOutcome)>,
}

fn report_retirements(
    memories: Option<&MemoryImport<'_>>,
    results: &[KeyImportResult],
    dry_run: bool,
) {
    if !dry_run && let Some(report) = memories.and_then(|memory| memory.on_retirement) {
        for result in results {
            if let KeyImportResult::Applied { outcome, .. } = result
                && outcome.retired
            {
                report(outcome);
            }
        }
    }
}

const GIT_DEADLINE: Duration = Duration::from_secs(60);

impl LocalWorkService {
    /// Import the data worktree's `.engram` folder.
    ///
    /// With a recorded folder head that names an imported source commit, and
    /// no `basis`, the folder must be committed: only the paths changed
    /// between that commit and the worktree's HEAD are read, through a
    /// verified Git delta, together with the tasks the last import held and
    /// the published tasks the exports since wrote, read again at HEAD, and
    /// admitted per task. Without one, with a `basis`, or with `rebuild`, the
    /// complete folder is imported against `basis` and its head recorded in
    /// the same transaction. After a complete export since the last import,
    /// the complete folder is imported against that recorded head itself. Either route imports the selected memories after
    /// the tasks, in the same transaction.
    ///
    /// # Errors
    /// Refuses a read-only service, a missing opt-in, another running export
    /// or import, an interrupted export over a changed folder, an uncommitted
    /// folder for an incremental import (remedy: commit it, or `--rebuild`),
    /// a complete route without a basis, a folder that carries the memory
    /// component when memories are not included, and every refusal of either
    /// route.
    pub fn import_task_sync_data_folder(
        &self,
        data: &DataWorktree,
        basis: Option<&DestinationBasis>,
        rebuild: bool,
        dry_run: bool,
        memories: Option<&MemoryImport<'_>>,
        now: DateTime<Utc>,
    ) -> Result<FolderImportReceipt> {
        self.require_task_sync_writer()?;
        self.require_task_sync_enabled()?;
        let _lock = lock_folder(data)?;
        directory::FolderWrite::require_no_unowned_journal(data)?;
        if memories.is_none() {
            let saved_basis = matches!(basis, Some(DestinationBasis::Recorded(_)));
            self.require_task_only_folder(data, "import", saved_basis, now)?;
        } else {
            crate::task_sync::memories::directory::require_no_pending_publication(data)?;
        }
        directory::require_no_capability_staging(data)?;
        let automatic = basis.is_none();
        let recovery = self.prepare_folder_import(data, basis, rebuild, dry_run, now)?;
        let envelope = data.checkout.join(".engram").join("project.json");
        let incoming_basis = directory::read_folder_basis(&envelope)
            .with_context(|| format!("reading {}", envelope.display()))?;
        if incoming_basis.memory_capable && memories.is_none() {
            return Err(TaskSyncRefusal::MemoryProfileRequiresOptIn
                .invalid("memory-capable profile requires explicit --include-memories")
                .into());
        }
        let confirmation = recovery.confirmation.as_ref().filter(|proof| {
            proof.head.profile.include_notes == incoming_basis.profile.include_notes
        });
        let no_memories: (
            BTreeMap<String, KeyHistory>,
            BTreeMap<String, ImportOptions>,
        ) = (BTreeMap::new(), BTreeMap::new());
        let (selected, options) = memories.map_or((&no_memories.0, &no_memories.1), |memories| {
            (memories.selected, memories.options)
        });
        let lookup = ExportProfile {
            include_notes: incoming_basis.profile.include_notes,
            origin_machine: None,
        };
        let recorded = match confirmation {
            Some(proof) => Some((proof.expected.clone(), proof.comparison_head())),
            None => self.task_sync_folder_head(&lookup, now)?,
        };
        let import_basis = recorded
            .as_ref()
            .map(|(_, head)| self.read_store_at(now)?.task_sync_folder_import_basis(head))
            .transpose()?
            .flatten();
        // The alias local planning is rendered under for admission: this
        // host's own, or the folder's while this host has exported none.
        let rendering = |head: &FolderHead| ExportProfile {
            origin_machine: head
                .profile
                .origin_machine
                .clone()
                .or_else(|| incoming_basis.profile.origin_machine.clone()),
            ..head.profile.clone()
        };
        let export_profile = recorded
            .as_ref()
            .map_or_else(|| lookup.clone(), |(_, head)| head.profile.clone());
        let guide_conflicts = |result: &mut TaskSyncImportResult| {
            for task in &mut result.tasks {
                if task.reason.as_deref() == Some("local_planning_changed") {
                    task.detail = Some(self.held_planning_rebuild_detail(
                        data,
                        &export_profile,
                        memories.is_some(),
                    ));
                }
            }
        };
        if automatic
            && recorded
                .as_ref()
                .is_some_and(|(_, head)| head.rebuild_needs_complete_import())
        {
            return Err(unbound_rebuild_basis_required(memories.is_some()));
        }
        let rewritten = recorded.as_ref().is_some_and(|(_, head)| {
            head.rebuild_needs_complete_import() || (head.rewritten && import_basis.is_some())
        });
        // An explicit basis selects the complete route, which records a
        // fresh head; without one, a head that names an imported commit
        // continues incrementally.
        let incremental = !rebuild && basis.is_none() && !rewritten && import_basis.is_some();
        if incremental {
            let (id, head) = recorded.context("incremental import without a head")?;
            let mut receipt = self.import_incremental(
                data,
                &id,
                &head,
                &import_basis.context("incremental import without a source")?,
                &rendering(&head),
                dry_run,
                selected,
                options,
                memories.map_or(0, |memories| memories.bytes),
                confirmation,
                now,
            )?;
            guide_conflicts(&mut receipt.result);
            report_retirements(memories, &receipt.memories, dry_run);
            recovery.finish(data)?;
            return Ok(receipt);
        }
        let own_head;
        let basis = match (basis, &recorded) {
            (Some(basis), _) => basis,
            (None, Some((_, head))) if rewritten => {
                own_head = DestinationBasis::Recorded(Box::new(directory::FolderBasis {
                    project_id: head.project_id.clone(),
                    profile: rendering(head),
                    export_basis: None,
                    incremental: Some(directory::IncrementalBasis {
                        format: directory::INCREMENTAL_FORMAT.into(),
                        inventory_root: head.inventory_root,
                        task_count: head.task_count,
                        cut: head.cut.clone(),
                    }),
                    memory_capable: false,
                }));
                &own_head
            }
            (None, _) => {
                return Err(TaskSyncRefusal::IncrementalBasisUnavailable.invalid(
                    "this complete import needs this host's own export basis: pass `--basis OWN_EXPORT/project.json`, or `--empty-basis` for an empty store. Once a committed folder is imported completely, later imports are incremental",
                )
                .into());
            }
        };
        let recovery_required = || committed_recovery_required(memories.is_some());
        let committed = if recovery.requires_committed_basis {
            if !data.folder_changes_admitted(memories.is_some())?.is_empty() {
                return Err(recovery_required());
            }
            Some(data.head_commit().ok_or_else(recovery_required)?.to_owned())
        } else {
            data.committed_folder()?
        };
        if automatic && rewritten && committed.is_none() {
            return Err(rebuilt_commit_required());
        }
        let folder = match memories {
            Some(memories) => {
                directory::read_resolved_data_folder_with_memories_locked(data, memories.fence)?
            }
            None => directory::read_resolved_data_folder(data)?,
        };
        // The folder is recorded as its commit only when that commit holds
        // exactly the envelope and task files read from disk: one an ignore
        // rule hides from Git status is on disk but not in the commit.
        if let Some(commit) = committed.as_deref() {
            data.check_commit(commit)?;
            let on_disk = folder
                .tree
                .tasks
                .iter()
                .map(|task| format!(".engram/tasks/{}.json", task.id.0))
                .chain(std::iter::once(".engram/project.json".to_owned()))
                .collect::<BTreeSet<_>>();
            if data.committed_task_files(commit)? != on_disk {
                return Err(TaskSyncRefusal::CommitContextRequired
                    .invalid(format!(
                        "the data folder's task files differ from its commit {commit}, for example a task file an ignore rule hides from Git: commit or remove it, then import again"
                    ))
                    .into());
            }
        }
        if recovery.requires_committed_basis {
            let commit = committed.as_deref().ok_or_else(recovery_required)?;
            let snapshot = crate::task_sync::git::read_task_snapshot_bounded(
                &data.checkout,
                &self.project_id,
                commit,
                GIT_DEADLINE,
            )?;
            if snapshot.tree != folder.tree
                || snapshot.envelope != incoming_basis
                || (memories.is_some()
                    && crate::task_sync::git::read_memory_snapshot(
                        &data.checkout,
                        &self.project_id,
                        commit,
                    )? != *selected)
            {
                return Err(recovery_required());
            }
        }
        let root = folder
            .tree
            .tasks
            .iter()
            .try_fold(InventoryRoot::ZERO, InventoryRoot::including)?;
        let task_count = u64::try_from(folder.tree.tasks.len())?;
        // The bytes the folder's task files hold, as read, not as re-encoded.
        let task_bytes =
            super::task_sync_folder_export::stored_task_bytes(&data.checkout.join(".engram"))?;
        let (outcome, head) = self.import_task_sync_folder_with_head(
            &folder,
            basis,
            selected,
            options,
            dry_run,
            now,
            Some(CompleteHead {
                previous: recorded.as_ref().map(|(id, _)| id),
                commit: committed.clone(),
                inventory_root: root,
                task_count,
                task_bytes,
                rebuild,
            }),
            // One session syncs a data folder at a time: the folder admitted
            // above is not listed again, and the tip is checked before applying.
            || data.check_head(),
        )?;
        report_retirements(memories, &outcome.memories, dry_run);
        if !recovery.requires_committed_basis
            || head
                .and_then(|head| head.import_basis())
                .is_some_and(|basis| committed.as_deref() == Some(basis.commit.as_str()))
        {
            recovery.finish(data)?;
        }
        let mut result = outcome.tasks;
        guide_conflicts(&mut result);
        Ok(FolderImportReceipt {
            // `complete` is an explicit basis chosen over a head the
            // incremental route could have continued from.
            route: if rebuild {
                "rebuild"
            } else if rewritten || import_basis.is_some() {
                "complete"
            } else {
                "enrol"
            },
            result,
            memories: outcome.memories,
            memory_partial: outcome.memory_partial,
        })
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "the incremental route's head and the memory component beside the import's own inputs"
    )]
    fn import_incremental(
        &self,
        data: &DataWorktree,
        id: &crate::ObjectId,
        head: &FolderHead,
        source: &FolderSource,
        rendering: &ExportProfile,
        dry_run: bool,
        memories: &BTreeMap<String, KeyHistory>,
        memory_options: &BTreeMap<String, ImportOptions>,
        memory_bytes: u64,
        confirmation: Option<&FolderExportConfirmation>,
        now: DateTime<Utc>,
    ) -> Result<FolderImportReceipt> {
        let incoming = data.committed_folder()?.ok_or_else(|| {
            TaskSyncRefusal::CommitContextRequired.invalid(
                "commit the merged folder in the data worktree before importing; an incremental import reads the commit, not loose files. Or run `engram task-sync import --rebuild --basis OWN_EXPORT/project.json` to import the complete folder",
            )
        })?;
        // A head with files an export wrote is never unchanged: when they were
        // discarded with no commit since, this import is what owes them again.
        // Selected memories are always imported.
        if incoming == source.commit
            && head.pending.is_empty()
            && !matches!(head.disk, FolderDisk::Written { .. })
            && memories.is_empty()
            && memory_options.is_empty()
        {
            if let Some(proof) = confirmation {
                self.lock_store_at(now)?.confirm_task_sync_folder_export(
                    proof,
                    dry_run,
                    now,
                    || data.check_head(),
                )?;
            }
            return Ok(FolderImportReceipt {
                route: "unchanged",
                result: TaskSyncImportResult {
                    task_count: usize::try_from(source.task_count).unwrap_or(usize::MAX),
                    changed: false,
                    dry_run,
                    tasks: Vec::new(),
                },
                memories: Vec::new(),
                memory_partial: false,
            });
        }
        // A new commit becomes the recorded source: its root must hold only
        // `.engram/`, as every data commit's does. The unchanged source above
        // was checked when it was recorded.
        data.check_commit(&incoming)?;
        let delta = crate::task_sync::git::read_verified_task_delta(
            &data.checkout,
            &self.project_id,
            &source.commit,
            source.inventory_root,
            &incoming,
            GIT_DEADLINE,
        )?;
        let mut count = source.task_count;
        for change in delta.changed.values() {
            match (&change.before, &change.after) {
                (None, Some(_)) => count += 1,
                (Some(_), None) => {
                    count = count
                        .checked_sub(1)
                        .context("source task count is inconsistent")?;
                }
                _ => {}
            }
        }
        // The stored bytes of every changed file, as Git holds them.
        let task_bytes = source
            .task_bytes
            .saturating_sub(delta.bytes_before)
            .saturating_add(delta.bytes_after);
        // The folder the delta leads to stays within what the complete reader
        // would take, envelope, policy files and memory component included.
        let folder = data.checkout.join(".engram");
        // Count the selected memory files using their encoded size.
        let mut other_bytes = memory_bytes;
        for name in ["project.json", ".gitignore", ".gitattributes"] {
            if let Ok(meta) = std::fs::symlink_metadata(folder.join(name)) {
                other_bytes = other_bytes.saturating_add(meta.len());
            }
        }
        directory::admit_folder_size(count, task_bytes.saturating_add(other_bytes))?;
        let mut changes = delta
            .changed
            .into_iter()
            .map(|(id, change)| (id, change.after))
            .collect::<std::collections::BTreeMap<_, _>>();
        let (discarded, mut written) = match &head.disk {
            FolderDisk::Written { from, files } => {
                discarded_writes(&data.checkout, &from.commit, &incoming, files)?
            }
            _ => (BTreeSet::new(), WrittenDocuments::new()),
        };
        let mut shared = std::collections::BTreeMap::new();
        // A task whose last write no commit holds was written onto `from`,
        // whose task files that export proved were this host's own: its bytes
        // there are this host's known state, whether this host published or
        // imported them. The incoming commit holding the same carries nothing
        // new, so the task joins the baseline as shared while the discarded
        // edit stays owed to the next export. Anything else there, a removal
        // or a return to older bytes included, is another host's change, and
        // meets that owed edit as divergent planning: never overwritten.
        if let FolderDisk::Written { from, .. } = &head.disk
            && !discarded.is_empty()
        {
            // The rule rests on one fact, checked here rather than assumed:
            // the commit an export was written onto held this host's recorded
            // inventory. From that inventory, the commits since must lead to
            // the same incoming root the verified delta from the last import
            // did. Read only along the paths those commits change.
            let reached = crate::task_sync::git::read_rooted_task_diff(
                &data.checkout,
                &self.project_id,
                &from.commit,
                from.inventory_root,
                &incoming,
                GIT_DEADLINE,
            )?
            .root;
            if reached != delta.root {
                return Err(TaskSyncRefusal::IncrementalBasisUnavailable
                    .invalid(format!(
                        "the commit {} this host's last export was written onto no longer holds the inventory that export recorded, so its discarded files cannot be told from another host's changes; run `engram task-sync import --rebuild --basis OWN_EXPORT/project.json` to import the complete folder",
                        from.commit
                    ))
                    .into());
            }
            let known = crate::task_sync::git::read_tasks_at(
                &data.checkout,
                &self.project_id,
                &from.commit,
                discarded.iter().copied(),
                GIT_DEADLINE,
            )?;
            for (id, document) in crate::task_sync::git::read_tasks_at(
                &data.checkout,
                &self.project_id,
                &incoming,
                discarded.iter().copied(),
                GIT_DEADLINE,
            )? {
                let own = known.get(&id).cloned().flatten();
                if document == own {
                    changes.remove(&id);
                    if let Some(own) = own {
                        shared.insert(id, own);
                    }
                } else {
                    changes.insert(id, document);
                }
            }
        }
        // What this host last agreed, for every task it exported since the
        // last import. A write the current files list is read above, where a
        // commit published it. One an earlier export wrote, whose commit the
        // export's base `from` already holds, is read at `from` when its file
        // there is the one this host recorded; a task whose last write was
        // discarded agrees on its bytes at `from`, this host's known state.
        // Any other falls back to the baseline.
        if let FolderDisk::Written { from, .. } = &head.disk {
            let earlier = head
                .exported
                .keys()
                .chain(&discarded)
                .copied()
                .filter(|id| !written.contains_key(id))
                .collect::<BTreeSet<_>>();
            if !earlier.is_empty() {
                for (id, document) in crate::task_sync::git::read_tasks_at(
                    &data.checkout,
                    &self.project_id,
                    &from.commit,
                    earlier,
                    GIT_DEADLINE,
                )? {
                    let agreed = discarded.contains(&id) || {
                        let at_from = document
                            .as_ref()
                            .map(super::task_sync_folder_export::written_fingerprint)
                            .transpose()?;
                        head.exported.get(&id) == Some(&at_from)
                    };
                    if agreed {
                        written.insert(id, document);
                    }
                }
            }
        }
        // The tasks this host exported and published since the last import
        // are read again at the incoming commit: another host may have
        // returned one to exactly its bytes at the source commit, or removed
        // one this host added, which the diff from there cannot show. One
        // still as this host wrote it changed nothing, and is shared from now
        // on. A folder written by a complete export has no commit to tell
        // published from discarded, so its exports are not read again.
        if !matches!(head.disk, FolderDisk::Unbound) {
            let published = head
                .exported
                .keys()
                .copied()
                .filter(|id| {
                    !changes.contains_key(id)
                        && !head.pending.contains_key(id)
                        && !discarded.contains(id)
                })
                .collect::<BTreeSet<_>>();
            for (id, document) in crate::task_sync::git::read_tasks_at(
                &data.checkout,
                &self.project_id,
                &incoming,
                published,
                GIT_DEADLINE,
            )? {
                let arrived = document
                    .as_ref()
                    .map(super::task_sync_folder_export::written_fingerprint)
                    .transpose()?;
                if head.exported.get(&id) != Some(&arrived) {
                    changes.insert(id, document);
                } else if let Some(document) = document {
                    shared.insert(id, document);
                }
            }
        }
        // The tasks the last import held are read again at the incoming
        // commit, whether or not it changed them, so a resolved hold applies.
        let reread = head
            .pending
            .keys()
            .copied()
            .filter(|id| !changes.contains_key(id))
            .collect::<Vec<_>>();
        changes.extend(crate::task_sync::git::read_tasks_at(
            &data.checkout,
            &self.project_id,
            &incoming,
            reread,
            GIT_DEADLINE,
        )?);
        // Every read above names the incoming commit by id. After the last
        // of them, that commit must still be the worktree's HEAD before its
        // outcome is recorded as the folder on disk.
        let actor = self.actor("task-sync:import", "import reported task planning state");
        let outcome = self
            .lock_store_at(now)?
            .apply_task_sync_folder_delta_with_confirmation(
                &FolderDeltaApply {
                    project: &self.project_id,
                    actor: &actor,
                    profile: rendering,
                    head: id,
                    incoming: FolderSource {
                        commit: incoming,
                        inventory_root: delta.root,
                        task_count: count,
                        task_bytes,
                    },
                    changes: &changes,
                    shared: &shared,
                    discarded: &discarded,
                    written: &written,
                    memories,
                    memory_options,
                    dry_run,
                    now,
                },
                confirmation,
                || data.check_head(),
            )?;
        Ok(FolderImportReceipt {
            route: "incremental",
            result: outcome.result,
            memories: outcome.memories,
            memory_partial: outcome.memory_partial,
        })
    }
}

/// The documents this host's exports wrote that a commit published, each
/// with the document written, `None` for a removal.
type WrittenDocuments = BTreeMap<WorkId, Option<crate::task_sync::TaskDocument>>;

/// The tasks whose files the last export wrote onto `from` that no commit
/// from there to `incoming` holds, nor `from` itself: discarded before a
/// commit. A file some commit holds was published, even when a later commit
/// changed it again, including a merge that kept the other side's version, so
/// every commit touching the path is listed, without history simplification.
/// Only the commits that touch each written path are read. Each published
/// file comes back too, as the document this host wrote, `None` for a
/// removal: what this host last agreed for that task.
fn discarded_writes(
    repository: &std::path::Path,
    from: &str,
    incoming: &str,
    files: &std::collections::BTreeMap<WorkId, Option<String>>,
) -> Result<(BTreeSet<WorkId>, WrittenDocuments)> {
    let query = |args: &[&str], missing: bool| {
        crate::task_sync::git::query(repository, args, missing, None, GIT_DEADLINE)
    };
    let range = format!("{from}..{incoming}");
    let mut discarded = BTreeSet::new();
    let mut written_documents = WrittenDocuments::new();
    for (&id, written) in files {
        let path = format!(".engram/tasks/{}.json", id.0);
        let commits = String::from_utf8(query(
            &[
                "rev-list",
                "--full-history",
                "--end-of-options",
                &range,
                "--",
                &path,
            ],
            false,
        )?)?;
        let mut published = false;
        for commit in std::iter::once(from).chain(commits.lines().filter(|line| !line.is_empty())) {
            let blob = String::from_utf8(query(
                &[
                    "rev-parse",
                    "--verify",
                    "--quiet",
                    &format!("{commit}:{path}"),
                ],
                true,
            )?)?;
            let bytes = match blob.trim() {
                "" => None,
                blob => Some(query(&["cat-file", "blob", blob], false)?),
            };
            let held = bytes
                .as_deref()
                .map(|bytes| crate::ObjectId::from_canonical_bytes(bytes).to_string());
            if held == *written {
                published = true;
                let document = bytes
                    .map(|bytes| -> Result<crate::task_sync::TaskDocument> {
                        let document: crate::task_sync::TaskDocument =
                            crate::task_sync::parse(&bytes)
                                .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
                        if document.id != id {
                            return Err(TaskSyncRefusal::InvalidGraph
                                .invalid("task filename/id mismatch")
                                .into());
                        }
                        Ok(document)
                    })
                    .transpose()?;
                written_documents.insert(id, document);
                break;
            }
        }
        if !published {
            discarded.insert(id);
        }
    }
    Ok((discarded, written_documents))
}
