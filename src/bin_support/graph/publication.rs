//! No-replace snapshot publication through retained directory capabilities.

use anyhow::{Context, Result, bail};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::{
    ambient_authority,
    fs::{Dir, OpenOptions},
};
use engram::graph_snapshot_files_are_equivalent;
use std::{
    ffi::OsString,
    io::{self, Read, Write},
    path::{Component, Path, PathBuf},
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum GraphSnapshotWriteOutcome {
    Saved,
    AlreadySaved,
}

struct Destination {
    // Keep the complete chain alive. On Windows these handles deny delete
    // sharing, protecting the paths cap-std reconstructs for link/removal.
    directories: Vec<Dir>,
    _protected: same_file::Handle,
    name: OsString,
    path: PathBuf,
}

impl Destination {
    fn open(database: &Path, out: &Path) -> Result<Self> {
        // components()/absolute() can erase directory-only spelling. Never
        // reinterpret such a request as a regular-file destination.
        let spelling = out.as_os_str().as_encoded_bytes();
        if spelling
            .last()
            .is_some_and(|byte| *byte == b'/' || (cfg!(windows) && *byte == b'\\'))
            || (cfg!(unix) && spelling.ends_with(b"/."))
        {
            bail!(
                "snapshot destination {} uses a directory-only spelling; supply a file name without a trailing separator",
                out.display()
            );
        }
        // Validate before Win32 absolute-path normalization can erase a final
        // dot or space. The absolute form is checked again below.
        #[cfg(windows)]
        for component in out.components() {
            if let Component::Normal(name) = component {
                validate_windows_component(name)?;
            }
        }
        let (home, _) = super::engram_home_and_project_digest(database)?;
        let projects_path = home.join("projects");
        let projects =
            Dir::open_ambient_dir(&projects_path, ambient_authority()).with_context(|| {
                format!(
                    "failed to open protected Engram project stores {}",
                    projects_path.display()
                )
            })?;
        let protected =
            same_file::Handle::from_file(projects.into_std_file()).with_context(|| {
                format!(
                    "failed to identify protected Engram project stores {}",
                    projects_path.display()
                )
            })?;
        let absolute = std::path::absolute(out)
            .with_context(|| format!("failed to resolve snapshot destination {}", out.display()))?;
        let (root, names) = destination_components(&absolute)?;
        let (name, parents) = names
            .split_last()
            .context("snapshot destination needs a file name")?;
        let mut walked = root.clone();
        let mut directories = vec![
            Dir::open_ambient_dir(root, ambient_authority())
                .with_context(|| format!("cannot open snapshot root {}", walked.display()))?,
        ];
        validate_directory(&directories[0], &protected, &walked)?;
        #[cfg(windows)]
        validate_volume_root(&directories[0])
            .with_context(|| format!("cannot validate snapshot root {}", walked.display()))?;
        for component in parents {
            walked.push(component);
            let parent = directories.last().expect("root directory exists");
            let child = match parent.open_dir_nofollow(component) {
                Ok(child) => child,
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    match parent.create_dir(component) {
                        Ok(()) => (),
                        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => (),
                        Err(error) => {
                            return Err(error).with_context(|| {
                                format!("failed to create snapshot directory {}", walked.display())
                            });
                        }
                    }
                    parent
                        .open_dir_nofollow(component)
                        .with_context(|| ancestor_open_context(parent, component, &walked))?
                }
                Err(error) => {
                    return Err(error)
                        .with_context(|| ancestor_open_context(parent, component, &walked));
                }
            };
            validate_directory(&child, &protected, &walked)?;
            directories.push(child);
        }
        Ok(Self {
            directories,
            _protected: protected,
            name: name.clone(),
            path: walked.join(name),
        })
    }

    fn parent(&self) -> &Dir {
        self.directories.last().expect("root directory exists")
    }

    fn existing(&self, bytes: &[u8]) -> Result<bool> {
        self.existing_after_inspection(bytes, || Ok(()))
    }

    fn existing_after_inspection(
        &self,
        bytes: &[u8],
        after_inspection: impl FnOnce() -> Result<()>,
    ) -> Result<bool> {
        let metadata = match self.parent().symlink_metadata(&self.name) {
            Ok(metadata) => metadata,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to inspect snapshot destination {}",
                        self.path.display()
                    )
                });
            }
        };
        require_regular_destination(&metadata, &self.path)?;
        let limit = super::MAX_GRAPH_SNAPSHOT_BYTES;
        require_destination_size(metadata.len(), limit, &self.path)?;
        after_inspection()?;
        let mut options = OpenOptions::new();
        options.read(true).follow(FollowSymlinks::No);
        #[cfg(unix)]
        {
            use cap_fs_ext::OpenOptionsSyncExt;
            // A FIFO swapped in after metadata inspection must not block open.
            options.nonblock(true);
        }
        let file = match self.parent().open_with(&self.name, &options) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(false),
            Err(error) => {
                return Err(error).with_context(|| {
                    format!(
                        "failed to open snapshot destination {} without following links",
                        self.path.display()
                    )
                });
            }
        };
        reject_reparse_file(&file, &self.path)?;
        let opened = file.metadata().with_context(|| {
            format!(
                "failed to inspect opened snapshot destination {}",
                self.path.display()
            )
        })?;
        require_regular_destination(&opened, &self.path)?;
        require_destination_size(opened.len(), limit, &self.path)?;
        let existing = read_existing_bounded(file, limit, &self.path)?;
        if !graph_snapshot_files_are_equivalent(&existing, bytes) {
            bail!(
                "snapshot destination {} already exists with different bytes",
                self.path.display()
            );
        }
        Ok(true)
    }
}

fn require_regular_destination(metadata: &cap_std::fs::Metadata, path: &Path) -> Result<()> {
    if !metadata.is_file() {
        bail!(
            "snapshot destination {} is not a regular file; links and special files are refused",
            path.display()
        );
    }
    Ok(())
}

fn require_destination_size(length: u64, limit: u64, path: &Path) -> Result<()> {
    if length > limit {
        bail!(
            "snapshot destination {} exceeds the {limit}-byte comparison limit",
            path.display()
        );
    }
    Ok(())
}

fn read_existing_bounded(reader: impl Read, limit: u64, path: &Path) -> Result<Vec<u8>> {
    let mut existing = Vec::new();
    reader
        .take(limit.saturating_add(1))
        .read_to_end(&mut existing)
        .with_context(|| format!("failed to read snapshot destination {}", path.display()))?;
    require_destination_size(u64::try_from(existing.len())?, limit, path)?;
    Ok(existing)
}

#[cfg(windows)]
fn validate_volume_root(root: &Dir) -> Result<()> {
    // cap-std resolves the parent from the already opened handle, not the
    // caller's drive spelling. A SUBST root below a volume has a different
    // parent and cannot bypass the protected-directory walk.
    let parent = root
        .open_parent_dir(ambient_authority())
        .context("cannot verify snapshot drive root")?;
    if same_file::Handle::from_file(root.try_clone()?.into_std_file())?
        != same_file::Handle::from_file(parent.into_std_file())?
    {
        bail!(
            "snapshot destination drive is not a volume root; drive aliases such as SUBST are unsupported; pass the real volume path instead"
        );
    }
    Ok(())
}

fn ancestor_open_context(parent: &Dir, name: &std::ffi::OsStr, path: &Path) -> String {
    if parent
        .symlink_metadata(name)
        .is_ok_and(|metadata| metadata.file_type().is_symlink())
    {
        ancestor_refusal(path)
    } else {
        format!("cannot open snapshot ancestor {}", path.display())
    }
}

fn ancestor_refusal(path: &Path) -> String {
    format!(
        "cannot bind snapshot ancestor {}: the entire destination path must use real directories, without symlinks or reparse points (including system links, linked homes and cloud placeholders); pass the resolved real directory path instead",
        path.display()
    )
}

#[cfg(windows)]
fn validate_windows_component(name: &std::ffi::OsStr) -> Result<()> {
    let text = name.to_string_lossy();
    let stem = text
        .split('.')
        .next()
        .unwrap_or_default()
        .trim_end_matches(' ')
        .to_ascii_uppercase();
    let reserved = matches!(
        stem.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || ["COM", "LPT"].iter().any(|prefix| {
        stem.strip_prefix(prefix).is_some_and(|suffix| {
            matches!(
                suffix,
                "0" | "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
            )
        })
    });
    // Verbatim paths can carry '/' inside a Normal component;
    // cap-std would then interpret it as another path boundary.
    if text.contains([':', '/', '\\']) || text.ends_with(['.', ' ']) || reserved {
        bail!("snapshot destination contains an unsupported Windows path component");
    }
    Ok(())
}

fn destination_components(path: &Path) -> Result<(PathBuf, Vec<OsString>)> {
    let mut root = PathBuf::new();
    let mut names = Vec::new();
    for component in path.components() {
        match component {
            Component::Prefix(prefix) => {
                #[cfg(windows)]
                match prefix.kind() {
                    std::path::Prefix::Disk(drive) | std::path::Prefix::VerbatimDisk(drive) => {
                        root.push(format!("{}:\\", char::from(drive)));
                    }
                    _ => bail!(
                        "snapshot file publication supports local drive paths, not UNC or device paths"
                    ),
                }
                #[cfg(not(windows))]
                root.push(prefix.as_os_str());
            }
            Component::RootDir => {
                if root.as_os_str().is_empty() {
                    root.push(component.as_os_str());
                }
            }
            Component::Normal(name) => {
                #[cfg(windows)]
                validate_windows_component(name)?;
                names.push(name.to_os_string());
            }
            Component::CurDir => (),
            Component::ParentDir => {
                #[cfg(unix)]
                bail!("snapshot destination must not contain parent-directory components");
                #[cfg(not(unix))]
                names
                    .pop()
                    .context("snapshot path escapes its filesystem root")?;
            }
        }
    }
    Ok((root, names))
}

fn validate_directory(directory: &Dir, protected: &same_file::Handle, path: &Path) -> Result<()> {
    reject_reparse_directory(directory, path)?;
    let identity = directory
        .try_clone()
        .and_then(|copy| same_file::Handle::from_file(copy.into_std_file()))
        .with_context(|| format!("cannot identify snapshot ancestor {}", path.display()))?;
    if identity == *protected {
        bail!("snapshot destination must be outside Engram's project stores");
    }
    Ok(())
}

fn reject_reparse_directory(directory: &Dir, path: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        let file = directory
            .try_clone()
            .with_context(|| {
                format!(
                    "cannot retain snapshot ancestor {} for inspection",
                    path.display()
                )
            })?
            .into_std_file();
        if file
            .metadata()
            .with_context(|| format!("cannot inspect snapshot ancestor {}", path.display()))?
            .file_attributes()
            & 0x400
            != 0
        {
            bail!("{}", ancestor_refusal(path));
        }
    }
    #[cfg(not(windows))]
    let _ = (directory, path);
    Ok(())
}

fn reject_reparse_file(file: &cap_std::fs::File, path: &Path) -> Result<()> {
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;
        let metadata = file
            .try_clone()
            .and_then(|copy| copy.into_std().metadata())
            .with_context(|| {
                format!(
                    "cannot inspect snapshot destination {} for reparse attributes",
                    path.display()
                )
            })?;
        if metadata.file_attributes() & 0x400 != 0 {
            bail!(
                "snapshot destination {} is a Windows reparse point",
                path.display()
            );
        }
    }
    #[cfg(not(windows))]
    let _ = (file, path);
    Ok(())
}

trait Publication {
    fn write_stage(&self, file: &mut cap_std::fs::File, bytes: &[u8]) -> io::Result<()> {
        file.write_all(bytes).and_then(|()| file.sync_all())
    }
    fn bound(&self) -> Result<()> {
        Ok(())
    }
    fn link(&self, parent: &Dir, stage: &Path, name: &Path) -> io::Result<()> {
        parent.hard_link(stage, parent, name)
    }
    fn cleanup(&self, parent: &Dir, stage: &Path) -> io::Result<()> {
        parent.remove_file(stage)
    }
    fn warning(&self, warning: &str) {
        eprintln!("WARNING: {warning}");
    }
    fn sync(&self, parent: &Dir) -> io::Result<()> {
        #[cfg(unix)]
        // open_dir may return O_PATH on Linux, which cannot be fsynced.
        // A normal read descriptor for the retained directory supports fsync.
        parent.open(".")?.sync_all()?;
        #[cfg(not(unix))]
        let _ = parent;
        Ok(())
    }
}
struct Filesystem;
impl Publication for Filesystem {}

pub(super) fn write_graph_snapshot_file(
    database: &Path,
    out: &Path,
    bytes: &[u8],
) -> Result<GraphSnapshotWriteOutcome> {
    write_with(database, out, bytes, &Filesystem)
}

fn write_with(
    database: &Path,
    out: &Path,
    bytes: &[u8],
    operations: &impl Publication,
) -> Result<GraphSnapshotWriteOutcome> {
    let destination = Destination::open(database, out)?;
    operations.bound()?;
    if destination.existing(bytes)? {
        return Ok(GraphSnapshotWriteOutcome::AlreadySaved);
    }
    let parent = destination.parent();
    let stage = PathBuf::from(format!(".graph-save-{}.tmp", uuid::Uuid::now_v7()));
    let stage_path = destination.path.with_file_name(&stage);
    let mut options = OpenOptions::new();
    options
        .create_new(true)
        .write(true)
        .follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = parent.open_with(&stage, &options).with_context(|| {
        format!(
            "failed to create private snapshot stage {}",
            stage_path.display()
        )
    })?;
    let staged = operations.write_stage(&mut file, bytes);
    drop(file);
    if let Err(error) = staged {
        cleanup_stage(&destination, &stage, operations, false);
        return Err(error).with_context(|| {
            format!(
                "failed to write and sync snapshot stage {} for {}",
                stage_path.display(),
                destination.path.display()
            )
        });
    }
    let outcome = match operations.link(parent, &stage, Path::new(&destination.name)) {
        Ok(()) => Ok(GraphSnapshotWriteOutcome::Saved),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            match destination.existing(bytes) {
                Ok(true) => Ok(GraphSnapshotWriteOutcome::AlreadySaved),
                Ok(false) => {
                    let context = "snapshot no-replace publication race: competing destination disappeared before inspection";
                    Err(error).context(context)
                }
                Err(error) => Err(error),
            }
        }
        Err(error) => {
            let context = "failed to publish snapshot without replacement; publication requires hard-link support";
            Err(error).context(context)
        }
    };
    cleanup_stage(&destination, &stage, operations, outcome.is_ok());
    let outcome = outcome.with_context(|| {
        format!(
            "snapshot publication to {} using stage {} failed",
            destination.path.display(),
            stage_path.display()
        )
    })?;
    operations.sync(parent).with_context(|| {
        let detail = "snapshot directory sync failed; durability is not confirmed";
        format!(
            "snapshot published at {}, but {detail}",
            destination.path.display()
        )
    })?;
    Ok(outcome)
}

fn cleanup_stage(
    destination: &Destination,
    stage: &Path,
    operations: &impl Publication,
    published: bool,
) {
    if let Err(error) = operations.cleanup(destination.parent(), stage) {
        let status = if published {
            "snapshot saved"
        } else {
            "snapshot publication failed"
        };
        let path = destination.path.display();
        let stage_path = destination.path.with_file_name(stage);
        let stage_path = stage_path.display();
        let disclosure = "this file may contain snapshot disclosure data";
        operations.warning(&format!(
            "{status} at {path}; unable to remove staging file {stage_path}: {error}; {disclosure}"
        ));
    }
}

#[cfg(test)]
mod tests;
