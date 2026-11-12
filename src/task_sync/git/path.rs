use std::path::{Path, PathBuf};

/// Spell a path for Git without Windows' verbatim prefix. This changes only
/// the argument spelling; filesystem identity must still be checked separately.
/// Windows device paths, volumes without a drive letter and non-Unicode paths
/// have no supported Git spelling. Other hosts preserve the original path.
#[must_use]
#[cfg(windows)]
pub fn plain_path(path: &Path) -> Option<PathBuf> {
    use std::path::{Component, Prefix};
    path.to_str()?;
    let mut components = path.components();
    let mut plain = match components.next() {
        Some(Component::Prefix(prefix)) => match prefix.kind() {
            Prefix::VerbatimDisk(letter) => PathBuf::from(format!("{}:", char::from(letter))),
            Prefix::VerbatimUNC(server, share) => {
                let mut unc = std::ffi::OsString::from(r"\\");
                unc.push(server);
                unc.push(r"\");
                unc.push(share);
                PathBuf::from(unc)
            }
            Prefix::Disk(_) | Prefix::UNC(..) => return Some(path.to_path_buf()),
            Prefix::Verbatim(_) | Prefix::DeviceNS(_) => return None,
        },
        _ => return Some(path.to_path_buf()),
    };
    for component in components {
        plain.push(component);
    }
    Some(plain)
}

/// Preserve a non-Windows path's spelling for Git.
#[must_use]
#[cfg(not(windows))]
#[allow(
    clippy::unnecessary_wraps,
    reason = "the Windows form has paths without a plain form"
)]
pub fn plain_path(path: &Path) -> Option<PathBuf> {
    Some(path.to_path_buf())
}
