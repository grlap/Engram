//! Which store a connection serves: the canonical path spelling that
//! readiness and doctor report, and the check that a control connection is
//! still current.
//!
//! No interface here proves which file SQLite holds. A path observed before
//! and after SQLite's open can be redirected during it, through a renamed
//! file, a symbolic link or a directory junction, so two such observations
//! cannot tell the file SQLite opened from whatever the path names later.

use std::{io, path::Path};

use super::{SessionId, SqliteStore, StoreError, work};

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

impl SqliteStore {
    /// Verifies that `connection_token` is still the session's current
    /// control connection, on one read snapshot; it writes nothing.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::ControlConnectionSuperseded`] when a newer
    /// connection has replaced it, and other [`StoreError`] values when the
    /// store cannot be read.
    pub(crate) fn verify_current_control_connection(
        &self,
        session_id: &SessionId,
        connection_token: &str,
    ) -> Result<(), StoreError> {
        work::on_one_snapshot(&self.connection, |connection| {
            Self::verify_control_connection(connection, session_id, connection_token)
        })
    }
}
