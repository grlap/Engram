//! Complete cold reads. No retention reference or exchange intent is invented.

use std::path::Path;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

mod evidence;

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
    let (envelope, metadata_bytes) = reader.metadata_with_size(&commit.tree, project)?;
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
