//! An exclusive file lock released by an explicit unlock.
//!
//! Closing a locked file is not a reliable release on Unix. There the lock
//! belongs to the open file description, and a child process started by any
//! thread of this process can share that description until it runs its own
//! program. A lock released only by closing stays held while such a child
//! exists, so the next holder is refused although nobody holds the lock. An
//! explicit unlock releases the description's lock whoever else shares it.

use std::fs::{File, TryLockError};

/// Holds an exclusive lock on `file` until dropped, then unlocks it explicitly.
pub(crate) struct HeldLock(File);

impl HeldLock {
    /// Takes the lock without waiting.
    ///
    /// # Errors
    /// Returns `WouldBlock` when another holder has the lock.
    pub(crate) fn try_acquire(file: File) -> Result<Self, TryLockError> {
        file.try_lock()?;
        Ok(Self(file))
    }

    /// The locked file, for reads that must use the locked handle.
    pub(crate) fn file(&self) -> &File {
        &self.0
    }
}

impl Drop for HeldLock {
    fn drop(&mut self) {
        let _ = self.0.unlock();
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::process::{Child, Command, Stdio};

    fn open(path: &std::path::Path) -> File {
        std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
            .unwrap()
    }

    /// A child that shares the lock's open file description, as one started
    /// by another thread does until it runs its own program. It is killed and
    /// reaped when dropped, also when a failed assertion unwinds the test.
    struct SharingChild(Child);

    impl SharingChild {
        fn spawn(file: &File) -> Self {
            Self(
                Command::new("sleep")
                    .arg("60")
                    .stdin(Stdio::from(file.try_clone().unwrap()))
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .unwrap(),
            )
        }
    }

    impl Drop for SharingChild {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    #[test]
    fn a_close_only_release_stays_held_while_a_child_shares_the_lock() {
        let home = crate::test_support::temp_home().unwrap();
        let path = home.path().join("prepare.lock");
        let file = open(&path);
        file.try_lock().unwrap();
        let _child = SharingChild::spawn(&file);
        drop(file);
        assert!(matches!(
            open(&path).try_lock(),
            Err(TryLockError::WouldBlock)
        ));
    }

    #[test]
    fn a_held_lock_is_released_on_drop_while_a_child_shares_it() {
        let home = crate::test_support::temp_home().unwrap();
        let path = home.path().join("prepare.lock");
        let held = HeldLock::try_acquire(open(&path)).unwrap();
        assert!(matches!(
            HeldLock::try_acquire(open(&path)),
            Err(TryLockError::WouldBlock)
        ));
        let _child = SharingChild::spawn(held.file());
        drop(held);
        drop(HeldLock::try_acquire(open(&path)).unwrap());
    }
}
