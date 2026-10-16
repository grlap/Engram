//! Bounded diagnostics; only the exclusive lock handle proves readiness.

use super::*;
use std::{fmt::Write as _, future::Future, io::Read, pin::Pin, time::SystemTime};

mod tests;

const ROLES: [&str; 6] = [
    "spawn",
    "root",
    "child",
    "grandchild",
    "observer",
    "execute",
];
const MAX_BYTES: u64 = 2048;

pub(in crate::task_sync::exchange) struct Trace {
    file: fs::File,
    started: Instant,
    stages: usize,
}

impl Trace {
    fn new(root: &Path, role: &str) -> Self {
        assert!(ROLES.contains(&role));
        Self {
            file: fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(root.join(format!("readiness-{role}.log")))
                .unwrap(),
            started: Instant::now(),
            stages: 0,
        }
    }

    pub(in crate::task_sync::exchange) fn for_command(command: &Command) -> Option<Self> {
        let root = command.get_envs().find_map(|(key, value)| {
            (key == "ENGRAM_EXCHANGE_FIXTURE_ROOT")
                .then_some(value)
                .flatten()
        })?;
        let mode = command.get_envs().find_map(|(key, value)| {
            (key == "ENGRAM_EXCHANGE_FIXTURE")
                .then_some(value)
                .flatten()
        })?;
        matches!(
            mode.to_str(),
            Some(
                "block"
                    | "root-exits"
                    | "flood"
                    | "stderr-flood"
                    | "secret-error"
                    | "fail-before-ready"
            )
        )
        .then(|| Self::new(Path::new(root), "spawn"))
    }

    pub(in crate::task_sync::exchange) fn stage(&mut self, stage: &'static str) {
        assert!(self.stages < 8 && stage.len() <= 64);
        self.stages += 1;
        let wall = SystemTime::now()
            .duration_since(SystemTime::UNIX_EPOCH)
            .unwrap();
        writeln!(
            self.file,
            "pid={} stage={stage} elapsed_us={} unix_us={}",
            std::process::id(),
            self.started.elapsed().as_micros(),
            wall.as_micros()
        )
        .unwrap();
        self.file.flush().unwrap();
        assert!(self.file.metadata().unwrap().len() <= MAX_BYTES);
    }
}

fn snapshot(root: &Path) -> String {
    let mut text = String::new();
    for role in ROLES {
        write!(text, "\n{role}: ").unwrap();
        match fs::File::open(root.join(format!("readiness-{role}.log"))) {
            Ok(file) => {
                let mut bytes = Vec::new();
                file.take(MAX_BYTES + 1).read_to_end(&mut bytes).unwrap();
                assert!(bytes.len() as u64 <= MAX_BYTES);
                text.push_str(&String::from_utf8(bytes).unwrap());
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                text.push_str("not entered");
            }
            Err(error) => write!(text, "read error: {error}").unwrap(),
        }
    }
    text
}

// The caller polls the same owned execution while waiting, so its existing
// watchdog bounds startup as well as cancellation. Native spawn, probe and
// diagnostic filesystem calls remain synchronous, with no preemption claim.
pub(super) async fn while_running(
    root: &Path,
    run: Pin<&mut impl Future<Output = Result<RunOutput, RunError>>>,
) -> Result<(), String> {
    tokio::select! {
        biased;
        result = run => {
            finished(root, &result);
            Err(format!("execution ended before readiness: {:?}; {}", result.as_ref().map(|_| ()), snapshot(root)))
        }
        result = ready(root) => result,
    }
}

async fn ready(root: &Path) -> Result<(), String> {
    let mut trace = Trace::new(root, "observer");
    trace.stage("first-ready-poll");
    loop {
        match lock_state(root, "grandchild") {
            Ok(LockState::Held) => {
                trace.stage("grandchild-held-observed");
                return Ok(());
            }
            Ok(LockState::Absent) => {}
            Ok(LockState::Released) => {
                trace.stage("lock-released-before-ready");
                return Err(format!(
                    "grandchild lock was released before readiness; {}",
                    snapshot(root)
                ));
            }
            Err(error) => {
                trace.stage("lock-probe-error");
                return Err(format!(
                    "readiness lock probe failed: {error}; {}",
                    snapshot(root)
                ));
            }
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

// A successful open is a released handle; NotFound is not-yet-created. Only
// ERROR_SHARING_VIOLATION witnesses an incompatible open handle to this file;
// it does not authenticate the holder's process identity.
pub(super) fn descendant_holds_lock(root: &Path, name: &str) -> bool {
    lock_state_or_panic(root, name) == LockState::Held
}

#[derive(Debug, PartialEq, Eq)]
enum LockState {
    Absent,
    Released,
    Held,
}

impl LockState {
    fn stage(&self) -> &'static str {
        match self {
            Self::Absent => "lock-absent",
            Self::Released => "lock-released",
            Self::Held => "lock-held",
        }
    }
}

fn lock_state_or_panic(root: &Path, name: &str) -> LockState {
    lock_state(root, name).unwrap_or_else(|error| {
        panic!(
            "readiness lock probe failed for {name}: {error}; {}",
            snapshot(root)
        )
    })
}

fn lock_state(root: &Path, name: &str) -> std::io::Result<LockState> {
    match fs::OpenOptions::new()
        .read(true)
        .write(true)
        .share_mode(0)
        .open(root.join(format!("{name}.lock")))
    {
        Err(error) if error.raw_os_error() == Some(32) => Ok(LockState::Held),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(LockState::Absent),
        Ok(_) => Ok(LockState::Released),
        Err(error) => Err(error),
    }
}

pub(super) fn finished(root: &Path, result: &Result<RunOutput, RunError>) {
    let mut trace = Trace::new(root, "execute");
    trace.stage(match result {
        Ok(_) => "execute-succeeded",
        Err(RunError::Spawn) => "execute-spawn-error",
        Err(RunError::Process) => "execute-process-error",
        Err(RunError::Git) => "execute-git-error",
        Err(RunError::Deadline) => "execute-deadline",
        Err(RunError::Cancelled) => "execute-cancelled",
        Err(_) => "execute-other-error",
    });
    // Do not print captured stdout/stderr: secret-error deliberately contains a URL.
    eprintln!(
        "fixture execution at {} finished: {:?}; {}",
        root.display(),
        result.as_ref().map(|_| ()),
        snapshot(root)
    );
}

// Descendants deliberately outlive their parent; the controller asserts death.
#[allow(clippy::zombie_processes)]
pub(super) fn process_fixture() {
    let mode = std::env::var("ENGRAM_EXCHANGE_FIXTURE").unwrap();
    let root = PathBuf::from(std::env::var_os("ENGRAM_EXCHANGE_FIXTURE_ROOT").unwrap());
    let role = match mode.as_str() {
        "child" => "child",
        "grandchild" => "grandchild",
        _ => "root",
    };
    let mut trace = Trace::new(&root, role);
    trace.stage("fixture-entry");
    trace.stage(match mode.as_str() {
        "child" => "mode-child",
        "grandchild" => "mode-grandchild",
        "block" => "mode-block",
        "root-exits" => "mode-root-exits",
        "flood" => "mode-flood",
        "stderr-flood" => "mode-stderr-flood",
        "secret-error" => "mode-secret-error",
        "fail-before-ready" => "mode-fail-before-ready",
        _ => panic!("unknown process fixture mode"),
    });
    if mode == "fail-before-ready" {
        std::process::exit(2);
    }
    if matches!(mode.as_str(), "child" | "grandchild") {
        let _locked = fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .share_mode(0)
            .open(root.join(format!("{mode}.lock")))
            .unwrap();
        trace.stage("exclusive-lock-held");
        if mode == "child" {
            trace.stage("before-grandchild-spawn");
            Fixture(root.clone())
                .command("grandchild")
                .stdin(Stdio::null())
                .spawn()
                .unwrap();
            trace.stage("after-grandchild-spawn");
        }
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    trace.stage("before-child-spawn");
    Fixture(root.clone())
        .command("child")
        .stdin(Stdio::null())
        .spawn()
        .unwrap();
    trace.stage("after-child-spawn");
    if mode == "block" {
        // Cancellation and future-drop tests observe the grandchild's actual
        // held handle in the supervisor. Its existing execution budget is the
        // only startup bound; the root has no competing readiness timer.
        loop {
            std::thread::sleep(Duration::from_secs(1));
        }
    }
    let start = Instant::now();
    let mut state = lock_state_or_panic(&root, "grandchild");
    trace.stage(state.stage());
    while state != LockState::Held {
        assert!(
            start.elapsed() < Duration::from_secs(8),
            "root readiness expired ({state:?}): {}",
            snapshot(&root)
        );
        std::thread::sleep(Duration::from_millis(5));
        state = lock_state_or_panic(&root, "grandchild");
    }
    trace.stage("grandchild-held-observed");
    match mode.as_str() {
        "root-exits" => std::process::exit(0),
        "flood" => loop {
            std::io::stdout().write_all(&[b'x'; 4096]).unwrap();
        },
        "stderr-flood" => loop {
            std::io::stderr().write_all(&[b'x'; 4096]).unwrap();
        },
        "secret-error" => {
            eprintln!("https://operator:secret-token@example.invalid/private");
            std::process::exit(2);
        }
        _ => loop {
            std::thread::sleep(Duration::from_secs(1));
        },
    }
}
