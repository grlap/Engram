//! Canonical store path spelling for readiness and doctor.

use std::{io, path::Path};

/// The canonical spelling of a store's database path, as readiness and
/// doctor report it: symbolic links and junctions resolved, and on Windows
/// the verbatim `\\?\` prefix removed (`\\?\UNC\server\share` becomes
/// `\\server\share`). It keeps the case and spelling the filesystem returns.
///
/// # Errors
///
/// Returns the I/O error when the path cannot be canonicalized, for example
/// because nothing exists there.
pub fn canonical_store_path(path: &Path) -> io::Result<String> {
    Ok(store_path_text(&std::fs::canonicalize(path)?))
}

/// A path's text without the Windows verbatim prefix, as
/// [`canonical_store_path`] spells it; elsewhere the path's display text.
#[must_use]
pub fn store_path_text(path: &Path) -> String {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        if let Some(rest) = text.strip_prefix(r"\\?\UNC\") {
            format!(r"\\{rest}")
        } else if let Some(rest) = text.strip_prefix(r"\\?\") {
            rest.to_owned()
        } else {
            text.into_owned()
        }
    }
    #[cfg(not(windows))]
    {
        path.display().to_string()
    }
}

#[cfg(all(test, windows))]
mod tests {
    use super::*;

    #[test]
    fn windows_store_paths_omit_the_verbatim_prefix() {
        assert_eq!(
            store_path_text(Path::new(r"\\?\C:\store\x.db")),
            r"C:\store\x.db"
        );
        assert_eq!(
            store_path_text(Path::new(r"\\?\UNC\server\share\x.db")),
            r"\\server\share\x.db"
        );
        assert_eq!(
            store_path_text(Path::new(r"C:\plain\x.db")),
            r"C:\plain\x.db"
        );
    }
}
