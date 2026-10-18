//! Explicit HTTPS Git execution. This module grants no commit or push authority.
//!
//! Engram launches Git directly. Git may use its own shell to invoke an admitted
//! credential helper; caller text is never a shell command. SSH is unsupported.
//! Windows children start suspended, join a kill-on-close job, then resume. A
//! hard parent exit before assignment can leave an inert suspended child: this
//! is not atomic confinement. After assignment, dropping the job kills its tree.

mod command;
#[cfg(all(test, windows))]
#[path = "tests/windows/diagnostics.rs"]
mod fixture_diagnostics;
#[cfg(windows)]
mod process;
/// Confined process control exists only on Windows. Elsewhere every launch is
/// refused before spawn, behind the same awaited interface.
#[cfg(not(windows))]
mod process {
    use super::{Cancellation, RunError, RunLimits, RunOutput};

    pub(super) fn execute(
        command: std::process::Command,
        _limits: RunLimits,
        _cancel: &Cancellation,
        _allow_missing_config: bool,
    ) -> std::future::Ready<Result<RunOutput, RunError>> {
        drop(command);
        std::future::ready(Err(RunError::UnsupportedPlatform))
    }
}
mod target;
#[cfg(test)]
mod tests;

// Single-thread Tokio tests observe the actual spawn boundary and inject an
// earlier service start without sleeps or a production clock API.
#[cfg(test)]
pub(crate) mod test_support {
    std::thread_local! {
        pub(crate) static LAUNCHES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
        pub(crate) static SERVICE_STARTED: std::cell::Cell<Option<std::time::Instant>> = const { std::cell::Cell::new(None) };
    }
}

pub use target::{ExchangeTarget, TargetSelection};

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

/// Caller cancellation is separate from a Git result or publication receipt.
#[derive(Clone, Debug, Default)]
pub struct Cancellation(Arc<AtomicBool>);

impl Cancellation {
    pub fn cancel(&self) {
        self.0.store(true, Ordering::Release);
    }

    pub fn is_cancelled(&self) -> bool {
        self.0.load(Ordering::Acquire)
    }
}

/// Per-request ceilings. They do not establish a transfer or repository quota.
#[derive(Clone, Copy, Debug)]
pub struct RunLimits {
    pub deadline: Duration,
    pub stdout_bytes: usize,
    pub stderr_bytes: usize,
}

impl Default for RunLimits {
    fn default() -> Self {
        Self {
            deadline: Duration::from_secs(30),
            stdout_bytes: 1024 * 1024,
            stderr_bytes: 64 * 1024,
        }
    }
}

impl RunLimits {
    fn validate(self) -> Result<(), RunError> {
        if self.deadline.is_zero()
            || self.deadline > Duration::from_secs(120)
            || self.stdout_bytes > 1024 * 1024
            || self.stderr_bytes > 1024 * 1024
        {
            return Err(RunError::InvalidLimits);
        }
        Ok(())
    }
}

/// Only these Git operations can reach the explicit network runner.
#[derive(Clone, Debug)]
pub enum ExchangeOperation {
    Observe,
    Fetch {
        commit: String,
        retention_ref: String,
    },
    Push {
        commit: String,
    },
}

/// Bounded Git stdout is protocol data; stderr is counted and never returned.
pub struct RunOutput {
    pub stdout: Vec<u8>,
    pub stderr_bytes: usize,
}

impl std::fmt::Debug for RunOutput {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("RunOutput")
            .field("stdout_bytes", &self.stdout.len())
            .field("stderr_bytes", &self.stderr_bytes)
            .finish()
    }
}

/// Refusals deliberately contain no URL, helper output or credential material.
#[derive(Clone, Debug, Eq, PartialEq, thiserror::Error)]
pub enum RunError {
    #[error("task exchange process control is supported only on Windows")]
    UnsupportedPlatform,
    #[error(
        "task exchange requires one direct HTTPS URL without userinfo, query, fragment or unsafe characters; use an ASCII unreserved path, '/' and percent encoding; SSH and other transports are unsupported"
    )]
    UnsupportedTarget,
    #[error(
        "task exchange requires manager, manager-core or wincred without helper arguments; remove unsupported helper configuration explicitly"
    )]
    UnsupportedHelper,
    #[error(
        "task exchange refuses credential.<redacted URL scope>.helper from {origin}; move an admitted helper to unscoped credential.helper or remove the scoped entry"
    )]
    UnsupportedScopedHelper { origin: String },
    #[error(
        "task exchange requires Git >= 2.31; found {found}; install a supported Git before using this workflow"
    )]
    UnsupportedGitVersion { found: String },
    #[error(
        "task exchange does not support Git URL rewrite configuration; choose a direct configured HTTPS destination"
    )]
    UnsupportedRewrite,
    #[error(
        "task exchange does not support configured bundle fetch sources or default push options; remove them explicitly for this workflow"
    )]
    UnsupportedConfiguration,
    #[error("task exchange target configuration changed; capture a new target")]
    TargetChanged,
    #[error("task exchange requires an ordinary repository and a canonical selected branch ref")]
    InvalidSelection,
    #[error("task exchange operation or retained commit/ref is invalid")]
    InvalidOperation,
    #[error("task exchange process limits exceed supported bounds")]
    InvalidLimits,
    #[error("task exchange was cancelled; no success or publication is implied")]
    Cancelled,
    #[error("task exchange deadline expired; a remote result may be unknown")]
    Deadline,
    #[error("task exchange output exceeded its bound; a remote result may be unknown")]
    OutputLimit,
    #[error("task exchange could not start its confined Git process")]
    Spawn,
    #[error("task exchange process or pipe failed; a remote result may be unknown")]
    Process,
    #[error("task exchange Git command failed; no success or publication is implied")]
    Git,
}

/// Execute a previously captured target after rechecking its effective configuration.
/// Opt-in and action authority belong to the service/calling workflow, not this handle.
///
/// # Errors
/// Refuses drift, unsupported operations, cancellation and bounded process failures.
pub async fn run(
    target: &ExchangeTarget,
    operation: &ExchangeOperation,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<RunOutput, RunError> {
    limits.validate()?;
    let started = Instant::now();
    let current = ExchangeTarget::capture(&target.selection, limits, cancel).await?;
    if &current != target {
        return Err(RunError::TargetChanged);
    }
    let command = command::network(target, operation)?;
    execute(command, remaining(limits, started)?, cancel).await
}

pub(crate) fn remaining(mut limits: RunLimits, started: Instant) -> Result<RunLimits, RunError> {
    limits.deadline = limits
        .deadline
        .checked_sub(started.elapsed())
        .ok_or(RunError::Deadline)?;
    if limits.deadline.is_zero() {
        return Err(RunError::Deadline);
    }
    Ok(limits)
}

async fn execute(
    command: std::process::Command,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<RunOutput, RunError> {
    limits.validate()?;
    if cancel.is_cancelled() {
        return Err(RunError::Cancelled);
    }
    process::execute(command, limits, cancel, false).await
}

async fn execute_config(
    command: std::process::Command,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<Vec<u8>, RunError> {
    limits.validate()?;
    if cancel.is_cancelled() {
        return Err(RunError::Cancelled);
    }
    Ok(process::execute(command, limits, cancel, true)
        .await?
        .stdout)
}
