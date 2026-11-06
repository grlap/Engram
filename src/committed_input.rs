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

/// Read the live registered input without consulting turn reports. Unlike
/// Engram's Node launcher, this reader admits eligible pinned submodules.
/// An unsupported repository is unavailable, never implicitly clean.
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
    let modules = run(&["submodule", "status", "--recursive"], false)
        .map_err(|error| anyhow::anyhow!("cannot read recursive submodule status: {error:#}"))?;
    let children = submodules(&modules)?;
    // Read filter configuration before root status can invoke child filters.
    for name in &children {
        let filters = query(
            &root.join(name),
            &[
                "config",
                "--null",
                "--get-regexp",
                "^filter\\..*\\.(clean|smudge|process)$",
            ],
            true,
            deadline,
        )
        .with_context(|| format!("submodule '{name}': cannot read filter configuration"))?;
        ensure!(
            filters.is_empty(),
            "submodule '{name}': configured Git filters are unsupported"
        );
    }
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
    for name in &children {
        ensure!(
            !status.split(|byte| *byte == 0).any(|entry| {
                entry.get(3..).is_some_and(|path| {
                    path == name.as_bytes() || path.starts_with(format!("{name}/").as_bytes())
                })
            }),
            "submodule '{name}': working tree is dirty"
        );
    }
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

fn submodules(bytes: &[u8]) -> Result<Vec<String>> {
    let text = std::str::from_utf8(bytes).context("Git returned non-UTF-8 submodule status")?;
    let mut children = Vec::new();
    for line in text.lines() {
        let prefix = *line
            .as_bytes()
            .first()
            .context("empty submodule status row")?;
        let row = line.get(1..).context("malformed submodule status")?;
        let (pin, path) = row
            .split_once(' ')
            .context("malformed submodule status row")?;
        let name = if path.ends_with(')') {
            path.rsplit_once(" (").map_or(path, |(name, _)| name)
        } else {
            path
        };
        ensure!(
            !name.is_empty()
                && Path::new(name)
                    .components()
                    .all(|part| matches!(part, std::path::Component::Normal(_)))
                && pin.len() == 40
                && pin.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "submodule '{name}': malformed path or commit"
        );
        match prefix {
            b' ' => children.push(name.to_owned()),
            b'-' => bail!("submodule '{name}': uninitialized"),
            b'+' => bail!("submodule '{name}': HEAD differs from its pin"),
            b'U' => bail!("submodule '{name}': conflicted gitlink"),
            _ => bail!("submodule '{name}': malformed status"),
        }
    }
    Ok(children)
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
        .stderr(if args[0] == "submodule" {
            Stdio::piped()
        } else {
            Stdio::null()
        });
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
        let mut command = Command::new("git");
        for (name, _) in std::env::vars_os() {
            if name
                .to_string_lossy()
                .to_ascii_uppercase()
                .starts_with("GIT_")
            {
                command.env_remove(name);
            }
        }
        // Checkouts and restores must use the same config scope as the reader.
        let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
        let output = command
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", null)
            .env("GIT_ATTR_NOSYSTEM", "1")
            .args(["-c", &format!("core.attributesFile={null}")])
            .args(["-c", &format!("core.excludesFile={null}")])
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
    fn configured_filters_are_unavailable_before_comparison() {
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
        git(root, &["config", "filter.test.clean", "must-not-run"]);
        assert!(read(root).unwrap_err().to_string().contains("filters"));
    }

    #[test]
    fn recursive_submodule_status_names_refusals() {
        let pin = crate::ObjectId::from_canonical_bytes(b"fixture commit").to_string();
        let pin = &pin[..40];
        assert_eq!(
            submodules(
                format!(" {pin} child (heads/main)\n {pin} child/nested (heads/main)\n").as_bytes()
            )
            .unwrap(),
            ["child", "child/nested"]
        );
        for (prefix, reason) in [
            ('?', "malformed"),
            ('-', "uninitialized"),
            ('+', "HEAD differs"),
            ('U', "conflicted"),
        ] {
            let error = submodules(format!("{prefix}{pin} child\n").as_bytes())
                .unwrap_err()
                .to_string();
            assert!(error.contains("child") && error.contains(reason), "{error}");
        }
    }
}
