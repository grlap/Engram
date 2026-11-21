//! The Unix runner owns the process group Git leads in its new session: every
//! process still in that group ends on every exit path of the launch, and a
//! process that leaves the group is bounded by the deadline but not owned.
//!
//! The test binary re-enters itself as the root, child and grandchild of one
//! launch. Each holds an exclusive `flock` on its own file for its whole life,
//! so the lock is free exactly when that process has ended. Every case observes
//! each lock held before its trigger and released after it.

mod policy;

use super::super::{Cancellation, RunError, RunLimits, command, execute, process};
use nix::fcntl::{Flock, FlockArg};
use std::{
    fs,
    future::Future,
    io::Write,
    path::{Path, PathBuf},
    pin::Pin,
    process::{Command, Stdio},
    time::{Duration, Instant},
};

/// Ends a stuck fixture. It says nothing about speed: a loaded host can take
/// far longer to start real processes than an idle one.
const HANG_GUARD: Duration = Duration::from_secs(60);
const FIXTURE: &str = "task_sync::exchange::tests::unix::process_fixture";

struct Fixture(PathBuf);

impl Fixture {
    fn new() -> Self {
        let path = std::env::current_dir()
            .unwrap()
            .join("target/tmp")
            .join(format!("exchange-unix-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self(path)
    }

    fn command(&self, mode: &str) -> Command {
        fixture_command(&self.0, mode)
    }

    fn lock(&self, role: &str) -> PathBuf {
        self.0.join(format!("{role}.lock"))
    }

    /// The role's process holds its lock right now.
    fn held(&self, role: &str) -> bool {
        let path = self.lock(role);
        if !self.0.join(format!("{role}.ready")).exists() {
            return false;
        }
        let file = fs::OpenOptions::new().read(true).open(path).unwrap();
        Flock::lock(file, FlockArg::LockExclusiveNonblock).is_err()
    }

    fn assert_released(&self, roles: &[&str]) {
        for role in roles {
            assert!(
                self.0.join(format!("{role}.ready")).exists(),
                "fixture {role} never started"
            );
            let start = Instant::now();
            loop {
                let file = fs::OpenOptions::new()
                    .read(true)
                    .open(self.lock(role))
                    .unwrap();
                if Flock::lock(file, FlockArg::LockExclusiveNonblock).is_ok() {
                    break;
                }
                assert!(start.elapsed() < HANG_GUARD, "{role} survived its session");
                std::thread::sleep(Duration::from_millis(5));
            }
        }
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.0);
    }
}

fn fixture_command(root: &Path, mode: &str) -> Command {
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .args(["--exact", FIXTURE, "--ignored", "--nocapture"])
        .env("ENGRAM_EXCHANGE_FIXTURE", mode)
        .env("ENGRAM_EXCHANGE_FIXTURE_ROOT", root);
    command
}

/// Drive the run until every named role holds its lock; the run must not end first.
async fn until_held<F: Future>(fixture: &Fixture, roles: &[&str], mut run: Pin<&mut F>)
where
    F::Output: std::fmt::Debug,
{
    let start = Instant::now();
    loop {
        if roles.iter().all(|role| fixture.held(role)) {
            return;
        }
        assert!(start.elapsed() < HANG_GUARD, "fixture never held {roles:?}");
        tokio::select! {
            output = run.as_mut() => panic!("run ended before {roles:?} held: {output:?}"),
            () = tokio::time::sleep(Duration::from_millis(5)) => {}
        }
    }
}

fn limits() -> RunLimits {
    RunLimits {
        deadline: HANG_GUARD,
        stdout_bytes: 4096,
        stderr_bytes: 4096,
    }
}

const TREE: [&str; 3] = ["root", "child", "grandchild"];

#[tokio::test]
async fn task_exchange_unix_cancellation_ends_every_process_in_the_session() {
    let fixture = Fixture::new();
    let cancel = Cancellation::default();
    let run = execute(fixture.command("block"), limits(), &cancel);
    tokio::pin!(run);
    until_held(&fixture, &TREE, run.as_mut()).await;
    cancel.cancel();
    assert_eq!(run.await.unwrap_err(), RunError::Cancelled);
    fixture.assert_released(&TREE);
}

#[tokio::test]
async fn task_exchange_unix_deadline_and_dropped_future_end_the_session() {
    // The deadline falls once the whole tree holds, at any host speed.
    let fixture = Fixture::new();
    let held = |_: &[u8], _: &[u8]| TREE.iter().all(|role| fixture.held(role));
    let output = process::execute_diagnostic(
        fixture.command("block"),
        limits(),
        &Cancellation::default(),
        Some(&held),
    )
    .await;
    assert_eq!(output.result.unwrap_err(), RunError::Deadline);
    // What was captured before the deadline stays within its bounds.
    assert!(output.stdout.len() <= limits().stdout_bytes);
    assert!(output.stderr.len() <= limits().stderr_bytes);
    fixture.assert_released(&TREE);

    // The clock's own expiry, on a root that never ends.
    let fixture = Fixture::new();
    let short = RunLimits {
        deadline: Duration::from_millis(200),
        ..limits()
    };
    let result = execute(fixture.command("block"), short, &Cancellation::default()).await;
    match result.unwrap_err() {
        RunError::Deadline => {}
        RunError::Cleanup { interrupted, .. } => {
            assert_eq!(interrupted.as_deref(), Some(&RunError::Deadline));
        }
        other => panic!("expected deadline or unproved deadline cleanup, got {other:?}"),
    }
    // Whatever part of the tree had started by then has ended.
    let started: Vec<&str> = TREE
        .into_iter()
        .filter(|role| fixture.0.join(format!("{role}.ready")).exists())
        .collect();
    fixture.assert_released(&started);

    // A dropped future ends the session too.
    let fixture = Fixture::new();
    {
        let cancel = Cancellation::default();
        let run = execute(fixture.command("block"), limits(), &cancel);
        tokio::pin!(run);
        until_held(&fixture, &TREE, run.as_mut()).await;
    }
    fixture.assert_released(&TREE);

    let missing = Command::new(fixture.0.join("absent-program"));
    assert_eq!(
        execute(missing, limits(), &Cancellation::default())
            .await
            .unwrap_err(),
        RunError::Spawn
    );
}

#[tokio::test]
async fn task_exchange_unix_root_exit_and_output_failure_end_the_helpers() {
    for mode in ["root-exits", "flood", "stderr-flood", "fails"] {
        let fixture = Fixture::new();
        let result = execute(fixture.command(mode), limits(), &Cancellation::default()).await;
        match mode {
            // The root's helpers still hold both pipes; ending them lets the
            // drain finish instead of waiting for the deadline.
            "root-exits" => {
                result.unwrap();
            }
            "flood" | "stderr-flood" => assert_eq!(result.unwrap_err(), RunError::OutputLimit),
            _ => assert_eq!(result.unwrap_err(), RunError::Git),
        }
        fixture.assert_released(&TREE);
    }
}

/// Ends an escapee the runner does not own, even when an assertion fails first.
struct Escapee<'a>(&'a Fixture);

impl Drop for Escapee<'_> {
    fn drop(&mut self) {
        if let Some(pid) = read_ids(&self.0.0, "escapee.pid")
            .first()
            .copied()
            .and_then(rustix::process::Pid::from_raw)
        {
            let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
        }
    }
}

/// The runner has reaped the fixture's root: this test process, its parent,
/// no longer has it as a child. Observing does not reap it.
fn root_reaped(fixture: &Fixture) -> bool {
    read_ids(&fixture.0, "root.pid")
        .first()
        .copied()
        .and_then(rustix::process::Pid::from_raw)
        .is_some_and(|root| {
            rustix::process::waitid(
                rustix::process::WaitId::Pid(root),
                rustix::process::WaitIdOptions::EXITED
                    | rustix::process::WaitIdOptions::NOHANG
                    | rustix::process::WaitIdOptions::NOWAIT,
            )
            .err()
                == Some(rustix::io::Errno::CHILD)
        })
}

fn read_ids(root: &Path, name: &str) -> Vec<i32> {
    fs::read_to_string(root.join(name))
        .map(|text| {
            text.split_whitespace()
                .map(|id| id.parse().unwrap())
                .collect()
        })
        .unwrap_or_default()
}

/// A process that leaves Git's process group, by starting a session of its
/// own or by moving to another group within the same session, leaves Engram's
/// ownership. It still holds the pipes, so the result is bounded by the
/// deadline, but the process itself is not ended; the test ends it.
#[tokio::test]
async fn task_exchange_unix_a_process_that_leaves_the_group_is_bounded_not_owned() {
    for (mode, own_session) in [("escape", true), ("group-escape", false)] {
        let fixture = Fixture::new();
        let escapee = Escapee(&fixture);
        // The deadline falls only once the escapee holds its lock and the runner
        // has reaped the root, at any host speed, so the run is still draining
        // the pipes the escapee holds and only the deadline ends it.
        let held = |_: &[u8], _: &[u8]| fixture.held("escapee") && root_reaped(&fixture);
        let output = process::execute_diagnostic(
            fixture.command(mode),
            limits(),
            &Cancellation::default(),
            Some(&held),
        )
        .await;
        assert_eq!(output.result.unwrap_err(), RunError::Deadline, "{mode}");
        fixture.assert_released(&["root"]);
        assert!(
            fixture.held("escapee"),
            "{mode}: the escapee was not expected to end"
        );
        let root = read_ids(&fixture.0, "root.pid")[0];
        let pid = read_ids(&fixture.0, "escapee.pid")[0];
        let ids = read_ids(&fixture.0, "escapee.ids");
        let [session, group] = ids[..] else {
            panic!("{mode}: the escapee recorded its session and group")
        };
        assert_eq!(group, pid, "{mode}: the escapee leads its own group");
        if own_session {
            assert_eq!(session, pid, "{mode}: the escapee leads its own session");
        } else {
            assert_eq!(session, root, "{mode}: the escapee stayed in Git's session");
        }
        drop(escapee);
        fixture.assert_released(&["escapee"]);
    }
}

/// Ends a group the runner stopped owning, even when an assertion fails first.
/// It signals only while a member still holds its lock, so a surviving member
/// keeps the group id reserved.
struct Group<'a>(&'a Fixture);

impl Drop for Group<'_> {
    fn drop(&mut self) {
        if !(self.0.held("child") || self.0.held("grandchild")) {
            return;
        }
        if let Some(group) = read_ids(&self.0.0, "root.pid")
            .first()
            .copied()
            .and_then(rustix::process::Pid::from_raw)
        {
            let _ = rustix::process::kill_process_group(group, rustix::process::Signal::KILL);
        }
    }
}

/// A failed observation of the root, other than `ECHILD`, leaves the group id
/// unproven as Engram's: the run fails and the group is never signalled, so
/// all its members outlive the run, including the root. No numeric-pid kill
/// fallback may signal a root whose ownership could not be established.
#[tokio::test]
async fn task_exchange_unix_a_failed_observation_never_signals_the_group() {
    let fixture = Fixture::new();
    let group = Group(&fixture);
    let cancel = Cancellation::default();
    let run = execute(fixture.command("block"), limits(), &cancel);
    tokio::pin!(run);
    until_held(&fixture, &TREE, run.as_mut()).await;
    process::fail_for(read_ids(&fixture.0, "root.pid")[0], true);
    let error = run.await.unwrap_err();
    assert!(
        matches!(&error, RunError::Cleanup { detail, .. } if detail.contains("root_observation")),
        "{error}"
    );
    assert!(
        TREE.iter().all(|role| fixture.held(role)),
        "a group whose observation failed was signalled"
    );
    drop(group);
    fixture.assert_released(&TREE);
}

/// A successful root exit cannot hide a denied cleanup signal. The runner's
/// reservation is retained until the fixture owner ends the failed group.
#[tokio::test]
async fn task_exchange_unix_group_signal_failure_refuses_success() {
    let fixture = Fixture::new();
    let group = Group(&fixture);
    let cancel = Cancellation::default();
    let run = execute(fixture.command("controlled-exit"), limits(), &cancel);
    tokio::pin!(run);
    until_held(&fixture, &TREE, run.as_mut()).await;
    process::fail_for(read_ids(&fixture.0, "root.pid")[0], false);
    fs::write(fixture.0.join("exit.ready"), b"").unwrap();
    let error = run.await.unwrap_err();
    assert!(
        matches!(&error, RunError::Cleanup { detail, .. } if detail.contains("group_signal")),
        "{error}"
    );
    drop(group);
    fixture.assert_released(&TREE);
}

#[test]
fn task_exchange_unix_drop_cleanup_outlives_the_runtime() {
    let fixture = Fixture::new();
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap();
    runtime.block_on(async {
        let cancel = Cancellation::default();
        let run = execute(fixture.command("block"), limits(), &cancel);
        tokio::pin!(run);
        until_held(&fixture, &TREE, run.as_mut()).await;
        // Future and pipes drop here, then its entire runtime drops below.
    });
    drop(runtime);
    fixture.assert_released(&TREE);
    assert!(process::no_live_members(
        read_ids(&fixture.0, "root.pid")[0]
    ));
}

#[tokio::test]
async fn task_exchange_unix_cleanup_verifies_a_forking_child() {
    let fixture = Fixture::new();
    let cancel = Cancellation::default();
    let run = execute(fixture.command("fork-storm"), limits(), &cancel);
    tokio::pin!(run);
    until_held(&fixture, &TREE, run.as_mut()).await;
    while !fixture.0.join("forked.ready").exists() {
        tokio::select! {
            result = run.as_mut() => panic!("forker ended early: {result:?}"),
            () = tokio::time::sleep(Duration::from_millis(5)) => {}
        }
    }
    cancel.cancel();
    assert_eq!(run.await.unwrap_err(), RunError::Cancelled);
    let roles: Vec<String> = fs::read_dir(&fixture.0)
        .unwrap()
        .map(Result::unwrap)
        .filter(|entry| entry.path().with_extension("ready").exists())
        .filter_map(|entry| {
            entry
                .file_name()
                .to_str()?
                .strip_suffix(".lock")
                .map(str::to_owned)
        })
        .collect();
    fixture.assert_released(&roles.iter().map(String::as_str).collect::<Vec<_>>());
    assert!(process::no_live_members(
        read_ids(&fixture.0, "root.pid")[0]
    ));
}

#[tokio::test]
async fn task_exchange_unix_real_git_runs_and_reports_its_status() {
    // A repository of its own, with its own home and global configuration, so
    // neither the enclosing checkout nor the host's Git settings decide it.
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.0.join("xdg")).unwrap();
    fs::write(fixture.0.join("global.config"), b"").unwrap();
    let mut version = Command::new("git");
    version.arg("--version");
    policy::isolate(&mut version, &fixture.0);
    let output = execute(version, limits(), &Cancellation::default())
        .await
        .unwrap();
    assert!(output.stdout.starts_with(b"git version "));
    let mut init = command::local(&fixture.0, &["init", "-q"]);
    policy::isolate(&mut init, &fixture.0);
    execute(init, limits(), &Cancellation::default())
        .await
        .unwrap();
    let mut failing = command::local(&fixture.0, &["rev-parse", "--verify", "refs/heads/absent"]);
    policy::isolate(&mut failing, &fixture.0);
    assert_eq!(
        execute(failing, limits(), &Cancellation::default())
            .await
            .unwrap_err(),
        RunError::Git
    );
}

#[test]
#[ignore = "subprocess entry; exercised by the owned Unix exchange tests"]
fn process_fixture() {
    let root = PathBuf::from(std::env::var_os("ENGRAM_EXCHANGE_FIXTURE_ROOT").unwrap());
    let mode = std::env::var("ENGRAM_EXCHANGE_FIXTURE").unwrap();
    match mode.as_str() {
        "storm-leaf" => {
            let role = format!("leaf-{}", std::process::id());
            let _lock = hold(&root, &role);
            std::thread::sleep(Duration::from_millis(2));
        }
        "forker" => {
            let _lock = hold(&root, "child");
            #[expect(
                clippy::zombie_processes,
                reason = "the runner kills this owned fixture group"
            )]
            let _grandchild = fixture_command(&root, "grandchild").spawn().unwrap();
            wait_ready(&root, "grandchild");
            loop {
                fixture_command(&root, "storm-leaf")
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap()
                    .wait()
                    .unwrap();
                fs::write(root.join("forked.ready"), b"").unwrap();
            }
        }
        "grandchild" => {
            let _lock = hold(&root, "grandchild");
            block();
        }
        "child" => {
            let _lock = hold(&root, "child");
            // The grandchild never ends on its own, so neither does this wait.
            let _ = fixture_command(&root, "grandchild").spawn().unwrap().wait();
            block();
        }
        "escapee" | "group-escapee" => {
            if mode == "escapee" {
                nix::unistd::setsid().unwrap();
            } else {
                let own = nix::unistd::Pid::from_raw(0);
                nix::unistd::setpgid(own, own).unwrap();
            }
            fs::write(root.join("escapee.pid"), std::process::id().to_string()).unwrap();
            let ids = format!(
                "{} {}",
                nix::unistd::getsid(None).unwrap(),
                nix::unistd::getpgrp()
            );
            fs::write(root.join("escapee.ids"), ids).unwrap();
            let _lock = hold(&root, "escapee");
            block();
        }
        "escape" | "group-escape" => {
            let _lock = hold(&root, "root");
            fs::write(root.join("root.pid"), std::process::id().to_string()).unwrap();
            let escapee = if mode == "escape" {
                "escapee"
            } else {
                "group-escapee"
            };
            #[allow(
                clippy::zombie_processes,
                reason = "the escapee outlives the root by design; the test ends it"
            )]
            let _escapee = fixture_command(&root, escapee).spawn().unwrap();
            wait_ready(&root, "escapee");
            std::process::exit(0);
        }
        mode => {
            let _lock = hold(&root, "root");
            fs::write(root.join("root.pid"), std::process::id().to_string()).unwrap();
            let mut child = fixture_command(
                &root,
                if mode == "fork-storm" {
                    "forker"
                } else {
                    "child"
                },
            );
            if mode == "controlled-exit" {
                // Cleanup must still be confirmed when no descendant retains
                // a pipe: draining output alone cannot establish termination.
                child.stdout(Stdio::null()).stderr(Stdio::null());
            }
            #[expect(
                clippy::zombie_processes,
                reason = "the root exits or blocks while its helpers still run; the runner ends them"
            )]
            let _child = child.spawn().unwrap();
            wait_ready(&root, "grandchild");
            match mode {
                "block" | "fork-storm" => block(),
                "root-exits" => std::process::exit(0),
                "controlled-exit" => {
                    wait_ready(&root, "exit");
                    std::process::exit(0);
                }
                "fails" => std::process::exit(1),
                "flood" => flood(&mut std::io::stdout()),
                "stderr-flood" => flood(&mut std::io::stderr()),
                _ => panic!("unknown Unix exchange fixture mode {mode}"),
            }
        }
    }
}

/// Hold this role's exclusive lock for the life of the process, then mark it ready.
fn hold(root: &Path, role: &str) -> Flock<fs::File> {
    let file = fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(root.join(format!("{role}.lock")))
        .unwrap();
    let lock = Flock::lock(file, FlockArg::LockExclusive)
        .map_err(|(_, errno)| errno)
        .unwrap();
    fs::write(root.join(format!("{role}.ready")), b"").unwrap();
    lock
}

fn wait_ready(root: &Path, role: &str) {
    let start = Instant::now();
    while !root.join(format!("{role}.ready")).exists() {
        assert!(start.elapsed() < HANG_GUARD, "fixture {role} never started");
        std::thread::sleep(Duration::from_millis(5));
    }
}

fn block() -> ! {
    loop {
        std::thread::sleep(Duration::from_secs(1));
    }
}

fn flood(pipe: &mut impl Write) -> ! {
    let bytes = vec![b'x'; 64 * 1024];
    loop {
        if pipe.write_all(&bytes).is_err() {
            block();
        }
    }
}
