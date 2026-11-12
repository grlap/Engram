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
    execute_sanitized(
        command,
        limits,
        cancel,
        allow_missing_config,
        #[cfg(test)]
        None,
    )
    .await
}

pub(super) async fn execute_sanitized(
    command: Command,
    limits: RunLimits,
    cancel: &Cancellation,
    allow_missing_config: bool,
    #[cfg(test)] deadline_hook: DeadlineHook<'_>,
) -> Result<RunOutput, RunError> {
    let mut capture = Capture::default();
    execute_into(
        command,
        limits,
        cancel,
        allow_missing_config,
        &mut capture,
        #[cfg(test)]
        deadline_hook,
    )
    .await?;
    Ok(RunOutput {
        stdout: capture.stdout,
        stderr_bytes: capture.stderr.len(),
    })
}

/// Classify only a completed, drained push command. Deadline, cancellation,
/// pipe and output-limit failures keep their original uncertainty.
pub(super) async fn execute_push(
    mut command: Command,
    limits: RunLimits,
    cancel: &Cancellation,
    commit: &str,
    reference: &str,
) -> Result<RunOutput, RunError> {
    command.env("LC_ALL", "C").env("LANG", "C");
    let mut capture = Capture::default();
    execute_into(
        command,
        limits,
        cancel,
        false,
        &mut capture,
        #[cfg(test)]
        None,
    )
    .await
    .map_err(|error| {
        super::push::classify_failure(error, capture.exit_code, &capture.stdout, commit, reference)
    })?;
    Ok(RunOutput {
        stdout: capture.stdout,
        stderr_bytes: capture.stderr.len(),
    })
}

#[derive(Default)]
struct Capture {
    stdout: Vec<u8>,
    stderr: Vec<u8>,
    exit_code: Option<i32>,
}

// Only the owned test supervisor can receive raw failure output. The production
// entry above still returns the same sanitized RunError and counts stderr only.
#[cfg(test)]
pub(super) struct DiagnosticOutput {
    pub(super) result: Result<(), RunError>,
    pub(super) stdout: Vec<u8>,
    pub(super) stderr: Vec<u8>,
}

#[cfg(test)]
pub(super) type DeadlineHook<'a> = Option<&'a (dyn Fn(&[u8], &[u8]) -> bool + Sync)>;

#[cfg(test)]
pub(super) async fn execute_diagnostic(
    command: Command,
    limits: RunLimits,
    cancel: &Cancellation,
    deadline_hook: DeadlineHook<'_>,
) -> DiagnosticOutput {
    let mut capture = Capture::default();
    let result = match limits.validate() {
        Err(error) => Err(error),
        Ok(()) if cancel.is_cancelled() => Err(RunError::Cancelled),
        Ok(()) => execute_into(command, limits, cancel, false, &mut capture, deadline_hook).await,
    };
    DiagnosticOutput {
        result,
        stdout: capture.stdout,
        stderr: capture.stderr,
    }
}

async fn execute_into(
    command: Command,
    limits: RunLimits,
    cancel: &Cancellation,
    allow_missing_config: bool,
    capture: &mut Capture,
    #[cfg(test)] deadline_hook: DeadlineHook<'_>,
) -> Result<(), RunError> {
    let started = Instant::now();
    #[cfg(test)]
    let mut stage_trace = super::tests::windows::readiness::Trace::for_command(&command);
    #[cfg(test)]
    let label = super::fixture_diagnostics::label(&command);
    #[cfg(test)]
    let subcommand = git_subcommand(&command);
    let mut wrapped = confined_command(command);
    // Native spawn itself is synchronous. Check the same deadline immediately
    // after it returns; no native-kernel preemption is claimed.
    #[cfg(test)]
    super::test_support::LAUNCHES.with(|count| count.set(count.get() + 1));
    #[cfg(test)]
    let trace = super::fixture_diagnostics::product(label);
    let result = async {
        #[cfg(test)]
        if let Some(trace) = &mut stage_trace {
            trace.stage("before-native-spawn");
        }
        let spawned = wrapped.spawn();
        #[cfg(test)]
        if let Some(trace) = &mut stage_trace {
            trace.stage(if spawned.is_ok() {
                "after-native-spawn"
            } else {
                "native-spawn-failed"
            });
        }
        let mut child = spawned.map_err(|_| RunError::Spawn)?;
        let stdout = child.stdout().take().ok_or(RunError::Process)?;
        let stderr = child.stderr().take().ok_or(RunError::Process)?;
        #[cfg(test)]
        let observed_out = std::sync::Mutex::new(Vec::new());
        #[cfg(test)]
        let observed_err = std::sync::Mutex::new(Vec::new());
        let stdout = bounded_read(
            stdout,
            limits.stdout_bytes,
            &mut capture.stdout,
            #[cfg(test)]
            deadline_hook.map(|_| &observed_out),
        );
        let stderr = bounded_read(
            stderr,
            limits.stderr_bytes,
            &mut capture.stderr,
            #[cfg(test)]
            deadline_hook.map(|_| &observed_err),
        );
        tokio::pin!(stdout, stderr);
        let mut out_done = false;
        let mut err_done = false;
        let mut status: Option<std::process::ExitStatus> = None;
        loop {
            if cancel.is_cancelled() {
                return Err(RunError::Cancelled);
            }
            if started.elapsed() >= limits.deadline {
                return Err(RunError::Deadline);
            }
            #[cfg(test)]
            if deadline_hook.is_some_and(|hook| {
                hook(&observed_out.lock().unwrap(), &observed_err.lock().unwrap())
            }) {
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
            if let Some(status) = status.as_ref().filter(|_| out_done && err_done) {
                capture.exit_code = status.code();
                if !(status.success() || allow_missing_config && status.code() == Some(1)) {
                    return Err(RunError::Git);
                }
                return Ok(());
            }
            tokio::select! {
                result = &mut stdout, if !out_done => { result?; out_done = true; }
                result = &mut stderr, if !err_done => { result?; err_done = true; }
                () = tokio::time::sleep(Duration::from_millis(5)) => {}
            }
        }
    }
    .await;
    // The readers borrowed the captured output until the block above ended.
    #[cfg(test)]
    if let (Err(RunError::Git), Some(subcommand)) = (&result, &subcommand) {
        report_git_failure(subcommand, capture.exit_code, &capture.stderr);
    }
    #[cfg(test)]
    trace.finish(result.as_ref().err());
    result
}

/// The Git subcommand only: never its other arguments, which may name a URL.
/// A command that does not run Git, such as a supervised test child, has none.
#[cfg(test)]
fn git_subcommand(command: &Command) -> Option<String> {
    let program = std::path::Path::new(command.get_program());
    if !program
        .file_stem()
        .is_some_and(|stem| stem.eq_ignore_ascii_case("git"))
    {
        return None;
    }
    let mut option_value = false;
    for arg in command.get_args() {
        if std::mem::take(&mut option_value) {
            continue;
        }
        if arg == "-c" || arg == "-C" {
            option_value = true;
            continue;
        }
        let arg = arg.to_string_lossy();
        if !arg.starts_with('-') {
            return Some(
                arg.chars()
                    .filter(char::is_ascii_graphic)
                    .take(32)
                    .collect(),
            );
        }
    }
    Some("unknown".into())
}

/// A failed Git command in a test build names its subcommand, exit code and a
/// short single-line stderr excerpt, so a refusal can be classified. The
/// production refusal stays the same sanitized `RunError` with no Git output.
#[cfg(test)]
fn report_git_failure(subcommand: &str, exit: Option<i32>, stderr: &[u8]) {
    const EXCERPT: usize = 512;
    let excerpt: String = String::from_utf8_lossy(&stderr[..stderr.len().min(EXCERPT)])
        .chars()
        .map(|character| {
            if character.is_control() {
                ' '
            } else {
                character
            }
        })
        .collect();
    eprintln!("exchange-git-failure subcommand={subcommand} exit={exit:?} stderr={excerpt}");
}

async fn bounded_read(
    mut pipe: impl AsyncRead + Unpin,
    limit: usize,
    output: &mut Vec<u8>,
    #[cfg(test)] observed: Option<&std::sync::Mutex<Vec<u8>>>,
) -> Result<(), RunError> {
    let mut buffer = [0; 4096];
    loop {
        let read_size = limit.saturating_sub(output.len()).min(buffer.len() - 1) + 1;
        let count = pipe
            .read(&mut buffer[..read_size])
            .await
            .map_err(|_| RunError::Process)?;
        if count == 0 {
            return Ok(());
        }
        append_bounded(output, &buffer[..count], limit)?;
        #[cfg(test)]
        if let Some(observed) = observed {
            observed.lock().unwrap().clone_from(output);
        }
    }
}

fn append_bounded(output: &mut Vec<u8>, bytes: &[u8], limit: usize) -> Result<(), RunError> {
    let remaining = limit.saturating_sub(output.len());
    output.extend_from_slice(&bytes[..bytes.len().min(remaining)]);
    if bytes.len() > remaining {
        return Err(RunError::OutputLimit);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_exchange_deadline_diagnostics_zero_exact_and_over_limit() {
        let mut output = Vec::new();
        assert_eq!(append_bounded(&mut output, b"", 0), Ok(()));
        assert_eq!(
            append_bounded(&mut output, b"x", 0),
            Err(RunError::OutputLimit)
        );
        assert_eq!(output, [] as [u8; 0]);
        assert_eq!(append_bounded(&mut output, b"ab", 4), Ok(()));
        assert_eq!(append_bounded(&mut output, b"cd", 4), Ok(()));
        assert_eq!(
            append_bounded(&mut output, b"e", 4),
            Err(RunError::OutputLimit)
        );
        assert_eq!(output, b"abcd");
        let mut output = Vec::new();
        assert_eq!(
            append_bounded(&mut output, b"abcde", 4),
            Err(RunError::OutputLimit)
        );
        assert_eq!(output, b"abcd");
    }
}
