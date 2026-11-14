//! A live, bounded read of one local committed input. This is endpoint
//! evidence, never a durable identity or a historical seal validator.

use anyhow::{Context, Result, bail, ensure};
mod eol;
use std::{
    path::{Path, PathBuf},
    process::{Command, Stdio},
    time::{Duration, Instant},
};

mod delta;

#[cfg(test)]
thread_local! {
    static READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn read_count() -> usize {
    READS.get()
}

// The Git subcommands this thread's observations ran, so tests can see which
// processes an observation started.
#[cfg(test)]
thread_local! {
    static COMMANDS: std::cell::RefCell<Vec<String>> = const { std::cell::RefCell::new(Vec::new()) };
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
    let _warning = crate::operation_time::Warning::start("registered committed input read");
    capture(
        root,
        crate::operation_time::now() + crate::source::MAX_BUDGET,
        false,
        &mut None,
    )
    .map(|value| value.input)
}

struct Capture {
    input: CommittedInput,
    tree: Option<String>,
    entries: Vec<crate::source::Entry>,
    source_id: Option<crate::source::SourceId>,
}

// The source API owns the warning through observation and final result processing.
pub(crate) fn observe(root: &Path, budget: Duration) -> crate::source::Observation {
    use crate::source::{Observation, RefusalCode, Status};
    if budget.is_zero() || budget > crate::source::MAX_BUDGET {
        return Observation::unavailable(
            None,
            RefusalCode::InputUnavailable,
            format!(
                "source budget must be between one and {} milliseconds",
                crate::source::MAX_BUDGET_MS
            ),
        );
    }
    let deadline = crate::operation_time::now() + budget;
    let mut known_root = None;
    let mut observed = match capture(root, deadline, true, &mut known_root) {
        Ok(value) => Observation {
            version: 1,
            status: if value.input.clean {
                Status::Clean
            } else {
                Status::NotClean
            },
            root: known_root.clone(),
            source: Some(format!("git-commit:{}", value.input.commit)),
            tree: value.tree,
            entries: value.entries,
            refusal: None,
            source_id: value.source_id,
            deadline: Some(deadline),
        },
        Err(error) => {
            Observation::unavailable(known_root, refusal_code(&error), format!("{error:#}"))
        }
    };
    observed.deadline = Some(deadline);
    observed
}

fn refusal_code(error: &anyhow::Error) -> crate::source::RefusalCode {
    use crate::{source::RefusalCode, task_sync::git::QueryFailure};
    error
        .chain()
        .find_map(|cause| match cause.downcast_ref::<QueryFailure>() {
            Some(QueryFailure::Deadline) => Some(RefusalCode::Deadline),
            Some(QueryFailure::OutputLimit) => Some(RefusalCode::OutputLimit),
            _ => None,
        })
        .unwrap_or(RefusalCode::InputUnavailable)
}

// The shared process helper may return after its allowance (for example while
// draining a closed child's pipe). An overdue result is never usable here.
fn before_deadline<T>(deadline: Instant, result: Result<T>) -> Result<T> {
    if crate::operation_time::now() >= deadline {
        return Err(crate::task_sync::git::QueryFailure::Deadline.into());
    }
    result
}

fn capture(
    root: &Path,
    deadline: Instant,
    details: bool,
    known_root: &mut Option<String>,
) -> Result<Capture> {
    #[cfg(test)]
    READS.set(READS.get() + 1);
    ensure!(
        root.is_absolute(),
        "the registered root is not an absolute local path"
    );
    let root = root
        .canonicalize()
        .context("the registered root is unavailable")?;
    if details {
        root.to_str().context("the registered root is not UTF-8")?;
        let text = crate::storage::store_path_text(&root);
        ensure!(
            Path::new(&text).is_absolute(),
            "the registered root has no plain absolute path"
        );
        *known_root = Some(text);
    }
    let git = resolve_git(&root)?;
    let environment = eol::Environment::for_read();
    let policy = eol::Policy::read(&git, &root, deadline, &environment)?;
    let fallback = policy.materialize(&root, deadline, &environment)?;
    let global = fallback.as_ref().map(|value| value.file.as_path());
    let run = |args: &[&str], missing| query(&git, &root, args, missing, deadline, global);
    let sha1 = |value: &str| -> Result<String> {
        ensure!(
            value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "registered committed input requires a SHA-1 commit; SHA-256 repositories are unsupported"
        );
        Ok(value.to_owned())
    };
    // One Git process reports the worktree top, its admin directory and HEAD;
    // the same read at the end detects movement during the observation.
    let locate = || -> Result<(PathBuf, PathBuf, String)> {
        let bytes = run(
            &[
                "rev-parse",
                "--show-toplevel",
                "--absolute-git-dir",
                "--verify",
                "HEAD^{commit}",
            ],
            false,
        )?;
        let text = String::from_utf8(bytes).context("Git returned a non-UTF-8 root path")?;
        if let [top, admin, head] = text.lines().collect::<Vec<_>>().as_slice()
            && !top.is_empty()
            && !admin.is_empty()
        {
            return Ok((
                PathBuf::from(top).canonicalize()?,
                PathBuf::from(admin).canonicalize()?,
                sha1(head)?,
            ));
        }
        // Git ends each value with a newline, so a path holding one (possible
        // on Unix) cannot be framed in a shared reply: read each on its own.
        let top = path_output(run(&["rev-parse", "--show-toplevel"], false)?)?;
        let admin = path_output(run(&["rev-parse", "--absolute-git-dir"], false)?)?;
        let head = run(&["rev-parse", "--verify", "HEAD^{commit}"], false)?;
        Ok((
            top.canonicalize()?,
            admin.canonicalize()?,
            sha1(std::str::from_utf8(&head)?.trim())?,
        ))
    };
    let (top, admin, commit) = locate()?;
    ensure!(
        top == root,
        "the registered root is not this Git worktree's top level"
    );
    let tree = if details {
        let bytes = run(
            &["rev-parse", "--verify", &format!("{commit}^{{tree}}")],
            false,
        )?;
        let value = std::str::from_utf8(&bytes)?.trim();
        ensure!(
            value.len() == 40 && value.bytes().all(|byte| byte.is_ascii_hexdigit()),
            "Git returned a malformed committed tree"
        );
        Some(value.to_owned())
    } else {
        None
    };
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
    let modules = submodule_status(|args| run(args, false))
        .context("cannot read recursive submodule status")?;
    let child_states = submodule_states(&modules, details)?;
    let children: Vec<_> = child_states
        .iter()
        .map(|child| child.path.clone())
        .collect();
    // Read filter configuration before root status can invoke child filters.
    guard_child_filters(
        &git,
        &root,
        &children,
        deadline,
        &policy,
        global,
        &environment,
    )?;
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
    let source_id = if details {
        Some(delta::capture(
            &Query {
                git: &git,
                global,
                deadline,
            },
            &root,
            &commit,
            tree.as_deref().context("missing captured tree")?,
            &status,
            &child_states,
        )?)
    } else {
        guard_child_dirt(&children, &status)?;
        None
    };
    let (end_top, end_admin, end_commit) = locate()?;
    ensure!(
        end_commit == commit,
        "HEAD moved during the committed-input read"
    );
    ensure!(
        end_top == root && end_admin == admin,
        "the registered Git worktree changed during the read"
    );
    let entries = if details {
        status_entries(&status)?
    } else {
        Vec::new()
    };
    before_deadline(
        deadline,
        Ok(Capture {
            input: CommittedInput {
                root,
                commit,
                clean: status.is_empty(),
                measured_at: chrono::Utc::now(),
            },
            tree,
            entries,
            source_id,
        }),
    )
}

fn guard_child_filters(
    git: &Path,
    root: &Path,
    children: &[String],
    deadline: Instant,
    policy: &eol::Policy,
    global: Option<&Path>,
    environment: &eol::Environment,
) -> Result<()> {
    for name in children {
        ensure!(
            eol::Policy::read(git, &root.join(name), deadline, environment).with_context(
                || format!("submodule '{name}': cannot read machine line-ending policy")
            )? == *policy,
            "submodule '{name}': contextual machine line-ending fallback differs from the root"
        );
        let filters = query(
            git,
            &root.join(name),
            &[
                "config",
                "--null",
                "--get-regexp",
                "^filter\\..*\\.(clean|smudge|process)$",
            ],
            true,
            deadline,
            global,
        )
        .with_context(|| format!("submodule '{name}': cannot read filter configuration"))?;
        ensure!(
            filters.is_empty(),
            "submodule '{name}': configured Git filters are unsupported"
        );
    }
    Ok(())
}

fn guard_child_dirt(children: &[String], status: &[u8]) -> Result<()> {
    for name in children {
        ensure!(
            !status.split(|byte| *byte == 0).any(|entry| {
                entry.get(3..).is_some_and(|path| {
                    path == name.as_bytes() || path.starts_with(format!("{name}/").as_bytes())
                })
            }),
            "submodule '{name}': working tree is dirty"
        );
    }
    Ok(())
}

fn status_entries(bytes: &[u8]) -> Result<Vec<crate::source::Entry>> {
    use crate::source::Entry;
    ensure!(
        bytes.is_empty() || bytes.last() == Some(&0),
        "Git returned truncated porcelain"
    );
    let mut records = bytes
        .strip_suffix(&[0])
        .unwrap_or(bytes)
        .split(|byte| *byte == 0);
    let mut entries = Vec::new();
    if bytes.is_empty() {
        return Ok(entries);
    }
    while let Some(record) = records.next() {
        ensure!(
            record.len() >= 4 && record[2] == b' ',
            "Git returned malformed porcelain"
        );
        let status = std::str::from_utf8(&record[..2])?;
        ensure!(
            status.bytes().all(|byte| b" MADRCUT?!".contains(&byte)) && status != "  ",
            "Git returned malformed porcelain status"
        );
        let path =
            std::str::from_utf8(&record[3..]).context("Git returned a non-UTF-8 changed path")?;
        ensure!(!path.is_empty(), "Git returned an empty changed path");
        let origin = if status.bytes().any(|byte| matches!(byte, b'R' | b'C')) {
            let value = records
                .next()
                .context("Git rename/copy has no origin path")?;
            let value =
                std::str::from_utf8(value).context("Git returned a non-UTF-8 origin path")?;
            ensure!(!value.is_empty(), "Git returned an empty origin path");
            Some(value.to_owned())
        } else {
            None
        };
        entries.push(Entry {
            status: status.to_owned(),
            path: path.to_owned(),
            origin,
        });
    }
    Ok(entries)
}

// Resolve before setting Git's cwd. In particular, Windows must never search
// an inspected checkout for git.exe. Relative PATH entries confer no trust.
fn resolve_git(root: &Path) -> Result<PathBuf> {
    let name = if cfg!(windows) { "git.exe" } else { "git" };
    for directory in std::env::split_paths(&std::env::var_os("PATH").unwrap_or_default()) {
        if !directory.is_absolute() {
            continue;
        }
        let Ok(path) = directory.join(name).canonicalize() else {
            continue;
        };
        if path.starts_with(root) || !path.is_file() {
            continue;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            if path.metadata()?.permissions().mode() & 0o111 == 0 {
                continue;
            }
        }
        return Ok(path);
    }
    bail!("a trusted absolute Git executable outside the inspected root is unavailable")
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct Child {
    path: String,
    commit: String,
    off_pin: bool,
}

#[cfg(test)]
fn submodules(bytes: &[u8]) -> Result<Vec<String>> {
    submodule_states(bytes, false)
        .map(|children| children.into_iter().map(|child| child.path).collect())
}

fn submodule_states(bytes: &[u8], allow_off_pin: bool) -> Result<Vec<Child>> {
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
        let name = if matches!(prefix, b' ' | b'+') && path.ends_with(')') {
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
            b' ' => children.push(Child {
                path: name.to_owned(),
                commit: pin.to_owned(),
                off_pin: false,
            }),
            b'-' => bail!("submodule '{name}': uninitialized"),
            b'+' if allow_off_pin => children.push(Child {
                path: name.to_owned(),
                commit: pin.to_owned(),
                off_pin: true,
            }),
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

/// The recursive submodule status. On Windows `git submodule` starts a shell
/// script that costs seconds even in a repository without submodules. Its rows
/// come only from gitlinks (mode 160000) in the index, so an index without one
/// has the same empty status, read with one quick query. Any failure of that
/// probe, such as a Git older than `ls-files --format`, runs the script itself.
/// So does an index too large for the query's output bound (seven bytes per
/// entry, so about 150,000 entries): the result is the same, only slower.
fn submodule_status(run: impl Fn(&[&str]) -> Result<Vec<u8>>) -> Result<Vec<u8>> {
    let gitlink_free = run(&["ls-files", "-z", "--format=%(objectmode)"])
        .is_ok_and(|modes| modes.split(|byte| *byte == 0).all(|mode| mode != b"160000"));
    if gitlink_free {
        return Ok(Vec::new());
    }
    run(&["submodule", "status", "--recursive"])
}

fn query(
    git: &Path,
    root: &Path,
    args: &[&str],
    missing: bool,
    deadline: Instant,
    global: Option<&Path>,
) -> Result<Vec<u8>> {
    query_with_input(git, root, args, missing, deadline, global, None)
}

struct Query<'a> {
    git: &'a Path,
    global: Option<&'a Path>,
    deadline: Instant,
}

impl Query<'_> {
    fn run(&self, root: &Path, args: &[&str], input: Option<&[u8]>) -> Result<Vec<u8>> {
        query_with_input(
            self.git,
            root,
            args,
            false,
            self.deadline,
            self.global,
            input,
        )
    }
}

fn query_with_input(
    git: &Path,
    root: &Path,
    args: &[&str],
    missing: bool,
    deadline: Instant,
    global: Option<&Path>,
    input: Option<&[u8]>,
) -> Result<Vec<u8>> {
    #[cfg(test)]
    COMMANDS.with_borrow_mut(|commands| commands.push(args.join(" ")));
    let mut command = git_command(git, root);
    if let Some(global) = global {
        // Git's config reader needs the plain Windows spelling, while Rust
        // keeps the canonical native path for file ownership and cleanup.
        command.env("GIT_CONFIG_GLOBAL", crate::storage::store_path_text(global));
    }
    command.args(args);
    run_query_with_input(command, args[0], missing, deadline, input)
}

fn git_command(git: &Path, root: &Path) -> Command {
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let mut command = Command::new(git);
    command
        .current_dir(root)
        .args(["--no-pager", "--no-optional-locks", "--no-replace-objects"]);
    // Command-scoped configuration also reaches Git's submodule helpers.
    // Deep checked-out children must not depend on user or repository config.
    #[cfg(windows)]
    command.args(["-c", "core.longpaths=true"]);
    for setting in [
        "core.fsmonitor=false",
        "core.checkStat=default",
        "core.trustctime=true",
        "core.ignoreStat=false",
        "core.untrackedCache=false",
        "core.safecrlf=false",
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
    command.stdin(Stdio::null());
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
    command
}

fn run_query(command: Command, name: &str, missing: bool, deadline: Instant) -> Result<Vec<u8>> {
    run_query_with_input(command, name, missing, deadline, None)
}

fn run_query_with_input(
    command: Command,
    name: &str,
    missing: bool,
    deadline: Instant,
    input: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let remaining = deadline.saturating_duration_since(crate::operation_time::now());
    if remaining.is_zero() {
        return Err(crate::task_sync::git::QueryFailure::Deadline.into());
    }
    let result = crate::task_sync::git::run_query_command(
        command,
        name,
        missing,
        input,
        remaining,
        crate::task_sync::git::QueryStderrPolicy::RequireEmpty,
    );
    before_deadline(deadline, result)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn late_query_or_postprocessing_result_refuses_and_typed_failures_survive_context() {
        use crate::{source::RefusalCode, task_sync::git::QueryFailure};
        let deadline = Instant::now()
            .checked_sub(Duration::from_millis(1))
            .unwrap();
        for result in [Ok(Vec::<u8>::new()), Ok(b"?? changed\0".to_vec())] {
            let error = before_deadline(deadline, result).unwrap_err();
            assert_eq!(refusal_code(&error), RefusalCode::Deadline);
        }
        let error = anyhow::Error::from(QueryFailure::OutputLimit)
            .context("cannot read recursive submodule status");
        assert_eq!(refusal_code(&error), RefusalCode::OutputLimit);
        let error = anyhow::Error::from(QueryFailure::Deadline).context("final Git query");
        assert_eq!(refusal_code(&error), RefusalCode::Deadline);
        let error = anyhow::Error::from(QueryFailure::Stderr {
            command: "status".to_owned(),
            message: "child path: Filename too long".to_owned(),
        })
        .context("registered input");
        assert_eq!(refusal_code(&error), RefusalCode::InputUnavailable);
        assert!(format!("{error:#}").contains("Filename too long"));
    }

    #[test]
    fn porcelain_preserves_rename_origins_and_refuses_incomplete_or_lossy_paths() {
        let entries = status_entries(b"R  new name\0old name\0?? added\0").unwrap();
        assert_eq!(entries[0].status, "R ");
        assert_eq!(entries[0].path, "new name");
        assert_eq!(entries[0].origin.as_deref(), Some("old name"));
        assert_eq!(entries[1].status, "??");
        for bytes in [
            b"?? missing terminator".as_slice(),
            b"R  new\0",
            b"?? \xff\0",
            b"zz path\0",
        ] {
            assert!(status_entries(bytes).is_err(), "{bytes:?}");
        }
    }

    #[test]
    fn a_sixty_second_budget_is_one_deadline_and_a_longer_one_refuses_before_reading() {
        use crate::source::{RefusalCode, Status};
        let scratch = crate::test_support::temp_home().unwrap();
        let root = scratch.path();
        git(root, &["init", "-q"]);
        let budget = Duration::from_millis(60_000);
        let before = Instant::now();
        let observation = observe(root, budget);
        let after = Instant::now();
        let deadline = observation
            .deadline
            .expect("an admitted budget sets the deadline");
        assert!(deadline >= before + budget && deadline <= after + budget);

        let observation = observe(root, budget + Duration::from_millis(1));
        assert_eq!(observation.status, Status::Unavailable);
        assert!(observation.deadline.is_none(), "refused before any read");
        assert_eq!(
            observation.refusal.unwrap().code,
            RefusalCode::InputUnavailable
        );
    }

    #[test]
    fn a_thirty_second_public_override_reads_under_one_sixty_second_deadline() {
        let scratch = crate::test_support::temp_home().unwrap();
        let root = scratch.path();
        git(root, &["init", "-q"]);
        let floor = Duration::from_millis(crate::source::MIN_PUBLIC_BUDGET_MS);
        let before = Instant::now();
        let observation = crate::source::inspect(root, crate::source::public_budget(30_000));
        let after = Instant::now();
        let deadline = observation
            .deadline
            .expect("a raised public budget still sets one deadline");
        assert!(deadline >= before + floor && deadline <= after + floor);
    }

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

    fn commit_all(root: &Path) {
        git(root, &["add", "-A"]);
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
    }

    /// Observe `root` and return the source and the Git subcommands it ran.
    fn observed(root: &Path) -> (crate::source::SourceId, Vec<String>) {
        COMMANDS.with_borrow_mut(Vec::clear);
        let observation = observe(root, Duration::from_secs(60));
        let source = observation
            .source_id
            .unwrap_or_else(|| panic!("{:?}", observation.refusal));
        (source, COMMANDS.with_borrow_mut(std::mem::take))
    }

    fn ran(commands: &[String], prefix: &str) -> usize {
        commands
            .iter()
            .filter(|command| command.starts_with(prefix))
            .count()
    }

    #[test]
    fn an_index_without_gitlinks_skips_the_submodule_script_clean_and_dirty() {
        let scratch = crate::test_support::temp_home().unwrap();
        let root = scratch.path();
        git(root, &["init", "-q"]);
        std::fs::write(root.join("tracked"), "checked\n").unwrap();
        commit_all(root);
        let head = String::from_utf8(git(root, &["rev-parse", "HEAD"])).unwrap();

        let (clean, commands) = observed(root);
        assert_eq!(clean.commit, head.trim());
        assert!(clean.submodules.is_empty() && clean.changes.is_empty());
        assert_eq!(ran(&commands, "submodule"), 0, "{commands:?}");
        // Location and HEAD are one read at each end, beside the tree read.
        assert!(ran(&commands, "rev-parse") <= 3, "{commands:?}");

        // A dirty capture rechecks the gitlink set, still without the script.
        std::fs::write(root.join("tracked"), "edited\n").unwrap();
        let (dirty, commands) = observed(root);
        assert_eq!(dirty.changes.len(), 1);
        assert_eq!(ran(&commands, "submodule"), 0, "{commands:?}");
    }

    #[cfg(unix)]
    #[test]
    fn a_checkout_path_holding_a_newline_is_still_observed() {
        let scratch = crate::test_support::temp_home().unwrap();
        let root = scratch.path().join("repo\nname");
        std::fs::create_dir(&root).unwrap();
        git(&root, &["init", "-q"]);
        std::fs::write(root.join("tracked"), "checked\n").unwrap();
        commit_all(&root);
        let head = String::from_utf8(git(&root, &["rev-parse", "HEAD"])).unwrap();
        let (source, _) = observed(&root);
        assert_eq!(source.commit, head.trim());
        assert!(source.submodules.is_empty() && source.changes.is_empty());
    }

    #[test]
    fn a_gitlink_runs_the_submodule_script_and_reports_the_pinned_child() {
        let scratch = crate::test_support::temp_home().unwrap();
        let source = scratch.path().join("child-source");
        let root = scratch.path().join("parent");
        for repository in [&source, &root] {
            std::fs::create_dir(repository).unwrap();
            git(repository, &["init", "-q"]);
            std::fs::write(repository.join("tracked"), "checked\n").unwrap();
            commit_all(repository);
        }
        git(
            &root,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "--",
                source.to_str().unwrap(),
                "child",
            ],
        );
        commit_all(&root);
        let pin = String::from_utf8(git(&root, &["rev-parse", "HEAD:child"])).unwrap();

        let (clean, commands) = observed(&root);
        assert_eq!(clean.submodules.len(), 1);
        assert_eq!(clean.submodules[0].path, "child");
        assert_eq!(clean.submodules[0].commit, pin.trim());
        assert_eq!(ran(&commands, "submodule status"), 1, "{commands:?}");

        // A dirty capture still rechecks the children through the script.
        std::fs::write(root.join("tracked"), "edited\n").unwrap();
        let (dirty, commands) = observed(&root);
        assert_eq!(dirty.submodules, clean.submodules);
        assert_eq!(ran(&commands, "submodule status"), 2, "{commands:?}");
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

    #[test]
    fn uninitialized_submodule_paths_keep_parenthesized_text() {
        let pin = crate::ObjectId::from_canonical_bytes(b"fixture commit").to_string();
        let pin = &pin[..40];
        for name in ["child (vendor)", " (vendor)", "child (vendor) (edition)"] {
            assert_eq!(
                submodules(format!("-{pin} {name}\n").as_bytes())
                    .unwrap_err()
                    .to_string(),
                format!("submodule '{name}': uninitialized")
            );
        }
    }

    #[test]
    fn conflicted_submodule_paths_keep_parenthesized_text() {
        let pin = crate::ObjectId::from_canonical_bytes(b"fixture commit").to_string();
        let pin = &pin[..40];
        for name in ["child (vendor)", " (vendor)", "child (vendor) (edition)"] {
            assert_eq!(
                submodules(format!("U{pin} {name}\n").as_bytes())
                    .unwrap_err()
                    .to_string(),
                format!("submodule '{name}': conflicted gitlink")
            );
        }
    }

    #[test]
    fn initialized_submodule_paths_remove_only_revision_annotation() {
        let pin = crate::ObjectId::from_canonical_bytes(b"fixture commit").to_string();
        let pin = &pin[..40];
        for name in ["child (vendor)", " (vendor)", "child (vendor) (edition)"] {
            assert_eq!(
                submodules(format!(" {pin} {name} (heads/main)\n").as_bytes()).unwrap(),
                [name]
            );
            assert_eq!(
                submodules(format!("+{pin} {name} (heads/main)\n").as_bytes())
                    .unwrap_err()
                    .to_string(),
                format!("submodule '{name}': HEAD differs from its pin")
            );
        }
    }
}
