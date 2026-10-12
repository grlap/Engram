use crate::task_sync::invalid;
use anyhow::{Context, Result};
use std::{
    fs::{self, File, OpenOptions},
    path::Path,
};

pub(super) struct ProgressLock(File);

impl ProgressLock {
    pub(super) fn acquire(directory: &Path) -> Result<Self> {
        fs::create_dir_all(directory)?;
        let path = directory.join("prepare.lock");
        if let Ok(meta) = fs::symlink_metadata(&path)
            && (!meta.is_file() || meta.file_type().is_symlink())
        {
            return Err(invalid("prepare lock must be an ordinary file").into());
        }
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(&path)?;
        match file.try_lock() {
            Ok(()) => Ok(Self(file)),
            Err(fs::TryLockError::WouldBlock) => {
                Err(invalid(format!("another exporter holds {}", path.display())).into())
            }
            Err(fs::TryLockError::Error(error)) => {
                Err(error).with_context(|| format!("locking {}", path.display()))
            }
        }
    }
}

impl Drop for ProgressLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}
