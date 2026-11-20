//! Read selected Git blobs between exact commits; graph admission stays with the caller.

use std::{
    collections::{BTreeMap, BTreeSet},
    fmt::Write as _,
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
use sha2::Digest;

mod verified;
pub use verified::{
    ChangedTask, VerifiedTaskDelta, read_rooted_task_diff, read_tasks_at, read_verified_task_delta,
};

const BOUND: Duration = crate::operation_time::MINIMUM;
pub(super) const MAX_DELTA_BYTES: usize = 128 * 1024 * 1024;
// The largest raw diff listing a delta between two admitted folders can
// produce: every task path on both sides, each record well under 512 bytes
// with the longest object ids, plus the tree entries.
use super::query::MAX_LISTING_BYTES;

/// Compare one commit with an export's exact paths and postimages. The raw
/// delta supplies blob identities without opening any task document in Git.
pub(crate) fn matches_export_commit(
    repository: &Path,
    project: &ProjectId,
    parent: Option<&str>,
    commit: &str,
    expected: &BTreeMap<&str, Option<&str>>,
    deadline: Duration,
) -> Result<bool> {
    task_sync::ledger::oid(commit)?;
    if let Some(parent) = parent {
        task_sync::ledger::oid(parent)?;
    }
    if parent == Some(commit) {
        return Ok(false);
    }
    let reader = Reader::bounded(repository, project, deadline)?;
    let bytes = reader.query(&["cat-file", "commit", commit])?;
    let object = super::observe_commit(commit, &bytes)?;
    if !object.parents.iter().map(String::as_str).eq(parent) {
        return Ok(false);
    }
    let mut args = vec![
        "diff-tree",
        "-r",
        "--raw",
        "-z",
        "--no-commit-id",
        "--no-renames",
        "--no-ext-diff",
        "--no-textconv",
        "--no-abbrev",
    ];
    if let Some(parent) = parent {
        args.push(parent);
    } else {
        args.push("--root");
    }
    args.extend([commit, "--"]);
    let raw = reader.listing(&args)?;
    let changes = match parse_changes(&raw) {
        Err(error)
            if matches!(
                error.downcast_ref::<StoreError>(),
                Some(StoreError::TaskSyncRefused {
                    refusal: TaskSyncRefusal::DestinationUnknownEntry
                        | TaskSyncRefusal::DestinationNotFile,
                    ..
                })
            ) =>
        {
            return Ok(false);
        }
        result => result?,
    };
    Ok(changes.len() == expected.len()
        && changes.iter().all(|change| {
            let Some(bytes) = change
                .path
                .strip_prefix(".engram/")
                .and_then(|path| expected.get(path))
            else {
                return false;
            };
            match bytes {
                None => change.status == "D",
                Some(bytes) => {
                    change.status != "D"
                        && blob_id(bytes.as_bytes(), commit.len()) == change.new_oid
                }
            }
        }))
}

/// Blob identities at the admitted parent, for selected export paths only.
pub(crate) struct ExportParentPaths {
    oid_length: usize,
    blobs: BTreeMap<String, Option<String>>,
}

impl ExportParentPaths {
    pub(crate) fn read(
        repository: &Path,
        project: &ProjectId,
        parent: Option<&str>,
        paths: BTreeSet<String>,
        deadline: Duration,
    ) -> Result<Self> {
        let Some(commit) = parent else {
            return Ok(Self {
                oid_length: 0,
                blobs: paths.into_iter().map(|path| (path, None)).collect(),
            });
        };
        if paths.is_empty() {
            return Ok(Self {
                oid_length: commit.len(),
                blobs: BTreeMap::new(),
            });
        }
        task_sync::ledger::oid(commit)?;
        let reader = Reader::bounded(repository, project, deadline)?;
        let mut input = String::new();
        for path in &paths {
            anyhow::ensure!(
                !path.contains(['\n', '\r', '\0']),
                "invalid path in retained export proof"
            );
            writeln!(input, "{commit}:.engram/{path}")?;
        }
        let args = ["cat-file", "--batch-check"];
        let remaining = reader
            .total_bound
            .checked_sub(reader.started.elapsed())
            .filter(|remaining| !remaining.is_zero())
            .ok_or(super::QueryFailure::Deadline)
            .map_err(|error| query_error(&args, &error.into(), None))?;
        let output = super::query::batch_path_metadata(
            reader.repository,
            input.as_bytes(),
            remaining.min(BOUND),
            u64::try_from(input.len() + 96 * paths.len())?,
        )
        .map_err(|error| query_error(&args, &error, None))?;
        let mut rows = std::str::from_utf8(&output)?.lines();
        let mut blobs = BTreeMap::new();
        for (path, expression) in paths.into_iter().zip(input.lines()) {
            let row = rows.next().ok_or_else(|| malformed_batch(&path))?;
            let before = if row.strip_suffix(" missing") == Some(expression) {
                None
            } else {
                let mut fields = row.split(' ');
                let oid = fields.next().ok_or_else(|| malformed_batch(&path))?;
                if task_sync::ledger::oid(oid).is_err()
                    || fields.next() != Some("blob")
                    || fields
                        .next()
                        .and_then(|value| value.parse::<u64>().ok())
                        .is_none()
                    || fields.next().is_some()
                {
                    return Err(malformed_batch(&path).into());
                }
                Some(oid.to_owned())
            };
            blobs.insert(path, before);
        }
        if rows.next().is_some() {
            return Err(TaskSyncRefusal::GitQueryFailed
                .invalid("local Git cat-file returned more export paths than asked")
                .into());
        }
        Ok(Self {
            oid_length: commit.len(),
            blobs,
        })
    }

    /// Compare either a disk preimage or a proposed postimage to the parent.
    pub(crate) fn matches(&self, path: &str, bytes: Option<&[u8]>) -> Result<bool> {
        let parent = self
            .blobs
            .get(path)
            .context("export path metadata missing")?;
        Ok(match (parent, bytes) {
            (None, None) => true,
            (Some(parent), Some(bytes)) => *parent == blob_id(bytes, self.oid_length),
            _ => false,
        })
    }
}

fn blob_id(bytes: &[u8], oid_length: usize) -> String {
    let header = format!("blob {}\0", bytes.len());
    if oid_length == 40 {
        let mut hash = sha1::Sha1::new();
        hash.update(header.as_bytes());
        hash.update(bytes);
        format!("{:x}", hash.finalize())
    } else {
        let mut hash = sha2::Sha256::new();
        hash.update(header.as_bytes());
        hash.update(bytes);
        format!("{:x}", hash.finalize())
    }
}

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
    let (old, _, old_memories) = reader.metadata_with_size(&prior_tree, project)?;
    let (new, _, new_memories) = reader.metadata_with_size(&incoming_tree, project)?;
    if old_memories || new_memories {
        return Err(TaskSyncRefusal::MemoryProfileRequiresOptIn
            .invalid("memory-capable commit delta requires explicit folder import --include-memories; delta import cannot consume memories")
            .into());
    }
    if old.profile.include_notes != new.profile.include_notes {
        return Err(TaskSyncRefusal::DivergentPlanning
            .invalid(
                "task commit delta note selection differs from its prior basis; rebuild explicitly",
            )
            .into());
    }
    let raw = reader.listing(&[
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
            || memory_path(&change.path, false)
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

    /// A path listing that grows with the change count, such as a commit
    /// diff, bounded for every task the folder limits allow on both sides.
    fn listing(&self, args: &[&str]) -> Result<Vec<u8>> {
        let remaining = self
            .total_bound
            .checked_sub(self.started.elapsed())
            .filter(|left| !left.is_zero())
            .ok_or(super::QueryFailure::Deadline)
            .map_err(|error| query_error(args, &anyhow::Error::from(error), None))?;
        super::query::run_listing_observed(
            self.repository,
            args,
            remaining.min(BOUND),
            MAX_LISTING_BYTES,
        )
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

    pub(super) fn metadata_with_size(
        &self,
        tree: &str,
        expected: &ProjectId,
    ) -> Result<(FolderBasis, usize, bool)> {
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
        let memory_profile = serde_json::from_slice::<serde_json::Value>(&bytes)?
            .get("memory_profile")
            .and_then(serde_json::Value::as_str)
            == Some(crate::task_sync::memories::FORMAT);
        Ok((basis, bytes.len(), memory_profile))
    }

    fn blob(&self, oid: &str, path: &str) -> Result<Vec<u8>> {
        #[cfg(test)]
        BLOB_READS.with(|reads| reads.borrow_mut().push(path.to_owned()));
        let args = ["cat-file", "blob", oid];
        self.raw_query(&args)
            .map_err(|error| query_error(&args, &error, Some(path)))
    }

    /// Task blobs through one Git process per batch, never one per blob, so a
    /// large delta stays within its deadline. Each blob keeps the per-file
    /// limit and all of them together the delta byte bound.
    pub(super) fn blobs(&self, wanted: &[(&str, &str)]) -> Result<Vec<Vec<u8>>> {
        self.batch(
            wanted,
            directory::MAX_FILE_BYTES,
            MAX_DELTA_BYTES,
            super::QueryStderrPolicy::ErrorOnly,
            &|refusal| match refusal {
                BatchRefusal::Output => TaskSyncRefusal::DestinationTreeLimit
                    .invalid(format!(
                        "the changed folder files of this commit delta, old and new together, exceed its byte bound of {MAX_DELTA_BYTES} bytes, or one of them exceeds the per-file limit of {} bytes; import the complete folder with `--rebuild`",
                        directory::MAX_FILE_BYTES
                    ))
                    .into(),
                BatchRefusal::Missing(path) => TaskSyncRefusal::GitQueryFailed
                    .invalid(format!(
                        "the object of folder file {path} is not in the local repository: fetch the data branch completely, or import the complete folder with `--rebuild`"
                    ))
                    .into(),
                BatchRefusal::File(path) => TaskSyncRefusal::DestinationFileLimit
                    .invalid(format!("folder file {path} exceeds the per-file limit"))
                    .into(),
                BatchRefusal::Total => TaskSyncRefusal::DestinationTreeLimit
                    .invalid(
                        "the changed folder files of this commit delta, old and new together, exceed its byte bound; import the complete folder with `--rebuild`",
                    )
                    .into(),
            },
        )
    }

    /// Memory files through one Git process per batch, never one per file,
    /// each within `per_file` bytes and all of them within `total`. Every size
    /// is checked before any body is read, so an oversized file is refused
    /// without Git streaming it, as the single-file read it replaces bounded
    /// each file. Like that read, any Git diagnostic on stderr refuses the
    /// batch even when Git succeeds.
    pub(super) fn memory_blobs(
        &self,
        wanted: &[(&str, &str)],
        per_file: u64,
        total: usize,
    ) -> Result<Vec<Vec<u8>>> {
        let policy = super::QueryStderrPolicy::RequireEmpty;
        let refuse: &dyn Fn(BatchRefusal<'_>) -> anyhow::Error = &|refusal| match refusal {
            BatchRefusal::Output | BatchRefusal::Total => {
                anyhow::anyhow!("memory snapshot exceeds {} MiB", total / (1024 * 1024))
            }
            BatchRefusal::Missing(path) => anyhow::anyhow!(
                "the object of memory file {path} is not in the local repository: fetch the data branch completely"
            ),
            BatchRefusal::File(path) => {
                anyhow::anyhow!("memory file {path} exceeds the per-file limit of {per_file} bytes")
            }
        };
        self.check_sizes(wanted, per_file, total, policy, refuse)?;
        self.batch(wanted, per_file, total, policy, refuse)
    }

    /// The size of every blob through one `cat-file --batch-check` process
    /// per chunk, refused as `refuse` names when a blob is missing, one
    /// exceeds `per_file` bytes or all of them together exceed `total_limit`.
    fn check_sizes(
        &self,
        wanted: &[(&str, &str)],
        per_file: u64,
        total_limit: usize,
        stderr_policy: super::QueryStderrPolicy,
        refuse: &dyn Fn(BatchRefusal<'_>) -> anyhow::Error,
    ) -> Result<()> {
        let mut total = 0_usize;
        for chunk in wanted.chunks(BATCH) {
            let mut input = Vec::with_capacity(chunk.len() * 65);
            for &(oid, _) in chunk {
                task_sync::ledger::oid(oid)
                    .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
                input.extend_from_slice(oid.as_bytes());
                input.push(b'\n');
            }
            let remaining = self
                .total_bound
                .checked_sub(self.started.elapsed())
                .filter(|left| !left.is_zero())
                .ok_or(super::QueryFailure::Deadline)?;
            let args = ["cat-file", "--batch-check"];
            let output = super::query::batch_blobs_observed(
                self.repository,
                &input,
                remaining.min(BOUND),
                u64::try_from(chunk.len() * 128)?,
                stderr_policy,
                true,
            )
            .map_err(|error| query_error(&args, &error, None))?;
            let mut rest = output.as_slice();
            for &(oid, path) in chunk {
                let end = rest
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .ok_or_else(|| malformed_batch(path))?;
                let size = blob_size(&rest[..end], oid, path, refuse)?;
                if size as u64 > per_file {
                    return Err(refuse(BatchRefusal::File(path)));
                }
                total = total.saturating_add(size);
                if total > total_limit {
                    return Err(refuse(BatchRefusal::Total));
                }
                rest = &rest[end + 1..];
            }
            if !rest.is_empty() {
                return Err(TaskSyncRefusal::GitQueryFailed
                    .invalid("local Git cat-file returned more batch records than asked")
                    .into());
            }
        }
        Ok(())
    }

    /// Blobs through one `cat-file --batch` process per chunk, refused as
    /// `refuse` names when a blob is missing, one exceeds `per_file` bytes or
    /// all of them together exceed `total_limit`.
    fn batch(
        &self,
        wanted: &[(&str, &str)],
        per_file: u64,
        total_limit: usize,
        stderr_policy: super::QueryStderrPolicy,
        refuse: &dyn Fn(BatchRefusal<'_>) -> anyhow::Error,
    ) -> Result<Vec<Vec<u8>>> {
        let mut blobs = Vec::with_capacity(wanted.len());
        let mut total = 0_usize;
        for chunk in wanted.chunks(BATCH) {
            let mut input = Vec::with_capacity(chunk.len() * 65);
            for &(oid, path) in chunk {
                task_sync::ledger::oid(oid)
                    .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
                #[cfg(test)]
                BLOB_READS.with(|reads| reads.borrow_mut().push(path.to_owned()));
                #[cfg(not(test))]
                let _ = path;
                input.extend_from_slice(oid.as_bytes());
                input.push(b'\n');
            }
            let remaining = self
                .total_bound
                .checked_sub(self.started.elapsed())
                .filter(|left| !left.is_zero())
                .ok_or(super::QueryFailure::Deadline)?;
            let args = ["cat-file", "--batch"];
            // The remaining byte budget and a header per object.
            let limit = u64::try_from(total_limit.saturating_sub(total) + chunk.len() * 128)?;
            let output = super::query::batch_blobs_observed(
                self.repository,
                &input,
                remaining.min(BOUND),
                limit,
                stderr_policy,
                false,
            )
            .map_err(|error| {
                // The batch output outgrew what the remaining bound admits
                // before its headers could be checked one by one.
                if matches!(
                    error.downcast_ref::<super::QueryFailure>(),
                    Some(super::QueryFailure::OutputLimit)
                ) {
                    refuse(BatchRefusal::Output)
                } else {
                    query_error(&args, &error, None)
                }
            })?;
            let mut rest = output.as_slice();
            for &(oid, path) in chunk {
                let malformed = || malformed_batch(path);
                let end = rest
                    .iter()
                    .position(|byte| *byte == b'\n')
                    .ok_or_else(malformed)?;
                let size = blob_size(&rest[..end], oid, path, refuse)?;
                if size as u64 > per_file {
                    return Err(refuse(BatchRefusal::File(path)));
                }
                total = total.saturating_add(size);
                if total > total_limit {
                    return Err(refuse(BatchRefusal::Total));
                }
                let body = rest.get(end + 1..end + 1 + size).ok_or_else(malformed)?;
                if rest.get(end + 1 + size) != Some(&b'\n') {
                    return Err(malformed().into());
                }
                blobs.push(body.to_vec());
                rest = &rest[end + 2 + size..];
            }
            if !rest.is_empty() {
                return Err(TaskSyncRefusal::GitQueryFailed
                    .invalid("local Git cat-file returned more batch records than asked")
                    .into());
            }
        }
        Ok(blobs)
    }

    /// Every entry below `tree`, trees included, through one Git process:
    /// each name is its path relative to `tree`.
    pub(super) fn recursive_entries(&self, tree: &str) -> Result<Vec<Entry>> {
        task_sync::ledger::oid(tree)
            .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
        parse_entries(&self.listing(&["ls-tree", "-r", "-t", "-z", tree])?)
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

/// Object ids per `cat-file` batch process: a full SHA-256 id and its newline
/// each, well under the input bound.
const BATCH: usize = 8 * 1024;

fn malformed_batch(path: &str) -> StoreError {
    TaskSyncRefusal::GitQueryFailed.invalid(format!(
        "local Git cat-file returned a malformed batch record for {path}"
    ))
}

/// The size a `cat-file` batch header gives for blob `oid`, or `refuse`'s
/// refusal when Git reports it missing.
fn blob_size(
    header: &[u8],
    oid: &str,
    path: &str,
    refuse: &dyn Fn(BatchRefusal<'_>) -> anyhow::Error,
) -> Result<usize> {
    let header = std::str::from_utf8(header).map_err(|_| malformed_batch(path))?;
    if header.strip_prefix(oid) == Some(" missing") {
        return Err(refuse(BatchRefusal::Missing(path)));
    }
    header
        .strip_prefix(oid)
        .and_then(|tail| tail.strip_prefix(" blob "))
        .and_then(|size| size.parse::<usize>().ok())
        .ok_or_else(|| malformed_batch(path).into())
}

/// Why a blob batch is refused: its output outgrew the remaining bound, a
/// named blob is missing or too large, or the blobs together are too large.
enum BatchRefusal<'a> {
    Output,
    Missing(&'a str),
    File(&'a str),
    Total,
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
    /// The base blob, so a verified delta can remove the old member exactly.
    pub(super) old_oid: String,
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
            && !memory_path(path, false)
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
            old_oid: fields[2].into(),
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
        || memory_path(path, true)
        || crate::task_sync::evidence::path::kind(path)
            == Some(crate::task_sync::evidence::path::Kind::Directory)
}

fn memory_path(path: &str, directory: bool) -> bool {
    if path == ".engram/memories" {
        return directory;
    }
    let Some(rest) = path.strip_prefix(".engram/memories/") else {
        return false;
    };
    let mut parts = rest.split('/');
    let Some(key) = parts.next() else {
        return false;
    };
    if crate::task_sync::memories::validate_key(key).is_err() {
        return false;
    }
    match (parts.next(), parts.next()) {
        (None, None) => directory,
        (Some("head.json"), None) => !directory,
        (Some(file), None) => {
            !directory
                && file
                    .strip_suffix(".json")
                    .is_some_and(|id| crate::ObjectId::from_stored(id.to_owned()).is_some())
        }
        _ => false,
    }
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

/// The paths of the blobs this thread read since the last call.
#[cfg(test)]
pub(crate) fn take_blob_reads() -> Vec<String> {
    BLOB_READS.with(|reads| std::mem::take(&mut *reads.borrow_mut()))
}

#[cfg(test)]
mod tests;
