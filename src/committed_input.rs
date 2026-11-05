//! A live, bounded read of one local committed input. This is endpoint
//! evidence, never a durable identity or a historical seal validator.

use anyhow::{Context, Result, bail, ensure};
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

#[cfg(test)]
thread_local! {
    static READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn read_count() -> usize {
    READS.get()
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct CommittedInput {
    pub(crate) root: PathBuf,
    pub(crate) commit: String,
    pub(crate) clean: bool,
    pub(crate) measured_at: chrono::DateTime<chrono::Utc>,
}

/// Match the launcher's committed-input predicate, without consulting turn
/// reports. An unsupported repository is unavailable, never implicitly clean.
pub(crate) fn read(root: &Path) -> Result<CommittedInput> {
    #[cfg(test)]
    READS.set(READS.get() + 1);
    ensure!(
        root.is_absolute(),
        "the registered root is not an absolute local path"
    );
    let root = root
        .canonicalize()
        .context("the registered root is unavailable")?;
    let deadline = Instant::now() + Duration::from_secs(8);
    let run = |args: &[&str], missing| query(&root, args, missing, deadline);
    let top = path_output(run(&["rev-parse", "--show-toplevel"], false)?)?;
    ensure!(
        top.canonicalize()? == root,
        "the registered root is not this Git worktree's top level"
    );
    let admin = path_output(run(&["rev-parse", "--absolute-git-dir"], false)?)?.canonicalize()?;
    let head = || -> Result<String> {
        let bytes = run(&["rev-parse", "--verify", "HEAD^{commit}"], false)?;
        let value = std::str::from_utf8(&bytes)?.trim();
        ensure!(
            value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "registered committed input requires a SHA-1 commit; SHA-256 repositories are unsupported"
        );
        Ok(value.to_owned())
    };
    let commit = head()?;
    let filters = run(
        &[
            "config",
            "--null",
            "--get-regexp",
            "^filter\\..*\\.(clean|smudge|process)$",
        ],
        true,
    )?;
    ensure!(
        filters.is_empty(),
        "configured Git filters are unsupported for committed input"
    );
    // Refuse before status: comparing worktree bytes could execute a filter
    // in a child repository. This is the launcher's current Gitlink policy.
    let index = run(&["ls-files", "--stage", "-z"], false)?;
    ensure!(
        !index
            .split(|byte| *byte == 0)
            .any(|entry| entry.starts_with(b"160000 ")),
        "Gitlinks are unsupported for committed input"
    );
    let flags = run(&["ls-files", "-v", "-z"], false)?;
    ensure!(
        !flags.split(|byte| *byte == 0).any(|entry| entry
            .first()
            .is_some_and(|tag| tag.is_ascii_lowercase() || *tag == b'S')),
        "skip-worktree, assume-unchanged and sparse inputs are unsupported"
    );
    let status = run(
        &[
            "status",
            "--porcelain=v1",
            "--untracked-files=all",
            "--ignore-submodules=none",
            "-z",
        ],
        false,
    )?;
    ensure!(
        head()? == commit,
        "HEAD moved during the committed-input read"
    );
    let end_top = path_output(run(&["rev-parse", "--show-toplevel"], false)?)?.canonicalize()?;
    let end_admin =
        path_output(run(&["rev-parse", "--absolute-git-dir"], false)?)?.canonicalize()?;
    ensure!(
        end_top == root && end_admin == admin,
        "the registered Git worktree changed during the read"
    );
    Ok(CommittedInput {
        root,
        commit,
        clean: status.is_empty(),
        measured_at: chrono::Utc::now(),
    })
}

fn path_output(bytes: Vec<u8>) -> Result<PathBuf> {
    let text = String::from_utf8(bytes).context("Git returned a non-UTF-8 root path")?;
    let text = text.trim_end_matches(['\r', '\n']);
    ensure!(!text.is_empty(), "Git returned no root path");
    Ok(PathBuf::from(text))
}

fn query(root: &Path, args: &[&str], missing: bool, deadline: Instant) -> Result<Vec<u8>> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        bail!("the committed-input read exceeded its deadline");
    }
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let mut command = Command::new("git");
    command
        .current_dir(root)
        .args(["--no-pager", "--no-optional-locks", "--no-replace-objects"]);
    for setting in [
        "core.fsmonitor=false",
        "core.checkStat=default",
        "core.trustctime=true",
        "core.ignoreStat=false",
        "core.untrackedCache=false",
        "status.showUntrackedFiles=all",
        "diff.ignoreSubmodules=none",
    ] {
        command.args(["-c", setting]);
    }
    command.args([
        "-c",
        &format!("core.excludesFile={null}"),
        "-c",
        &format!("core.attributesFile={null}"),
    ]);
    command
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::null());
    for (name, _) in std::env::vars_os() {
        if name
            .to_string_lossy()
            .to_ascii_uppercase()
            .starts_with("GIT_")
        {
            command.env_remove(name);
        }
    }
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null)
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1");
    crate::task_sync::git::run_query_command(command, args[0], missing, None, remaining)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn git(root: &Path, args: &[&str]) -> Vec<u8> {
        let output = Command::new("git")
            .current_dir(root)
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    #[test]
    fn live_commit_distinguishes_dirty_reverted_and_equal_tree_commits() {
        let scratch = crate::test_support::temp_home().unwrap();
        let root = scratch.path();
        git(root, &["init", "-q"]);
        std::fs::write(root.join("tracked"), "checked\n").unwrap();
        git(root, &["add", "tracked"]);
        let commit = || {
            git(
                root,
                &[
                    "-c",
                    "user.name=Test",
                    "-c",
                    "user.email=test@example.invalid",
                    "commit",
                    "--no-gpg-sign",
                    "-qm",
                    "fixture",
                    "--allow-empty",
                ],
            )
        };
        commit();
        let checked = read(root).unwrap();
        assert!(checked.clean);
        std::fs::write(root.join("tracked"), "edited\n").unwrap();
        assert!(!read(root).unwrap().clean);
        git(root, &["restore", "tracked"]);
        let reverted = read(root).unwrap();
        assert_eq!(reverted.commit, checked.commit);
        assert!(reverted.clean);
        let tree = git(root, &["rev-parse", "HEAD^{tree}"]);
        commit();
        assert_eq!(git(root, &["rev-parse", "HEAD^{tree}"]), tree);
        assert_ne!(read(root).unwrap().commit, checked.commit);
        assert!(read(&root.join("missing")).is_err());
        std::fs::create_dir(root.join("child")).unwrap();
        assert!(read(&root.join("child")).is_err());
    }

    #[test]
    fn hiding_flags_and_filters_are_unavailable_before_comparison() {
        let scratch = crate::test_support::temp_home().unwrap();
        let root = scratch.path();
        git(root, &["init", "-q"]);
        std::fs::write(root.join("tracked"), "checked\n").unwrap();
        git(root, &["add", "tracked"]);
        git(
            root,
            &[
                "-c",
                "user.name=Test",
                "-c",
                "user.email=test@example.invalid",
                "commit",
                "--no-gpg-sign",
                "-qm",
                "fixture",
            ],
        );
        git(root, &["update-index", "--assume-unchanged", "tracked"]);
        assert!(
            read(root)
                .unwrap_err()
                .to_string()
                .contains("assume-unchanged")
        );
        git(root, &["update-index", "--no-assume-unchanged", "tracked"]);
        git(root, &["config", "filter.test.clean", "must-not-run"]);
        assert!(read(root).unwrap_err().to_string().contains("filters"));
    }
}
