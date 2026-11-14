//! One Git diff checked against a recorded inventory root, in place of a read
//! of every task at a commit.
//!
//! The base commit's root was verified when it was recorded, so the incoming
//! root follows from the changed paths alone: remove each changed path's old
//! member, add its new one. Unchanged blobs are never opened.

use std::{collections::BTreeMap, path::Path, time::Duration};

use anyhow::Result;

use super::{
    MAX_DELTA_BYTES, Reader, evidence_path, parse_changes, policy_path, query_error, task_path,
};
use crate::{
    ProjectId, WorkId,
    task_sync::{
        self, TaskDocument, TaskSyncRefusal, directory::FolderBasis, inventory::InventoryRoot,
    },
};

/// One changed task path: its document at the base commit and at the
/// incoming one. A rename is a removal and an addition.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ChangedTask {
    pub before: Option<TaskDocument>,
    pub after: Option<TaskDocument>,
}

/// The changed tasks between two commits and the incoming root they imply.
#[derive(Clone, Debug)]
pub struct VerifiedTaskDelta {
    pub base_commit: String,
    pub incoming_commit: String,
    /// The exact base and incoming commit objects that were read.
    pub base_object: super::super::CommitObject,
    pub incoming_object: super::super::CommitObject,
    /// The base envelope as written; its embedded basis is not trusted.
    pub base: FolderBasis,
    /// The incoming envelope as written; its embedded basis is not trusted.
    pub incoming: FolderBasis,
    /// Every task path the diff names, also one whose bytes decode the same.
    pub changed: BTreeMap<WorkId, ChangedTask>,
    /// The recorded base root with every change applied.
    pub root: InventoryRoot,
}

/// Read the task changes from `base` to `incoming` and the inventory root
/// they imply from `base_root`. `base` must be an ancestor of `incoming`, so
/// a force-moved branch refuses even when its contents match. One deadline
/// bounds every Git query of the read.
///
/// # Errors
/// Refuses a missing or non-commit selection, a non-descendant or
/// unprovable history, a changed disclosure, unsafe paths or modes,
/// malformed documents, the byte bound and bounded Git failures.
pub fn read_verified_task_delta(
    repository: &Path,
    project: &ProjectId,
    base: &str,
    base_root: InventoryRoot,
    incoming: &str,
    deadline: Duration,
) -> Result<VerifiedTaskDelta> {
    read(
        repository, project, base, base_root, incoming, deadline, true,
    )
}

/// The same rooted read between two exact commits whose ancestry is not part
/// of the question, such as an original publication and the merge resolving
/// it. Only the root arithmetic and the selected blobs are checked.
///
/// # Errors
/// Refuses as [`read_verified_task_delta`] does, except for history.
pub fn read_rooted_task_diff(
    repository: &Path,
    project: &ProjectId,
    base: &str,
    base_root: InventoryRoot,
    incoming: &str,
    deadline: Duration,
) -> Result<VerifiedTaskDelta> {
    read(
        repository, project, base, base_root, incoming, deadline, false,
    )
}

fn read(
    repository: &Path,
    project: &ProjectId,
    base: &str,
    base_root: InventoryRoot,
    incoming: &str,
    deadline: Duration,
    ancestry: bool,
) -> Result<VerifiedTaskDelta> {
    let _warning = crate::operation_time::Warning::start("Git verified task delta read");
    let reader = Reader::bounded(repository, project, deadline)?;
    let base_object = reader.commit_object(base, "recorded source")?;
    let incoming_object = reader.commit_object(incoming, "incoming")?;
    if ancestry {
        require_ancestor(&reader, base, incoming)?;
    }
    let old = reader.metadata(&base_object.tree, project)?;
    let new = reader.metadata(&incoming_object.tree, project)?;
    if old.profile.include_notes != new.profile.include_notes {
        return Err(TaskSyncRefusal::DivergentPlanning
            .invalid(
                "task commit delta note selection differs from its prior basis; rebuild explicitly",
            )
            .into());
    }
    let raw = reader.query(&[
        "diff-tree",
        "-r",
        "-t",
        "--raw",
        "-z",
        "--no-commit-id",
        "--no-renames",
        "--no-ext-diff",
        "--no-textconv",
        "--no-abbrev",
        base,
        incoming,
        "--",
        ".engram",
    ])?;
    let mut changed = BTreeMap::new();
    let mut root = base_root;
    let mut total = 0_usize;
    for change in parse_changes(&raw)? {
        if change.path == ".engram/project.json"
            || policy_path(&change.path)
            || evidence_path(&change.path)
        {
            continue;
        }
        let id = task_path(&change.path)?;
        let before = (change.status != "A")
            .then(|| document(&reader, &change.old_oid, &change.path, id, &mut total))
            .transpose()?;
        let after = (change.status != "D")
            .then(|| document(&reader, &change.new_oid, &change.path, id, &mut total))
            .transpose()?;
        if let Some(before) = &before {
            root = root.excluding(before)?;
        }
        if let Some(after) = &after {
            root = root.including(after)?;
        }
        changed.insert(id, ChangedTask { before, after });
    }
    let project_document = task_sync::ProjectDocument {
        format: task_sync::FORMAT.into(),
        project_id: project.clone(),
        notes_included: new.profile.include_notes,
    };
    task_sync::validate_selected_documents(
        &project_document,
        changed.values().filter_map(|task| task.after.as_ref()),
    )
    .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
    Ok(VerifiedTaskDelta {
        base_commit: base.into(),
        incoming_commit: incoming.into(),
        base_object,
        incoming_object,
        base: old,
        incoming: new,
        changed,
        root,
    })
}

impl VerifiedTaskDelta {
    /// Require the root this delta implies to be the one an exchange declared.
    ///
    /// # Errors
    /// Refuses a different root by name.
    pub fn require_root(&self, declared: InventoryRoot) -> Result<()> {
        if self.root != declared {
            return Err(TaskSyncRefusal::InventoryRootMismatch
                .invalid(format!(
                    "the Git delta from {} to {} implies inventory root {}, not the declared {declared}",
                    self.base_commit, self.incoming_commit, self.root
                ))
                .into());
        }
        Ok(())
    }
}

/// Read selected task documents at one commit by path, `None` for an absent
/// one. Only the named paths are listed and opened, never the whole tree.
///
/// # Errors
/// Refuses a missing commit, unsafe modes, malformed documents, the byte
/// bound and bounded Git failures.
pub fn read_tasks_at(
    repository: &Path,
    project: &ProjectId,
    commit: &str,
    ids: impl IntoIterator<Item = WorkId>,
    deadline: Duration,
) -> Result<BTreeMap<WorkId, Option<TaskDocument>>> {
    let mut selected = ids
        .into_iter()
        .map(|id| (id, None))
        .collect::<BTreeMap<_, _>>();
    if selected.is_empty() {
        return Ok(selected);
    }
    let reader = Reader::bounded(repository, project, deadline)?;
    let tree = reader.commit_tree(commit, "selected")?;
    let paths = selected
        .keys()
        .map(|id| format!(".engram/tasks/{}.json", id.0))
        .collect::<Vec<_>>();
    let mut args = vec!["ls-tree", "-z", "--full-name", tree.as_str(), "--"];
    args.extend(paths.iter().map(String::as_str));
    let mut total = 0_usize;
    for entry in super::parse_entries(&reader.query(&args)?)? {
        let id = task_path(&entry.name)?;
        if entry.mode != "100644" || entry.kind != "blob" {
            return Err(TaskSyncRefusal::DestinationNotFile
                .invalid("task snapshot requires ordinary task blobs")
                .into());
        }
        let slot = selected.get_mut(&id).ok_or_else(|| {
            TaskSyncRefusal::InvalidGraph.invalid("Git listed a task path that was not selected")
        })?;
        *slot = Some(document(&reader, &entry.oid, &entry.name, id, &mut total)?);
    }
    Ok(selected)
}

fn require_ancestor(reader: &Reader<'_>, base: &str, incoming: &str) -> Result<()> {
    if base == incoming {
        return Ok(());
    }
    let args = ["merge-base", "--is-ancestor", base, incoming];
    match reader.raw_query(&args) {
        Ok(_) => Ok(()),
        Err(error)
            if matches!(
                error.downcast_ref::<super::super::QueryFailure>(),
                Some(super::super::QueryFailure::Nonzero { code: Some(1), .. })
            ) =>
        {
            // A shallow history can hide the path between the two commits, so
            // only a complete one proves they diverged. Nothing is fetched.
            if reader
                .query(&["rev-parse", "--is-shallow-repository"])?
                .trim_ascii()
                == b"true"
            {
                return Err(TaskSyncRefusal::SourceHistoryUnavailable
                    .invalid(format!(
                        "a shallow repository cannot prove {incoming} descends from the recorded source commit {base}; no object is fetched"
                    ))
                    .into());
            }
            Err(TaskSyncRefusal::SourceHistoryDiverged
                .invalid(format!(
                    "the recorded source commit {base} is not an ancestor of {incoming}; rebuild explicitly"
                ))
                .into())
        }
        Err(error) => Err(query_error(&args, &error, None)),
    }
}

fn document(
    reader: &Reader<'_>,
    oid: &str,
    path: &str,
    id: WorkId,
    total: &mut usize,
) -> Result<TaskDocument> {
    let bytes = reader.blob(oid, path)?;
    *total = total.checked_add(bytes.len()).unwrap_or(usize::MAX);
    if *total > MAX_DELTA_BYTES {
        return Err(TaskSyncRefusal::DestinationTreeLimit
            .invalid("task commit delta exceeds byte bound")
            .into());
    }
    let task: TaskDocument =
        task_sync::parse(&bytes).map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
    if task.id != id {
        return Err(TaskSyncRefusal::InvalidGraph
            .invalid("task filename/id mismatch")
            .into());
    }
    Ok(task)
}
