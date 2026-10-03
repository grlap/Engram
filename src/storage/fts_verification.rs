//! Read-only verification shared by the full-text projections: one pass over
//! a table's stored content, SQLite's own index check, and the rule that
//! separates a damaged table (a bounded finding) from an operational failure
//! (an error the caller sees).

use std::collections::HashMap;

use rusqlite::Connection;

use super::StoreError;

/// A finding label naming `prefix` and a bounded, printable copy of SQLite's
/// own words.
pub(super) fn bounded_finding(prefix: &str, detail: &str) -> String {
    let bounded = detail
        .chars()
        .take(160)
        .map(|ch| {
            if ch.is_ascii_graphic() || ch == ' ' {
                ch
            } else {
                ' '
            }
        })
        .collect::<String>();
    let bounded = bounded.trim();
    format!(
        "{prefix}:{}",
        if bounded.is_empty() {
            "unknown"
        } else {
            bounded
        }
    )
}

/// A damaged table's detail, or the operational failure (busy, I/O, memory
/// and the like) as an error: only corruption becomes a finding.
fn corruption(error: rusqlite::Error) -> Result<String, StoreError> {
    match error.sqlite_error_code() {
        Some(rusqlite::ErrorCode::DatabaseCorrupt | rusqlite::ErrorCode::NotADatabase) => {
            Ok(error.to_string())
        }
        _ => Err(error.into()),
    }
}

/// SQLite's integrity check of one full-text table, which compares every
/// posting with the table's content: `None` when it is sound, otherwise the
/// finding under `prefix`.
pub(super) fn fts_index_finding(
    connection: &Connection,
    table: &str,
    prefix: &str,
) -> Result<Option<String>, StoreError> {
    match connection.query_row(
        &format!("PRAGMA main.integrity_check('{table}')"),
        [],
        |row| row.get::<_, String>(0),
    ) {
        Ok(result) if result == "ok" => Ok(None),
        Ok(result) => Ok(Some(bounded_finding(prefix, &result))),
        Err(error) => corruption(error).map(|detail| Some(bounded_finding(prefix, &detail))),
    }
}

#[cfg(test)]
thread_local! {
    static FTS_CONTENT_SCANS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// How many full-text content passes ran on this thread.
#[cfg(test)]
pub(crate) fn fts_content_scans() -> usize {
    FTS_CONTENT_SCANS.with(std::cell::Cell::get)
}

/// One pass over a full-text table's stored content, grouped by the key the
/// first column holds, every row kept so a repeat stays visible. A table too
/// damaged to read gives its detail instead, so no row is invented missing.
pub(super) fn fts_content<T>(
    connection: &Connection,
    sql: &str,
    row: impl Fn(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
) -> Result<Result<HashMap<String, Vec<T>>, String>, StoreError> {
    #[cfg(test)]
    FTS_CONTENT_SCANS.with(|count| count.set(count.get() + 1));
    let mut statement = match connection.prepare(sql) {
        Ok(statement) => statement,
        Err(error) => return corruption(error).map(Err),
    };
    let mut rows = match statement.query([]) {
        Ok(rows) => rows,
        Err(error) => return corruption(error).map(Err),
    };
    let mut content: HashMap<String, Vec<T>> = HashMap::new();
    loop {
        let next = match rows.next() {
            Ok(Some(next)) => next,
            Ok(None) => return Ok(Ok(content)),
            Err(error) => return corruption(error).map(Err),
        };
        let read = next
            .get::<_, String>(0)
            .and_then(|key| row(next).map(|value| (key, value)));
        match read {
            Ok((key, value)) => content.entry(key).or_default().push(value),
            Err(error) => return corruption(error).map(Err),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn failure(code: i32) -> rusqlite::Error {
        rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), Some("detail".into()))
    }

    // Only a damaged database becomes a finding; busy, I/O, memory and other
    // operational failures reach the caller as errors.
    #[test]
    fn only_corruption_becomes_a_finding() {
        for code in [rusqlite::ffi::SQLITE_CORRUPT, rusqlite::ffi::SQLITE_NOTADB] {
            assert!(corruption(failure(code)).is_ok(), "{code}");
        }
        for code in [
            rusqlite::ffi::SQLITE_BUSY,
            rusqlite::ffi::SQLITE_IOERR,
            rusqlite::ffi::SQLITE_NOMEM,
            rusqlite::ffi::SQLITE_ERROR,
        ] {
            assert!(corruption(failure(code)).is_err(), "{code}");
        }
    }

    #[test]
    fn a_finding_is_bounded_and_printable() {
        let label = bounded_finding(
            "object_fts:fts_content",
            &format!("bad\n\u{1b}{}", "x".repeat(300)),
        );
        let detail = label.strip_prefix("object_fts:fts_content:").unwrap();
        assert_eq!(detail.chars().count(), 160);
        assert!(detail.is_ascii() && !detail.chars().any(char::is_control));
        assert_eq!(
            bounded_finding("object_fts:fts_content", "\r\n"),
            "object_fts:fts_content:unknown"
        );
    }
}
