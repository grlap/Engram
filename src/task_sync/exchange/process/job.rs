//! A job owns descendants even when Git exits before its credential helper.

use process_wrap::tokio::{ChildWrapper, CommandWrap, CreationFlags, JobObject, KillOnDrop};
use std::{
    io,
    process::{Command, ExitStatus, Stdio},
};
use tokio::process::{ChildStderr, ChildStdout};
use windows::Win32::System::Threading::CREATE_NO_WINDOW;

pub(in super::super) fn confined_command(mut command: Command) -> CommandWrap {
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

/// The job closes, and kills what it still holds, on every drop.
pub(super) struct Tree(Box<dyn ChildWrapper>);

impl Tree {
    pub(super) fn spawn(command: Command) -> io::Result<Self> {
        confined_command(command).spawn().map(Self)
    }

    pub(super) fn stdout(&mut self) -> Option<ChildStdout> {
        self.0.stdout().take()
    }

    pub(super) fn stderr(&mut self) -> Option<ChildStderr> {
        self.0.stderr().take()
    }

    /// The root's status once it has exited; the rest of the job is then
    /// terminated. The wrapper's potentially blocking completion-port wait is
    /// never awaited: the job stays owned while draining and closes on drop.
    pub(super) fn root_exited(&mut self) -> io::Result<Option<ExitStatus>> {
        let status = self.0.try_wait()?;
        if status.is_some() {
            self.0.start_kill()?;
        }
        Ok(status)
    }
}
