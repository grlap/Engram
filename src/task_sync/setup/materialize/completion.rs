//! Completion repair retains partial bytes inside the admitted private operation.

use super::super::files::{PhysicalId, file_identity};
use super::{Directory, Path, Result, ensure, journal::Record};
use std::io::{Read, Seek, SeekFrom, Write};

pub(super) fn check_source(private: &Directory, record: &Record) -> Result<Directory> {
    let recovery = private.child(Path::new("completion-recovery"))?;
    ensure!(
        recovery.identity()? == record.completion_recovery
            && recovery.names(1)? == [std::ffi::OsString::from("source")]
            && file_identity(&recovery.open_file(Path::new("source"), false, false)?)?
                == record.completion_source,
        "completion recovery source identity or inventory changed; preserve it"
    );
    Ok(recovery)
}

pub(super) fn publish(
    private: &Directory,
    record: &Record,
    bytes: &[u8],
    checkpoint: &mut impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    let recovery = check_source(private, record)?;
    let pending_name = Path::new(".complete.pending");
    if let Some(partial) = private.read(pending_name)? {
        ensure!(
            bytes.starts_with(&partial),
            "completion pending bytes differ; preserve them"
        );
        // The first interrupted source remains untouched, including its identity.
        // The journal already binds this empty recovery file before publication.
        let mut source = recovery.open_file(Path::new("source"), true, false)?;
        ensure!(
            file_identity(&source)? == record.completion_source,
            "completion recovery source identity changed"
        );
        let mut prefix = Vec::new();
        Read::by_ref(&mut source)
            .take(bytes.len() as u64 + 1)
            .read_to_end(&mut prefix)?;
        ensure!(
            bytes.starts_with(&prefix),
            "completion recovery prefix differs; preserve it"
        );
        checkpoint("complete-repair-started")?;
        source.seek(SeekFrom::End(0))?;
        let remaining = &bytes[prefix.len()..];
        let split = remaining.len() / 2;
        source.write_all(&remaining[..split])?;
        checkpoint("complete-repair-prefix")?;
        source.write_all(&remaining[split..])?;
        source.sync_all()?;
        drop(source);
        ensure!(
            recovery.read(Path::new("source"))?.as_deref() == Some(bytes),
            "completion recovery bytes changed"
        );
        checkpoint("complete-repair-synced")?;
        recovery
            .dir()
            .hard_link("source", private.dir(), "complete")?;
    } else {
        ensure!(
            recovery.read(Path::new("source"))?.as_deref() == Some(b""),
            "completion recovery source exists without its original pending witness"
        );
        let mut pending = private.open_file(pending_name, true, true)?;
        checkpoint("complete-pending-created")?;
        let split = bytes.len() / 2;
        pending.write_all(&bytes[..split])?;
        checkpoint("complete-pending-prefix")?;
        pending.write_all(&bytes[split..])?;
        pending.sync_all()?;
        drop(pending);
        checkpoint("complete-pending-synced")?;
        private
            .dir()
            .hard_link(pending_name, private.dir(), "complete")?;
    }
    Ok(())
}

pub(super) fn check_published(
    private: &Directory,
    bytes: &[u8],
    identity: PhysicalId,
) -> Result<()> {
    let recovery = private.child(Path::new("completion-recovery"))?;
    for (directory, name) in [(private, ".complete.pending"), (&recovery, "source")] {
        if directory.read(Path::new(name))?.as_deref() == Some(bytes)
            && file_identity(&directory.open_file(Path::new(name), false, false)?)? == identity
        {
            return Ok(());
        }
    }
    anyhow::bail!("materialization completion publication identity changed")
}
