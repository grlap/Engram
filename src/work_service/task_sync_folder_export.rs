//! The ordinary folder export: incremental from the recorded folder head, or
//! the named bootstrap or rebuild of the complete folder.
//!
//! Every route plans on one snapshot, retains its exact file operations, then
//! records the next folder head while its predecessor is still current, and
//! only then touches the folder. An interrupted publication is finished by
//! the next export when its head was recorded, and dropped when it was not.

use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, Result};
use chrono::{DateTime, Utc};
use serde::Serialize;

use super::LocalWorkService;
use crate::{
    WorkId,
    storage::{FolderExportPlan, FolderHead, FolderHeadReason},
    task_sync::{
        self, TaskDocument, TaskSyncRefusal,
        data_worktree::DataWorktree,
        directory::{self, FolderWrite, INCREMENTAL_FORMAT, IncrementalBasis},
        inventory::InventoryRoot,
        notes::NoteOmissions,
        progress::ExportProfile,
    },
};

/// What one folder export did.
#[derive(Debug, Serialize)]
pub struct FolderExportReceipt {
    /// `bootstrap`, `rebuild`, `incremental` or `unchanged`.
    pub route: &'static str,
    pub notes_included: bool,
    pub inventory_root: InventoryRoot,
    pub task_count: u64,
    /// Task files written and removed; an unchanged file is not rewritten.
    pub written: usize,
    pub removed: usize,
    pub omissions: NoteOmissions,
    pub activity_omissions: NoteOmissions,
}

impl LocalWorkService {
    /// Export shared planning into the data worktree's `.engram` folder.
    ///
    /// With a recorded folder head only the tasks changed since it are
    /// written, and the folder on disk must still be the one that head
    /// records. With none, or with `rebuild`, the complete folder is written
    /// and a fresh head seeded; the receipt names that route. Held tasks keep
    /// the other host's bytes on every route.
    ///
    /// # Errors
    /// Refuses a read-only service, a missing opt-in, a folder that is not the
    /// recorded one (remedy: commit or restore it, or `--rebuild`), a moved
    /// head, every refusal of the plan, and unsafe or failed file writes.
    pub fn export_task_sync_data_folder(
        &self,
        data: &DataWorktree,
        profile: &ExportProfile,
        rebuild: bool,
        now: DateTime<Utc>,
    ) -> Result<FolderExportReceipt> {
        self.require_task_sync_writer()?;
        self.require_task_sync_enabled()?;
        self.finish_interrupted_export(data, now)?;
        let plan = self.plan_task_sync_folder_export(profile, rebuild, now)?;
        let folder = data.checkout.join(".engram");
        let (route, files, root, count) = if plan.complete {
            complete(&plan, &folder, profile)?
        } else {
            let Some(planned) = incremental(&plan, data, profile)? else {
                let (_, head) = plan
                    .head
                    .as_ref()
                    .context("incremental plan without a head")?;
                return Ok(FolderExportReceipt {
                    route: "unchanged",
                    notes_included: plan.project.notes_included,
                    inventory_root: head.inventory_root,
                    task_count: head.task_count,
                    written: 0,
                    removed: 0,
                    omissions: plan.omissions,
                    activity_omissions: plan.activity_omissions,
                });
            };
            planned
        };
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
            source: previous.and_then(|(_, head)| head.source.clone()),
            folder_commit: None,
            pending: previous
                .map(|(_, head)| head.pending.clone())
                .unwrap_or_default(),
        };
        let mut files = files;
        files.insert(
            "project.json".into(),
            Some(String::from_utf8(directory::incremental_envelope(
                &plan.project,
                profile,
                IncrementalBasis {
                    format: INCREMENTAL_FORMAT.into(),
                    inventory_root: root,
                    task_count: count,
                    cut: plan.cut.clone(),
                },
            )?)?),
        );
        let written = files
            .iter()
            .filter(|(path, bytes)| path.as_str() != "project.json" && bytes.is_some())
            .count();
        let removed = files.values().filter(|bytes| bytes.is_none()).count();
        let write = FolderWrite {
            head: serde_json::to_value(&next)?,
            files,
        };
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
            written,
            removed,
            omissions: plan.omissions,
            activity_omissions: plan.activity_omissions,
        })
    }

    /// Finish a publication whose head was recorded, or drop one whose head
    /// was not; the folder is never touched before its head.
    fn finish_interrupted_export(&self, data: &DataWorktree, now: DateTime<Utc>) -> Result<()> {
        let Some(write) = FolderWrite::pending(data)? else {
            return Ok(());
        };
        let planned: FolderHead = serde_json::from_value(write.head.clone())?;
        let current = self.task_sync_folder_head(&planned.profile, now)?;
        if current.is_some_and(|(_, head)| head == planned) {
            write.publish(data)
        } else {
            FolderWrite::discard(data)
        }
    }
}

type Files = BTreeMap<String, Option<String>>;

fn path(id: WorkId) -> String {
    format!("tasks/{}.json", id.0)
}

/// The complete folder: every exported task, removal of every other task
/// file, and held tasks left with the bytes already on disk.
fn complete(
    plan: &FolderExportPlan,
    folder: &Path,
    profile: &ExportProfile,
) -> Result<(&'static str, Files, InventoryRoot, u64)> {
    let pending = plan
        .head
        .as_ref()
        .map(|(_, head)| head.pending.clone())
        .unwrap_or_default();
    let mut files = Files::new();
    let mut root = InventoryRoot::ZERO;
    let mut count = 0_u64;
    for (&id, document) in &plan.changes {
        if pending.contains(&id) {
            continue;
        }
        if let Some(document) = document {
            root = root.including(document)?;
            count += 1;
            files.insert(
                path(id),
                Some(String::from_utf8(task_sync::pretty(document)?)?),
            );
        }
    }
    for id in &pending {
        if let Some(held) = on_disk(folder, *id)? {
            root = root.including(&held)?;
            count += 1;
        }
    }
    // Any other task file left from an earlier folder is removed.
    let tasks = folder.join("tasks");
    if tasks.try_exists()? {
        for entry in std::fs::read_dir(&tasks)? {
            let name = entry?.file_name();
            let name = name.to_str().context("task file name is not UTF-8")?;
            let id = name
                .strip_suffix(".json")
                .and_then(|id| uuid::Uuid::parse_str(id).ok())
                .map(WorkId)
                .ok_or_else(|| {
                    TaskSyncRefusal::DestinationUnknownEntry
                        .invalid("the task folder holds an entry the format does not define")
                })?;
            if !pending.contains(&id) && !files.contains_key(&path(id)) {
                files.insert(path(id), None);
            }
        }
    }
    profile.options().validate()?;
    let route = if plan.head.is_none() {
        "bootstrap"
    } else {
        "rebuild"
    };
    Ok((route, files, root, count))
}

/// Only the changed tasks, against a folder that is still the recorded one.
fn incremental(
    plan: &FolderExportPlan,
    data: &DataWorktree,
    profile: &ExportProfile,
) -> Result<Option<(&'static str, Files, InventoryRoot, u64)>> {
    let folder = data.checkout.join(".engram");
    let folder = folder.as_path();
    let (_, head) = plan
        .head
        .as_ref()
        .context("incremental plan without a head")?;
    // After an import the folder is exactly its commit; after an export it
    // carries the envelope that export wrote.
    let recorded = match &head.folder_commit {
        Some(commit) => data.committed_folder()?.as_deref() == Some(commit.as_str()),
        None => {
            let expected = IncrementalBasis {
                format: INCREMENTAL_FORMAT.into(),
                inventory_root: head.inventory_root,
                task_count: head.task_count,
                cut: head.cut.clone(),
            };
            directory::read_folder_basis(&folder.join("project.json")).is_ok_and(|basis| {
                basis.incremental.as_ref() == Some(&expected)
                    && basis.profile == *profile
                    && basis.project_id == plan.project.project_id
            })
        }
    };
    if !recorded {
        return Err(TaskSyncRefusal::IncrementalBasisUnavailable
            .invalid(
                "the data folder on disk is not the one the last export or import recorded; \
                 commit or restore it, or run `engram task-sync export --rebuild` to write the \
                 complete folder and record it afresh",
            )
            .into());
    }
    if plan.changes.is_empty() {
        return Ok(None);
    }
    let mut files = Files::new();
    let mut root = head.inventory_root;
    let mut count = head.task_count;
    for (&id, document) in &plan.changes {
        if let Some(old) = on_disk(folder, id)? {
            root = root.excluding(&old)?;
            count = count
                .checked_sub(1)
                .context("folder task count is inconsistent")?;
        }
        if let Some(document) = document {
            root = root.including(document)?;
            count += 1;
        }
        files.insert(
            path(id),
            document
                .as_ref()
                .map(|document| {
                    Ok::<_, anyhow::Error>(String::from_utf8(task_sync::pretty(document)?)?)
                })
                .transpose()?,
        );
    }
    Ok(Some(("incremental", files, root, count)))
}

/// One task document as it is in the folder, read alone.
fn on_disk(folder: &Path, id: WorkId) -> Result<Option<TaskDocument>> {
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
    let task: TaskDocument = task_sync::parse(&std::fs::read(&file)?)
        .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
    if task.id != id {
        return Err(TaskSyncRefusal::InvalidGraph
            .invalid("task filename/id mismatch")
            .into());
    }
    Ok(Some(task))
}
