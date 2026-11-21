//! Complete cold reads. No retention reference or exchange intent is invented.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::Path,
};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

mod evidence;
mod memories;

use super::delta::{self, Change, Reader};
use crate::{
    ObjectId, ProjectId,
    task_sync::{TaskSyncRefusal, TaskTree, directory::FolderBasis, memories::KeyHistory},
};

/// Exact local commit bytes observed without creating a retention reference.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct CommitObject {
    pub oid: String,
    pub tree: String,
    pub parents: Vec<String>,
    pub raw_bytes_fingerprint: ObjectId,
}

/// Complete typed snapshot for a named cold audit, not admission or delivery.
#[derive(Clone, Debug)]
pub struct CommitTaskSnapshot {
    pub commit: CommitObject,
    pub tree: TaskTree,
    /// Embedded export comparison may be stale after a task-text merge.
    pub envelope: FolderBasis,
}

/// Parse the same commit object for cold reads and explicit retained execution.
/// Callers establish exact object selection before supplying these bytes.
pub(crate) fn observe_commit(commit: &str, bytes: &[u8]) -> Result<CommitObject> {
    use crate::task_sync::ledger::{oid, require};
    oid(commit)?;
    let headers = bytes
        .windows(2)
        .position(|pair| pair == b"\n\n")
        .context("commit header boundary missing")?;
    let mut lines = bytes[..headers].split(|byte| *byte == b'\n');
    let tree = std::str::from_utf8(lines.next().context("commit tree missing")?)?
        .strip_prefix("tree ")
        .context("commit tree header missing")?
        .to_owned();
    oid(&tree)?;
    require(tree.len() == commit.len(), "mixed Git object formats")?;
    let mut parents = Vec::new();
    for line in lines {
        if let Some(parent) = line.strip_prefix(b"parent ") {
            let parent = std::str::from_utf8(parent)?;
            oid(parent)?;
            require(parent.len() == commit.len(), "mixed Git parent format")?;
            parents.push(parent.to_owned());
        }
    }
    require(parents.len() <= 64, "too many retained commit parents")?;
    Ok(CommitObject {
        oid: commit.into(),
        tree,
        parents,
        raw_bytes_fingerprint: format!("{:x}", Sha256::digest(bytes))
            .parse()
            .map_err(anyhow::Error::msg)?,
    })
}

/// Read every task document from an exact commit through bounded local Git.
/// Worktree files, refs, index, retention and SQLite remain untouched.
///
/// # Errors
/// Refuses missing commits, unknown entries, unsafe modes/paths, malformed
/// envelopes/documents, incomplete graphs, disclosure violations and limits.
pub fn read_task_snapshot(
    repository: &Path,
    project: &ProjectId,
    commit: &str,
) -> Result<CommitTaskSnapshot> {
    read_with_limit(repository, project, commit, delta::MAX_DELTA_BYTES)
}

/// Reads the exact committed memory component without consulting worktree files.
///
/// # Errors
/// Refuses malformed memory trees, foreign scope or absent capability metadata.
pub fn read_memory_snapshot(
    repository: &Path,
    project: &ProjectId,
    commit: &str,
) -> Result<BTreeMap<String, KeyHistory>> {
    let (histories, _) =
        read_selected_memories(&Reader::new(repository, project)?, project, commit)?;
    Ok(histories)
}

pub(crate) fn read_memory_snapshot_with_bytes(
    repository: &Path,
    project: &ProjectId,
    commit: &str,
    deadline: std::time::Duration,
) -> Result<(BTreeMap<String, KeyHistory>, u64)> {
    let reader = Reader::bounded(repository, project, deadline)?;
    let (histories, bytes) = read_selected_memories(&reader, project, commit)?;
    Ok((histories, u64::try_from(bytes)?))
}

fn read_selected_memories(
    reader: &Reader<'_>,
    project: &ProjectId,
    commit: &str,
) -> Result<(BTreeMap<String, KeyHistory>, usize)> {
    let commit = reader.commit_object(commit, "memory export basis")?;
    let roots = reader.entries(&commit.tree, &[".engram"])?;
    let Some(root) = roots.first() else {
        return Ok((BTreeMap::new(), 0));
    };
    if root.kind != "tree" || root.mode != "040000" {
        return Ok((BTreeMap::new(), 0));
    }
    let children = reader.entries(&root.oid, &[])?;
    if !children.iter().any(|entry| entry.name == "memories") {
        return Ok((BTreeMap::new(), 0));
    }
    let project_file = children
        .iter()
        .find(|entry| entry.name == "project.json")
        .ok_or_else(|| {
            crate::task_sync::directory::invalid_memory_export_basis(
                &commit.oid,
                "project envelope is absent while memories exist",
            )
        })?;
    anyhow::ensure!(
        project_file.mode == "100644" && project_file.kind == "blob",
        crate::task_sync::directory::invalid_memory_export_basis(
            &commit.oid,
            format!(
                "expected an ordinary 100644 blob, observed {} {}",
                project_file.mode, project_file.kind
            )
        )
    );
    let bytes = reader.evidence_blob(&project_file.oid, 1024 * 1024)?;
    let (basis, capable) =
        crate::task_sync::directory::committed_memory_envelope(&bytes, &commit.oid)?;
    anyhow::ensure!(
        basis.project_id == *project,
        TaskSyncRefusal::OriginMismatch
            .invalid("task commit project differs from selected project")
    );
    if children
        .iter()
        .any(|entry| entry.name == "tasks" && (entry.mode != "040000" || entry.kind != "tree"))
    {
        return Err(TaskSyncRefusal::DestinationNotDirectory
            .invalid("task commit tasks entry must be a tree")
            .into());
    }
    for entry in children {
        if entry.name == "memories" {
            anyhow::ensure!(
                capable,
                crate::task_sync::directory::invalid_memory_export_basis(
                    &commit.oid,
                    "memories exist but memory_profile capability is absent or null"
                )
            );
            return memories::read(reader, &entry, project);
        }
    }
    Ok((BTreeMap::new(), 0))
}

pub(super) fn read_with_limit(
    repository: &Path,
    project: &ProjectId,
    commit: &str,
    byte_limit: usize,
) -> Result<CommitTaskSnapshot> {
    let _warning = crate::operation_time::Warning::start("Git task snapshot read");
    let reader = Reader::cold(repository, project)?;
    read_selected(&reader, project, commit, byte_limit)
}

pub(crate) fn read_task_snapshot_bounded(
    repository: &Path,
    project: &ProjectId,
    commit: &str,
    deadline: std::time::Duration,
) -> Result<CommitTaskSnapshot> {
    let _warning = crate::operation_time::Warning::start("Git task snapshot read");
    let reader = Reader::bounded(repository, project, deadline)?;
    read_selected(&reader, project, commit, delta::MAX_DELTA_BYTES)
}

fn read_selected(
    reader: &Reader<'_>,
    project: &ProjectId,
    commit: &str,
    byte_limit: usize,
) -> Result<CommitTaskSnapshot> {
    let commit = reader.commit_object(commit, "cold audit")?;
    let selected = read_tree(reader, project, &commit.tree, byte_limit)?;
    Ok(CommitTaskSnapshot {
        commit,
        tree: selected.tasks,
        envelope: selected.envelope,
    })
}

struct SelectedTree {
    tasks: TaskTree,
    envelope: FolderBasis,
    memory_histories: BTreeMap<String, KeyHistory>,
    has_evidence: bool,
    policy: BTreeMap<String, String>,
}

pub(crate) fn ordinary_data_policy(
    repository: &Path,
    project: &ProjectId,
    commit: &str,
    deadline: std::time::Duration,
) -> Result<BTreeMap<String, String>> {
    crate::task_sync::ledger::oid(commit)?;
    let reader = Reader::bounded(repository, project, deadline)?;
    reader
        .entries(commit, &[".engram/.gitignore", ".engram/.gitattributes"])?
        .into_iter()
        .map(|entry| {
            let name = entry
                .name
                .strip_prefix(".engram/")
                .context("invalid data policy path")?;
            anyhow::ensure!(
                matches!(name, ".gitignore" | ".gitattributes")
                    && entry.mode == "100644"
                    && entry.kind == "blob",
                "data policy entry must be an ordinary blob"
            );
            Ok((name.to_owned(), entry.oid))
        })
        .collect()
}

/// Admit the exact tree of an ordinary data commit or a staged data merge.
/// Both components must pass disclosure before this tree can advance or
/// leave the checkout.
pub(crate) fn admit_ordinary_data_tree(
    repository: &Path,
    project: &ProjectId,
    object: &str,
    deadline: std::time::Duration,
    remote_policy: Option<&BTreeMap<String, String>>,
) -> Result<()> {
    use anyhow::ensure;
    let _warning = crate::operation_time::Warning::start("Git ordinary data tree admission");
    crate::task_sync::ledger::oid(object)?;
    let reader = Reader::bounded(repository, project, deadline)?;
    let roots = reader.entries(object, &[])?;
    ensure!(
        roots.len() == 1
            && roots[0].name == ".engram"
            && roots[0].kind == "tree"
            && roots[0].mode == "040000",
        "ordinary data tree requires only an ordinary .engram directory"
    );
    let selected = read_tree(&reader, project, object, delta::MAX_DELTA_BYTES)?;
    ensure!(
        !selected.has_evidence,
        "sync_evidence_profile_unsupported: use the explicit folder and evidence exchange"
    );
    ensure!(
        !selected.envelope.profile.include_notes
            && selected.envelope.profile.origin_machine.is_none(),
        "sync requires this project's committed task or task-and-memory profile without notes or activity"
    );
    crate::task_sync::setup::disclose_tasks(selected.tasks.tasks.iter())?;
    let tasks = selected
        .tasks
        .tasks
        .iter()
        .map(|task| task.id)
        .collect::<BTreeSet<_>>();
    ensure!(
        selected.memory_histories.values().all(|history| {
            history
                .revisions
                .values()
                .all(|revision| match &revision.provenance.retiring_target {
                    Some(crate::domain::ProjectMemoryRetiringTarget::Local { work_id, .. }) => {
                        tasks.contains(work_id)
                    }
                    _ => true,
                })
        }),
        "memory disclosure refused: a historical retirement target is absent from the selected task inventory"
    );
    if let Some(remote_policy) = remote_policy {
        for (name, object) in selected.policy {
            if remote_policy.get(&name) == Some(&object) {
                continue;
            }
            let bytes = reader.evidence_blob(&object, 1024 * 1024)?;
            ensure!(
                crate::task_sync::setup::is_generated_data_policy(&name, &bytes)?,
                "policy disclosure refused: review and publish the unpublished Git policy yourself, then rerun sync"
            );
        }
    }
    Ok(())
}

fn read_tree(
    reader: &Reader<'_>,
    project: &ProjectId,
    tree: &str,
    byte_limit: usize,
) -> Result<SelectedTree> {
    let (envelope, mut metadata_bytes, memory_profile) =
        reader.metadata_with_size(tree, project)?;
    let roots = reader.entries(tree, &[".engram"])?;
    // metadata_with_size has already checked this exact immutable root's mode.
    let root = roots.first().context("task snapshot root disappeared")?;
    let entries = reader.entries(&root.oid, &[])?;
    let mut changes = Vec::new();
    let mut memory_histories = BTreeMap::new();
    let mut has_evidence = false;
    let mut policy = BTreeMap::new();
    for entry in entries {
        match entry.name.as_str() {
            "project.json" => (), // The same immutable blob was strictly decoded above.
            ".gitignore" | ".gitattributes" => {
                if entry.mode != "100644" || entry.kind != "blob" {
                    return Err(TaskSyncRefusal::DestinationNotFile
                        .invalid("data policy entry must be an ordinary blob")
                        .into());
                }
                policy.insert(entry.name, entry.oid);
            }
            "tasks" => {
                for task in reader.entries(&entry.oid, &[])? {
                    let path = format!(".engram/tasks/{}", task.name);
                    delta::task_path(&path)?;
                    if task.mode != "100644" || task.kind != "blob" {
                        return Err(TaskSyncRefusal::DestinationNotFile
                            .invalid("task snapshot requires ordinary task blobs")
                            .into());
                    }
                    changes.push(Change {
                        path,
                        status: "A".into(),
                        old_oid: String::new(),
                        new_oid: task.oid,
                    });
                    if changes.len() > 100_000 {
                        return Err(TaskSyncRefusal::DestinationTreeLimit
                            .invalid("task snapshot exceeds task count bound")
                            .into());
                    }
                }
            }
            "evidence" => {
                evidence::validate(reader, entry)?;
                has_evidence = true;
            }
            "memories" => {
                anyhow::ensure!(
                    memory_profile,
                    "memory Git tree requires its advertised project capability"
                );
                let (histories, memory_bytes) = memories::read(reader, &entry, project)?;
                metadata_bytes = metadata_bytes
                    .checked_add(memory_bytes)
                    .context("memory snapshot byte overflow")?;
                memory_histories = histories;
            }
            _ => {
                return Err(TaskSyncRefusal::DestinationUnknownEntry
                    .invalid("task snapshot contains an unknown .engram entry")
                    .into());
            }
        }
    }
    let remaining = byte_limit.checked_sub(metadata_bytes).ok_or_else(|| {
        TaskSyncRefusal::DestinationTreeLimit.invalid("task snapshot exceeds byte bound")
    })?;
    let (documents, _) = delta::read_documents(
        reader,
        changes,
        project,
        envelope.profile.include_notes,
        remaining,
    )?;
    let tree = TaskTree {
        project: crate::task_sync::ProjectDocument {
            format: crate::task_sync::FORMAT.into(),
            project_id: project.clone(),
            notes_included: envelope.profile.include_notes,
        },
        tasks: documents.into_values().collect(),
    };
    tree.validate_folder()
        .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
    Ok(SelectedTree {
        tasks: tree,
        envelope,
        memory_histories,
        has_evidence,
        policy,
    })
}
