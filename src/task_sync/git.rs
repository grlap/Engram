//! Hardened local Git query construction for optional task exchange and diagnostics.

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
    if let Some(parent) = repository.parent() {
        command.env("GIT_CEILING_DIRECTORIES", parent);
    }
    command
}
