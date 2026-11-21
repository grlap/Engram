//! A pre-created OS thread owns each child independently of Tokio's runtime.
//! The unreaped leader reserves its group id throughout cleanup. Unproved
//! cleanup retains that reservation in a bounded registry and returns failure.

mod members;

use super::super::RunError;
use process_wrap::tokio::{ChildWrapper, CommandWrap, ProcessSession, ResetSigmask};
use rustix::process::{Pid, Signal, WaitId, WaitIdOptions, kill_process_group, waitid};
use std::{
    any::Any,
    io,
    process::{Command, ExitStatus, Stdio},
    sync::{
        Arc, Condvar, Mutex, MutexGuard, OnceLock,
        atomic::{AtomicBool, AtomicI32, Ordering},
    },
    time::{Duration, Instant},
};
use tokio::process::{ChildStderr, ChildStdout};

const POLL: Duration = Duration::from_millis(5);
const MAX_PENDING: usize = 64;
static PENDING: OnceLock<Mutex<Vec<Arc<Control>>>> = OnceLock::new();

#[cfg(test)]
pub(in super::super) fn no_live_members(group: i32) -> bool {
    u32::try_from(group)
        .ok()
        .and_then(|group| members::snapshot(group).ok())
        .is_some_and(|members| members.iter().all(|member| member.terminal))
}

#[cfg(test)]
pub(in super::super) fn fail_for(group: i32, observation: bool) {
    let controllers = lock(pending());
    let control = controllers
        .iter()
        .find(|c| c.group.load(Ordering::Acquire) == group)
        .expect("owned fixture controller");
    if observation {
        control
            .observe_failure
            .store(rustix::io::Errno::INVAL.raw_os_error(), Ordering::Release);
    } else {
        control
            .signal_failure
            .store(rustix::io::Errno::PERM.raw_os_error(), Ordering::Release);
    }
    control.wake.notify_all();
}

#[cfg(test)]
std::thread_local! {
    pub(in super::super) static OBSERVE_FAILURE: std::cell::Cell<Option<rustix::io::Errno>> = const { std::cell::Cell::new(None) };
    pub(in super::super) static SIGNAL_FAILURE: std::cell::Cell<Option<rustix::io::Errno>> = const { std::cell::Cell::new(None) };
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    // Poisoning must never discard the only child owner.
    mutex
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}
fn pending() -> &'static Mutex<Vec<Arc<Control>>> {
    PENDING.get_or_init(Mutex::default)
}

struct Owned {
    root: Box<dyn ChildWrapper>,
    group: Option<Pid>,
}
struct Control {
    owner: Mutex<Option<Owned>>,
    prepared: AtomicBool,
    stop: AtomicBool,
    timed_out: AtomicBool,
    wake: Condvar,
    result: Mutex<Option<Result<ExitStatus, RunError>>>,
    deadline: Instant,
    operation_deadline: Instant,
    observe_failure: AtomicI32,
    signal_failure: AtomicI32,
    group: AtomicI32,
}

impl Control {
    fn failure(
        &self,
        group: Option<Pid>,
        phase: &'static str,
        error: Option<&io::Error>,
        pids: &[u32],
    ) -> RunError {
        let group = group.map(Pid::as_raw_pid).or_else(|| {
            let known = self.group.load(Ordering::Acquire);
            (known != 0).then_some(known)
        });
        let errno = error.and_then(io::Error::raw_os_error);
        let fallback = group.and_then(|pid| u32::try_from(pid).ok());
        let pids = if pids.is_empty() {
            fallback.as_slice()
        } else {
            pids
        };
        // Membership errors contain only our generated checks or kernel error
        // descriptions. Bound and sanitize that explanation; never command text.
        let reason: String = if phase == "member_enumeration" {
            error
                .map(ToString::to_string)
                .unwrap_or_default()
                .chars()
                .take(160)
                .map(|c| {
                    if c.is_ascii_alphanumeric() || matches!(c, ' ' | '_' | '-' | '.' | ':' | '=') {
                        c
                    } else {
                        '_'
                    }
                })
                .collect()
        } else {
            String::new()
        };
        RunError::Cleanup {
            detail: format!(
                "group={group:?} check={phase} errno={errno:?} reason={reason} remaining_or_unproven={:?} total={} reservation=retained",
                &pids[..pids.len().min(32)],
                pids.len()
            ),
            interrupted: None,
        }
    }
    fn publish(&self, result: Result<ExitStatus, RunError>) {
        *lock(&self.result) = Some(result);
        self.wake.notify_all();
    }
    fn pause(&self) {
        let state = lock(&self.result);
        let delay = POLL.min(self.deadline.saturating_duration_since(Instant::now()));
        drop(
            self.wake
                .wait_timeout(state, delay)
                .unwrap_or_else(std::sync::PoisonError::into_inner),
        );
    }
    fn observe(&self, group: Pid) -> io::Result<bool> {
        let injected = self.observe_failure.swap(0, Ordering::AcqRel);
        if injected != 0 {
            return Err(io::Error::from_raw_os_error(injected));
        }
        waitid(
            WaitId::Pid(group),
            WaitIdOptions::EXITED | WaitIdOptions::NOHANG | WaitIdOptions::NOWAIT,
        )
        .map(|status| status.is_some())
        .map_err(Into::into)
    }
    fn signal(&self, group: Pid) -> io::Result<()> {
        let injected = self.signal_failure.swap(0, Ordering::AcqRel);
        if injected != 0 {
            return Err(io::Error::from_raw_os_error(injected));
        }
        kill_process_group(group, Signal::KILL).map_err(Into::into)
    }
    fn run(&self, owned: &mut Owned) -> Result<ExitStatus, RunError> {
        let group = owned
            .group
            .ok_or_else(|| self.failure(None, "root_identity", None, &[]))?;
        if !(&*owned.root as &dyn Any).is::<tokio::process::Child>() {
            return Err(self.failure(Some(group), "root_wrapper", None, &[]));
        }
        let group_id = u32::try_from(group.as_raw_pid())
            .map_err(|_| self.failure(Some(group), "group_identity", None, &[]))?;
        let mut terminal_before = Vec::new();
        let mut remaining = vec![group_id];
        let mut ending = false;
        loop {
            // No other Engram code reaps this child. A foreign reaper or
            // SIGCHLD auto-reaping violates that ownership precondition.
            let exited = self
                .observe(group)
                .map_err(|e| self.failure(Some(group), "root_observation", Some(&e), &remaining))?;
            if Instant::now() >= self.operation_deadline
                && !exited
                && !self.stop.load(Ordering::Acquire)
            {
                self.timed_out.store(true, Ordering::Release);
            }
            ending |= exited
                || self.stop.load(Ordering::Acquire)
                || Instant::now() >= self.operation_deadline;
            if ending {
                let signalled = self.signal(group);
                let snapshot = members::snapshot(group_id).map_err(|e| {
                    self.failure(Some(group), "member_enumeration", Some(&e), &remaining)
                })?;
                if !snapshot
                    .iter()
                    .any(|m| m.pid == group_id && (!exited || m.terminal))
                {
                    return Err(self.failure(
                        Some(group),
                        "root_layout_or_state",
                        None,
                        &[group_id],
                    ));
                }
                remaining = snapshot
                    .iter()
                    .filter(|m| !m.terminal)
                    .map(|m| m.pid)
                    .collect();
                // EPERM is ambiguous on macOS (including zombie-only groups).
                // Only an independently complete terminal snapshot admits it.
                if let Err(error) = signalled {
                    let benign = error.raw_os_error()
                        == Some(rustix::io::Errno::SRCH.raw_os_error())
                        || cfg!(target_os = "macos")
                            && remaining.is_empty()
                            && error.raw_os_error() == Some(rustix::io::Errno::PERM.raw_os_error());
                    if !benign {
                        return Err(self.failure(
                            Some(group),
                            "group_signal",
                            Some(&error),
                            &remaining,
                        ));
                    }
                }
                // Newly published identities force another pass. This is a
                // bounded verifier, not a universal guarantee about forks whose
                // publication the kernel has not yet exposed.
                if exited
                    && remaining.is_empty()
                    && snapshot.iter().all(|m| terminal_before.contains(m))
                {
                    if Instant::now() >= self.deadline {
                        return Err(self.failure(
                            Some(group),
                            "cleanup_deadline",
                            None,
                            &remaining,
                        ));
                    }
                    return owned
                        .root
                        .try_wait()
                        .map_err(|e| self.failure(Some(group), "root_reap", Some(&e), &[group_id]))?
                        .ok_or_else(|| {
                            self.failure(Some(group), "root_reap_unproven", None, &[group_id])
                        });
                }
                terminal_before = snapshot.into_iter().filter(|m| m.terminal).collect();
            }
            if Instant::now() >= self.deadline {
                return Err(self.failure(Some(group), "cleanup_deadline", None, &remaining));
            }
            self.pause();
        }
    }
}

pub(super) struct Tree {
    control: Arc<Control>,
    stdout: Option<ChildStdout>,
    stderr: Option<ChildStderr>,
}
impl Tree {
    pub(super) fn spawn(
        mut command: Command,
        started: Instant,
        allowance: Duration,
    ) -> Result<Self, RunError> {
        if !cfg!(any(target_os = "macos", target_os = "linux")) {
            return Err(RunError::UnsupportedPlatform);
        }
        let deadline = started
            .checked_add(allowance)
            .ok_or(RunError::InvalidLimits)?;
        let reserve = (allowance / 4).min(Duration::from_secs(1));
        let operation_deadline = deadline
            .checked_sub(reserve)
            .ok_or(RunError::InvalidLimits)?;
        let control = Arc::new(Control {
            owner: Mutex::new(None),
            prepared: AtomicBool::new(false),
            stop: AtomicBool::new(false),
            timed_out: AtomicBool::new(false),
            wake: Condvar::new(),
            result: Mutex::new(None),
            deadline,
            operation_deadline,
            observe_failure: AtomicI32::new(0),
            signal_failure: AtomicI32::new(0),
            group: AtomicI32::new(0),
        });
        {
            let mut registry = lock(pending());
            if registry.len() >= MAX_PENDING {
                return Err(control.failure(None, "pending_cleanup_capacity", None, &[]));
            }
            registry.push(control.clone());
        }
        let worker = control.clone();
        // Creating a thread can fail, so it must happen BEFORE Git exists.
        if std::thread::Builder::new()
            .name("engram-git-cleanup".into())
            .spawn(move || {
                while !worker.prepared.load(Ordering::Acquire) {
                    worker.pause();
                }
                let mut custody = lock(&worker.owner);
                let Some(owned) = custody.as_mut() else {
                    return;
                };
                // The owner remains outside this unwind boundary and in the
                // registry on every failure, including panic and deadline.
                let outcome =
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| worker.run(owned)))
                        .unwrap_or_else(|_| {
                            Err(worker.failure(owned.group, "cleanup_worker_panic", None, &[]))
                        });
                if outcome.is_ok() {
                    custody.take();
                    lock(pending()).retain(|entry| !Arc::ptr_eq(entry, &worker));
                }
                drop(custody);
                worker.publish(outcome);
            })
            .is_err()
        {
            lock(pending()).retain(|entry| !Arc::ptr_eq(entry, &control));
            return Err(RunError::Spawn);
        }
        command
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        let mut wrapped = CommandWrap::from(tokio::process::Command::from(command));
        wrapped.wrap(ProcessSession).wrap(ResetSigmask);
        let Ok(mut root) = wrapped.spawn() else {
            control.prepared.store(true, Ordering::Release);
            control.wake.notify_all();
            lock(pending()).retain(|entry| !Arc::ptr_eq(entry, &control));
            return Err(RunError::Spawn);
        };
        // The wrapper's group wait must not reap members behind our back.
        for _ in 0..4 {
            if (&*root as &dyn Any).is::<tokio::process::Child>() {
                break;
            }
            root = root.into_inner();
        }
        let group = root
            .id()
            .and_then(|id| i32::try_from(id).ok())
            .and_then(Pid::from_raw);
        let stdout = root.stdout().take();
        let stderr = root.stderr().take();
        if let Some(group) = group {
            control.group.store(group.as_raw_pid(), Ordering::Release);
        }
        *lock(&control.owner) = Some(Owned { root, group });
        control.prepared.store(true, Ordering::Release);
        control.wake.notify_all();
        Ok(Self {
            control,
            stdout,
            stderr,
        })
    }
    pub(super) fn stdout(&mut self) -> Option<ChildStdout> {
        self.stdout.take()
    }
    pub(super) fn stderr(&mut self) -> Option<ChildStderr> {
        self.stderr.take()
    }
    pub(super) fn root_exited(&mut self) -> Result<Option<ExitStatus>, RunError> {
        #[cfg(test)]
        {
            if let Some(error) = OBSERVE_FAILURE.with(std::cell::Cell::take) {
                self.control
                    .observe_failure
                    .store(error.raw_os_error(), Ordering::Release);
            }
            if let Some(error) = SIGNAL_FAILURE.with(std::cell::Cell::take) {
                self.control
                    .signal_failure
                    .store(error.raw_os_error(), Ordering::Release);
            }
        }
        let result = lock(&self.control.result).clone().transpose()?;
        if result.is_some() && self.control.timed_out.load(Ordering::Acquire) {
            return Err(RunError::Deadline);
        }
        Ok(result)
    }
    pub(super) async fn finish(
        &mut self,
        interrupted: Result<(), RunError>,
    ) -> Result<(), RunError> {
        self.control.stop.store(true, Ordering::Release);
        self.control.wake.notify_all();
        loop {
            if let Some(result) = lock(&self.control.result).clone() {
                return match result {
                    Ok(_) => interrupted,
                    Err(RunError::Cleanup { detail, .. }) => Err(RunError::Cleanup {
                        detail,
                        interrupted: interrupted.err().map(Box::new),
                    }),
                    Err(error) => Err(error),
                };
            }
            if Instant::now() >= self.control.deadline {
                let mut failure =
                    self.control
                        .failure(None, "cleanup_worker_completion_deadline", None, &[]);
                if let RunError::Cleanup {
                    interrupted: cause, ..
                } = &mut failure
                {
                    *cause = interrupted.err().map(Box::new);
                }
                return Err(failure);
            }
            tokio::time::sleep(
                POLL.min(
                    self.control
                        .deadline
                        .saturating_duration_since(Instant::now()),
                ),
            )
            .await;
        }
    }
}
impl Drop for Tree {
    fn drop(&mut self) {
        self.control.stop.store(true, Ordering::Release);
        self.control.wake.notify_all();
    }
}

/// Runs after the CLI runtime is gone. Work is proportional to pending
/// controllers, bounded by `MAX_PENDING` and each original absolute deadline.
pub(in super::super) fn drain() -> Result<(), RunError> {
    let controllers = lock(pending()).clone();
    let mut failure = None;
    for controller in controllers {
        controller.stop.store(true, Ordering::Release);
        controller.wake.notify_all();
        let mut result = lock(&controller.result);
        while result.is_none() && Instant::now() < controller.deadline {
            let remaining = controller
                .deadline
                .saturating_duration_since(Instant::now());
            result = controller
                .wake
                .wait_timeout(result, remaining)
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .0;
        }
        match result.as_ref() {
            Some(Ok(_)) => {}
            Some(Err(error)) => {
                failure.get_or_insert_with(|| error.clone());
            }
            None => {
                failure.get_or_insert_with(|| {
                    controller.failure(None, "cleanup_worker_completion", None, &[])
                });
            }
        }
    }
    failure.map_or(Ok(()), Err)
}
