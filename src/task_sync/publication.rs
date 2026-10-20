//! Publish a complete staged directory without replacing any destination entry.

use std::{io, path::Path};

use super::TaskSyncRefusal;

pub(super) fn publish(staging: &Path, destination: &Path) -> anyhow::Result<()> {
    #[cfg(test)]
    if UNSUPPORTED_AFTER.get().is_some_and(|remaining| {
        UNSUPPORTED_AFTER.set(Some(remaining.saturating_sub(1)));
        remaining == 0
    }) {
        return publish_with(staging, destination, |_, _| {
            Err(io::ErrorKind::Unsupported.into())
        });
    }
    publish_with(staging, destination, native_move)
}

fn publish_with(
    staging: &Path,
    destination: &Path,
    move_directory: impl FnOnce(&Path, &Path) -> io::Result<()>,
) -> anyhow::Result<()> {
    // Call the namespace operation directly: an existence check cannot reserve
    // the destination. In particular, never retry with rename or hard_link.
    move_directory(staging, destination).map_err(|error| {
        if capability_unavailable(&error) {
            TaskSyncRefusal::PublicationUnsupported
                .invalid("filesystem cannot publish a directory atomically without replacement")
                .into()
        } else {
            error.into()
        }
    })
}

#[cfg(test)]
thread_local! {
    static UNSUPPORTED_AFTER: std::cell::Cell<Option<usize>> = const { std::cell::Cell::new(None) };
}

#[cfg(test)]
pub(super) fn with_unsupported_native_move<T>(action: impl FnOnce() -> T) -> T {
    with_unsupported_native_move_after(0, action)
}

#[cfg(test)]
pub(super) fn with_unsupported_native_move_after<T>(
    successful_moves: usize,
    action: impl FnOnce() -> T,
) -> T {
    struct Restore(Option<usize>);
    impl Drop for Restore {
        fn drop(&mut self) {
            UNSUPPORTED_AFTER.set(self.0);
        }
    }
    let _restore = Restore(UNSUPPORTED_AFTER.replace(Some(successful_moves)));
    action()
}

fn capability_unavailable(error: &io::Error) -> bool {
    if error.kind() == io::ErrorKind::Unsupported {
        return true;
    }
    #[cfg(unix)]
    {
        // The flags are fixed and valid. EINVAL means this filesystem does not
        // implement NOREPLACE; this is local to the operation, never cached.
        error.raw_os_error().is_some_and(|code| {
            [
                rustix::io::Errno::INVAL.raw_os_error(),
                rustix::io::Errno::NOSYS.raw_os_error(),
                rustix::io::Errno::OPNOTSUPP.raw_os_error(),
                #[cfg(not(target_os = "redox"))]
                rustix::io::Errno::NOTSUP.raw_os_error(),
            ]
            .contains(&code)
        })
    }
    #[cfg(not(unix))]
    {
        false
    }
}

#[cfg(any(
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "redox"
))]
fn native_move(staging: &Path, destination: &Path) -> io::Result<()> {
    use rustix::fs::{CWD, RenameFlags, renameat_with};
    renameat_with(CWD, staging, CWD, destination, RenameFlags::NOREPLACE).map_err(Into::into)
}

#[cfg(windows)]
fn native_move(staging: &Path, destination: &Path) -> io::Result<()> {
    // tempfile's Windows implementation uses MoveFileExW without
    // MOVEFILE_REPLACE_EXISTING. Its Unix hard-link fallback is never used.
    let mut pending = tempfile::TempPath::try_from_path(staging)?;
    pending.disable_cleanup(true);
    pending
        .persist_noclobber(destination)
        .map_err(|error| error.error)
}

#[cfg(not(any(
    windows,
    target_os = "linux",
    target_os = "android",
    target_vendor = "apple",
    target_os = "redox"
)))]
fn native_move(_: &Path, _: &Path) -> io::Result<()> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "no native no-replace directory move is available",
    ))
}

#[cfg(test)]
mod tests;
