//! Hardened local Git query construction for optional task exchange and diagnostics.

mod delta;
mod evidence;
mod path;
mod query;
mod snapshot;

#[cfg(test)]
pub(crate) use delta::take_blob_reads;
pub use delta::{
    ChangedTask, CommitTaskDelta, VerifiedTaskDelta, read_rooted_task_diff, read_task_delta,
    read_tasks_at, read_verified_task_delta,
};
pub(crate) use delta::{
    ExportParentPaths, matches_export_commit, matches_staged_export, merged_task_membership,
};
#[cfg(test)]
pub(crate) use evidence::decode_selected_pack;
pub(crate) use evidence::read_selected_evidence_remaining;
pub use evidence::{CommitEvidenceBatch, SelectionMismatch, read_selected_evidence};
pub use path::plain_path;
pub(crate) use path::set_repository_ceiling;
#[cfg(test)]
pub(crate) use query::BEFORE_QUERY;
pub(crate) use query::Failure as QueryFailure;
pub(crate) use query::StderrPolicy as QueryStderrPolicy;
pub(crate) use query::run as query;
pub(crate) use query::run_command as run_query_command;
pub(crate) use query::run_observed as query_observed;
#[cfg(all(test, windows))]
pub(crate) use query::test_clock::PendingQuery;
#[cfg(test)]
pub(crate) use query::wait_until_exit;
pub(crate) use query::{MAX_LISTING_BYTES, run_listing_observed as query_listing};

pub(crate) fn changed_task_ids(
    repository: &Path,
    before: &str,
    after: &str,
    deadline: std::time::Duration,
) -> anyhow::Result<std::collections::BTreeSet<crate::WorkId>> {
    crate::task_sync::ledger::oid(before)?;
    crate::task_sync::ledger::oid(after)?;
    let paths = query_listing(
        repository,
        &[
            "diff-tree",
            "--name-only",
            "--no-commit-id",
            "--no-renames",
            "-r",
            "-z",
            before,
            after,
            "--",
            ".engram/tasks",
        ],
        deadline,
        MAX_LISTING_BYTES,
    )?;
    paths
        .split(|byte| *byte == 0)
        .filter(|path| !path.is_empty())
        .map(|path| delta::task_path(std::str::from_utf8(path)?))
        .collect()
}
pub use snapshot::read_memory_snapshot;
pub use snapshot::{CommitObject, CommitTaskSnapshot, read_task_snapshot};
pub(crate) use snapshot::{admit_ordinary_data_tree, observe_commit, ordinary_data_policy};
pub(crate) use snapshot::{read_memory_snapshot_with_bytes, read_task_snapshot_bounded};

use std::{
    path::Path,
    process::{Command, Stdio},
};

const GIT_HARDENING: &[&str] = &[
    "--no-pager",
    "--no-optional-locks",
    "--no-replace-objects",
    "-c",
    "core.commitGraph=false",
    "-c",
    "core.longpaths=true",
    "-c",
    "core.fsmonitor=",
    "-c",
    "core.hooksPath=",
    "-c",
    "credential.helper=",
    "-c",
    "protocol.allow=never",
];

/// Environment that could point git at another repository, object store,
/// shallow file or configuration; cleared so the named repository alone is
/// read. It holds every repository-local variable git names (`git rev-parse
/// --local-env-vars`) except `GIT_GRAFT_FILE`, which [`command`] sets.
const CLEARED_GIT_ENVIRONMENT: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_PREFIX",
    "GIT_INTERNAL_SUPER_PREFIX",
    "GIT_INDEX_FILE",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_SHALLOW_FILE",
    "GIT_NO_REPLACE_OBJECTS",
    "GIT_REPLACE_REF_BASE",
    "GIT_NAMESPACE",
    "GIT_CONFIG",
    "GIT_CONFIG_PARAMETERS",
    "GIT_CONFIG_COUNT",
];

/// Construct a non-interactive local Git query with transport, hooks, monitor,
/// replacement ancestry and repository redirection disabled. The caller bounds
/// execution and admits attributes before commands that compare worktree bytes.
pub fn command(repository: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("git");
    command
        .current_dir(repository)
        .args(GIT_HARDENING)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_ALLOW_PROTOCOL", "")
        // Git reads the null device as an empty graft file on every platform.
        .env("GIT_GRAFT_FILE", "/dev/null")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    for name in CLEARED_GIT_ENVIRONMENT {
        command.env_remove(name);
    }
    set_repository_ceiling(&mut command, repository);
    command
}
