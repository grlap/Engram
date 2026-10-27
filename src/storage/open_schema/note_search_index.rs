//! Explicit one-time note-index cutover; ordinary open never builds projections.

use super::{
    Connection, Duration, Path, SchemaDurability, SchemaOwner, SqliteStore, StoreError,
    current_schema_definition_issue, different_build_store_error, open_sqlite_file, work,
};

impl SqliteStore {
    pub(super) fn open_projection_maintenance(
        path: &Path,
        missing_note_index: bool,
    ) -> Result<Connection, StoreError> {
        if !path.is_file() {
            return Err(StoreError::InvalidControlProjection(format!(
                "projection-repair target {} is not an existing file",
                path.display()
            )));
        }
        let connection = open_sqlite_file(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)?;
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch("PRAGMA foreign_keys = ON; PRAGMA synchronous = NORMAL;")?;
        if missing_note_index {
            work::note_index::require_missing_on(&connection)?;
        }
        Self::preflight_projection_maintenance(&connection)?;
        Ok(connection)
    }

    fn preflight_projection_maintenance(connection: &Connection) -> Result<(), StoreError> {
        if !Self::sqlite_user_schema_exists(connection)? {
            return Err(StoreError::StoreNotInitialized);
        }
        if Self::current_core_durable_schema_issue(connection)?.is_some() {
            return Err(different_build_store_error());
        }
        work::preflight_schema(connection, false)?;
        Self::require_task_local_cursor_schema(connection)?;
        Self::preflight_host_path_policy(connection, None)?;
        Self::preflight_control_policy_schema(connection)
            .map_err(|error| Self::unreadable_active_rule_set_refusal(connection).unwrap_or(error))
    }

    /// Build only the missing public-note search index in one transaction.
    /// Consumers must be stopped; all other projection shapes must be current.
    /// This checks the new index, not the store's exhaustive integrity audit.
    ///
    /// # Errors
    /// Refuses existing or damaged target objects, other schema/policy issues,
    /// canonical note findings and SQLite errors. Every refusal rolls back.
    pub fn build_note_search_index(path: &Path) -> Result<usize, StoreError> {
        let connection = Self::open_projection_maintenance(path, true)?;
        // Acquire the writer before checking/creating the target. The immediate
        // transaction prevents another builder from passing the absence check.
        connection.execute_batch("BEGIN IMMEDIATE;")?;
        Self::preflight_projection_maintenance(&connection)?;
        let checked = work::note_index::build_missing_on(&connection)?;
        for owner in [SchemaOwner::Core, SchemaOwner::Work] {
            for durability in [SchemaDurability::Durable, SchemaDurability::Rebuildable] {
                if let Some(issue) =
                    current_schema_definition_issue(&connection, owner, durability)?
                {
                    return Err(StoreError::InvalidWorkProjection(issue));
                }
            }
        }
        connection.execute_batch("COMMIT;")?;
        Ok(checked)
    }
}
