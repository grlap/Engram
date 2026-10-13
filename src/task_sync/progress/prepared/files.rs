//! Read-only capabilities. All ancestors stay open; no path is created or followed.

use anyhow::{Context, Result, ensure};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use std::{
    ffi::OsString,
    io::Read,
    path::{Component, Path},
};

pub(super) struct Directory(Vec<Dir>);

impl Directory {
    pub(super) fn open(path: &Path) -> Result<Self> {
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
                        #[cfg(windows)]
                        check_directory(&directories[0])?;
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
                Component::ParentDir => {
                    anyhow::bail!("unresolved parent component in prepared path")
                }
            }
        }
        if directories.is_empty() {
            directories.push(Dir::open_ambient_dir(anchor, cap_std::ambient_authority())?);
            #[cfg(windows)]
            check_directory(&directories[0])?;
        }
        Ok(Self(directories))
    }

    fn dir(&self) -> &Dir {
        self.0.last().expect("retained directory")
    }

    pub(super) fn child(&self, name: &Path) -> Result<Self> {
        ensure!(
            matches!(
                name.components().collect::<Vec<_>>().as_slice(),
                [Component::Normal(_)]
            ),
            "expected one child directory name"
        );
        let mut directories = self
            .0
            .iter()
            .map(Dir::try_clone)
            .collect::<std::io::Result<Vec<_>>>()?;
        let child = self.dir().open_dir_nofollow(name)?;
        #[cfg(windows)]
        check_directory(&child)?;
        directories.push(child);
        Ok(Self(directories))
    }

    pub(super) fn same_directory(&self, other: &Self) -> Result<bool> {
        let left = same_file::Handle::from_file(self.dir().try_clone()?.into_std_file())?;
        let right = same_file::Handle::from_file(other.dir().try_clone()?.into_std_file())?;
        Ok(left == right)
    }

    pub(super) fn names(&self, limit: usize) -> Result<Vec<OsString>> {
        let mut names = Vec::new();
        for entry in self.dir().entries()? {
            ensure!(
                names.len() < limit,
                "prepared directory entry bound exceeded"
            );
            names.push(entry?.file_name());
        }
        names.sort();
        Ok(names)
    }

    pub(super) fn check_file(&self, name: &Path, limit: usize) -> Result<()> {
        self.open_file(name, limit).map(drop)
    }

    pub(super) fn read(&self, name: &Path, limit: usize) -> Result<Vec<u8>> {
        let file = self.open_file(name, limit)?;
        let mut bytes = Vec::new();
        file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= limit, "prepared input grew past byte bound");
        Ok(bytes)
    }

    fn open_file(&self, name: &Path, limit: usize) -> Result<std::fs::File> {
        ensure!(
            matches!(
                name.components().collect::<Vec<_>>().as_slice(),
                [Component::Normal(_)]
            ),
            "expected one file name"
        );
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        #[cfg(unix)]
        {
            use cap_fs_ext::OpenOptionsSyncExt;
            options.nonblock(true);
        }
        let file = self.dir().open_with(name, &options)?.into_std();
        let meta = file.metadata()?;
        ensure!(
            meta.is_file() && meta.len() <= limit as u64,
            "prepared input must be an ordinary bounded file"
        );
        #[cfg(windows)]
        {
            use std::os::windows::fs::MetadataExt;
            ensure!(
                meta.file_attributes() & 0x400 == 0,
                "prepared file is a reparse point"
            );
        }
        Ok(file)
    }
}

#[cfg(windows)]
fn check_directory(directory: &Dir) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        ensure!(
            directory
                .try_clone()?
                .into_std_file()
                .metadata()?
                .file_attributes()
                & 0x400
                == 0,
            "prepared ancestor is a reparse point"
        );
    }
    Ok(())
}
