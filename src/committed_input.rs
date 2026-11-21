//! A live, bounded read of one local committed input. This is endpoint
//! evidence, never a durable identity or a historical seal validator.

use anyhow::{Context, Result, bail, ensure};
use std::{
    collections::BTreeSet,
    path::{Path, PathBuf},
    process::{ChildStdout, Command, Stdio},
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

struct Capture {
    entries: Vec<crate::source::Entry>,
    source_id: crate::source::SourceId,
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
    let mut observed = match capture(root, deadline, &mut known_root) {
        Ok(value) => Observation {
            version: 1,
            status: if value.source_id.is_clean() {
                Status::Clean
            } else {
                Status::NotClean
            },
            root: known_root,
            source: Some(format!("git-commit:{}", value.source_id.commit)),
            tree: Some(value.source_id.tree.clone()),
            entries: value.entries,
            refusal: None,
            source_id: Some(value.source_id),
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

fn capture(root: &Path, deadline: Instant, known_root: &mut Option<String>) -> Result<Capture> {
    use crate::source::SourceId;
    #[cfg(test)]
    READS.set(READS.get() + 1);
    ensure!(
        root.is_absolute(),
        "the registered root is not an absolute local path"
    );
    let root = root
        .canonicalize()
        .context("the registered root is unavailable")?;
    let text = crate::storage::store_path_text(&root);
    ensure!(
        root.to_str().is_some() && Path::new(&text).is_absolute(),
        "unsupported worktree path"
    );
    *known_root = Some(text);
    let git = resolve_git(&root)?;
    let query = Query {
        git: &git,
        deadline,
    };
    let repository = query.for_repository(&root);
    check_toplevel(&repository, &root)?;
    let filters = repository.filter_overrides()?;
    let repository = Repository {
        filter_overrides: &filters,
        ..repository
    };
    let (commit, delta, entries) = delta::capture(&query, &repository)?;
    let tree = commit_id(&repository, &format!("{commit}^{{tree}}"))?;
    before_deadline(
        deadline,
        Ok(Capture {
            entries,
            source_id: SourceId {
                commit,
                tree,
                submodules: Vec::new(),
                changes: Vec::new(),
                git_delta: Some(delta),
            },
        }),
    )
}

fn check_toplevel(repository: &Repository<'_, '_>, expected: &Path) -> Result<()> {
    let top = path_output(repository.run(&["rev-parse", "--show-toplevel"])?)?;
    ensure!(
        top.canonicalize()? == expected.canonicalize()?,
        "unavailable Git worktree association"
    );
    Ok(())
}

fn valid_oid(value: &str) -> bool {
    value.len() == 40
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn commit_id(repository: &Repository<'_, '_>, reference: &str) -> Result<String> {
    let bytes = repository.run(&["rev-parse", "--verify", reference])?;
    let value = std::str::from_utf8(&bytes)?.trim();
    ensure!(valid_oid(value), "invalid Git SHA-1 object identity");
    Ok(value.to_owned())
}

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

fn path_output(bytes: Vec<u8>) -> Result<PathBuf> {
    let text = String::from_utf8(bytes).context("Git returned a non-UTF-8 root path")?;
    let text = text.trim_end_matches(['\r', '\n']);
    ensure!(!text.is_empty(), "Git returned no root path");
    Ok(PathBuf::from(text))
}

struct Query<'a> {
    git: &'a Path,
    deadline: Instant,
}

impl<'a> Query<'a> {
    fn for_repository<'r>(&self, root: &'r Path) -> Repository<'a, 'r> {
        Repository {
            git: self.git,
            root,
            deadline: self.deadline,
            filter_overrides: &[],
        }
    }
}

struct Repository<'a, 'r> {
    git: &'a Path,
    root: &'r Path,
    deadline: Instant,
    filter_overrides: &'r [String],
}

impl Repository<'_, '_> {
    fn filter_overrides(&self) -> Result<Vec<String>> {
        // Discover the configured driver names before Git reads the worktree.
        let mut command = git_command(self.git, self.root);
        command.args([
            "config",
            "--null",
            "--includes",
            "--name-only",
            "--get-regexp",
            "^filter\\..+\\.(clean|smudge|process|required)$",
        ]);
        let bytes = run_query(command, "config", true, self.deadline, None)?;
        let keys = std::str::from_utf8(&bytes).context("non-UTF-8 Git filter configuration")?;
        ensure!(
            keys.is_empty() || keys.ends_with('\0'),
            "truncated Git filter keys"
        );
        let mut drivers = BTreeSet::new();
        for key in keys.split_terminator('\0') {
            let (name, _) = key
                .strip_prefix("filter.")
                .and_then(|key| key.rsplit_once('.'))
                .context("invalid Git filter key")?;
            ensure!(
                !name.is_empty() && !name.contains(['=', '\r', '\n']),
                "unsupported Git filter driver name {name:?}"
            );
            drivers.insert(name);
        }
        Ok(drivers
            .iter()
            .flat_map(|name| {
                ["clean=", "smudge=", "process=", "required=true"]
                    .map(|setting| format!("filter.{name}.{setting}"))
            })
            .collect())
    }

    fn run(&self, args: &[&str]) -> Result<Vec<u8>> {
        self.run_with_input(args, false, None)
    }

    fn run_with_input(
        &self,
        args: &[&str],
        missing: bool,
        input: Option<&[u8]>,
    ) -> Result<Vec<u8>> {
        run_query(self.command(args), args[0], missing, self.deadline, input)
    }

    fn stream<T: Send + 'static>(
        &self,
        args: &[&str],
        read: impl FnOnce(ChildStdout) -> Result<T> + Send + 'static,
    ) -> Result<T> {
        let remaining = query_budget(self.deadline)?;
        let result = crate::task_sync::git::stream_query_command(
            self.command(args),
            args[0],
            remaining,
            crate::task_sync::git::QueryStderrPolicy::RequireEmpty,
            read,
        );
        before_deadline(self.deadline, result)
    }

    fn command(&self, args: &[&str]) -> Command {
        let mut command = git_command(self.git, self.root);
        for setting in self.filter_overrides {
            command.args(["-c", setting]);
        }
        command.args(args);
        command
    }
}

fn git_command(git: &Path, root: &Path) -> Command {
    let mut command = Command::new(git);
    command.current_dir(root).args([
        "--no-pager",
        "--no-optional-locks",
        "--no-replace-objects",
        "--literal-pathspecs",
    ]);
    // Apply the same controls to each checkout while leaving its line-ending
    // configuration to Git.
    #[cfg(windows)]
    command.args(["-c", "core.longpaths=true"]);
    command.args([
        "-c",
        "core.fsmonitor=false",
        "-c",
        "core.checkStat=default",
        "-c",
        "core.trustctime=true",
        "-c",
        "core.ignoreStat=false",
        "-c",
        "core.untrackedCache=false",
        "-c",
        "core.safecrlf=false",
        "-c",
        "diff.ignoreSubmodules=none",
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
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_NO_LAZY_FETCH", "1")
        .env("GIT_TRACE2", "0")
        .env("GIT_TRACE2_EVENT", "0")
        .env("GIT_TRACE2_PERF", "0");
    #[cfg(test)]
    isolate_fixture_git_config(&mut command, root);
    #[cfg(not(test))]
    {
        // Preserve Git's normal config scopes without inheriting invocation
        // overrides, repository selectors or tracing from the caller.
        for name in [
            "GIT_CONFIG_NOSYSTEM",
            "GIT_CONFIG_SYSTEM",
            "GIT_CONFIG_GLOBAL",
        ] {
            if let Some(value) = std::env::var_os(name) {
                command.env(name, value);
            }
        }
    }
    command
}

#[cfg(test)]
fn isolate_fixture_git_config(command: &mut Command, root: &Path) {
    let null = if cfg!(windows) { "NUL" } else { "/dev/null" };
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", null)
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env("XDG_CONFIG_HOME", root.join(".git").join("engram-fixture"));
}

fn run_query(
    command: Command,
    name: &str,
    missing: bool,
    deadline: Instant,
    input: Option<&[u8]>,
) -> Result<Vec<u8>> {
    let remaining = query_budget(deadline)?;
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

fn query_budget(deadline: Instant) -> Result<Duration> {
    let remaining = deadline.saturating_duration_since(crate::operation_time::now());
    if remaining.is_zero() {
        return Err(crate::task_sync::git::QueryFailure::Deadline.into());
    }
    Ok(remaining)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn git_query_rejects_partial_output_even_when_git_exits_successfully() {
        const FIXTURE: &str = "ENGRAM_SOURCE_QUERY_WARNING_FIXTURE";
        if std::env::var_os(FIXTURE).is_some() {
            println!("fixture output");
            if std::env::var_os(FIXTURE).as_deref() == Some(std::ffi::OsStr::new("partial")) {
                eprintln!("warning: could not open directory 'hidden/': Permission denied");
            }
            return;
        }
        let run = |mode| {
            let mut command = Command::new(std::env::current_exe().unwrap());
            command
                .args([
                    "--exact",
                    "committed_input::tests::git_query_rejects_partial_output_even_when_git_exits_successfully",
                    "--nocapture",
                ])
                .env(FIXTURE, mode);
            run_query(
                command,
                "status",
                false,
                Instant::now() + Duration::from_secs(10),
                None,
            )
        };
        let bytes = run("complete").unwrap();
        assert!(String::from_utf8(bytes).unwrap().contains("fixture output"));
        let error = run("partial").unwrap_err();
        assert!(error.to_string().contains("could not open directory"));
    }

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
        isolate_fixture_git_config(&mut command, root);
        let output = command.current_dir(root).args(args).output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn source_id(root: &Path) -> crate::source::SourceId {
        let observation = observe(root, crate::source::MAX_BUDGET);
        observation
            .source_id
            .unwrap_or_else(|| panic!("{:?}", observation.refusal))
    }

    fn commit_file(root: &Path, content: &str) {
        std::fs::write(root.join("tracked"), content).unwrap();
        git(root, &["add", "tracked"]);
        git(
            root,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "fixture",
            ],
        );
    }

    #[test]
    fn source_git_delta_compares_staged_worktree_and_untracked_changes() {
        let fixture = crate::test_support::temp_home().unwrap();
        let root = fixture.path();
        git(root, &["init", "-q"]);
        commit_file(root, "base\n");
        let clean = source_id(root);
        assert!(clean.is_clean());
        assert_eq!(source_id(root), clean);

        std::fs::write(root.join("tracked"), "stage A\n").unwrap();
        git(root, &["add", "tracked"]);
        std::fs::write(root.join("tracked"), "same worktree\n").unwrap();
        let first = source_id(root);
        assert!(!first.is_clean());
        std::fs::write(root.join("tracked"), "stage B\n").unwrap();
        git(root, &["add", "tracked"]);
        std::fs::write(root.join("tracked"), "same worktree\n").unwrap();
        let second = source_id(root);
        assert_eq!(first.commit, second.commit);
        assert_ne!(first, second);

        std::fs::write(root.join("tracked"), "other worktree\n").unwrap();
        let worktree = source_id(root);
        assert_ne!(second, worktree);
        std::fs::write(root.join("untracked"), "addition\n").unwrap();
        assert_ne!(worktree, source_id(root));
    }

    #[test]
    fn source_binds_head_and_tree_to_the_status_observation() {
        let fixture = crate::test_support::temp_home().unwrap();
        let root = fixture.path();
        git(root, &["init", "-q"]);
        commit_file(root, "old checkout\n");
        let old = source_id(root);
        commit_file(root, "new checkout\n");
        let new = source_id(root);
        git(root, &["checkout", "--detach", "-q", &old.commit]);

        let switched_root = root.to_path_buf();
        let switched_commit = new.commit.clone();
        crate::task_sync::git::BEFORE_QUERY.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move |query| {
                if query == "status" {
                    git(
                        &switched_root,
                        &["checkout", "--detach", "-q", &switched_commit],
                    );
                }
            }));
        });
        let observed = observe(root, crate::source::MAX_BUDGET);
        crate::task_sync::git::BEFORE_QUERY.with(|hook| *hook.borrow_mut() = None);

        assert_ne!(new, old);
        assert_eq!(observed.source_id, Some(new));
    }

    #[test]
    fn source_hashes_an_unstaged_patch_larger_than_the_metadata_bound() {
        let fixture = crate::test_support::temp_home().unwrap();
        let root = fixture.path();
        git(root, &["init", "-q"]);
        commit_file(root, "base\n");
        let clean = source_id(root);

        let mut replacement = "long replacement line\n".repeat(70_000);
        std::fs::write(root.join("tracked"), &replacement).unwrap();
        let dirty = source_id(root);
        assert!(!dirty.is_clean());
        assert_eq!(dirty.commit, clean.commit);
        assert_eq!(dirty, source_id(root));

        replacement.push_str("different ending\n");
        std::fs::write(root.join("tracked"), replacement).unwrap();
        assert_ne!(dirty, source_id(root));
    }

    #[test]
    fn source_refuses_an_unmerged_index_without_a_partial_identity() {
        let fixture = crate::test_support::temp_home().unwrap();
        let root = fixture.path();
        git(root, &["init", "-q"]);
        commit_file(root, "base\n");
        let base = source_id(root).commit;
        commit_file(root, "first side\n");
        let first = source_id(root).commit;
        git(root, &["checkout", "--detach", "-q", &base]);
        commit_file(root, "second side\n");
        let second = source_id(root).commit;
        git(root, &["read-tree", "--empty"]);
        git(root, &["read-tree", "-i", "-m", &base, &first, &second]);
        let status = git(root, &["status", "--porcelain=v2", "--untracked-files=no"]);
        assert!(
            status.starts_with(b"u UU "),
            "fixture must have an unmerged index"
        );

        let observed = observe(root, crate::source::MAX_BUDGET);
        assert_eq!(observed.status, crate::source::Status::Unavailable);
        assert!(observed.source_id.is_none());
        assert!(observed.refusal.unwrap().message.contains("unmerged"));
    }

    #[cfg(windows)]
    #[test]
    fn source_accepts_more_unstaged_paths_than_fit_on_one_windows_command_line() {
        let fixture = crate::test_support::temp_home().unwrap();
        let root = fixture.path();
        git(root, &["init", "-q"]);
        let paths: Vec<_> = (0..400)
            .map(|index| root.join(format!("{}-{index}.txt", "path".repeat(25))))
            .collect();
        for path in &paths {
            std::fs::write(path, "old\n").unwrap();
        }
        git(root, &["add", "."]);
        commit_file(root, "base\n");
        for path in &paths {
            std::fs::write(path, "new bytes\n").unwrap();
        }

        let dirty = source_id(root);
        assert!(!dirty.is_clean());
        assert_eq!(dirty, source_id(root));
    }

    #[test]
    fn source_git_delta_keeps_index_and_checkout_gitlink_changes_separate() {
        let fixture = crate::test_support::temp_home().unwrap();
        let origin = fixture.path().join("origin");
        let root = fixture.path().join("root");
        std::fs::create_dir(&origin).unwrap();
        std::fs::create_dir(&root).unwrap();
        git(&origin, &["init", "-q"]);
        commit_file(&origin, "a\n");
        let a = String::from_utf8(git(&origin, &["rev-parse", "HEAD"])).unwrap();
        commit_file(&origin, "b\n");
        let b = String::from_utf8(git(&origin, &["rev-parse", "HEAD"])).unwrap();
        git(&root, &["init", "-q"]);
        commit_file(&root, "parent\n");
        git(
            &root,
            &[
                "-c",
                "protocol.file.allow=always",
                "submodule",
                "add",
                "-q",
                origin.to_str().unwrap(),
                "child",
            ],
        );
        let child = root.join("child");
        git(&child, &["checkout", "--detach", "-q", a.trim()]);
        git(&root, &["add", ".gitmodules", "child"]);
        let new_link = source_id(&root).git_delta.unwrap().gitlinks.remove(0);
        assert_eq!(new_link.head_pin, None);
        assert_eq!(new_link.index_pin.as_deref(), Some(a.trim()));
        assert_eq!(new_link.checkout_head, new_link.index_pin);
        git(
            &root,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-qm",
                "initial gitlink",
            ],
        );
        let clean = source_id(&root);
        assert!(clean.is_clean());
        assert_eq!(
            clean.git_delta.unwrap().gitlinks,
            [] as [crate::source::Gitlink; 0]
        );

        git(
            &root,
            &[
                "update-index",
                "--cacheinfo",
                &format!("160000,{},child", b.trim()),
            ],
        );
        let staged = source_id(&root);
        assert!(!staged.is_clean());
        let link = &staged.git_delta.as_ref().unwrap().gitlinks[0];
        assert_eq!(link.head_pin.as_deref(), Some(a.trim()));
        assert_eq!(link.index_pin.as_deref(), Some(b.trim()));
        assert_eq!(link.checkout_head.as_deref(), Some(a.trim()));

        git(&child, &["checkout", "--detach", "-q", b.trim()]);
        let checked_out = source_id(&root);
        assert_ne!(checked_out, staged);
        let link = &checked_out.git_delta.as_ref().unwrap().gitlinks[0];
        assert_eq!(link.head_pin.as_deref(), Some(a.trim()));
        assert_eq!(link.index_pin.as_deref(), Some(b.trim()));
        assert_eq!(link.checkout_head.as_deref(), Some(b.trim()));

        git(&root, &["rm", "--cached", "--", "child"]);
        let removed = source_id(&root).git_delta.unwrap().gitlinks.remove(0);
        assert_eq!(removed.head_pin.as_deref(), Some(a.trim()));
        assert_eq!(removed.index_pin, None);
        assert_eq!(removed.checkout_head.as_deref(), Some(b.trim()));
        std::fs::rename(&child, fixture.path().join("saved-child")).unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;

            std::fs::create_dir(root.join("ordinary-a")).unwrap();
            std::fs::create_dir(root.join("ordinary-b")).unwrap();
            symlink("ordinary-a", &child).unwrap();
            let first = source_id(&root);
            std::fs::remove_file(&child).unwrap();
            symlink("ordinary-b", &child).unwrap();
            let second = source_id(&root);
            assert_eq!(
                first.git_delta.as_ref().unwrap().gitlinks,
                second.git_delta.as_ref().unwrap().gitlinks
            );
            assert_ne!(first, second);
            std::fs::remove_file(&child).unwrap();
        }
        std::fs::write(&child, "ordinary staged endpoint\n").unwrap();
        git(&root, &["add", "--", "child"]);
        std::fs::write(&child, "first ordinary worktree endpoint\n").unwrap();
        let first = source_id(&root);
        std::fs::write(&child, "second ordinary worktree endpoint\n").unwrap();
        assert_ne!(first, source_id(&root));
    }

    #[test]
    fn source_does_not_read_dirt_or_run_filters_inside_an_undeclared_gitlink() {
        let fixture = crate::test_support::temp_home().unwrap();
        let root = fixture.path();
        git(root, &["init", "-q"]);
        let child = root.join("child");
        std::fs::create_dir(&child).unwrap();
        git(&child, &["init", "-q"]);
        commit_file(&child, "child\n");
        git(root, &["add", "--", "child"]);
        commit_file(root, "parent\n");
        let clean = source_id(root);
        assert!(clean.is_clean());

        git(
            &child,
            &["config", "filter.fixture.clean", "echo ran > filter-ran"],
        );
        std::fs::write(child.join(".gitattributes"), "tracked filter=fixture\n").unwrap();
        std::fs::write(child.join("tracked"), "changed inside the child\n").unwrap();
        std::fs::write(child.join("new-file"), "added inside the child\n").unwrap();
        assert_eq!(source_id(root), clean);
        assert!(!child.join("filter-ran").exists());

        std::fs::rename(&child, root.join(".git/saved-child")).unwrap();
        std::fs::create_dir(&child).unwrap();
        assert_eq!(source_id(root), clean);
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
        let checked = source_id(root);
        assert!(checked.is_clean());
        std::fs::write(root.join("tracked"), "edited\n").unwrap();
        assert!(!source_id(root).is_clean());
        git(root, &["restore", "tracked"]);
        let reverted = source_id(root);
        assert_eq!(reverted.commit, checked.commit);
        assert!(reverted.is_clean());
        let tree = git(root, &["rev-parse", "HEAD^{tree}"]);
        commit();
        assert_eq!(git(root, &["rev-parse", "HEAD^{tree}"]), tree);
        assert_ne!(source_id(root).commit, checked.commit);
        assert!(
            observe(&root.join("missing"), crate::source::MAX_BUDGET)
                .refusal
                .is_some()
        );
        std::fs::create_dir(root.join("child")).unwrap();
        assert!(
            observe(&root.join("child"), crate::source::MAX_BUDGET)
                .refusal
                .is_some()
        );
    }

    #[test]
    fn active_filter_is_unavailable_without_running_the_configured_command() {
        let scratch = crate::test_support::temp_home().unwrap();
        let root = scratch.path();
        git(root, &["init", "-q"]);
        commit_file(root, "checked\n");
        std::fs::write(root.join(".gitattributes"), "tracked filter=fixture\n").unwrap();
        std::fs::write(
            root.join("tracked"),
            "changed since the committed version\n",
        )
        .unwrap();
        git(
            root,
            &["config", "filter.fixture.clean", "echo ran > filter-ran"],
        );
        let observation = observe(root, crate::source::MAX_BUDGET);
        assert!(observation.source_id.is_none());
        let refusal = observation.refusal.unwrap();
        assert_eq!(refusal.code, crate::source::RefusalCode::InputUnavailable);
        assert!(refusal.message.contains("clean filter 'fixture' failed"));
        assert!(!root.join("filter-ran").exists());
    }

    #[test]
    fn staged_gitlink_is_in_the_delta_before_its_checkout_is_declared() {
        let scratch = crate::test_support::temp_home().unwrap();
        let root = scratch.path();
        git(root, &["init", "-q"]);
        commit_file(root, "parent\n");
        let path = "child (vendor)";
        let child = root.join(path);
        std::fs::create_dir(&child).unwrap();
        git(&child, &["init", "-q"]);
        commit_file(&child, "child\n");
        let child_head = String::from_utf8(git(&child, &["rev-parse", "HEAD"])).unwrap();
        git(
            root,
            &[
                "update-index",
                "--add",
                "--cacheinfo",
                &format!("160000,{},{path}", child_head.trim()),
            ],
        );
        let undeclared = source_id(root);
        assert!(!undeclared.is_clean());
        assert!(
            undeclared
                .git_delta
                .as_ref()
                .is_some_and(|delta| delta.gitlinks.is_empty())
        );

        std::fs::write(
            root.join(".gitmodules"),
            format!("[submodule \"vendor\"]\npath = {path}\nurl = ../vendor\n"),
        )
        .unwrap();
        let mut delta = source_id(root).git_delta.unwrap();
        let link = delta.gitlinks.remove(0);
        assert_eq!(link.path, path);
        assert_eq!(link.head_pin, None);
        assert_eq!(link.index_pin.as_deref(), Some(child_head.trim()));
        assert_eq!(link.checkout_head, link.index_pin);
    }
}
