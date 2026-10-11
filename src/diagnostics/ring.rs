//! One fixed home-wide ring, serialized between cooperating processes.
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use std::{
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom, Write},
    path::{Path, PathBuf},
};

pub(super) const FILE_BYTES: u64 = 1_048_576;
const FILES: usize = 4;

pub(super) trait Sink: Send {
    fn append(&self, line: &[u8]) -> io::Result<()>;
}

pub(super) struct Ring {
    home: PathBuf,
}

impl Ring {
    pub(super) fn new(home: &Path) -> Self {
        Self {
            home: home.to_owned(),
        }
    }

    fn name(index: usize) -> String {
        format!("engram-{index}.jsonl")
    }
}

impl Sink for Ring {
    fn append(&self, line: &[u8]) -> io::Result<()> {
        if line.len() > super::MAX_RECORD_BYTES {
            return Err(io::ErrorKind::InvalidData.into());
        }
        // Retain the parent and child capabilities until every file is closed.
        // No-follow opens close the metadata/open race for owned destinations.
        let home = Dir::open_ambient_dir(&self.home, ambient_authority())?;
        let directory = match home.open_dir_nofollow("diagnostics") {
            Ok(directory) => directory,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                match home.create_dir("diagnostics") {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
                    Err(error) => return Err(error),
                }
                home.open_dir_nofollow("diagnostics")?
            }
            Err(error) => return Err(error),
        };
        let lock = open(&directory, "engram.lock", false)?;
        lock.try_lock().map_err(|error| match error {
            fs::TryLockError::WouldBlock => io::Error::from(io::ErrorKind::WouldBlock),
            fs::TryLockError::Error(error) => error,
        })?;
        if lock.metadata()?.len() != 0 {
            return Err(io::ErrorKind::InvalidData.into());
        }
        for index in 0..FILES {
            check(&directory, &Self::name(index))?;
        }
        let mut file = open(&directory, &Self::name(0), false)?;
        let length = repair_tail(&mut file)?;
        for index in 0..FILES {
            if directory
                .metadata(Self::name(index))
                .is_ok_and(|metadata| metadata.len() > FILE_BYTES)
            {
                return Err(io::ErrorKind::InvalidData.into());
            }
        }
        if length.saturating_add(line.len() as u64) > FILE_BYTES {
            drop(file);
            match directory.remove_file(Self::name(FILES - 1)) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
            for index in (0..FILES - 1).rev() {
                match directory.rename(Self::name(index), &directory, Self::name(index + 1)) {
                    Ok(()) => {}
                    Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                    Err(error) => return Err(error),
                }
            }
            file = open(&directory, &Self::name(0), true)?;
        }
        let before = file.seek(SeekFrom::End(0))?;
        if let Err(error) = write_record(&mut file, line) {
            // Avoid appending after a failed partial record. A crashed writer's
            // torn tail is bounded and repaired under the same lock next time.
            let _ = file.set_len(before);
            return Err(error);
        }
        drop(file);
        drop(lock);
        Ok(())
    }
}

pub(super) fn write_record(writer: &mut impl Write, line: &[u8]) -> io::Result<()> {
    writer.write_all(line)?;
    writer.flush()
}

fn check(directory: &Dir, name: &str) -> io::Result<()> {
    let metadata = match directory.symlink_metadata(name) {
        Ok(value) => value,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    Ok(())
}

fn open(directory: &Dir, name: &str, create_new: bool) -> io::Result<File> {
    check(directory, name)?;
    let mut options = OpenOptions::new();
    options.read(true).write(true).follow(FollowSymlinks::No);
    if create_new {
        options.create_new(true);
    } else {
        options.create(true).truncate(false);
    }
    #[cfg(unix)]
    {
        use cap_fs_ext::OpenOptionsSyncExt;
        options.nonblock(true);
    }
    let file = directory.open_with(name, &options)?.into_std();
    let metadata = file.metadata()?;
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
        if metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0 {
            return Err(io::ErrorKind::InvalidInput.into());
        }
    }
    if !metadata.is_file() {
        return Err(io::ErrorKind::InvalidInput.into());
    }
    Ok(file)
}

fn repair_tail(file: &mut File) -> io::Result<u64> {
    let length = file.metadata()?.len();
    if length > FILE_BYTES {
        return Err(io::ErrorKind::InvalidData.into());
    }
    if length == 0 {
        return Ok(0);
    }
    let take = length.min(super::MAX_RECORD_BYTES as u64);
    file.seek(SeekFrom::Start(length - take))?;
    let mut tail = vec![0; usize::try_from(take).map_err(|_| io::ErrorKind::InvalidData)?];
    file.read_exact(&mut tail)?;
    if tail.last() == Some(&b'\n') {
        return Ok(length);
    }
    let end = tail
        .iter()
        .rposition(|byte| *byte == b'\n')
        .map_or(length - take, |index| length - take + index as u64 + 1);
    if end != 0 && !tail.contains(&b'\n') {
        return Err(io::ErrorKind::InvalidData.into());
    }
    file.set_len(end)?;
    Ok(end)
}
