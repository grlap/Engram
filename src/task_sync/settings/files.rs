//! Narrow no-follow capabilities for the one host-local setting file.

use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, DirBuilder, OpenOptions};
use std::{
    io::{Read, Write},
    path::{Component, Path, PathBuf},
};

pub(super) const MAX_BYTES: usize = 16 * 1024;
const NAME: &str = "task-sync-setting.json";

pub(super) enum Failure {
    Io,
    Oversized,
}

impl From<std::io::Error> for Failure {
    fn from(_: std::io::Error) -> Self {
        Self::Io
    }
}

pub(super) struct Directory(Vec<Dir>);

impl Directory {
    pub(super) fn open(path: &Path, create: bool) -> Result<Option<Self>, Failure> {
        let absolute = std::path::absolute(path)?;
        let mut anchor = PathBuf::new();
        let mut directories: Vec<Dir> = Vec::new();
        for component in absolute.components() {
            match component {
                Component::Prefix(_) | Component::RootDir => anchor.push(component),
                Component::Normal(name) => {
                    if directories.is_empty() {
                        let root = Dir::open_ambient_dir(&anchor, cap_std::ambient_authority())?;
                        #[cfg(windows)]
                        check_directory(&root)?;
                        directories.push(root);
                    }
                    let parent = directories.last().ok_or(Failure::Io)?;
                    match parent.symlink_metadata(name) {
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                            if !create {
                                return Ok(None);
                            }
                            #[cfg(unix)]
                            let builder = {
                                use cap_std::fs::DirBuilderExt;
                                let mut builder = DirBuilder::new();
                                builder.mode(0o700);
                                builder
                            };
                            #[cfg(not(unix))]
                            let builder = DirBuilder::new();
                            match parent.create_dir_with(name, &builder) {
                                Ok(()) => (),
                                Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                                }
                                Err(error) => return Err(error.into()),
                            }
                        }
                        Err(error) => return Err(error.into()),
                        Ok(meta) if !meta.is_dir() || meta.file_type().is_symlink() => {
                            return Err(Failure::Io);
                        }
                        Ok(_) => (),
                    }
                    // An entry observed above but missing now is drift, never absence.
                    let child = parent.open_dir_nofollow(name)?;
                    #[cfg(windows)]
                    check_directory(&child)?;
                    directories.push(child);
                }
                Component::CurDir => (),
                Component::ParentDir => return Err(Failure::Io),
            }
        }
        if directories.is_empty() {
            return Err(Failure::Io);
        }
        Ok(Some(Self(directories)))
    }

    fn dir(&self) -> &Dir {
        self.0
            .last()
            .expect("directory traversal always retains the root")
    }

    pub(super) fn read(&self) -> Result<Option<Vec<u8>>, Failure> {
        match self.dir().symlink_metadata(NAME) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
            Ok(meta) if !meta.is_file() || meta.file_type().is_symlink() => {
                return Err(Failure::Io);
            }
            Ok(_) => (),
        }
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        #[cfg(unix)]
        {
            use cap_fs_ext::OpenOptionsSyncExt;
            options.nonblock(true);
        }
        #[cfg(windows)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.share_mode(7); // READ | WRITE | DELETE: status is no reservation.
        }
        let file = self.dir().open_with(NAME, &options)?.into_std();
        let metadata = file.metadata()?;
        if !metadata.is_file() {
            return Err(Failure::Io);
        }
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            if metadata.file_attributes() & 0x400 != 0 {
                return Err(Failure::Io);
            }
        }
        if metadata.len() > MAX_BYTES as u64 {
            return Err(Failure::Oversized);
        }
        let mut bytes = Vec::new();
        file.take(MAX_BYTES as u64 + 1).read_to_end(&mut bytes)?;
        if bytes.len() > MAX_BYTES {
            return Err(Failure::Oversized);
        }
        Ok(Some(bytes))
    }

    pub(super) fn replace(&self, bytes: &[u8]) -> Result<(), Failure> {
        // Explicit setters replace malformed bytes; unsuitable filesystem objects refuse.
        match self.read() {
            Ok(_) | Err(Failure::Oversized) => (),
            Err(failure) => return Err(failure),
        }
        let name = format!("task-sync-setting-{}.tmp", uuid::Uuid::now_v7());
        let mut options = OpenOptions::new();
        options
            .write(true)
            .create_new(true)
            .follow(FollowSymlinks::No);
        #[cfg(unix)]
        {
            use cap_std::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        let mut file = self.dir().open_with(&name, &options)?.into_std();
        let result = (|| {
            file.write_all(bytes)?;
            file.sync_all()?;
            drop(file);
            self.dir().rename(&name, self.dir(), NAME)?;
            Ok(())
        })();
        // Only our exclusive temporary name is removed; a crash may leave it inert.
        let _ = self.dir().remove_file(name);
        result
    }
}

#[cfg(windows)]
fn check_directory(directory: &Dir) -> Result<(), Failure> {
    use std::os::windows::fs::MetadataExt;
    let metadata = directory.try_clone()?.into_std_file().metadata()?;
    if metadata.file_attributes() & 0x400 != 0 {
        return Err(Failure::Io);
    }
    Ok(())
}
