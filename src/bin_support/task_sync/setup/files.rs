//! Retained no-follow directory handles keep private publication inside Git admin.

use anyhow::{Context, Result, ensure};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use std::{
    io::Read,
    path::{Component, Path},
};

pub(super) struct Directory(Vec<Dir>);
pub(super) const MAX_PRIVATE_BYTES: usize = 2 * 1024 * 1024;

impl Directory {
    pub fn open(path: &Path) -> Result<Self> {
        let absolute = std::path::absolute(path)?;
        let mut anchor = std::path::PathBuf::new();
        let mut directories = Vec::new();
        for component in absolute.components() {
            match component {
                Component::Prefix(_) | Component::RootDir => anchor.push(component),
                Component::Normal(name) => {
                    if directories.is_empty() {
                        directories.push(Dir::open_ambient_dir(
                            &anchor,
                            cap_std::ambient_authority(),
                        )?);
                    }
                    let child = directories
                        .last()
                        .context("directory root missing")?
                        .open_dir_nofollow(name)?;
                    #[cfg(windows)]
                    check_directory(&child)?;
                    directories.push(child);
                }
                Component::CurDir => (),
                Component::ParentDir => anyhow::bail!("unresolved parent component in setup path"),
            }
        }
        if directories.is_empty() {
            directories.push(Dir::open_ambient_dir(anchor, cap_std::ambient_authority())?);
        }
        #[cfg(windows)]
        check_directory(&directories[0])?;
        Ok(Self(directories))
    }

    pub fn dir(&self) -> &Dir {
        self.0.last().expect("retained directory")
    }

    pub fn open_file(&self, name: &Path, write: bool, create: bool) -> Result<std::fs::File> {
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(write)
            .create_new(create)
            .follow(FollowSymlinks::No);
        #[cfg(unix)]
        {
            use cap_fs_ext::OpenOptionsSyncExt;
            use cap_std::fs::OpenOptionsExt;
            options.nonblock(true).mode(0o600);
        }
        let file = self.dir().open_with(name, &options)?.into_std();
        let meta = file.metadata()?;
        ensure!(meta.is_file(), "setup input must be an ordinary file");
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            ensure!(
                meta.file_attributes() & 0x400 == 0,
                "setup file is a reparse point"
            );
        }
        Ok(file)
    }

    pub fn read(&self, name: &Path) -> Result<Option<Vec<u8>>> {
        match self.dir().symlink_metadata(name) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
            Ok(meta) => ensure!(
                meta.is_file()
                    && !meta.file_type().is_symlink()
                    && meta.len() <= MAX_PRIVATE_BYTES as u64,
                "setup input must be an ordinary bounded file"
            ),
        }
        let file = self.open_file(name, false, false)?;
        ensure!(
            file.metadata()?.len() <= MAX_PRIVATE_BYTES as u64,
            "setup input exceeded byte bound"
        );
        let mut bytes = Vec::new();
        file.take(MAX_PRIVATE_BYTES as u64 + 1)
            .read_to_end(&mut bytes)?;
        ensure!(
            bytes.len() <= MAX_PRIVATE_BYTES,
            "setup input grew past byte bound"
        );
        Ok(Some(bytes))
    }
}

#[cfg(windows)]
fn check_directory(directory: &Dir) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        let meta = directory.try_clone()?.into_std_file().metadata()?;
        ensure!(
            meta.file_attributes() & 0x400 == 0,
            "setup ancestor is a reparse point"
        );
    }
    Ok(())
}
