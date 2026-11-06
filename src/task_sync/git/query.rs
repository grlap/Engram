//! Shared bounded local Git execution; callers supply fixed argument vectors.

use anyhow::{Context, Result, ensure};
use std::{
    io::{Read, Write},
    path::Path,
    process::{Child, ExitStatus, Stdio},
    time::{Duration, Instant},
};

const MAX_BYTES: u64 = 1024 * 1024;

#[derive(Clone, Copy, Eq, PartialEq)]
pub(crate) enum StderrPolicy {
    ErrorOnly,
    RequireEmpty,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum Failure {
    #[error("local Git admission query refused: {command} (exit {code:?})")]
    Nonzero { command: String, code: Option<i32> },
    #[error("Git admission output exceeded byte bound")]
    OutputLimit,
    #[error("local Git query exceeded its deadline")]
    Deadline,
    #[error("local Git query {command} reported unreadable input: {message}")]
    Stderr { command: String, message: String },
}

pub(crate) fn run(
    root: &Path,
    args: &[&str],
    missing: bool,
    input: Option<&[u8]>,
    bound: Duration,
) -> Result<Vec<u8>> {
    ensure!(
        !bound.is_zero() && bound <= Duration::from_secs(30),
        "invalid local Git query deadline"
    );
    ensure!(
        input.is_none_or(|bytes| bytes.len() as u64 <= MAX_BYTES),
        "Git admission input exceeded byte bound"
    );
    let mut command = super::command(root, args);
    command.env_remove("GIT_ATTR_SOURCE");
    command.env("GIT_LITERAL_PATHSPECS", "1");
    if args.first() == Some(&"check-ignore") {
        // check-ignore accepts pathname input, not pathspec magic. These inputs
        // are generated canonical project/task paths, never caller patterns.
        for name in [
            "GIT_LITERAL_PATHSPECS",
            "GIT_GLOB_PATHSPECS",
            "GIT_NOGLOB_PATHSPECS",
            "GIT_ICASE_PATHSPECS",
        ] {
            command.env_remove(name);
        }
    }
    run_command(
        command,
        args[0],
        missing,
        input,
        bound,
        StderrPolicy::ErrorOnly,
    )
}

/// Run an already isolated local query with the same output and time bounds.
/// The caller owns its Git configuration and fixed argument vector.
pub(crate) fn run_command(
    mut command: std::process::Command,
    command_name: &str,
    missing: bool,
    input: Option<&[u8]>,
    bound: Duration,
    stderr_policy: StderrPolicy,
) -> Result<Vec<u8>> {
    ensure!(
        !bound.is_zero() && bound <= Duration::from_secs(30),
        "invalid local Git query deadline"
    );
    ensure!(
        input.is_none_or(|bytes| bytes.len() as u64 <= MAX_BYTES),
        "Git admission input exceeded byte bound"
    );
    command.stdout(Stdio::piped());
    if stderr_policy == StderrPolicy::RequireEmpty {
        command.stderr(Stdio::piped());
    }
    if input.is_some() {
        command.stdin(Stdio::piped());
    }
    let mut child = command.spawn().context("start local Git admission query")?;
    let mut stderr_result = child.stderr.take().map(|stderr| {
        let (sender, receiver) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let mut bytes = Vec::new();
            let result = stderr
                .take(MAX_BYTES + 1)
                .read_to_end(&mut bytes)
                .map(|_| bytes);
            let _ = sender.send(result);
        });
        receiver
    });
    let input_result = if let Some(input) = input {
        let input = input.to_vec();
        let mut stdin = child.stdin.take().context("missing Git stdin")?;
        let (sender, receiver) = std::sync::mpsc::channel();
        // A stalled stdin write must not bypass the existing child deadline.
        std::thread::spawn(move || {
            let _ = sender.send(stdin.write_all(&input));
        });
        Some(receiver)
    } else {
        None
    };
    let stdout = child.stdout.take().context("missing Git stdout")?;
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let mut bytes = Vec::new();
        let result = stdout
            .take(MAX_BYTES + 1)
            .read_to_end(&mut bytes)
            .map(|_| bytes);
        let _ = sender.send(result);
    });
    let start = Instant::now();
    let status = wait_for_exit(&mut child, start, bound)?;
    let bytes = receiver.recv_timeout(
        bound
            .saturating_sub(start.elapsed())
            .max(Duration::from_secs(1)),
    )??;
    if let Some(receiver) = input_result {
        receiver.recv_timeout(
            bound
                .saturating_sub(start.elapsed())
                .max(Duration::from_secs(1)),
        )??;
    }
    if bytes.len() as u64 > MAX_BYTES {
        return Err(Failure::OutputLimit.into());
    }
    let strict_stderr = if stderr_policy == StderrPolicy::RequireEmpty {
        let receiver = stderr_result
            .take()
            .ok_or_else(|| stderr_failure(command_name, b"stderr capture is missing"))?;
        Some(receive_stderr(
            &receiver,
            command_name,
            bound.saturating_sub(start.elapsed()),
        )?)
    } else {
        None
    };
    if !(status.success() || (missing && status.code() == Some(1) && bytes.is_empty())) {
        let error = anyhow::Error::from(Failure::Nonzero {
            command: command_name.into(),
            code: status.code(),
        });
        if let Some(bytes) = strict_stderr {
            if !bytes.is_empty() {
                return Err(error.context(stderr_failure(command_name, &bytes)));
            }
        } else if let Some(receiver) = stderr_result {
            let bytes = receiver.recv_timeout(bound.saturating_sub(start.elapsed()))??;
            ensure!(
                bytes.len() as u64 <= MAX_BYTES,
                "Git admission error output exceeded byte bound"
            );
            return Err(error.context(String::from_utf8_lossy(&bytes).trim().to_owned()));
        }
        return Err(error);
    }
    if let Some(stderr) = strict_stderr
        && !stderr.is_empty()
    {
        return Err(stderr_failure(command_name, &stderr).into());
    }
    Ok(bytes)
}

fn stderr_failure(command: &str, bytes: &[u8]) -> Failure {
    let text = String::from_utf8_lossy(bytes);
    let mut message = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("Git wrote diagnostic output")
        .to_owned();
    let mut end = message.len().min(1024);
    while !message.is_char_boundary(end) {
        end -= 1;
    }
    message.truncate(end);
    Failure::Stderr {
        command: command.to_owned(),
        message,
    }
}

fn receive_stderr(
    receiver: &std::sync::mpsc::Receiver<std::io::Result<Vec<u8>>>,
    command: &str,
    remaining: Duration,
) -> Result<Vec<u8>> {
    use std::sync::mpsc::RecvTimeoutError;
    let bytes = match receiver.recv_timeout(remaining) {
        Ok(Ok(bytes)) => bytes,
        Ok(Err(error)) => {
            return Err(
                stderr_failure(command, format!("stderr read failed: {error}").as_bytes()).into(),
            );
        }
        Err(RecvTimeoutError::Timeout) => return Err(Failure::Deadline.into()),
        Err(RecvTimeoutError::Disconnected) => {
            return Err(stderr_failure(command, b"stderr read did not complete").into());
        }
    };
    if bytes.len() as u64 > MAX_BYTES {
        return Err(Failure::OutputLimit.into());
    }
    Ok(bytes)
}

fn wait_for_exit(child: &mut Child, start: Instant, bound: Duration) -> Result<ExitStatus> {
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if start.elapsed() < bound => {
                std::thread::sleep(Duration::from_millis(10));
            }
            Ok(None) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(Failure::Deadline.into());
            }
            Err(error) => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(error).context("local Git query failed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn successful_stderr_is_not_usable_input() {
        const ENV: &str = "ENGRAM_GIT_QUERY_STDERR_TEST";
        if let Ok(mode) = std::env::var(ENV) {
            if mode == "stderr" {
                eprintln!("error: child path: Filename too long");
            } else if mode == "long" {
                eprintln!("{}\nsecond line", "🙂".repeat(1024));
            }
            return;
        }
        let child = |mode| {
            let mut command = std::process::Command::new(std::env::current_exe().unwrap());
            command.args([
                "--exact",
                "task_sync::git::query::tests::successful_stderr_is_not_usable_input",
                "--nocapture",
            ]);
            command.env(ENV, mode).stderr(Stdio::piped());
            command
        };
        let run = |mode, policy| {
            run_command(
                child(mode),
                "status",
                false,
                None,
                Duration::from_secs(5),
                policy,
            )
        };
        assert!(run("stderr", StderrPolicy::ErrorOnly).is_ok());
        let error = run("stderr", StderrPolicy::RequireEmpty).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<Failure>(),
            Some(Failure::Stderr { .. })
        ));
        assert!(error.to_string().contains("Filename too long"));
        assert!(run("empty", StderrPolicy::RequireEmpty).is_ok());
        let error = run("long", StderrPolicy::RequireEmpty).unwrap_err();
        let Some(Failure::Stderr { message, .. }) = error.downcast_ref::<Failure>() else {
            panic!("expected a named stderr refusal: {error}");
        };
        assert_eq!(message.len(), 1024);
        assert!(!message.contains("second line"));
    }

    #[test]
    fn stderr_requires_complete_bounded_delivery() {
        let (sender, receiver) = std::sync::mpsc::channel();
        drop(sender);
        let error = receive_stderr(&receiver, "status", Duration::from_secs(1)).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<Failure>(),
            Some(Failure::Stderr { .. })
        ));

        let (_sender, receiver) = std::sync::mpsc::channel();
        let error = receive_stderr(&receiver, "status", Duration::ZERO).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<Failure>(),
            Some(Failure::Deadline)
        ));

        let (sender, receiver) = std::sync::mpsc::channel();
        sender
            .send(Ok(vec![b'x'; usize::try_from(MAX_BYTES).unwrap() + 1]))
            .unwrap();
        let error = receive_stderr(&receiver, "status", Duration::from_secs(1)).unwrap_err();
        assert!(matches!(
            error.downcast_ref::<Failure>(),
            Some(Failure::OutputLimit)
        ));
    }
}
