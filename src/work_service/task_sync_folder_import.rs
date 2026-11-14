//! The ordinary folder import: incremental from the recorded folder head's
//! source commit, or the named complete route (enrolment or rebuild) that
//! seeds or replaces the head.

use std::time::Duration;

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;

use super::LocalWorkService;
use crate::{
    storage::{
        FolderDeltaApply, FolderHead, FolderHeadReason, FolderSource, TaskSyncImportResult,
        TaskSyncTaskStatus,
    },
    task_sync::{
        TaskSyncRefusal,
        data_worktree::DataWorktree,
        directory::{self, DestinationBasis},
        inventory::InventoryRoot,
        progress::ExportProfile,
    },
};

/// What one folder import did, and by which route.
#[derive(Debug, Serialize)]
pub struct FolderImportReceipt {
    /// `incremental`, `unchanged`, `enrol` or `rebuild`.
    pub route: &'static str,
    #[serde(flatten)]
    pub result: TaskSyncImportResult,
}

const GIT_DEADLINE: Duration = Duration::from_secs(60);

impl LocalWorkService {
    /// Import the data worktree's `.engram` folder.
    ///
    /// With a recorded folder head that names an imported source commit, the
    /// folder must be committed: only the paths changed between that commit
    /// and the worktree's HEAD are read, through a verified Git delta, and
    /// admitted per task. Without one, or with `rebuild`, the complete folder
    /// is imported against `basis` as before and the head is seeded afresh.
    ///
    /// # Errors
    /// Refuses a read-only service, a missing opt-in, an uncommitted folder
    /// for an incremental import (remedy: commit it, or `--rebuild`), a
    /// complete route without a basis, and every refusal of either route.
    pub fn import_task_sync_data_folder(
        &self,
        data: &DataWorktree,
        basis: Option<&DestinationBasis>,
        rebuild: bool,
        dry_run: bool,
        now: DateTime<Utc>,
    ) -> Result<FolderImportReceipt> {
        self.require_task_sync_writer()?;
        self.require_task_sync_enabled()?;
        let envelope = data.checkout.join(".engram").join("project.json");
        let incoming_basis = directory::read_folder_basis(&envelope)
            .with_context(|| format!("reading {}", envelope.display()))?;
        let lookup = ExportProfile {
            include_notes: incoming_basis.profile.include_notes,
            origin_machine: None,
        };
        let recorded = self.task_sync_folder_head(&lookup, now)?;
        let incremental = !rebuild
            && recorded
                .as_ref()
                .is_some_and(|(_, head)| head.source.is_some());
        if incremental {
            let (id, head) = recorded.context("incremental import without a head")?;
            return self.import_incremental(data, &id, &head, dry_run, now);
        }
        let basis = basis.ok_or_else(|| {
            TaskSyncRefusal::IncrementalBasisUnavailable.invalid(
                "this complete import needs this host's own export basis: pass `--basis OWN_EXPORT/project.json`, or `--empty-basis` for an empty store",
            )
        })?;
        let committed = data.committed_folder()?;
        let folder = directory::read_resolved_data_folder(data)?;
        let result = self.import_task_sync_folder(&folder, basis, dry_run, now)?;
        if !dry_run {
            let profile = match basis {
                DestinationBasis::Recorded(recorded) => recorded.profile.clone(),
                DestinationBasis::Empty => folder.profile.clone(),
            };
            let root = folder
                .tree
                .tasks
                .iter()
                .try_fold(InventoryRoot::ZERO, InventoryRoot::including)?;
            let count = u64::try_from(folder.tree.tasks.len())?;
            let pending = result
                .tasks
                .iter()
                .filter(|task| task.status != TaskSyncTaskStatus::Applied)
                .map(|task| task.task)
                .collect::<std::collections::BTreeSet<_>>();
            // A partial import keeps the earlier source, as the incremental
            // route does, so the held tasks are read again by the next import.
            let source = if pending.is_empty() {
                committed.clone().map(|commit| FolderSource {
                    commit,
                    inventory_root: root,
                    task_count: count,
                })
            } else {
                recorded.as_ref().and_then(|(_, head)| head.source.clone())
            };
            let next = FolderHead {
                project_id: self.project_id.clone(),
                profile,
                previous: recorded.as_ref().map(|(id, _)| id.clone()),
                reason: if rebuild {
                    FolderHeadReason::Rebuild
                } else {
                    FolderHeadReason::Import
                },
                cut: self
                    .read_store_at(now)?
                    .task_sync_project_cut(&self.project_id)?,
                inventory_root: root,
                task_count: count,
                source,
                folder_commit: committed,
                pending,
            };
            self.record_task_sync_folder_head(&next, now)?;
        }
        Ok(FolderImportReceipt {
            route: if rebuild { "rebuild" } else { "enrol" },
            result,
        })
    }

    fn import_incremental(
        &self,
        data: &DataWorktree,
        id: &crate::ObjectId,
        head: &FolderHead,
        dry_run: bool,
        now: DateTime<Utc>,
    ) -> Result<FolderImportReceipt> {
        let source = head
            .source
            .as_ref()
            .context("incremental import without a source")?;
        let incoming = data.committed_folder()?.ok_or_else(|| {
            TaskSyncRefusal::CommitContextRequired.invalid(
                "commit the merged folder in the data worktree before importing; an incremental import reads the commit, not loose files. Or run `engram task-sync import --rebuild --basis OWN_EXPORT/project.json` to import the complete folder",
            )
        })?;
        if incoming == source.commit && head.pending.is_empty() {
            return Ok(FolderImportReceipt {
                route: "unchanged",
                result: TaskSyncImportResult {
                    task_count: usize::try_from(source.task_count).unwrap_or(usize::MAX),
                    changed: false,
                    dry_run,
                    tasks: Vec::new(),
                },
            });
        }
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
        let changes = delta
            .changed
            .into_iter()
            .map(|(id, change)| (id, change.after))
            .collect();
        let actor = self.actor("task-sync:import", "import reported task planning state");
        let outcome = self
            .lock_store_at(now)?
            .apply_task_sync_folder_delta(&FolderDeltaApply {
                project: &self.project_id,
                actor: &actor,
                profile: &head.profile,
                head: id,
                incoming: FolderSource {
                    commit: incoming,
                    inventory_root: delta.root,
                    task_count: count,
                },
                changes: &changes,
                dry_run,
                now,
            })?;
        Ok(FolderImportReceipt {
            route: "incremental",
            result: outcome.result,
        })
    }
}
