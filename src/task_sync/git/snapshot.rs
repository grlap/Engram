//! Complete cold reads. No retention reference or exchange intent is invented.

use std::{collections::BTreeMap, path::Path};

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

mod evidence;
mod memories;

use super::delta::{self, Change, Reader};
use crate::{
    ObjectId, ProjectId,
    task_sync::{TaskSyncRefusal, TaskTree, directory::FolderBasis},
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

/// Raw committed task bytes and identities of untouched policy/evidence files.
/// This is transient publication admission, not a retained exchange checkpoint.
pub(crate) struct TaskPublicationBasis {
    pub documents: std::collections::BTreeMap<String, Vec<u8>>,
    pub auxiliary: std::collections::BTreeMap<String, String>,
    pub memory_files: std::collections::BTreeMap<String, String>,
}

fn publication_basis_refusal(check: &str, detail: impl std::fmt::Display) -> anyhow::Error {
    TaskSyncRefusal::MemoryTaskExportBasisInvalid.invalid(format!("check={check}; {detail}; preserve the selected commit and reconcile its task/data paths and limits before retrying refresh")).into()
}

pub(crate) fn read_task_publication_basis(
    repository: &Path,
    project: &ProjectId,
    commit: &str,
) -> Result<Option<TaskPublicationBasis>> {
    let reader = Reader::new(repository, project)?;
    let object = reader.commit_object(commit, "task refresh basis")?;
    let roots = reader.entries(&object.tree, &[".engram"])?;
    let Some(root) = roots.first() else {
        return Ok(None);
    };
    if !reader
        .entries(&root.oid, &[])?
        .iter()
        .any(|entry| entry.name == "project.json")
    {
        return Ok(None);
    }
    reader.metadata_with_size(&object.tree, project)?;
    let mut pending = vec![(format!("{}:.engram", object.oid), String::new())];
    let mut documents = std::collections::BTreeMap::new();
    let mut auxiliary = std::collections::BTreeMap::new();
    let mut memory_files = std::collections::BTreeMap::new();
    let mut total = 0usize;
    let mut count = 0usize;
    while let Some((tree, prefix)) = pending.pop() {
        for entry in reader.entries(&tree, &[])? {
            count += 1;
            anyhow::ensure!(
                count <= 400_016,
                publication_basis_refusal(
                    "committed_entry_limit",
                    format_args!("observed={count}; limit=400016")
                )
            );
            let name = format!("{prefix}{}", entry.name);
            if entry.kind == "tree" && entry.mode == "040000" {
                anyhow::ensure!(
                    name == "tasks"
                        || name == "evidence"
                        || name.starts_with("evidence/")
                        || name == "memories"
                        || name.starts_with("memories/"),
                    publication_basis_refusal(
                        "committed_directory",
                        format_args!("path={name:?}; allowed=false")
                    )
                );
                pending.push((entry.oid, format!("{name}/")));
                continue;
            }
            anyhow::ensure!(
                entry.kind == "blob" && entry.mode == "100644",
                publication_basis_refusal(
                    "committed_blob_mode",
                    format_args!(
                        "path={name:?}; kind={}; mode={}; expected=blob/100644",
                        entry.kind, entry.mode
                    )
                )
            );
            if name == "project.json" || name.starts_with("tasks/") {
                if name.starts_with("tasks/") {
                    delta::task_path(&format!(".engram/{name}"))?;
                }
                let bytes = reader.evidence_blob(&entry.oid, 1024 * 1024)?;
                total = total
                    .checked_add(bytes.len())
                    .context("task refresh basis byte overflow")?;
                anyhow::ensure!(
                    total <= delta::MAX_DELTA_BYTES,
                    publication_basis_refusal(
                        "committed_byte_limit",
                        format_args!("observed={total}; limit={}", delta::MAX_DELTA_BYTES)
                    )
                );
                documents.insert(name, bytes);
            } else if name.starts_with("memories/") {
                memory_files.insert(name, entry.oid);
            } else {
                anyhow::ensure!(
                    name == ".gitignore"
                        || name == ".gitattributes"
                        || name.starts_with("evidence/"),
                    publication_basis_refusal(
                        "committed_file",
                        format_args!("path={name:?}; allowed=false")
                    )
                );
                auxiliary.insert(name, entry.oid);
            }
        }
    }
    Ok(Some(TaskPublicationBasis {
        documents,
        auxiliary,
        memory_files,
    }))
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
) -> Result<std::collections::BTreeMap<String, crate::task_sync::memories::KeyHistory>> {
    let reader = Reader::new(repository, project)?;
    let commit = reader.commit_object(commit, "memory export basis")?;
    let roots = reader.entries(&commit.tree, &[".engram"])?;
    let Some(root) = roots.first() else {
        return Ok(BTreeMap::default());
    };
    if root.kind != "tree" || root.mode != "040000" {
        return Ok(BTreeMap::default());
    }
    let children = reader.entries(&root.oid, &[])?;
    if !children.iter().any(|entry| entry.name == "memories") {
        return Ok(BTreeMap::default());
    }
    let Some(project_file) = children.iter().find(|entry| {
        entry.name == "project.json" && entry.mode == "100644" && entry.kind == "blob"
    }) else {
        return Ok(BTreeMap::default());
    };
    let bytes = reader.evidence_blob(&project_file.oid, 1024 * 1024)?;
    if crate::task_sync::directory::folder_basis_from_bytes(&bytes).is_err() {
        return Ok(BTreeMap::default());
    }
    let (_, _, capable) = reader.metadata_with_size(&commit.tree, project)?;
    for entry in children {
        if entry.name == "memories" {
            anyhow::ensure!(capable, "committed memories require their capability");
            return Ok(memories::read(&reader, &entry, project)?.0);
        }
    }
    Ok(BTreeMap::default())
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
    let (envelope, mut metadata_bytes, memory_profile) =
        reader.metadata_with_size(&commit.tree, project)?;
    let roots = reader.entries(&commit.tree, &[".engram"])?;
    // metadata_with_size has already checked this exact immutable root's mode.
    let root = roots.first().context("task snapshot root disappeared")?;
    let entries = reader.entries(&root.oid, &[])?;
    let mut changes = Vec::new();
    for entry in entries {
        match entry.name.as_str() {
            "project.json" => (), // The same immutable blob was strictly decoded above.
            ".gitignore" | ".gitattributes" => {
                if entry.mode != "100644" || entry.kind != "blob" {
                    return Err(TaskSyncRefusal::DestinationNotFile
                        .invalid("data policy entry must be an ordinary blob")
                        .into());
                }
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
                        new_oid: task.oid,
                    });
                    if changes.len() > 100_000 {
                        return Err(TaskSyncRefusal::DestinationTreeLimit
                            .invalid("task snapshot exceeds task count bound")
                            .into());
                    }
                }
            }
            "evidence" => evidence::validate(reader, entry)?,
            "memories" => {
                anyhow::ensure!(
                    memory_profile,
                    "memory Git tree requires its advertised project capability"
                );
                metadata_bytes = metadata_bytes
                    .checked_add(memories::validate(reader, &entry, project)?)
                    .context("memory snapshot byte overflow")?;
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
    Ok(CommitTaskSnapshot {
        commit,
        tree,
        envelope,
    })
}
