//! Read-only schema identity and classification of existing open refusals.
//! These diagnostics do not introduce another store-admission scheme.

use super::{
    CanonicalObject, Connection, ObjectHash, Path, StoreError, current_schema_reference,
    stored_schema_definitions,
};

pub(super) const CORE_PROJECTION_REFUSAL_PREFIX: &str =
    "the store is missing a rebuildable projection: ";
pub(super) const WORK_PROJECTION_REFUSAL: &str = "current local-work schema is missing rebuildable projections; run `engram doctor --repair-projections` explicitly";

/// Diagnostic category of an ordinary store-open refusal.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum StoreOpenRefusalKind {
    ProjectionRepairRequired,
    DifferentBuildSchema,
    CorruptStore,
    Busy,
    Permission,
    Io,
    PathPolicy { recorded: String, requested: String },
}

/// Classifies existing open and diagnostic-read errors; it performs no I/O.
#[must_use]
pub fn store_open_refusal_kind(error: &StoreError) -> StoreOpenRefusalKind {
    match error {
        StoreError::InvalidControlProjection(message)
            if message == super::DIFFERENT_BUILD_STORE_MESSAGE =>
        {
            StoreOpenRefusalKind::DifferentBuildSchema
        }
        StoreError::InvalidControlProjection(message)
            if message.starts_with(CORE_PROJECTION_REFUSAL_PREFIX) =>
        {
            StoreOpenRefusalKind::ProjectionRepairRequired
        }
        StoreError::InvalidWorkProjection(message) if message == WORK_PROJECTION_REFUSAL => {
            StoreOpenRefusalKind::ProjectionRepairRequired
        }
        StoreError::InvalidControlSession(message) => path_policy_mismatch(message).map_or(
            StoreOpenRefusalKind::CorruptStore,
            |(recorded, requested)| StoreOpenRefusalKind::PathPolicy {
                recorded: recorded.into(),
                requested: requested.into(),
            },
        ),
        StoreError::InvalidControlProjection(message)
            if (message.starts_with("control-policy recovery target ")
                || message.starts_with("projection-repair target "))
                && message.ends_with(" is not an existing file") =>
        {
            StoreOpenRefusalKind::Io
        }
        StoreError::Sqlite(error) => match error.sqlite_error_code() {
            Some(
                rusqlite::ErrorCode::DatabaseBusy
                | rusqlite::ErrorCode::DatabaseLocked
                | rusqlite::ErrorCode::FileLockingProtocolFailed,
            ) => StoreOpenRefusalKind::Busy,
            Some(
                rusqlite::ErrorCode::PermissionDenied
                | rusqlite::ErrorCode::ReadOnly
                | rusqlite::ErrorCode::AuthorizationForStatementDenied,
            ) => StoreOpenRefusalKind::Permission,
            Some(
                rusqlite::ErrorCode::SystemIoFailure
                | rusqlite::ErrorCode::CannotOpen
                | rusqlite::ErrorCode::DiskFull
                | rusqlite::ErrorCode::NoLargeFileSupport,
            ) => StoreOpenRefusalKind::Io,
            _ => StoreOpenRefusalKind::CorruptStore,
        },
        _ => StoreOpenRefusalKind::CorruptStore,
    }
}

// Ordinary open's existing error remains unchanged (including the host protocol).
// Extract only its fixed, generated policy fields; never reread a racing store
// to guess which policy caused this refusal.
fn path_policy_mismatch(message: &str) -> Option<(&str, &str)> {
    let tail = message.strip_prefix("the store's persisted host path policy (")?;
    let (recorded, tail) = tail.split_once(") differs from this opener's (")?;
    let (requested, _) = tail.split_once("); if the project moved ")?;
    Some((recorded, requested))
}

/// Hashes the exact normalized schema reference used by ordinary admission.
///
/// # Errors
/// Returns an error when the in-memory current schema cannot be constructed.
pub fn running_schema_reference() -> Result<ObjectHash, StoreError> {
    Ok(CanonicalObject::freeze(&current_schema_reference()?)?
        .hash()
        .clone())
}

/// Reads the existing store's normalized definitions without opening a usable
/// store, initializing a database, or executing DDL.
///
/// # Errors
/// Returns an error when the existing file or schema cannot be read.
pub fn store_schema_reference(path: &Path) -> Result<ObjectHash, StoreError> {
    let connection = Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(super::Duration::from_secs(5))?;
    Ok(
        CanonicalObject::freeze(&stored_schema_definitions(&connection)?)?
            .hash()
            .clone(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_identity_uses_admission_definitions_without_mutating_store() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("schema.db");
        drop(super::super::SqliteStore::open(&path).unwrap());
        let bytes = std::fs::read(&path).unwrap();
        assert_eq!(
            store_schema_reference(&path).unwrap(),
            running_schema_reference().unwrap()
        );
        assert_eq!(std::fs::read(&path).unwrap(), bytes);
        let connection = Connection::open(&path).unwrap();
        connection
            .execute_batch("CREATE TABLE diagnostic_difference(value TEXT)")
            .unwrap();
        drop(connection);
        assert_ne!(
            store_schema_reference(&path).unwrap(),
            running_schema_reference().unwrap()
        );
        let absent = directory.path().join("absent.db");
        assert!(store_schema_reference(&absent).is_err());
        assert!(!absent.exists());
    }
}
