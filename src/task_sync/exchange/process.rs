//! A job owns descendants even when Git exits before its credential helper.

use super::{Cancellation, RunError, RunLimits, RunOutput};
use process_wrap::tokio::{CommandWrap, CreationFlags, JobObject, KillOnDrop};
use std::{
    process::{Command, Stdio},
    time::{Duration, Instant},
};
use tokio::io::{AsyncRead, AsyncReadExt};
use windows::Win32::System::Threading::CREATE_NO_WINDOW;

pub(super) fn confined_command(mut command: Command) -> CommandWrap {
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let native = tokio::process::Command::from(command);
    let mut wrapped = CommandWrap::from(native);
    wrapped
        .wrap(CreationFlags(CREATE_NO_WINDOW))
        .wrap(KillOnDrop)
        .wrap(JobObject);
    wrapped
}

pub(super) async fn execute(
    command: Command,
    limits: RunLimits,
    cancel: &Cancellation,
    allow_missing_config: bool,
) -> Result<RunOutput, RunError> {
    let started = Instant::now();
    let mut wrapped = confined_command(command);
    // Native spawn itself is synchronous. Check the same deadline immediately
    // after it returns; no native-kernel preemption is claimed.
    #[cfg(test)]
    super::test_support::LAUNCHES.with(|count| count.set(count.get() + 1));
    let mut child = wrapped.spawn().map_err(|_| RunError::Spawn)?;
    let stdout = child.stdout().take().ok_or(RunError::Process)?;
    let stderr = child.stderr().take().ok_or(RunError::Process)?;
    let stdout = bounded_read(stdout, limits.stdout_bytes);
    let stderr = bounded_read(stderr, limits.stderr_bytes);
    tokio::pin!(stdout, stderr);
    let mut out: Option<Vec<u8>> = None;
    let mut err: Option<Vec<u8>> = None;
    let mut status: Option<std::process::ExitStatus> = None;
    loop {
        if cancel.is_cancelled() {
            return Err(RunError::Cancelled);
        }
        if started.elapsed() >= limits.deadline {
            return Err(RunError::Deadline);
        }
        if status.is_none() {
            status = child.try_wait().map_err(|_| RunError::Process)?;
            if status.is_some() {
                // Do not await the wrapper's potentially blocking completion-
                // port wait. Terminate the job, keep it owned while draining,
                // and close it on every return/drop (including cancellation).
                child.start_kill().map_err(|_| RunError::Process)?;
            }
        }
        if let (Some(status), Some(_), Some(err)) = (status.as_ref(), out.as_ref(), err.as_ref()) {
            if !(status.success() || allow_missing_config && status.code() == Some(1)) {
                return Err(RunError::Git);
            }
            let stderr_bytes = err.len();
            return Ok(RunOutput {
                stdout: out.take().ok_or(RunError::Process)?,
                stderr_bytes,
            });
        }
        tokio::select! {
            result = &mut stdout, if out.is_none() => { out = Some(result?); }
            result = &mut stderr, if err.is_none() => { err = Some(result?); }
            () = tokio::time::sleep(Duration::from_millis(5)) => {}
        }
    }
}

async fn bounded_read(mut pipe: impl AsyncRead + Unpin, limit: usize) -> Result<Vec<u8>, RunError> {
    let mut output = Vec::new();
    let mut buffer = [0; 4096];
    loop {
        let remaining = limit.saturating_sub(output.len());
        let read_size = remaining.min(buffer.len() - 1) + 1;
        let count = pipe
            .read(&mut buffer[..read_size])
            .await
            .map_err(|_| RunError::Process)?;
        if count == 0 {
            return Ok(output);
        }
        if count > remaining {
            return Err(RunError::OutputLimit);
        }
        output.extend_from_slice(&buffer[..count]);
    }
}
