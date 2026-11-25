//! Bounded Git commands for ordinary folder sync.

use anyhow::{Context, Result, anyhow, ensure};
use std::{
    path::Path,
    process::{Command, ExitStatus, Stdio},
    time::Instant,
};
use tokio::io::{AsyncRead, AsyncReadExt};

pub(crate) struct Output {
    pub status: ExitStatus,
    pub stdout: Vec<u8>,
}

pub(crate) async fn run(mut command: Command, action: &str) -> Result<Output> {
    let started = Instant::now();
    command.stdout(Stdio::piped()).stderr(Stdio::null());
    #[cfg(windows)]
    let mut command = {
        let mut command = crate::task_sync::exchange::confined_command(command);
        command.command_mut().stderr(Stdio::null());
        command
    };
    #[cfg(unix)]
    let mut command = {
        use process_wrap::tokio::{CommandWrap, ProcessGroup};
        let mut command = CommandWrap::from(tokio::process::Command::from(command));
        command.wrap(ProcessGroup::leader());
        command
    };
    let _warning = crate::operation_time::Warning::start("task-sync Git");
    let mut child = command
        .spawn()
        .with_context(|| format!("start Git {action}"))?;
    let stdout = child.stdout().take().context("missing Git stdout")?;
    let run = async {
        let stdout = read_output(stdout).await?;
        // Wait for Git itself without entering the wrapper's blocking job or
        // process-group wait. The wrapper still owns termination on failure.
        let status = child.inner_mut().wait().await?;
        Ok::<_, anyhow::Error>(Output { status, stdout })
    };
    let result = match tokio::time::timeout(
        crate::operation_time::MINIMUM.saturating_sub(started.elapsed()),
        run,
    )
    .await
    {
        Ok(result) => result.with_context(|| format!("Git {action}")),
        Err(_) => Err(anyhow!(
            "Git {action} exceeded 60 seconds; inspect the data branch before retrying"
        )),
    };
    if result.is_err() {
        // Signal the job or process group without starting another wait past
        // the deadline. On Windows dropping the wrapper also closes the job.
        let _ = child.start_kill();
    }
    result
}

async fn read_output(stdout: impl AsyncRead + Unpin) -> Result<Vec<u8>> {
    let mut bytes = Vec::new();
    stdout
        .take(super::MAX_LISTING_BYTES + 1)
        .read_to_end(&mut bytes)
        .await?;
    ensure!(
        bytes.len() as u64 <= super::MAX_LISTING_BYTES,
        "Git output exceeded its bound"
    );
    Ok(bytes)
}

pub(crate) fn network(repository: &Path, args: &[&str]) -> Command {
    let mut command = super::configured_command(repository);
    command
        .args(args)
        .env("GIT_ALLOW_PROTOCOL", "file:https:ssh")
        .env("GCM_INTERACTIVE", "Never")
        .env("GIT_SSH_COMMAND", "ssh -o BatchMode=yes")
        .env("LC_ALL", "C");
    command
}

pub(crate) async fn checked(command: Command, action: &str) -> Result<Vec<u8>> {
    let output = run(command, action).await?;
    ensure!(
        output.status.success(),
        "Git {action} failed ({})",
        output.status
    );
    Ok(output.stdout)
}

pub(crate) fn oid(bytes: &[u8]) -> Result<String> {
    let oid = std::str::from_utf8(bytes)?.trim();
    crate::task_sync::ledger::oid(oid)?;
    Ok(oid.to_owned())
}
