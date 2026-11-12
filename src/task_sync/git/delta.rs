//! Read selected Git blobs between exact commits; graph admission stays with the caller.

use std::{
    collections::{BTreeMap, BTreeSet},
    path::{Component, Path},
    time::{Duration, Instant},
};

use crate::{
    ProjectId, WorkId,
    storage::StoreError,
    task_sync::{
        self, TaskDocument, TaskSyncRefusal,
        directory::{self, FolderBasis},
    },
};
use anyhow::{Context, Result};

const BOUND: Duration = crate::operation_time::MINIMUM;
pub(super) const MAX_DELTA_BYTES: usize = 128 * 1024 * 1024;

/// Typed changed documents, not a complete admitted tree or a stored record.
/// The caller supplies an already-admitted prior commit and validates the
/// touched graph and retained identities before importing anything.
#[derive(Clone, Debug)]
pub struct CommitTaskDelta {
    pub prior_commit: String,
    pub incoming_commit: String,
    /// Embedded envelope text, not a recomputed inventory or recorded baseline.
    pub prior: FolderBasis,
    /// May be stale after a task-text merge; never use it as verified inventory.
    pub incoming: FolderBasis,
    pub upserts: BTreeMap<WorkId, TaskDocument>,
    pub removed: BTreeSet<WorkId>,
}

/// Read an exact old-to-new `.engram` tree delta, including intervening merges.
/// Only changed task blobs and the two project documents are opened. Renames
/// are removals plus additions. No worktree file, ref, index or store is changed.
///
/// # Errors
/// Refuses missing commit bases, unsafe paths/modes, invalid changed JSON or
/// document fields, profile/project mismatches, and bounded Git failures.
/// Graph relationships and collisions with unchanged records are not admitted.
pub fn read_task_delta(
    repository: &Path,
    project: &ProjectId,
    prior: &str,
    incoming: &str,
) -> Result<CommitTaskDelta> {
    let _warning = crate::operation_time::Warning::start("Git task delta read");
    let reader = Reader::new(repository, project)?;
    let prior_tree = reader.commit_tree(prior, "prior")?;
    let incoming_tree = reader.commit_tree(incoming, "incoming")?;
    let old = reader.metadata(&prior_tree, project)?;
    let new = reader.metadata(&incoming_tree, project)?;
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
        prior,
        incoming,
        "--",
        ".engram",
    ])?;
    let changes = parse_changes(&raw)?;
    let (upserts, removed) = read_documents(
        &reader,
        changes,
        project,
        new.profile.include_notes,
        MAX_DELTA_BYTES,
    )?;
    Ok(CommitTaskDelta {
        prior_commit: prior.into(),
        incoming_commit: incoming.into(),
        prior: old,
        incoming: new,
        upserts,
        removed,
    })
}

pub(super) fn read_documents(
    reader: &Reader<'_>,
    changes: Vec<Change>,
    project: &ProjectId,
    include_notes: bool,
    byte_limit: usize,
) -> Result<(BTreeMap<WorkId, TaskDocument>, BTreeSet<WorkId>)> {
    let mut upserts = BTreeMap::new();
    let mut removed = BTreeSet::new();
    let mut total_bytes = 0_usize;
    for change in changes {
        if change.path == ".engram/project.json"
            || policy_path(&change.path)
            || evidence_path(&change.path)
        {
            continue;
        }
        let id = task_path(&change.path)?;
        if change.status == "D" {
            removed.insert(id);
        } else {
            let bytes = reader.blob(&change.new_oid, &change.path)?;
            total_bytes = total_bytes.checked_add(bytes.len()).ok_or_else(|| {
                TaskSyncRefusal::DestinationTreeLimit
                    .invalid("task commit delta exceeds byte bound")
            })?;
            if total_bytes > byte_limit {
                return Err(TaskSyncRefusal::DestinationTreeLimit
                    .invalid("task commit delta exceeds byte bound")
                    .into());
            }
            let task: TaskDocument = task_sync::parse(&bytes)
                .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
            if task.id != id {
                return Err(TaskSyncRefusal::InvalidGraph
                    .invalid("task filename/id mismatch")
                    .into());
            }
            upserts.insert(id, task);
        }
    }
    let document = task_sync::ProjectDocument {
        format: task_sync::FORMAT.into(),
        project_id: project.clone(),
        notes_included: include_notes,
    };
    task_sync::validate_selected_documents(&document, upserts.values())
        .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
    Ok((upserts, removed))
}

pub(super) struct Reader<'a> {
    repository: &'a Path,
    started: Instant,
    total_bound: Duration,
}

impl<'a> Reader<'a> {
    pub(super) fn new(repository: &'a Path, project: &ProjectId) -> Result<Self> {
        if !repository.is_absolute()
            || repository
                .components()
                .any(|part| matches!(part, Component::ParentDir))
            || project.0.trim().is_empty()
        {
            return Err(TaskSyncRefusal::InvalidGraph
                .invalid("task Git read requires an absolute repository and nonblank project")
                .into());
        }
        Ok(Self {
            repository,
            started: Instant::now(),
            total_bound: BOUND,
        })
    }

    pub(super) fn cold(repository: &'a Path, project: &ProjectId) -> Result<Self> {
        let mut reader = Self::new(repository, project)?;
        reader.total_bound = Duration::from_secs(600);
        Ok(reader)
    }

    pub(super) fn bounded(
        repository: &'a Path,
        project: &ProjectId,
        deadline: Duration,
    ) -> Result<Self> {
        anyhow::ensure!(!deadline.is_zero(), "task Git read deadline is exhausted");
        let mut reader = Self::new(repository, project)?;
        reader.total_bound = deadline;
        Ok(reader)
    }

    fn query(&self, args: &[&str]) -> Result<Vec<u8>> {
        self.raw_query(args)
            .map_err(|error| query_error(args, &error, None))
    }

    fn raw_query(&self, args: &[&str]) -> Result<Vec<u8>> {
        let remaining = self
            .total_bound
            .checked_sub(self.started.elapsed())
            .filter(|left| !left.is_zero())
            .ok_or(super::QueryFailure::Deadline)?;
        super::query::run_observed(self.repository, args, false, None, remaining.min(BOUND))
    }

    fn commit_tree(&self, commit: &str, selection: &str) -> Result<String> {
        Ok(self.commit_object(commit, selection)?.tree)
    }

    pub(super) fn commit_object(
        &self,
        commit: &str,
        selection: &str,
    ) -> Result<super::CommitObject> {
        task_sync::ledger::oid(commit)
            .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
        let expression = format!("{commit}^{{commit}}");
        let args = [
            "rev-parse",
            "--verify",
            "--quiet",
            "--end-of-options",
            &expression,
        ];
        let resolved = match self.raw_query(&args) {
            Ok(bytes) => bytes,
            Err(error)
                if matches!(
                    error.downcast_ref::<super::QueryFailure>(),
                    Some(super::QueryFailure::Nonzero { code: Some(1), .. })
                ) =>
            {
                return Err(missing_commit(selection).into());
            }
            Err(error) => return Err(query_error(&args, &error, None)),
        };
        if resolved != format!("{commit}\n").as_bytes() {
            return Err(TaskSyncRefusal::InvalidGraph
                .invalid(
                    "source commit must be a full exact object id, not a ref, tag or abbreviation",
                )
                .into());
        }
        let bytes = self.query(&["cat-file", "commit", commit])?;
        super::observe_commit(commit, &bytes).map_err(|error| {
            TaskSyncRefusal::InvalidGraph
                .invalid(error.to_string())
                .into()
        })
    }

    pub(super) fn entries(&self, tree: &str, names: &[&str]) -> Result<Vec<Entry>> {
        let mut args = vec!["ls-tree", "-z", tree, "--"];
        args.extend_from_slice(names);
        parse_entries(&self.query(&args)?)
    }

    fn metadata(&self, tree: &str, expected: &ProjectId) -> Result<FolderBasis> {
        Ok(self.metadata_with_size(tree, expected)?.0)
    }

    pub(super) fn metadata_with_size(
        &self,
        tree: &str,
        expected: &ProjectId,
    ) -> Result<(FolderBasis, usize)> {
        let root = self.entries(tree, &[".engram"])?;
        let root = root
            .first()
            .filter(|entry| {
                root.len() == 1
                    && entry.name == ".engram"
                    && entry.mode == "040000"
                    && entry.kind == "tree"
            })
            .ok_or_else(|| {
                TaskSyncRefusal::InvalidGraph
                    .invalid("task commit basis requires an ordinary .engram tree")
            })?;
        let entries = self.entries(&root.oid, &["project.json", "tasks"])?;
        let project = entries
            .iter()
            .find(|entry| {
                entry.name == "project.json" && entry.mode == "100644" && entry.kind == "blob"
            })
            .ok_or_else(|| {
                TaskSyncRefusal::DestinationNotFile
                    .invalid("task commit basis requires an ordinary project.json blob")
            })?;
        if entries
            .iter()
            .any(|entry| entry.name == "tasks" && (entry.mode != "040000" || entry.kind != "tree"))
        {
            return Err(TaskSyncRefusal::DestinationNotDirectory
                .invalid("task commit tasks entry must be a tree")
                .into());
        }
        let bytes = self.blob(&project.oid, ".engram/project.json")?;
        let basis = directory::folder_basis_from_bytes(&bytes)?;
        if basis.project_id != *expected {
            return Err(TaskSyncRefusal::OriginMismatch
                .invalid("task commit project differs from selected project")
                .into());
        }
        Ok((basis, bytes.len()))
    }

    fn blob(&self, oid: &str, path: &str) -> Result<Vec<u8>> {
        #[cfg(test)]
        BLOB_READS.with(|reads| reads.borrow_mut().push(path.to_owned()));
        let args = ["cat-file", "blob", oid];
        self.raw_query(&args)
            .map_err(|error| query_error(&args, &error, Some(path)))
    }

    pub(super) fn evidence_blob(&self, oid: &str, limit: usize) -> Result<Vec<u8>> {
        let remaining = self
            .total_bound
            .checked_sub(self.started.elapsed())
            .filter(|left| !left.is_zero())
            .ok_or(super::QueryFailure::Deadline)?;
        super::query::evidence_blob_observed(self.repository, oid, remaining.min(BOUND), limit)
    }
}

fn query_error(args: &[&str], error: &anyhow::Error, document: Option<&str>) -> anyhow::Error {
    let refusal = if matches!(
        error.downcast_ref::<super::QueryFailure>(),
        Some(super::QueryFailure::OutputLimit)
    ) {
        if args.starts_with(&["cat-file", "blob"]) {
            TaskSyncRefusal::DestinationFileLimit
        } else {
            TaskSyncRefusal::DestinationTreeLimit
        }
    } else {
        TaskSyncRefusal::GitQueryFailed
    };
    let target = document.map_or(String::new(), |path| format!(" document {path}"));
    refusal
        .invalid(format!(
            "local Git {}{target} read failed: {error}",
            args[0]
        ))
        .into()
}

fn missing_commit(selection: &str) -> StoreError {
    TaskSyncRefusal::IncrementalBasisUnavailable.invalid(format!(
        "{selection} source commit basis is missing or is not an exact commit; rebuild explicitly"
    ))
}

pub(super) struct Entry {
    pub(super) mode: String,
    pub(super) kind: String,
    pub(super) oid: String,
    pub(super) name: String,
}

fn parse_entries(bytes: &[u8]) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    let mut names = BTreeSet::new();
    for record in nul_records(bytes)? {
        let (header, name) = record.split_once('\t').context("invalid Git tree entry")?;
        let fields = header.split(' ').collect::<Vec<_>>();
        if fields.len() != 3 {
            return Err(TaskSyncRefusal::InvalidGraph
                .invalid("invalid Git tree entry")
                .into());
        }
        task_sync::ledger::oid(fields[2])?;
        if !names.insert(name) {
            return Err(TaskSyncRefusal::InvalidGraph
                .invalid("duplicate Git metadata entry")
                .into());
        }
        entries.push(Entry {
            mode: fields[0].into(),
            kind: fields[1].into(),
            oid: fields[2].into(),
            name: name.into(),
        });
    }
    Ok(entries)
}

pub(super) struct Change {
    pub(super) path: String,
    pub(super) status: String,
    pub(super) new_oid: String,
}

fn parse_changes(bytes: &[u8]) -> Result<Vec<Change>> {
    let records = nul_records(bytes)?;
    if !records.len().is_multiple_of(2) {
        return Err(TaskSyncRefusal::InvalidGraph
            .invalid("invalid NUL Git delta records")
            .into());
    }
    let mut changes = Vec::new();
    let mut seen = BTreeSet::new();
    for pair in records.as_chunks::<2>().0 {
        let fields = pair[0]
            .strip_prefix(':')
            .context("invalid Git delta header")?
            .split(' ')
            .collect::<Vec<_>>();
        if fields.len() != 5 {
            return Err(TaskSyncRefusal::InvalidGraph
                .invalid("invalid Git delta header")
                .into());
        }
        if !matches!(fields[2].len(), 40 | 64) || fields[2].len() != fields[3].len() {
            return Err(TaskSyncRefusal::InvalidGraph
                .invalid("invalid Git delta object ids")
                .into());
        }
        let path = pair[1];
        if path != ".engram/project.json"
            && !policy_path(path)
            && !evidence_path(path)
            && !directory_path(path)
        {
            task_path(path)?;
        }
        let mode = if directory_path(path) {
            "040000"
        } else {
            "100644"
        };
        let valid = match fields[4] {
            "A" => {
                fields[0] == "000000"
                    && fields[1] == mode
                    && fields[2].bytes().all(|byte| byte == b'0')
            }
            "D" => {
                fields[0] == mode
                    && fields[1] == "000000"
                    && fields[3].bytes().all(|byte| byte == b'0')
            }
            "M" => fields[0] == mode && fields[1] == mode,
            _ => false,
        };
        if !valid {
            return Err(TaskSyncRefusal::DestinationNotFile
                .invalid(
                    "task commit delta refuses non-ordinary modes or unsupported change status",
                )
                .into());
        }
        if fields[4] != "A" {
            task_sync::ledger::oid(fields[2])?;
        }
        if fields[4] != "D" {
            task_sync::ledger::oid(fields[3])?;
        }
        if !seen.insert(path) {
            return Err(TaskSyncRefusal::InvalidGraph
                .invalid("duplicate Git delta path")
                .into());
        }
        if directory_path(path) {
            continue;
        }
        changes.push(Change {
            path: path.into(),
            status: fields[4].into(),
            new_oid: fields[3].into(),
        });
    }
    Ok(changes)
}

fn policy_path(path: &str) -> bool {
    matches!(path, ".engram/.gitignore" | ".engram/.gitattributes")
}

fn evidence_path(path: &str) -> bool {
    crate::task_sync::evidence::path::kind(path)
        == Some(crate::task_sync::evidence::path::Kind::Pack)
}

fn directory_path(path: &str) -> bool {
    matches!(path, ".engram" | ".engram/tasks")
        || crate::task_sync::evidence::path::kind(path)
            == Some(crate::task_sync::evidence::path::Kind::Directory)
}

pub(super) fn task_path(path: &str) -> Result<WorkId> {
    let id = path
        .strip_prefix(".engram/tasks/")
        .and_then(|tail| tail.strip_suffix(".json"))
        .and_then(|value| uuid::Uuid::parse_str(value).ok());
    match id {
        Some(id) if path == format!(".engram/tasks/{id}.json") => Ok(WorkId(id)),
        _ => Err(TaskSyncRefusal::DestinationUnknownEntry
            .invalid("task commit delta path is not a canonical task document")
            .into()),
    }
}

fn nul_records(bytes: &[u8]) -> Result<Vec<&str>> {
    if bytes.is_empty() {
        return Ok(Vec::new());
    }
    let bytes = bytes
        .strip_suffix(&[0])
        .ok_or_else(|| TaskSyncRefusal::InvalidGraph.invalid("Git records must end with NUL"))?;
    bytes
        .split(|byte| *byte == 0)
        .map(|part| {
            std::str::from_utf8(part).map_err(|_| {
                TaskSyncRefusal::DestinationNonUtf8Name
                    .invalid("Git task path is not UTF-8")
                    .into()
            })
        })
        .collect()
}

#[cfg(test)]
thread_local! { static BLOB_READS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) }; }

#[cfg(test)]
mod tests;
