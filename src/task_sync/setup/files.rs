//! Retained no-follow directory handles keep private publication inside Git admin.

use anyhow::{Context, Result, ensure};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, OpenOptions};
use std::{
    ffi::OsString,
    io::Read,
    path::{Component, Path},
};

#[derive(Clone, Copy, Debug, serde::Deserialize, Eq, PartialEq, serde::Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct PhysicalId {
    device: u64,
    inode: u64,
}

impl PhysicalId {
    pub fn same_device(self, other: Self) -> bool {
        self.device == other.device
    }
}

pub(super) fn file_identity(file: &std::fs::File) -> Result<PhysicalId> {
    identity(&cap_std::fs::Metadata::from_file(file)?)
}

fn identity(metadata: &cap_std::fs::Metadata) -> Result<PhysicalId> {
    use cap_fs_ext::MetadataExt;
    let id = PhysicalId {
        device: metadata.dev(),
        inode: metadata.ino(),
    };
    ensure!(
        id.inode != 0,
        "filesystem did not supply a physical file identity"
    );
    Ok(id)
}

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

    pub fn identity(&self) -> Result<PhysicalId> {
        identity(&self.dir().dir_metadata()?)
    }

    pub fn child(&self, name: &Path) -> Result<Self> {
        one_name(name)?;
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

    pub fn create_child(&self, name: &Path) -> Result<Self> {
        one_name(name)?;
        let builder = {
            #[cfg(unix)]
            {
                use cap_std::fs::DirBuilderExt;
                let mut builder = cap_std::fs::DirBuilder::new();
                builder.mode(0o700);
                builder
            }
            #[cfg(not(unix))]
            cap_std::fs::DirBuilder::new()
        };
        self.dir().create_dir_with(name, &builder)?;
        self.child(name)
    }

    pub fn names(&self, limit: usize) -> Result<Vec<OsString>> {
        let mut names = Vec::new();
        for entry in self.dir().entries()? {
            ensure!(
                names.len() < limit,
                "materialization directory entry bound exceeded"
            );
            names.push(entry?.file_name());
        }
        names.sort();
        Ok(names)
    }

    pub fn names_with_room(&self, limit: usize, published: &[&str]) -> Result<Vec<OsString>> {
        let names = self.names(limit)?;
        let missing = published
            .iter()
            .filter(|expected| {
                !names
                    .iter()
                    .any(|name| name.as_os_str() == std::ffi::OsStr::new(**expected))
            })
            .count();
        ensure!(
            names.len() + missing <= limit,
            "materialization directory has no room for declared publication within entry bound"
        );
        Ok(names)
    }

    pub fn open_file(&self, name: &Path, write: bool, create: bool) -> Result<std::fs::File> {
        one_name(name)?;
        let mut options = OpenOptions::new();
        options
            .read(true)
            .write(write)
            .create_new(create)
            .follow(FollowSymlinks::No);
        #[cfg(windows)]
        if !write {
            use cap_std::fs::OpenOptionsExt;
            options.share_mode(1);
        }
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
        self.read_bounded(name, MAX_PRIVATE_BYTES)
    }

    pub fn read_bounded(&self, name: &Path, limit: usize) -> Result<Option<Vec<u8>>> {
        ensure!(
            limit <= MAX_PRIVATE_BYTES,
            "setup read bound exceeds private limit"
        );
        match self.dir().symlink_metadata(name) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
            Ok(meta) => ensure!(
                meta.is_file() && !meta.file_type().is_symlink() && meta.len() <= limit as u64,
                "setup input must be an ordinary bounded file"
            ),
        }
        let file = self.open_file(name, false, false)?;
        ensure!(
            file.metadata()?.len() <= limit as u64,
            "setup input exceeded byte bound"
        );
        let mut bytes = Vec::new();
        file.take(limit as u64 + 1).read_to_end(&mut bytes)?;
        ensure!(bytes.len() <= limit, "setup input grew past byte bound");
        Ok(Some(bytes))
    }
}

fn one_name(name: &Path) -> Result<()> {
    ensure!(
        matches!(
            name.components().collect::<Vec<_>>().as_slice(),
            [Component::Normal(_)]
        ),
        "expected one ordinary child name"
    );
    Ok(())
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
