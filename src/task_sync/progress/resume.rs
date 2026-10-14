//! Opt-out recovery only: inspect an already published pending generation without cleanup.

use super::{
    ExportProfile, PrepareReceipt, PreparedSelection, directory, invalid, journal, prepared,
};
use crate::ProjectId;
use anyhow::{Context, Result, ensure};
use cap_fs_ext::{FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::OpenOptions;
use std::path::Path;

pub(crate) fn resume_pending(
    destination: &Path,
    progress: &Path,
    store: &Path,
    project: &ProjectId,
    profile: &ExportProfile,
    rebaseline: bool,
) -> Result<PrepareReceipt> {
    ensure!(
        !rebaseline,
        "task sync is not enabled; a new baseline cannot start"
    );
    let supplied_progress = progress;
    let progress = prepared::files::Directory::open(supplied_progress)
        .context("task sync is not enabled and no admitted pending preparation is available")?;
    let progress_path = super::resolved_child(supplied_progress)?;
    // Existing-only open: a losing or disabled caller creates no progress or lock.
    let mut options = OpenOptions::new();
    options.read(true).write(true).follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_fs_ext::OpenOptionsSyncExt;
        options.nonblock(true);
    }
    let lock = progress
        .dir()
        .open_with("prepare.lock", &options)?
        .into_std();
    ensure!(
        lock.metadata()?.is_file(),
        "prepare lock must be an ordinary file"
    );
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            lock.metadata()?.file_attributes() & 0x400 == 0,
            "prepare lock is a reparse point"
        );
    }
    lock.try_lock()
        .map_err(|error| invalid(format!("pending preparation busy: {error}")))?;
    let names = progress.names(3)?;
    let name = names
        .iter()
        .rev()
        .find(|name| {
            name.to_str()
                .is_some_and(|name| name.starts_with("generation-"))
        })
        .context("task sync is not enabled and no admitted pending preparation is available")?;
    let generation = progress.child(Path::new(name))?;
    let state = generation.read(Path::new("state.json"), 1024 * 1024)?;
    let meta: journal::Metadata = crate::task_sync::parse(&state)?;
    let candidate = prepared::read_selected(
        &progress_path,
        &PreparedSelection {
            operation: meta.operation,
            destination: destination.into(),
            project: project.clone(),
            profile: profile.clone(),
        },
        true,
    )?;
    ensure!(
        candidate.provenance.store == std::fs::canonicalize(store)?,
        "pending preparation must retain its original store binding"
    );
    // Recheck the pinned metadata immediately before the only output write.
    ensure!(
        generation.read(Path::new("state.json"), 1024 * 1024)? == state
            && progress.names(3)? == names,
        "pending preparation changed during recovery"
    );
    // Only after complete non-mutating admission may the original journal
    // recovery clean its recognized completion temporaries. No staging-only or
    // unknown entry passed the reader above, and no new capture is possible here.
    let pending = journal::load(&progress_path)?
        .current
        .context("admitted pending preparation disappeared")?;
    ensure!(
        !pending.complete
            && pending.meta.operation == meta.operation
            && pending.documents == candidate.documents,
        "pending preparation changed after recovery admission"
    );
    directory::write_documents(&pending.meta.destination, &pending.documents)?;
    pending.finish()?;
    Ok(pending.receipt(true))
}
