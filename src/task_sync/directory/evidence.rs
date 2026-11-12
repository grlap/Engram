//! Admitted data-folder namespace checks, without pack reads or activation.
use super::{Path, Result, TaskSyncRefusal, fs, regular_directory};
use crate::task_sync::evidence::path::{self, Kind};

pub(super) fn validate(root: &Path) -> Result<()> {
    let mut pending = vec![(root.to_path_buf(), ".engram/evidence".to_owned())];
    while let Some((location, name)) = pending.pop() {
        match path::kind(&name) {
            Some(Kind::Directory) => {
                regular_directory(&location)?;
                for entry in fs::read_dir(&location)? {
                    let entry = entry?;
                    let child = entry.file_name().into_string().map_err(|_| {
                        TaskSyncRefusal::DestinationNonUtf8Name
                            .invalid("evidence path must be UTF-8")
                    })?;
                    pending.push((entry.path(), format!("{name}/{child}")));
                }
            }
            Some(Kind::Pack) => {
                let metadata = fs::symlink_metadata(location)?;
                if metadata.file_type().is_symlink() {
                    return Err(TaskSyncRefusal::DestinationLink
                        .invalid("evidence carrier file must not be a link")
                        .into());
                }
                if !metadata.is_file() {
                    return Err(TaskSyncRefusal::DestinationNotFile
                        .invalid("evidence carrier pack must be an ordinary file")
                        .into());
                }
            }
            None => {
                return Err(TaskSyncRefusal::DestinationUnknownEntry
                    .invalid("unknown evidence carrier path")
                    .into());
            }
        }
    }
    Ok(())
}
