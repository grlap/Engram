//! SQLite filename arguments, separate from logical paths in stored receipts.

use std::path::Path;

use rusqlite::{Connection, OpenFlags};

use super::StoreError;

/// Opens a disk file with the caller's flags and the locking Windows long-path VFS.
/// Logical paths are not replaced by the temporary native filename argument.
///
/// # Errors
/// Refuses unrepresentable filenames and reports SQLite open failures.
pub fn open_sqlite_file(path: &Path, flags: OpenFlags) -> Result<Connection, StoreError> {
    if path.as_os_str().is_empty() || path == Path::new(":memory:") {
        return Connection::open_with_flags(path, flags).map_err(StoreError::Sqlite);
    }
    open_filename(path, &sqlite_filename(path)?, flags)
}

fn open_filename(path: &Path, name: &str, flags: OpenFlags) -> Result<Connection, StoreError> {
    #[cfg(windows)]
    let opened = Connection::open_with_flags_and_vfs(name, flags, "win32-longpath");
    #[cfg(not(windows))]
    let opened = Connection::open_with_flags(name, flags);
    opened.map_err(|source| match source.sqlite_error_code() {
        Some(rusqlite::ErrorCode::CannotOpen | rusqlite::ErrorCode::SystemIoFailure) => {
            StoreError::SqliteFile {
                path: path.to_path_buf(),
                utf16_length: path_length(path),
                source: Box::new(source),
            }
        }
        _ => StoreError::Sqlite(source),
    })
}

/// Checks filename conversion and capacity without opening or creating a file.
/// It does not promise that future filesystem access will succeed.
///
/// # Errors
/// Refuses a filename that SQLite cannot represent on this host.
pub fn validate_sqlite_file_path(path: &Path) -> Result<(), StoreError> {
    sqlite_filename(path).map(|_| ())
}

pub(super) fn sqlite_filename(path: &Path) -> Result<String, StoreError> {
    #[cfg(not(windows))]
    {
        // Preserve non-Windows filename handling; rusqlite requires UTF-8 too.
        path.to_str()
            .map(str::to_owned)
            .ok_or_else(|| invalid_path(path, "filename is not UTF-8"))
    }
    #[cfg(windows)]
    {
        const SUFFIX_AND_NUL: usize = "-journal".len() + 1;
        let absolute = std::path::absolute(path).map_err(|error| {
            invalid_path(path, &format!("cannot resolve absolute filename: {error}"))
        })?;
        if !matches!(absolute.components().next(), Some(std::path::Component::Prefix(prefix))
            if matches!(prefix.kind(), std::path::Prefix::Disk(_) | std::path::Prefix::UNC(_, _)
                | std::path::Prefix::VerbatimDisk(_) | std::path::Prefix::VerbatimUNC(_, _)))
        {
            return Err(invalid_path(
                path,
                "filename must name a drive or UNC file, not a device",
            ));
        }
        let text = absolute
            .to_str()
            .ok_or_else(|| invalid_path(path, "filename contains unpaired UTF-16 surrogates"))?;
        let native = if text.starts_with(r"\\?\") {
            text.to_owned()
        } else if let Some(unc) = text.strip_prefix(r"\\") {
            format!(r"\\?\UNC\{unc}")
        } else if absolute.is_absolute() {
            format!(r"\\?\{text}")
        } else {
            return Err(invalid_path(
                path,
                "filename is not an absolute drive or UNC path",
            ));
        };
        if native.contains('\0') {
            return Err(invalid_path(path, "filename contains NUL"));
        }
        // Reserve SQLite's longest journal suffix and its terminating NUL.
        // The bundled locking long-path VFS has 65534 bytes of filename capacity.
        if native.encode_utf16().count() + SUFFIX_AND_NUL > 32767
            || native.len() + SUFFIX_AND_NUL > 65534
        {
            return Err(invalid_path(
                path,
                "filename exceeds Windows/SQLite long-path capacity, including journal suffix",
            ));
        }
        Ok(native)
    }
}

fn invalid_path(path: &Path, reason: &str) -> StoreError {
    StoreError::SqlitePath {
        path: path.to_path_buf(),
        utf16_length: path_length(path),
        reason: reason.to_owned(),
    }
}

pub(super) fn file_io_error(path: &Path, source: std::io::Error) -> StoreError {
    StoreError::StoreFileIo {
        path: path.to_path_buf(),
        utf16_length: path_length(path),
        source,
    }
}

fn path_length(path: &Path) -> usize {
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        path.as_os_str().encode_wide().count()
    }
    #[cfg(not(windows))]
    {
        path.as_os_str().to_string_lossy().encode_utf16().count()
    }
}

pub(super) fn open_immutable(path: &Path) -> Result<Connection, StoreError> {
    let name = immutable_uri(path)?;
    open_filename(
        path,
        &name,
        OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI,
    )
}

fn immutable_uri(path: &Path) -> Result<String, StoreError> {
    let absolute = std::path::absolute(path)
        .map_err(|error| invalid_path(path, &format!("cannot resolve filename: {error}")))?;
    let name = sqlite_filename(&absolute)?;
    // On Windows the parser yields one URI slash followed by the native
    // verbatim path. winFullPathname removes that slash, preserving \\?\.
    #[cfg(windows)]
    let name = format!("/{name}");
    let mut uri = String::from("file:");
    for byte in name.bytes() {
        if byte.is_ascii_alphanumeric() || matches!(byte, b'/' | b':' | b'-' | b'_' | b'.' | b'~') {
            uri.push(char::from(byte));
        } else {
            use std::fmt::Write as _;
            write!(uri, "%{byte:02X}").expect("writing to String");
        }
    }
    uri.push_str("?immutable=1");
    Ok(uri)
}

pub(super) fn vacuum_into(connection: &Connection, path: &Path) -> Result<(), StoreError> {
    let target = sqlite_filename(path)?;
    connection
        .execute("VACUUM INTO ?1", [&target])
        .map_err(|source| StoreError::SqliteFile {
            path: path.to_path_buf(),
            utf16_length: path_length(path),
            source: Box::new(source),
        })?;
    Ok(())
}

#[cfg(test)]
mod tests;
