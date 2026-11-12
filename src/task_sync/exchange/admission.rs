//! Read-only admission of one retained request. The private fields prevent a
//! caller from substituting an uninspected tree at the storage boundary.

use super::{
    Cancellation, ExchangeTarget, RunLimits, TargetSelection, remaining,
    retained::{git_directory, local},
    verify_retained_commit_remaining,
};
use crate::{
    ObjectId,
    storage::SqliteStore,
    task_sync::{
        TaskTree,
        ledger::{Binding, Direction, ImportReceipt, Intent, OutgoingProgress, oid},
        retention,
    },
};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use std::{collections::BTreeMap, time::Instant};

/// An observed input, not a grant or a permanent filesystem snapshot. Storage
/// rechecks the retained files and local planning basis in its write transaction.
#[derive(Clone, Debug)]
pub struct InspectedExchange {
    pub(crate) binding_id: ObjectId,
    pub(crate) binding: Binding,
    pub(crate) intent: Intent,
    pub(crate) tree: TaskTree,
}

impl InspectedExchange {
    /// Observes again, immediately before a durable transition, the Git state
    /// this value was inspected against: the admin directory, selected branch
    /// and head, target configuration and the exact retained commit pins. An
    /// outgoing operation may also find its branch already at the retained
    /// commit. Nothing is repaired or recreated; any drift refuses.
    pub(crate) async fn revalidate(&self, limits: RunLimits, cancel: &Cancellation) -> Result<()> {
        let started = Instant::now();
        observe_git(
            &self.binding,
            &self.intent,
            self.intent.direction == Direction::Outgoing,
            limits,
            started,
            cancel,
        )
        .await?;
        self.recheck_files()
    }

    pub(crate) fn recheck_files(&self) -> Result<()> {
        ensure!(
            TaskTree::from_stored_documents(&retention::read_for_intent(
                &self.binding,
                &self.intent
            )?)? == self.tree,
            "retained input changed after inspection"
        );
        ensure!(
            retention::commit_selection(
                &self.binding.repository.git_directory,
                self.intent.operation
            )?
            .as_ref()
                == Some(&self.intent.commit),
            "retained commit selection changed after inspection"
        );
        Ok(())
    }
}

/// Inspect local Git/configuration and exact task blobs without fetching,
/// creating a commit, changing a ref, enabling sync or writing progress.
///
/// # Errors
/// Refuses binding/configuration drift, missing pins, tree mismatch and limits.
pub async fn inspect_exchange(
    binding_id: &ObjectId,
    binding: &Binding,
    intent: &Intent,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<InspectedExchange> {
    let started = std::time::Instant::now();
    limits.validate_request()?;
    let _warning = crate::operation_time::Warning::start("task exchange inspect exchange");
    let limits = crate::task_sync::exchange::remaining(limits, started)?;
    inspect_exchange_remaining(binding_id, binding, intent, limits, cancel).await
}

// The public ingress admitted the full budget; internal calls spend its remainder.
pub(crate) async fn inspect_exchange_remaining(
    binding_id: &ObjectId,
    binding: &Binding,
    intent: &Intent,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<InspectedExchange> {
    binding.validate()?;
    intent.validate()?;
    ensure!(
        intent.commit.retention_ref
            == format!("refs/engram/task-sync/exchange/{}/commit", intent.operation),
        "commit belongs to another operation"
    );
    let started = Instant::now();
    observe_git(
        binding,
        intent,
        intent.direction == Direction::Outgoing,
        limits,
        started,
        cancel,
    )
    .await?;
    let repository = &binding.repository.main_checkout;
    let documents = retention::read_for_intent(binding, intent)?;
    let tree = TaskTree::from_stored_documents(&documents)?;
    let listed = local(
        repository,
        &[
            "ls-tree",
            "-r",
            "-z",
            "--full-tree",
            &intent.commit.oid,
            "--",
            ".engram/project.json",
            ".engram/tasks",
        ],
        remaining(limits, started)?,
        cancel,
    )
    .await?;
    let mut blobs = BTreeMap::new();
    for entry in listed
        .split(|byte| *byte == 0)
        .filter(|entry| !entry.is_empty())
    {
        let (header, path) = entry.split_at(
            entry
                .iter()
                .position(|byte| *byte == b'\t')
                .context("invalid Git tree entry")?,
        );
        let header = std::str::from_utf8(header)?.split(' ').collect::<Vec<_>>();
        ensure!(
            header.len() == 3 && header[0] == "100644" && header[1] == "blob",
            "task entry must be an ordinary non-executable blob"
        );
        oid(header[2])?;
        let path = std::str::from_utf8(&path[1..])?
            .strip_prefix(".engram/")
            .context("unexpected task tree path")?;
        ensure!(
            blobs
                .insert(path.to_owned(), header[2].to_owned())
                .is_none(),
            "duplicate Git task path"
        );
    }
    ensure!(
        blobs.keys().eq(documents.keys()),
        "retained Git task inventory differs"
    );
    // Hash existing raw files in bounded argument batches, with no -w and no
    // filters. This avoids one subprocess per task and never reserializes JSON.
    let paths = documents.keys().collect::<Vec<_>>();
    let mut offset = 0;
    while offset < paths.len() {
        let mut owned = Vec::new();
        let mut bytes = 0_usize;
        while offset + owned.len() < paths.len() && owned.len() < 64 {
            let path =
                git_path_argument(&intent.candidate.directory.join(paths[offset + owned.len()]))?;
            ensure!(
                path.len() <= 8192,
                "retained candidate path exceeds Git argument bound"
            );
            if !owned.is_empty() && bytes + path.len() > 8192 {
                break;
            }
            bytes += path.len();
            owned.push(path);
        }
        let mut args = vec!["hash-object", "--no-filters", "--"];
        args.extend(owned.iter().map(String::as_str));
        let hashes = local(repository, &args, remaining(limits, started)?, cancel).await?;
        let hashes = std::str::from_utf8(&hashes)?.lines().collect::<Vec<_>>();
        ensure!(
            hashes.len() == owned.len(),
            "incomplete Git blob comparison"
        );
        for (index, hash) in hashes.iter().enumerate() {
            ensure!(
                blobs[paths[offset + index]] == *hash,
                "retained Git task blob differs"
            );
        }
        offset += owned.len();
    }
    let inspected = InspectedExchange {
        binding_id: binding_id.clone(),
        binding: binding.clone(),
        intent: intent.clone(),
        tree,
    };
    inspected.recheck_files()?;
    Ok(inspected)
}

/// A retained candidate path as Git receives it on its command line. Canonical
/// Windows paths are verbatim (`\\?\C:\...`), which Git for Windows cannot open
/// as an argument; the shared Git spelling names the same file. Stored binding
/// paths are unchanged.
///
/// # Errors
/// Refuses a path with no supported Git spelling or that is not UTF-8.
fn git_path_argument(path: &std::path::Path) -> Result<String> {
    let plain = crate::task_sync::git::plain_path(path)
        .context("retained candidate path has no supported Git spelling")?;
    Ok(plain
        .to_str()
        .context("non-UTF8 retained candidate path")?
        .to_owned())
}

/// The Git observations an inspection rests on. `published_head` admits an
/// outgoing branch that already points at the operation's retained commit.
async fn observe_git(
    binding: &Binding,
    intent: &Intent,
    published_head: bool,
    limits: RunLimits,
    started: Instant,
    cancel: &Cancellation,
) -> Result<()> {
    observe_git_selection(
        binding,
        GitSelection {
            expected_head: intent.expected_head.as_deref(),
            commit: &intent.commit,
            common_commit: intent.common_commit.as_ref(),
            published_head,
        },
        limits,
        started,
        cancel,
    )
    .await
}

/// The selected branch was read and points somewhere other than the request
/// expects: a definite observation, unlike an unavailable or uncertain Git read.
#[derive(Debug)]
pub(super) struct SelectedHeadChanged;

impl std::fmt::Display for SelectedHeadChanged {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("selected branch head changed")
    }
}

impl std::error::Error for SelectedHeadChanged {}

#[derive(Clone, Copy)]
pub(super) struct GitSelection<'a> {
    pub expected_head: Option<&'a str>,
    pub commit: &'a crate::task_sync::ledger::RetainedCommit,
    pub common_commit: Option<&'a crate::task_sync::ledger::RetainedCommit>,
    pub published_head: bool,
}

pub(super) async fn observe_git_selection(
    binding: &Binding,
    selected: GitSelection<'_>,
    limits: RunLimits,
    started: Instant,
    cancel: &Cancellation,
) -> Result<()> {
    observe_git_selection_data(binding, selected, limits, started, cancel)
        .await
        .map(|_| ())
}

/// Return this inspection's fresh worktree observation so callers can compare
/// their captured basis without performing a second complete observation.
pub(super) async fn observe_git_selection_data(
    binding: &Binding,
    selected: GitSelection<'_>,
    limits: RunLimits,
    started: Instant,
    cancel: &Cancellation,
) -> Result<crate::task_sync::data_worktree::DataWorktree> {
    use crate::task_sync::data_worktree::{Budget, DataWorktree};
    let repository = &binding.repository.main_checkout;
    let budget = Budget::within(started, limits.deadline)?;
    let data = DataWorktree::from_binding_within(binding, budget)?;
    data.check_commit_within(&selected.commit.oid, budget)?;
    if let Some(base) = selected.common_commit {
        data.check_commit_within(&base.oid, budget)?;
    }
    let admin = git_directory(repository, remaining(limits, started)?, cancel).await?;
    ensure!(
        std::fs::canonicalize(&admin)? == std::fs::canonicalize(&binding.repository.git_directory)?,
        "repository admin binding changed"
    );
    let branch = local(
        repository,
        &["symbolic-ref", "--quiet", "HEAD"],
        remaining(limits, started)?,
        cancel,
    )
    .await?;
    ensure!(
        branch == format!("{}\n", binding.branch).as_bytes(),
        "selected branch changed"
    );
    let head = local(
        repository,
        &[
            "for-each-ref",
            "--format=%(refname) %(objectname) %(symref)",
            &binding.branch,
        ],
        remaining(limits, started)?,
        cancel,
    )
    .await?;
    let expected = selected.expected_head.map_or_else(Vec::new, |head| {
        format!("{} {head} \n", binding.branch).into_bytes()
    });
    let published = format!("{} {} \n", binding.branch, selected.commit.oid).into_bytes();
    ensure!(
        head == expected || (selected.published_head && head == published),
        SelectedHeadChanged
    );
    let target = ExchangeTarget::capture_remaining(
        &TargetSelection {
            repository: repository.clone(),
            remote: binding.target.remote.clone(),
            reference: binding.target.reference.clone(),
        },
        remaining(limits, started)?,
        cancel,
    )
    .await?;
    ensure!(
        target.fingerprint()? == binding.target.configuration_fingerprint,
        "selected target configuration changed"
    );
    verify_retained_commit_remaining(
        repository,
        selected.commit,
        remaining(limits, started)?,
        cancel,
    )
    .await?;
    if let Some(base) = selected.common_commit {
        verify_retained_commit_remaining(repository, base, remaining(limits, started)?, cancel)
            .await?;
    }
    Ok(data)
}

/// Records an inspected request after observing its Git state again. This is
/// the supported route: the storage call alone cannot see Git.
///
/// # Errors
/// Refuses Git drift since inspection, and every refusal of the storage call.
pub async fn prepare_exchange(
    store: &mut SqliteStore,
    input: &InspectedExchange,
    now: DateTime<Utc>,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<ObjectId> {
    let started = std::time::Instant::now();
    limits.validate_request()?;
    let _warning = crate::operation_time::Warning::start("task exchange prepare exchange");
    let limits = crate::task_sync::exchange::remaining(limits, started)?;
    prepare_exchange_remaining(store, input, now, limits, cancel).await
}

// The public ingress admitted the full budget; internal calls spend its remainder.
pub(crate) async fn prepare_exchange_remaining(
    store: &mut SqliteStore,
    input: &InspectedExchange,
    now: DateTime<Utc>,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<ObjectId> {
    input.revalidate(limits, cancel).await?;
    Ok(store.prepare_task_sync_exchange(input, now)?)
}

/// Applies an inspected import after observing its Git state again.
///
/// # Errors
/// Refuses Git drift since inspection, and every refusal of the storage call.
pub async fn apply_exchange(
    store: &mut SqliteStore,
    input: &InspectedExchange,
    dry_run: bool,
    now: DateTime<Utc>,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<ImportReceipt> {
    let started = std::time::Instant::now();
    limits.validate_request()?;
    let _warning = crate::operation_time::Warning::start("task exchange apply exchange");
    let limits = crate::task_sync::exchange::remaining(limits, started)?;
    apply_exchange_remaining(store, input, dry_run, now, limits, cancel).await
}

// The public ingress admitted the full budget; internal calls spend its remainder.
pub(crate) async fn apply_exchange_remaining(
    store: &mut SqliteStore,
    input: &InspectedExchange,
    dry_run: bool,
    now: DateTime<Utc>,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<ImportReceipt> {
    input.revalidate(limits, cancel).await?;
    Ok(store.apply_task_sync_exchange(input, dry_run, now)?)
}

/// Records one outgoing step after observing its Git state again.
///
/// # Errors
/// Refuses Git drift since inspection, and every refusal of the storage call.
pub async fn record_exchange_progress(
    store: &mut SqliteStore,
    input: &InspectedExchange,
    progress: OutgoingProgress,
    now: DateTime<Utc>,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<ObjectId> {
    let started = std::time::Instant::now();
    limits.validate_request()?;
    let _warning = crate::operation_time::Warning::start("task exchange record exchange progress");
    let limits = crate::task_sync::exchange::remaining(limits, started)?;
    record_exchange_progress_remaining(store, input, progress, now, limits, cancel).await
}

// The public ingress admitted the full budget; internal calls spend its remainder.
pub(crate) async fn record_exchange_progress_remaining(
    store: &mut SqliteStore,
    input: &InspectedExchange,
    progress: OutgoingProgress,
    now: DateTime<Utc>,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<ObjectId> {
    input.revalidate(limits, cancel).await?;
    Ok(store.record_task_sync_exchange_progress(input, progress, now)?)
}

#[cfg(all(test, windows))]
mod tests;
