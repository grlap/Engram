//! Completion repair retains partial bytes inside the admitted private operation.

use super::{Directory, Path, Result, ensure};
use std::io::{Read, Seek, SeekFrom, Write};

pub(super) fn check_source(private: &Directory) -> Result<Directory> {
    let recovery = private.child(Path::new("completion-recovery"))?;
    ensure!(
        recovery.names(1)? == [std::ffi::OsString::from("source")],
        "completion recovery inventory changed; preserve it"
    );
    Ok(recovery)
}

pub(super) fn publish(
    private: &Directory,
    bytes: &[u8],
    checkpoint: &mut impl FnMut(&str) -> Result<()>,
) -> Result<()> {
    let recovery = check_source(private)?;
    let pending_name = Path::new(".complete.pending");
    if private.read(pending_name)?.is_some() {
        // The first interrupted record remains untouched, whatever it holds:
        // an older build wrote the record with more fields. The recovery file,
        // created empty with the journal and never published, takes the
        // complete bytes; a prefix of them is kept, anything else rewritten.
        let mut source = recovery.open_file(Path::new("source"), true, false)?;
        let mut prefix = Vec::new();
        Read::by_ref(&mut source)
            .take(bytes.len() as u64 + 1)
            .read_to_end(&mut prefix)?;
        if !bytes.starts_with(&prefix) {
            source.set_len(0)?;
            prefix.clear();
        }
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
