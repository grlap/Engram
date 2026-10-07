//! The doctor's on-request landing check. For each landing a completion seal
//! records, it asks the named local repository whether the commit exists and
//! lies on the remote branch the seal names. It reads only local objects and
//! remote-tracking refs, never fetches, and reports each finding by name.

use std::{
    io::Read,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

use anyhow::{Result, bail};
use engram::{HostPathPolicy, ProjectId, SqliteStore, storage::RecordedLanding};

use super::refusals::{Phase, report_error, with_build};

/// How long one git call may run before the check gives up on it.
const GIT_TIMEOUT: Duration = Duration::from_secs(30);

/// The most output the check reads from one git call; the one call it reads
/// prints a single object name.
const MAX_GIT_OUTPUT: u64 = 1024;

/// The separator git splits its path lists at, `GIT_CEILING_DIRECTORIES`
/// among them.
const PATH_LIST_SEPARATOR: u8 = if cfg!(windows) { b';' } else { b':' };

/// Options placed before every git subcommand. The repository's own
/// configuration must not run a program or reach a network on the check's
/// behalf: no pager, no file-system monitor, no hooks, no credential helper,
/// no transport, and no optional locks taken in the repository. Nothing the
/// repository stores beside its commits may rewrite ancestry: replace refs are
/// ignored, the commit-graph file is not read, and (in [`git_command`]) the
/// graft file is empty.
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
/// --local-env-vars`) except `GIT_GRAFT_FILE`, which [`git_command`] sets.
const CLEARED_GIT_ENVIRONMENT: &[&str] = &[
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_IMPLICIT_WORK_TREE",
    "GIT_COMMON_DIR",
    "GIT_PREFIX",
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

/// One read-only git call in `repository`, hardened as above; it never
/// prompts, never fetches missing objects, and its output is discarded. An
/// empty protocol allow-list overrides every `protocol.*` setting the
/// repository's configuration could make, so no transport is permitted even
/// on a git that ignores the lazy-fetch switch. Git may not climb above
/// `repository` looking for another repository, so the named directory itself
/// must be the repository. The ceiling holds only for a path already resolved
/// as [`resolve_repository`] resolves it: git compares it with the real
/// directory it starts in.
pub(crate) fn git_command(repository: &Path, args: &[&str]) -> Command {
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

/// How one git call ended.
#[derive(Debug, Eq, PartialEq)]
enum GitRun {
    Exit(i32),
    Unavailable,
    TimedOut,
    Failed(String),
}

fn spawn_failure(error: &std::io::Error) -> GitRun {
    if error.kind() == std::io::ErrorKind::NotFound {
        GitRun::Unavailable
    } else {
        GitRun::Failed(error.to_string())
    }
}

fn run(mut command: Command) -> GitRun {
    match command.spawn() {
        Ok(mut child) => wait(&mut child, GIT_TIMEOUT),
        Err(error) => spawn_failure(&error),
    }
}

/// Like [`run`], and also returns the start of what git printed.
fn run_reading(command: Command) -> (GitRun, String) {
    run_reading_within(command, GIT_TIMEOUT)
}

/// Runs `command` within `bound` and returns how it ended with the start of
/// what it printed. A separate thread reads the output, so a full pipe cannot
/// hold git; once the output bound is read the pipe closes and further writes
/// fail. The read shares the call's time bound, and nothing waits on the
/// pipe beyond it: on Windows `git.exe` is often a launcher whose own child,
/// the real git, holds the pipe and can outlive a stopped launcher. Output
/// that has not arrived by then makes the call a failure, never an empty
/// answer. A git that exits right at the bound still gets a second for its
/// output to arrive.
fn run_reading_within(mut command: Command, bound: Duration) -> (GitRun, String) {
    command.stdout(Stdio::piped());
    let started = Instant::now();
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(error) => return (spawn_failure(&error), String::new()),
    };
    let (sender, receiver) = std::sync::mpsc::channel();
    if let Some(stdout) = child.stdout.take() {
        std::thread::spawn(move || {
            let mut printed = Vec::new();
            let _ = stdout.take(MAX_GIT_OUTPUT).read_to_end(&mut printed);
            let _ = sender.send(printed);
        });
    }
    let ended = wait(&mut child, bound);
    if !matches!(ended, GitRun::Exit(_)) {
        return (ended, String::new());
    }
    let left = bound
        .saturating_sub(started.elapsed())
        .max(Duration::from_secs(1));
    match receiver.recv_timeout(left) {
        Ok(printed) => (ended, String::from_utf8_lossy(&printed).into_owned()),
        Err(_) => (
            GitRun::Failed("git's output stayed open past the time bound".into()),
            String::new(),
        ),
    }
}

/// Waits within `bound` for the process the call started. Every way out but
/// a normal exit stops and reaps that process first. Where it is a launcher
/// for the real git, as `git.exe` often is on Windows, a stopped launcher's
/// child can run on; the check does not wait for it.
fn wait(child: &mut Child, bound: Duration) -> GitRun {
    let started = Instant::now();
    loop {
        let failed = match child.try_wait() {
            Ok(Some(status)) => {
                return status.code().map_or_else(
                    || GitRun::Failed("git ended without an exit code".into()),
                    GitRun::Exit,
                );
            }
            Ok(None) if started.elapsed() >= bound => GitRun::TimedOut,
            Ok(None) => {
                std::thread::sleep(Duration::from_millis(10));
                continue;
            }
            Err(error) => GitRun::Failed(error.to_string()),
        };
        let _ = child.kill();
        let _ = child.wait();
        return failed;
    }
}

/// What the local repository says about one recorded landing.
#[derive(Debug, Eq, PartialEq)]
pub(crate) enum LandingStatus {
    /// The commit exists and lies on the named remote branch.
    Verified,
    /// The repository has no commit whose full id is the recorded one.
    CommitAbsent,
    /// The remote branch has no local remote-tracking ref to compare with.
    RemoteRefMissing,
    /// The commit exists but is not on the named remote branch.
    NotOnBranch,
    /// The stored landing fails the shape every write checks, so it names
    /// no commit or branch git should be asked about.
    Malformed(String),
    /// A git call failed or ran too long; the reason names it.
    Unverifiable(String),
}

impl LandingStatus {
    fn word(&self) -> &'static str {
        match self {
            Self::Verified => "verified",
            Self::CommitAbsent => "commit_absent",
            Self::RemoteRefMissing => "remote_ref_missing",
            Self::NotOnBranch => "not_on_branch",
            Self::Malformed(_) => "malformed",
            Self::Unverifiable(_) => "unverifiable",
        }
    }

    /// One finding, with the stored names passed through the same terminal
    /// text policy `show` applies to them.
    fn line(&self, recorded: &RecordedLanding) -> String {
        let landing = &recorded.landing;
        let remote_branch =
            engram::terminal_error_line(&format!("{}/{}", landing.remote, landing.branch));
        let subject = format!(
            "landing {}: commit {}",
            recorded.work_ref,
            engram::terminal_error_line(&landing.commit)
        );
        match self {
            Self::Verified => format!("{subject} is on {remote_branch}"),
            Self::CommitAbsent => format!("{subject} absent from this repository"),
            Self::RemoteRefMissing => format!(
                "{subject}: remote ref {remote_branch} not present locally — fetch to verify"
            ),
            Self::NotOnBranch => format!("{subject} not on {remote_branch}"),
            Self::Malformed(reason) => format!("{subject} is malformed: {reason}"),
            Self::Unverifiable(reason) => format!("{subject} could not be checked: {reason}"),
        }
    }
}

/// A landing's installed build in full, with what it is worth, on a line of
/// its own: it has no part in the Git finding above it.
fn installed_build_line(landing: &engram::domain::CompletionLanding) -> String {
    landing.installed_build.as_deref().map_or_else(
        || format!("installed build: {}", landing.installed_build_assurance()),
        |build| {
            format!(
                "installed build: {} ({})",
                engram::terminal_error_line(build),
                landing.installed_build_assurance()
            )
        },
    )
}

/// A landing's status when the repository could not be read: only the shape
/// check that needs no git runs, so a malformed landing still reads as
/// malformed and any other as unverifiable, never as verified.
fn status_without_repository(recorded: &RecordedLanding) -> LandingStatus {
    recorded
        .landing
        .validate()
        .map_or_else(LandingStatus::Malformed, |()| {
            LandingStatus::Unverifiable("the repository could not be read".into())
        })
}

fn unverifiable(run: GitRun) -> LandingStatus {
    LandingStatus::Unverifiable(match run {
        GitRun::Exit(code) => format!("git exited with {code}"),
        GitRun::Unavailable => "git is unavailable".into(),
        GitRun::TimedOut => format!("git ran longer than {} seconds", GIT_TIMEOUT.as_secs()),
        GitRun::Failed(reason) => reason,
    })
}

/// Git's answer to one question the check asks: exit 0 is yes, exit 1 is
/// `no`, and any other ending, such as a damaged repository's fatal error, is
/// no answer.
fn answer(run: GitRun, no: LandingStatus) -> Result<(), LandingStatus> {
    match run {
        GitRun::Exit(0) => Ok(()),
        GitRun::Exit(1) => Err(no),
        other => Err(unverifiable(other)),
    }
}

/// Checks one landing in `repository`: its shape first, since a stored seal
/// may have been written around validation, then the commit, then the remote
/// branch's local ref, then whether the branch contains the commit.
pub(crate) fn check_landing(repository: &Path, recorded: &RecordedLanding) -> LandingStatus {
    let landing = &recorded.landing;
    if let Err(reason) = landing.validate() {
        return LandingStatus::Malformed(reason);
    }
    let commit = format!("{}^{{commit}}", landing.commit);
    let (ran, named) = run_reading(git_command(
        repository,
        &["rev-parse", "--verify", "--quiet", &commit],
    ));
    if let Err(status) = answer(ran, LandingStatus::CommitAbsent) {
        return status;
    }
    // Git also accepts a prefix of a longer object name and peels a tag to
    // its commit, so the commit counts only when git names it by exactly the
    // recorded id.
    if named.trim_end() != landing.commit {
        return LandingStatus::CommitAbsent;
    }
    // Only that exact full ref counts: git's name guessing would otherwise
    // accept a local branch or tag of the same name. Once it exists, the
    // full name resolves to it first.
    let remote_ref = format!("refs/remotes/{}/{}", landing.remote, landing.branch);
    let ran = run(git_command(
        repository,
        &["show-ref", "--verify", "--quiet", &remote_ref],
    ));
    if let Err(status) = answer(ran, LandingStatus::RemoteRefMissing) {
        return status;
    }
    let ran = run(git_command(
        repository,
        &["merge-base", "--is-ancestor", &landing.commit, &remote_ref],
    ));
    match answer(ran, LandingStatus::NotOnBranch) {
        Ok(()) => LandingStatus::Verified,
        Err(LandingStatus::NotOnBranch) => not_on_branch_unless_shallow(repository),
        Err(status) => status,
    }
}

/// A shallow repository cuts the branch's history at its boundary, so a
/// commit that is present but lies beyond the boundary reads as off the
/// branch. There git's "no" is not an answer; a "yes" still is, because the
/// boundary hides parents and never adds them.
fn not_on_branch_unless_shallow(repository: &Path) -> LandingStatus {
    let (ran, shallow) = run_reading(git_command(
        repository,
        &["rev-parse", "--is-shallow-repository"],
    ));
    match (ran, shallow.trim_end()) {
        (GitRun::Exit(0), "false") => LandingStatus::NotOnBranch,
        (GitRun::Exit(0), "true") => LandingStatus::Unverifiable(
            "the repository is shallow, so the branch's history may be cut before the commit; deepen it to verify".into(),
        ),
        (GitRun::Exit(0), _) => {
            LandingStatus::Unverifiable("git did not say whether the repository is shallow".into())
        }
        (other, _) => unverifiable(other),
    }
}

/// `std::fs::canonicalize` gives a Windows path its verbatim form (`\\?\C:\…`
/// or `\\?\UNC\server\share\…`); the check hands git the plain form, which
/// names the same directory, for both its working directory and its ceiling.
/// A path with no plain form, such as a volume without a drive letter, has
/// none here; neither has one that is not Unicode, which UTF-8, the form git
/// for Windows works in, cannot carry. Git would recognise no ceiling for
/// either.
#[cfg(windows)]
fn plain_path(path: &Path) -> Option<PathBuf> {
    use std::path::{Component, Prefix};
    path.to_str()?;
    let mut components = path.components();
    let mut plain = match components.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::VerbatimDisk(letter) => PathBuf::from(format!("{}:", char::from(letter))),
            Prefix::VerbatimUNC(server, share) => {
                let mut unc = std::ffi::OsString::from(r"\\");
                unc.push(server);
                unc.push(r"\");
                unc.push(share);
                PathBuf::from(unc)
            }
            Prefix::Disk(_) | Prefix::UNC(..) => return Some(path.to_path_buf()),
            Prefix::Verbatim(_) | Prefix::DeviceNS(_) => return None,
        },
        _ => return Some(path.to_path_buf()),
    };
    for component in components {
        plain.push(component);
    }
    Some(plain)
}

#[cfg(not(windows))]
#[allow(
    clippy::unnecessary_wraps,
    reason = "the Windows form has paths without a plain form"
)]
fn plain_path(path: &Path) -> Option<PathBuf> {
    Some(path.to_path_buf())
}

/// The directory to run git in: `repository` with links, `..` and relative
/// parts resolved, because git bounds its search with the parent of the real
/// directory it starts in. A path git could not be bounded in is refused: one
/// with no plain form, and one whose parent holds git's path-list separator,
/// which would split it into other ceilings.
fn resolve_repository(repository: &Path) -> Result<PathBuf, String> {
    let resolved = match std::fs::canonicalize(repository) {
        Ok(resolved) => plain_path(&resolved).ok_or_else(|| {
            format!(
                "{} resolves to {}, a path git's search cannot be bounded in",
                repository.display(),
                resolved.display()
            )
        })?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Err(format!("{} is not a directory", repository.display()));
        }
        Err(error) => return Err(format!("cannot resolve {}: {error}", repository.display())),
    };
    if !resolved.is_dir() {
        return Err(format!("{} is not a directory", repository.display()));
    }
    if let Some(parent) = resolved.parent()
        && parent
            .as_os_str()
            .as_encoded_bytes()
            .contains(&PATH_LIST_SEPARATOR)
    {
        return Err(format!(
            "{} lies in a directory whose path contains '{}', git's path-list separator, so git's search cannot be bounded there",
            resolved.display(),
            char::from(PATH_LIST_SEPARATOR)
        ));
    }
    Ok(resolved)
}

/// The resolved repository, or why it cannot be checked.
fn open_repository(repository: &Path) -> Result<PathBuf, String> {
    let resolved = resolve_repository(repository)?;
    match run(git_command(&resolved, &["rev-parse", "--git-dir"])) {
        GitRun::Exit(0) => Ok(resolved),
        // Git exits 128 both for a directory that is no repository and for one
        // it refuses to open, such as one with dubious ownership.
        GitRun::Exit(code) => Err(format!(
            "git could not open {} as a repository (git exited with {code})",
            resolved.display()
        )),
        GitRun::Unavailable => Err("git is unavailable".into()),
        GitRun::TimedOut => Err(format!(
            "git ran longer than {} seconds",
            GIT_TIMEOUT.as_secs()
        )),
        GitRun::Failed(reason) => Err(reason),
    }
}

/// `engram doctor --check-landings`: checks every landing the store's seals
/// record against `repository`, printing one finding per landing. Fails when
/// any landing is not verified or the repository cannot be read.
pub(crate) fn check_landings(
    database: &Path,
    identity: Option<HostPathPolicy>,
    project_id: &ProjectId,
    repository: &Path,
    json: bool,
) -> Result<()> {
    let store = match SqliteStore::open_with_host_path_identity(database, identity) {
        Ok(store) => store,
        Err(error) => return report_error(database, project_id, &error, json, Phase::Open),
    };
    let recorded = match store.recorded_landings() {
        Ok(recorded) => recorded,
        Err(error) => {
            return report_error(database, project_id, &error, json, Phase::LandingCheck);
        }
    };
    // A repository that cannot be resolved or read is reported by name, with
    // the path as given.
    let (repository, problem) = match open_repository(repository) {
        Ok(resolved) => (resolved, None),
        Err(problem) => (
            std::path::absolute(repository).unwrap_or_else(|_| repository.to_path_buf()),
            Some(problem),
        ),
    };
    let repository = repository.as_path();
    // Every recorded landing is listed, with its installed build, whether or
    // not the repository could be read: the build does not depend on Git.
    // Without a repository, only the shape check that needs no git still runs.
    let findings: Vec<(&RecordedLanding, LandingStatus)> = recorded
        .iter()
        .map(|landing| {
            let status = match &problem {
                None => check_landing(repository, landing),
                Some(_) => status_without_repository(landing),
            };
            (landing, status)
        })
        .collect();
    let unverified = findings
        .iter()
        .filter(|(_, status)| *status != LandingStatus::Verified)
        .count();
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&with_build(serde_json::json!({
                "mode": "landing_check",
                "project_id": project_id,
                "repository": repository.display().to_string(),
                "repository_problem": problem,
                "recorded": recorded.len(),
                // Git's answer and the installed build are kept apart: the
                // build is the agent's assertion, never compared with a build,
                // least of all with the executable running this check.
                "landings": findings.iter().map(|(recorded, status)| serde_json::json!({
                    "work_ref": recorded.work_ref,
                    "commit": recorded.landing.commit,
                    "remote": recorded.landing.remote,
                    "branch": recorded.landing.branch,
                    "status": status.word(),
                    "finding": status.line(recorded),
                    "installed_build": recorded.landing.installed_build,
                    "installed_build_assurance": recorded.landing.installed_build_assurance(),
                })).collect::<Vec<_>>(),
            })))?
        );
    } else {
        println!(
            "Landing check in {}: {} landing(s) recorded",
            repository.display(),
            recorded.len()
        );
        if let Some(problem) = &problem {
            println!("Repository not checked: {problem}");
        }
        for (recorded, status) in &findings {
            println!("{}", status.line(recorded));
            println!("  {}", installed_build_line(&recorded.landing));
        }
    }
    if let Some(problem) = problem {
        bail!("landing check could not read the repository: {problem}");
    }
    if unverified > 0 {
        bail!("{unverified} recorded landing(s) not verified in this repository");
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::ffi::OsStr;

    use super::*;
    use engram::domain::{CompletionLanding, WorkId};

    #[test]
    fn every_git_call_carries_the_hardening() {
        let command = git_command(Path::new("."), &["cat-file", "-e", "HEAD"]);
        let args: Vec<&OsStr> = command.get_args().collect();
        let hardening: Vec<&OsStr> = GIT_HARDENING.iter().map(OsStr::new).collect();
        assert_eq!(args[..hardening.len()], hardening[..]);
        assert_eq!(
            args[hardening.len()..],
            [OsStr::new("cat-file"), OsStr::new("-e"), OsStr::new("HEAD")]
        );
        for required in [
            "--no-pager",
            "--no-optional-locks",
            "--no-replace-objects",
            "core.commitGraph=false",
            "core.fsmonitor=",
            "core.hooksPath=",
            "credential.helper=",
            "protocol.allow=never",
        ] {
            assert!(args.contains(&OsStr::new(required)), "{required}");
        }
        let envs: Vec<(&OsStr, Option<&OsStr>)> = command.get_envs().collect();
        assert!(envs.contains(&(OsStr::new("GIT_TERMINAL_PROMPT"), Some(OsStr::new("0")))));
        assert!(envs.contains(&(OsStr::new("GIT_NO_LAZY_FETCH"), Some(OsStr::new("1")))));
        assert!(envs.contains(&(OsStr::new("GIT_ALLOW_PROTOCOL"), Some(OsStr::new("")))));
        assert!(envs.contains(&(OsStr::new("GIT_GRAFT_FILE"), Some(OsStr::new("/dev/null")))));
        for name in CLEARED_GIT_ENVIRONMENT {
            assert!(envs.contains(&(OsStr::new(name), None)), "{name}");
        }
        assert_eq!(command.get_current_dir(), Some(Path::new(".")));
        let nested = git_command(Path::new("/scratch/repository"), &["rev-parse"]);
        assert!(
            nested
                .get_envs()
                .any(|(name, value)| name == "GIT_CEILING_DIRECTORIES"
                    && value == Some(Path::new("/scratch").as_os_str()))
        );
    }

    /// A process that exits at once while a child it started keeps the
    /// output pipe open, as the real git started by a Git for Windows
    /// launcher can.
    fn exits_while_its_child_holds_the_pipe() -> Command {
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            let mut command = Command::new("cmd");
            command
                .args(["/d", "/c"])
                .raw_arg("start /b ping -n 20 127.0.0.1 & exit 0");
            command
        }
        #[cfg(not(windows))]
        {
            let mut command = Command::new("sh");
            command.args(["-c", "sleep 20 & exit 0"]);
            command
        }
    }

    #[test]
    fn output_a_child_holds_open_ends_the_call_within_its_bound() {
        let started = Instant::now();
        let (ran, printed) = run_reading_within(
            exits_while_its_child_holds_the_pipe(),
            Duration::from_secs(2),
        );
        assert!(
            matches!(&ran, GitRun::Failed(reason)
                if reason == "git's output stayed open past the time bound"),
            "{ran:?} {printed:?}"
        );
        assert!(printed.is_empty(), "{printed:?}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn a_call_past_its_bound_is_stopped_without_waiting_for_output() {
        let mut command = Command::new(if cfg!(windows) { "ping" } else { "sleep" });
        if cfg!(windows) {
            command.args(["-n", "20", "127.0.0.1"]);
        } else {
            command.arg("20");
        }
        let started = Instant::now();
        let (ran, printed) = run_reading_within(command, Duration::from_secs(1));
        assert_eq!(ran, GitRun::TimedOut);
        assert!(printed.is_empty(), "{printed:?}");
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "{:?}",
            started.elapsed()
        );
    }

    #[test]
    fn every_repository_local_git_variable_is_cleared_or_set() {
        // The reference is git's own list, read at run time.
        let listed = std::process::Command::new("git")
            .args(["rev-parse", "--local-env-vars"])
            .output()
            .expect("run git");
        assert!(listed.status.success(), "{listed:?}");
        let command = git_command(Path::new("."), &["rev-parse"]);
        let envs: Vec<&OsStr> = command.get_envs().map(|(name, _)| name).collect();
        let names = String::from_utf8(listed.stdout).expect("utf-8");
        assert!(names.lines().any(|name| name == "GIT_SHALLOW_FILE"));
        for name in names.lines() {
            assert!(
                envs.contains(&OsStr::new(name)),
                "{name} is neither cleared nor set"
            );
        }
    }

    /// Confines fixture commands to the named directory and the test's own
    /// configuration: no system or global configuration, signing or hooks.
    fn fixture_git_command(repository: &Path, args: &[&str], mut command: Command) -> Command {
        let repository = resolve_repository(repository).expect("resolved fixture directory");
        let global = repository
            .parent()
            .expect("the repository lies in the scratch home")
            .join("empty.gitconfig");
        if !global.exists() {
            std::fs::write(&global, b"").expect("empty git configuration");
        }
        command
            .current_dir(&repository)
            .args([
                "-c",
                "user.email=landing@test",
                "-c",
                "user.name=landing",
                "-c",
                "commit.gpgsign=false",
                "-c",
                "tag.gpgsign=false",
                "-c",
                "core.hooksPath=",
                "-c",
                "init.defaultBranch=master",
            ])
            .args(args)
            .env("GIT_TERMINAL_PROMPT", "0")
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", &global)
            .env_remove("GIT_GRAFT_FILE")
            .env_remove("GIT_DEFAULT_REF_FORMAT")
            .env_remove("GIT_DEFAULT_HASH")
            .env(
                "GIT_CEILING_DIRECTORIES",
                repository.parent().expect("fixture parent"),
            );
        for name in CLEARED_GIT_ENVIRONMENT {
            command.env_remove(name);
        }
        command
    }

    fn git(repository: &Path, args: &[&str]) -> String {
        let output = fixture_git_command(repository, args, Command::new("git"))
            .output()
            .expect("run git");
        assert!(output.status.success(), "git {args:?}: {output:?}");
        String::from_utf8(output.stdout)
            .expect("utf-8")
            .trim()
            .to_owned()
    }

    #[derive(Debug, Eq, PartialEq)]
    enum SnapshotEntry {
        Directory,
        File(Vec<u8>),
        Symlink(std::ffi::OsString),
    }

    fn snapshot(directory: &Path) -> std::collections::BTreeMap<PathBuf, SnapshotEntry> {
        fn collect(
            root: &Path,
            directory: &Path,
            files: &mut std::collections::BTreeMap<PathBuf, SnapshotEntry>,
        ) {
            for entry in std::fs::read_dir(directory).unwrap_or_else(|error| {
                panic!("snapshot directory {}: {error}", directory.display())
            }) {
                let path = entry.expect("snapshot entry").path();
                let relative = path.strip_prefix(root).expect("snapshot root").to_owned();
                let kind = std::fs::symlink_metadata(&path)
                    .unwrap_or_else(|error| panic!("snapshot metadata {}: {error}", path.display()))
                    .file_type();
                if kind.is_symlink() {
                    let target = std::fs::read_link(&path).unwrap_or_else(|error| {
                        panic!("snapshot link {}: {error}", path.display())
                    });
                    files.insert(relative, SnapshotEntry::Symlink(target.into_os_string()));
                } else if kind.is_dir() {
                    files.insert(relative, SnapshotEntry::Directory);
                    collect(root, &path, files);
                } else if kind.is_file() {
                    let bytes = std::fs::read(&path).unwrap_or_else(|error| {
                        panic!("snapshot bytes {}: {error}", path.display())
                    });
                    files.insert(relative, SnapshotEntry::File(bytes));
                } else {
                    panic!("unsupported snapshot entry {}: {kind:?}", path.display());
                }
            }
        }
        let mut files = std::collections::BTreeMap::new();
        collect(directory, directory, &mut files);
        files
    }

    #[test]
    fn fixture_snapshot_preserves_bytes_and_links_without_following_targets() {
        let home = crate::test_support::temp_home().expect("scratch directory");
        let tree = home.path().join("snapshot");
        let first = home.path().join("first");
        let second = home.path().join("second");
        let missing = home.path().join("missing");
        for directory in [&tree, &first, &second, &missing] {
            std::fs::create_dir(directory).unwrap();
        }
        std::fs::create_dir(tree.join("empty")).unwrap();
        std::fs::write(tree.join("bytes"), [0, 255, 10]).unwrap();
        for target in [&first, &second] {
            std::fs::write(target.join("same"), b"equal target bytes").unwrap();
        }
        let link = tree.join("link");
        let dangling = tree.join("dangling");
        let cycle = tree.join("cycle");
        crate::test_support::make_dir_link(&first, &link);
        crate::test_support::make_dir_link(&missing, &dangling);
        crate::test_support::make_dir_link(&tree, &cycle);
        std::fs::remove_dir(&missing).unwrap();
        assert_eq!(
            std::fs::metadata(&dangling).unwrap_err().kind(),
            std::io::ErrorKind::NotFound
        );
        // Windows refuses a directory handle before reading its missing target.
        assert_eq!(
            std::fs::read(&dangling).unwrap_err().kind(),
            if cfg!(windows) {
                std::io::ErrorKind::PermissionDenied
            } else {
                std::io::ErrorKind::NotFound
            }
        );
        let before = snapshot(&tree);
        assert_eq!(before.len(), 5);
        assert_eq!(before[Path::new("empty")], SnapshotEntry::Directory);
        assert_eq!(
            before[Path::new("bytes")],
            SnapshotEntry::File(vec![0, 255, 10])
        );
        for name in ["link", "dangling", "cycle"] {
            assert_eq!(
                before[Path::new(name)],
                SnapshotEntry::Symlink(
                    std::fs::read_link(tree.join(name))
                        .unwrap()
                        .into_os_string()
                )
            );
        }
        std::fs::write(first.join("same"), b"changed outside snapshot").unwrap();
        assert_eq!(snapshot(&tree), before);
        std::fs::write(first.join("same"), b"equal target bytes").unwrap();
        crate::test_support::remove_dir_link(&link);
        crate::test_support::make_dir_link(&second, &link);
        assert_ne!(snapshot(&tree), before);
        for entry in [&link, &dangling, &cycle] {
            crate::test_support::remove_dir_link(entry);
        }
        let ordinary = snapshot(&tree);
        std::fs::write(tree.join("bytes"), [0, 254, 10]).unwrap();
        assert_ne!(snapshot(&tree), ordinary);
    }

    #[test]
    fn fixture_snapshot_link_equality_preserves_target_spelling() {
        let original = PathBuf::from("../first");
        for spelling in [".././first", "../first/"] {
            let changed = PathBuf::from(spelling);
            assert_eq!(original, changed);
            assert_ne!(
                SnapshotEntry::Symlink(original.clone().into_os_string()),
                SnapshotEntry::Symlink(changed.into_os_string())
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn fixture_snapshot_preserves_raw_file_link_targets() {
        let home = crate::test_support::temp_home().expect("scratch directory");
        let tree = home.path().join("snapshot");
        std::fs::create_dir(&tree).unwrap();
        for name in ["first", "second"] {
            std::fs::write(home.path().join(name), b"equal target bytes").unwrap();
        }
        let link = tree.join("link");
        std::os::unix::fs::symlink("../first", &link).unwrap();
        let dangling = tree.join("dangling");
        std::os::unix::fs::symlink("../missing", &dangling).unwrap();
        let before = snapshot(&tree);
        assert_eq!(before.len(), 2);
        assert_eq!(
            before[Path::new("link")],
            SnapshotEntry::Symlink("../first".into())
        );
        assert_eq!(
            before[Path::new("dangling")],
            SnapshotEntry::Symlink("../missing".into())
        );
        for spelling in [".././first", "../first/"] {
            std::fs::remove_file(&link).unwrap();
            std::os::unix::fs::symlink(spelling, &link).unwrap();
            assert_ne!(snapshot(&tree), before);
            assert_eq!(
                snapshot(&tree)[Path::new("link")],
                SnapshotEntry::Symlink(spelling.into())
            );
        }
        std::fs::remove_file(&link).unwrap();
        std::os::unix::fs::symlink("../second", &link).unwrap();
        assert_ne!(snapshot(&tree), before);
        std::fs::remove_file(&link).unwrap();
        std::fs::remove_file(&dangling).unwrap();
    }

    #[test]
    fn fixture_commands_ignore_inherited_repository_routing() {
        let home = crate::test_support::temp_home().expect("scratch directory");
        let victim = home.path().join("victim");
        let fixture = home.path().join("fixture");
        let linked = home.path().join("linked");
        std::fs::create_dir_all(&victim).expect("victim directory");
        std::fs::create_dir_all(&fixture).expect("fixture directory");
        git(&victim, &["init", "-q", "."]);
        std::fs::write(victim.join("untouched.txt"), b"victim").expect("victim content");
        git(&victim, &["add", "untouched.txt"]);
        git(&victim, &["commit", "-q", "-m", "victim"]);
        let before = snapshot(&victim);
        let victim_git = victim.join(".git");
        let run = |repository: &Path, args: &[&str]| {
            let mut command = Command::new("git");
            for name in CLEARED_GIT_ENVIRONMENT {
                command.env(name, "hostile");
            }
            command
                .env("GIT_DIR", &victim_git)
                .env("GIT_WORK_TREE", &victim)
                .env("GIT_COMMON_DIR", &victim_git)
                .env("GIT_INDEX_FILE", victim_git.join("index"))
                .env("GIT_OBJECT_DIRECTORY", victim_git.join("objects"))
                .env(
                    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
                    victim_git.join("objects"),
                )
                .env("GIT_SHALLOW_FILE", victim_git.join("shallow"))
                .env("GIT_GRAFT_FILE", victim_git.join("info/grafts"))
                .env("GIT_CEILING_DIRECTORIES", &victim)
                .env("GIT_CONFIG_COUNT", "1")
                .env("GIT_CONFIG_KEY_0", "core.bare")
                .env("GIT_CONFIG_VALUE_0", "true")
                .env("GIT_DEFAULT_REF_FORMAT", "invalid")
                .env("GIT_DEFAULT_HASH", "invalid");
            let mut command = fixture_git_command(repository, args, command);
            let envs: Vec<_> = command.get_envs().collect();
            for name in [
                "GIT_DIR",
                "GIT_WORK_TREE",
                "GIT_INDEX_FILE",
                "GIT_CONFIG_COUNT",
                "GIT_GRAFT_FILE",
            ] {
                assert!(envs.contains(&(OsStr::new(name), None)), "{name}");
            }
            let resolved = resolve_repository(repository).expect("resolved fixture");
            assert!(envs.contains(&(
                OsStr::new("GIT_CEILING_DIRECTORIES"),
                Some(resolved.parent().expect("fixture parent").as_os_str()),
            )));
            command.output().expect("run fixture git")
        };
        for (repository, args) in [
            (&fixture, vec!["init", "-q", "."]),
            (&fixture, vec!["add", "content.txt"]),
            (&fixture, vec!["commit", "-q", "-m", "fixture"]),
            (
                &fixture,
                vec!["worktree", "add", "-q", "-b", "linked", "../linked"],
            ),
        ] {
            if args[0] == "add" {
                std::fs::write(fixture.join("content.txt"), b"fixture").expect("fixture content");
            }
            let output = run(repository, &args);
            assert!(output.status.success(), "git {args:?}: {output:?}");
        }
        let top = git(&fixture, &["rev-parse", "--show-toplevel"]);
        assert_eq!(
            std::fs::canonicalize(top).unwrap(),
            std::fs::canonicalize(&fixture).unwrap()
        );
        assert_eq!(git(&fixture, &["show", "HEAD:content.txt"]), "fixture");
        assert_eq!(git(&fixture, &["ls-files"]), "content.txt");
        assert!(linked.join(".git").is_file());
        std::fs::write(linked.join("linked.txt"), b"linked").expect("linked content");
        for args in [
            vec!["add", "linked.txt"],
            vec!["commit", "-q", "-m", "linked"],
        ] {
            let output = run(&linked, &args);
            assert!(output.status.success(), "git {args:?}: {output:?}");
        }
        assert_eq!(git(&linked, &["show", "HEAD:linked.txt"]), "linked");
        let plain = fixture.join("plain");
        std::fs::create_dir(&plain).expect("plain directory");
        assert_eq!(
            run(&plain, &["rev-parse", "--show-toplevel"]).status.code(),
            Some(128)
        );
        assert_eq!(
            snapshot(&victim),
            before,
            "victim tree and Git bytes changed"
        );
    }

    fn recorded(commit: &str, remote: &str, branch: &str) -> RecordedLanding {
        RecordedLanding {
            work_id: WorkId(uuid::Uuid::nil()),
            work_ref: "w-000000000000".into(),
            landing: CompletionLanding {
                commit: commit.into(),
                remote: remote.into(),
                branch: branch.into(),
                pushed_at: chrono::Utc::now(),
                installed_build: None,
            },
        }
    }

    #[test]
    fn a_landing_is_checked_against_local_objects_and_remote_refs() {
        let home = crate::test_support::temp_home().expect("scratch directory");
        let repository = home.path().join("landed");
        std::fs::create_dir_all(&repository).expect("repository directory");
        git(&repository, &["init", "-q", "."]);
        git(
            &repository,
            &["commit", "-q", "--allow-empty", "-m", "landed"],
        );
        let landed = git(&repository, &["rev-parse", "HEAD"]);
        git(
            &repository,
            &["update-ref", "refs/remotes/origin/master", &landed],
        );
        git(
            &repository,
            &["commit", "-q", "--allow-empty", "-m", "later"],
        );
        let later = git(&repository, &["rev-parse", "HEAD"]);

        let status = |commit: &str, remote: &str, branch: &str| {
            check_landing(&repository, &recorded(commit, remote, branch))
        };
        assert_eq!(status(&landed, "origin", "master"), LandingStatus::Verified);
        assert_eq!(
            status(&later, "origin", "master"),
            LandingStatus::NotOnBranch
        );
        assert_eq!(
            status(&landed, "origin", "release"),
            LandingStatus::RemoteRefMissing
        );
        let absent = "f".repeat(landed.len());
        assert_eq!(
            status(&absent, "origin", "master"),
            LandingStatus::CommitAbsent
        );
        // A SHA-256 id names no object in this SHA-1 repository.
        assert_eq!(
            status(&"f".repeat(64), "origin", "master"),
            LandingStatus::CommitAbsent
        );
        // An annotated tag's id peels to the landed commit, yet it is not
        // that commit's id.
        git(
            &repository,
            &["tag", "-a", "-m", "tagged", "tagged", &landed],
        );
        let tag = git(&repository, &["rev-parse", "tagged"]);
        assert_ne!(tag, landed);
        assert_eq!(
            status(&tag, "origin", "master"),
            LandingStatus::CommitAbsent
        );
        // A stored landing written around validation is not handed to git:
        // `HEAD` resolves in this repository, yet it is no landed commit.
        assert!(matches!(
            status("HEAD", "origin", "master"),
            LandingStatus::Malformed(reason) if reason.contains("landing commit")
        ));
        assert!(matches!(
            status(&landed, "origin", "master~1"),
            LandingStatus::Malformed(reason) if reason.contains("landing branch")
        ));
        assert_eq!(
            open_repository(&repository),
            Ok(
                plain_path(&std::fs::canonicalize(&repository).expect("resolved repository"))
                    .expect("a plain form")
            )
        );
        let absent_line = LandingStatus::CommitAbsent.line(&recorded(&absent, "origin", "master"));
        assert!(absent_line.contains(&absent) && absent_line.contains("absent"));
        let missing_line =
            LandingStatus::RemoteRefMissing.line(&recorded(&landed, "origin", "release"));
        assert!(
            missing_line
                .contains("remote ref origin/release not present locally — fetch to verify")
        );
        let off_line = LandingStatus::NotOnBranch.line(&recorded(&later, "origin", "master"));
        assert!(off_line.contains("not on origin/master"));

        // A replace ref cannot put a commit on the branch: replacing the
        // branch tip with a child of the later commit would otherwise make the
        // later commit its ancestor.
        git(
            &repository,
            &["commit", "-q", "--allow-empty", "-m", "child"],
        );
        let child = git(&repository, &["rev-parse", "HEAD"]);
        git(
            &repository,
            &["update-ref", &format!("refs/replace/{landed}"), &child],
        );
        assert_eq!(
            status(&later, "origin", "master"),
            LandingStatus::NotOnBranch
        );
        // Nor can a graft: giving the branch tip the later commit as its
        // parent would make that commit its ancestor.
        let info = repository.join(".git").join("info");
        std::fs::create_dir_all(&info).expect("info directory");
        std::fs::write(info.join("grafts"), format!("{landed} {later}\n")).expect("graft file");
        assert_eq!(
            status(&later, "origin", "master"),
            LandingStatus::NotOnBranch
        );
        // Only the exact remote-tracking ref counts: a local branch whose name
        // spells it is not that ref.
        git(
            &repository,
            &[
                "update-ref",
                "refs/heads/refs/remotes/origin/release",
                &landed,
            ],
        );
        assert_eq!(
            status(&landed, "origin", "release"),
            LandingStatus::RemoteRefMissing
        );

        // The scratch directory lies inside this repository's own worktree,
        // yet a plain directory there is not a repository: git may not climb
        // above the directory the check names, however that is spelled.
        let plain = home.path().join("plain");
        std::fs::create_dir_all(plain.join("sub")).expect("plain directory");
        let refused = |path: &Path, reason: &str| {
            let problem = open_repository(path).expect_err("the directory is refused");
            assert!(problem.contains(reason), "{}: {problem}", path.display());
        };
        refused(&plain, "as a repository (git exited with 128)");
        refused(
            &plain.join("sub").join(".."),
            "as a repository (git exited with 128)",
        );
        let links = home.path().join("links");
        std::fs::create_dir_all(&links).expect("links directory");
        let link = links.join("plain");
        crate::test_support::make_dir_link(&plain, &link);
        refused(&link, "as a repository (git exited with 128)");
        crate::test_support::remove_dir_link(&link);
        refused(&home.path().join("missing"), "is not a directory");
        // Git would split this parent's path into two ceilings.
        let separated = home
            .path()
            .join(format!("a{}b", char::from(PATH_LIST_SEPARATOR)))
            .join("landed");
        std::fs::create_dir_all(&separated).expect("separated directory");
        refused(&separated, "path-list separator");
        // Asked about a directory that is no repository, git fails, and a
        // failure is no answer about the commit.
        assert!(matches!(
            check_landing(&plain, &recorded(&landed, "origin", "master")),
            LandingStatus::Unverifiable(reason) if reason == "git exited with 128"
        ));
    }

    #[test]
    fn a_damaged_repository_leaves_a_landing_unverifiable() {
        let home = crate::test_support::temp_home().expect("scratch directory");
        let repository = home.path().join("damaged");
        std::fs::create_dir_all(&repository).expect("repository directory");
        git(&repository, &["init", "-q", "."]);
        git(
            &repository,
            &["commit", "-q", "--allow-empty", "-m", "landed"],
        );
        let landed = git(&repository, &["rev-parse", "HEAD"]);
        git(
            &repository,
            &["update-ref", "refs/remotes/origin/master", &landed],
        );
        git(&repository, &["pack-refs", "--all"]);
        let check = || check_landing(&repository, &recorded(&landed, "origin", "master"));
        assert_eq!(check(), LandingStatus::Verified);
        let packed = repository.join(".git").join("packed-refs");
        let mut refs = std::fs::read_to_string(&packed).expect("packed refs");
        refs.push_str("not a packed ref\n");
        std::fs::write(&packed, refs).expect("damaged packed refs");
        assert!(matches!(
            check(),
            LandingStatus::Unverifiable(reason) if reason == "git exited with 128"
        ));
    }

    #[test]
    fn a_shallow_boundary_leaves_an_off_branch_answer_unverifiable() {
        let home = crate::test_support::temp_home().expect("scratch directory");
        let repository = home.path().join("shallow");
        std::fs::create_dir_all(&repository).expect("repository directory");
        git(&repository, &["init", "-q", "."]);
        let mut commits = Vec::new();
        for message in ["landed", "boundary", "tip"] {
            git(
                &repository,
                &["commit", "-q", "--allow-empty", "-m", message],
            );
            commits.push(git(&repository, &["rev-parse", "HEAD"]));
        }
        let [landed, boundary, tip] = &commits[..] else {
            panic!("three commits")
        };
        git(
            &repository,
            &["update-ref", "refs/remotes/origin/master", tip],
        );
        git(
            &repository,
            &["commit", "-q", "--allow-empty", "-m", "off the branch"],
        );
        let off = git(&repository, &["rev-parse", "HEAD"]);
        let status =
            |commit: &str| check_landing(&repository, &recorded(commit, "origin", "master"));
        assert_eq!(status(landed), LandingStatus::Verified);
        // A shallow boundary at the middle commit hides its parent, the landed
        // commit, which is still present: git answers "not an ancestor".
        std::fs::write(
            repository.join(".git").join("shallow"),
            format!("{boundary}\n"),
        )
        .expect("shallow boundary");
        assert!(matches!(
            status(landed),
            LandingStatus::Unverifiable(reason) if reason.contains("the repository is shallow")
        ));
        // Within the fetched history a "yes" still stands. Every "no" in a
        // shallow repository is unverifiable, even for a commit after the
        // branch tip, since the check cannot tell which history is missing.
        assert_eq!(status(boundary), LandingStatus::Verified);
        assert!(matches!(
            status(&off),
            LandingStatus::Unverifiable(reason) if reason.contains("the repository is shallow")
        ));
    }

    #[cfg(windows)]
    #[test]
    fn a_verbatim_path_is_given_to_git_in_its_plain_form() {
        use std::os::windows::ffi::OsStringExt;
        let plain = |path: &str| plain_path(Path::new(path));
        assert_eq!(
            plain(r"\\?\C:\work\repository"),
            Some(PathBuf::from(r"C:\work\repository"))
        );
        assert_eq!(
            plain(r"\\?\UNC\server\share\repository"),
            Some(PathBuf::from(r"\\server\share\repository"))
        );
        assert_eq!(
            plain(r"C:\work\repository"),
            Some(PathBuf::from(r"C:\work\repository"))
        );
        // A volume without a drive letter has no plain form.
        assert_eq!(plain(r"\\?\Volume{0}\repository"), None);
        // Nor has a path that is not Unicode: here a lone surrogate.
        let mut wide: Vec<u16> = r"\\?\C:\work\".encode_utf16().collect();
        wide.push(0xD800);
        assert_eq!(
            plain_path(Path::new(&std::ffi::OsString::from_wide(&wide))),
            None
        );
    }

    #[test]
    fn a_landing_in_a_sha256_repository_names_the_full_id() {
        let home = crate::test_support::temp_home().expect("scratch directory");
        let repository = home.path().join("sha256");
        std::fs::create_dir_all(&repository).expect("repository directory");
        git(&repository, &["init", "-q", "--object-format=sha256", "."]);
        git(
            &repository,
            &["commit", "-q", "--allow-empty", "-m", "landed"],
        );
        let landed = git(&repository, &["rev-parse", "HEAD"]);
        assert_eq!(landed.len(), 64);
        git(
            &repository,
            &["update-ref", "refs/remotes/origin/master", &landed],
        );
        let status =
            |commit: &str| check_landing(&repository, &recorded(commit, "origin", "master"));
        assert_eq!(status(&landed), LandingStatus::Verified);
        // Git resolves a unique prefix, but 40 characters are not this
        // repository's id of the commit.
        assert_eq!(status(&landed[..40]), LandingStatus::CommitAbsent);
    }

    // A stored installed build is shown through the terminal text policy: a
    // value holding controls, as a seal written around validation might, can
    // neither forge a line nor reach the terminal raw. A legitimate build is
    // shown in full, and an absent one says so.
    #[test]
    fn the_installed_build_line_passes_the_terminal_text_policy() {
        let mut landing = recorded(&"a".repeat(40), "origin", "master").landing;
        assert_eq!(
            installed_build_line(&landing),
            "installed build: no installed build recorded"
        );
        let build = "0123456789abcdef".repeat(4);
        landing.installed_build = Some(build.clone());
        assert_eq!(
            installed_build_line(&landing),
            format!("installed build: {build} (asserted, unchecked)")
        );
        landing.installed_build = Some(format!("{build}\u{1b}[31m\u{7}\nnext:\u{202e}fake"));
        let line = installed_build_line(&landing);
        for raw in ['\u{1b}', '\u{7}', '\n', '\r', '\u{202e}'] {
            assert!(!line.contains(raw), "{line:?}");
        }
        assert!(
            line.starts_with(&format!("installed build: {build}")),
            "{line}"
        );
        assert!(line.ends_with(" (asserted, unchecked)"), "{line}");
        assert_eq!(line.lines().count(), 1);
    }

    // Without a repository, a landing whose stored shape fails validation is
    // still named malformed, never verified or merely unverifiable, and its
    // installed build stays on its own line.
    #[test]
    fn without_a_repository_a_malformed_landing_reads_malformed() {
        let mut malformed = recorded("not-a-commit", "origin", "master");
        malformed.landing.installed_build = Some("0123456789abcdef".repeat(4));
        let status = status_without_repository(&malformed);
        assert!(matches!(status, LandingStatus::Malformed(_)), "{status:?}");
        assert_eq!(status.word(), "malformed");
        assert!(installed_build_line(&malformed.landing).contains(&"0123456789abcdef".repeat(4)));
        let valid = recorded(&"a".repeat(40), "origin", "master");
        assert_eq!(
            status_without_repository(&valid),
            LandingStatus::Unverifiable("the repository could not be read".into())
        );
    }
}
