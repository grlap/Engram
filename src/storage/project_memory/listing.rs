//! Rebuildable, head-bound metadata for history-independent list pages.

use super::{
    Connection, ObjectId, OptionalExtension, SqliteStore, StoreError, StoredProjectMemory, params,
};
use crate::domain::{ProjectId, ProjectMemoryRetiringTargetDropped};

pub(in crate::storage) const TABLE: &str = "project_memory_list_metadata";
pub(in crate::storage) const DDL: &str = "CREATE TABLE project_memory_list_metadata (
    project_id TEXT NOT NULL,
    key TEXT NOT NULL,
    memory_id TEXT NOT NULL,
    version_id TEXT NOT NULL,
    assertion_id TEXT NOT NULL,
    revision INTEGER NOT NULL CHECK(revision > 0),
    dropped_target_json TEXT,
    PRIMARY KEY(project_id, key)
) STRICT;";

#[derive(Debug, Eq, PartialEq)]
pub(in crate::storage) struct Metadata {
    memory_id: String,
    version_id: String,
    assertion_id: String,
    revision: i64,
    dropped_target_json: Option<String>,
}

impl Metadata {
    pub(in crate::storage) fn from_history(
        history: &[StoredProjectMemory],
        assertion_id: &ObjectId,
    ) -> Result<Self, StoreError> {
        let head = history
            .last()
            .ok_or_else(|| invalid("canonical history is empty"))?;
        Ok(Self {
            memory_id: head.version.memory_id.0.to_string(),
            version_id: head.version_id.as_str().into(),
            assertion_id: assertion_id.as_str().into(),
            revision: i64::try_from(super::read::history_revision(history)?)
                .map_err(|_| invalid("revision exceeds its stored range"))?,
            dropped_target_json: super::history::retiring_target_dropped(
                history,
                history.len() - 1,
            )
            .map(|dropped| serde_json::to_string(&dropped))
            .transpose()?,
        })
    }

    pub(in crate::storage) fn write(
        &self,
        connection: &Connection,
        project: &ProjectId,
        key: &str,
    ) -> Result<(), StoreError> {
        connection.execute(
            "INSERT INTO project_memory_list_metadata
             (project_id, key, memory_id, version_id, assertion_id, revision, dropped_target_json)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
             ON CONFLICT(project_id, key) DO UPDATE SET
                 memory_id=excluded.memory_id, version_id=excluded.version_id,
                 assertion_id=excluded.assertion_id, revision=excluded.revision,
                 dropped_target_json=excluded.dropped_target_json",
            params![
                project.0,
                key,
                self.memory_id,
                self.version_id,
                self.assertion_id,
                self.revision,
                self.dropped_target_json
            ],
        )?;
        Ok(())
    }
}

fn invalid(reason: &str) -> StoreError {
    StoreError::InvalidMemoryProjection(format!(
        "project memory listing metadata {reason}; run `engram doctor --repair-projections` explicitly"
    ))
}

fn read(connection: &Connection, project: &ProjectId, key: &str) -> Result<Metadata, StoreError> {
    connection
        .query_row(
            "SELECT memory_id, version_id, assertion_id, revision, dropped_target_json
         FROM project_memory_list_metadata WHERE project_id=?1 AND key=?2",
            params![project.0, key],
            |row| {
                Ok(Metadata {
                    memory_id: row.get(0)?,
                    version_id: row.get(1)?,
                    assertion_id: row.get(2)?,
                    revision: row.get(3)?,
                    dropped_target_json: row.get(4)?,
                })
            },
        )
        .optional()
        .map_err(|error| match error {
            rusqlite::Error::InvalidColumnType(..)
            | rusqlite::Error::FromSqlConversionFailure(..)
            | rusqlite::Error::IntegralValueOutOfRange(..) => invalid("has invalid column values"),
            error => StoreError::Sqlite(error),
        })?
        .ok_or_else(|| invalid("is missing for a selected head"))
}

pub(super) fn for_head(
    connection: &Connection,
    project: &ProjectId,
    key: &str,
    head: &StoredProjectMemory,
    assertion_id: &ObjectId,
) -> Result<(u64, Option<ProjectMemoryRetiringTargetDropped>), StoreError> {
    let metadata = read(connection, project, key)?;
    if metadata.memory_id != head.version.memory_id.0.to_string()
        || metadata.version_id != head.version_id.as_str()
        || metadata.assertion_id != assertion_id.as_str()
        || metadata.revision < 1
        || (head.version.parents.is_empty() != (metadata.revision == 1))
    {
        return Err(invalid("does not bind the selected canonical head"));
    }
    let revision =
        u64::try_from(metadata.revision).map_err(|_| invalid("has an invalid revision"))?;
    let dropped: Option<ProjectMemoryRetiringTargetDropped> = metadata
        .dropped_target_json
        .map(|json| serde_json::from_str(&json))
        .transpose()
        .map_err(|_| invalid("has an invalid dropped retirement target"))?;
    if let Some(dropped) = &dropped {
        if head.version.retiring_target.is_some()
            || head.version.retiring_target_cleared
            || dropped.revision < 2
            || dropped.revision > revision
        {
            return Err(invalid("has inconsistent retirement metadata"));
        }
        super::retirement::validate_retiring_target_shape(&dropped.target)
            .map_err(|_| invalid("has an invalid retirement target"))?;
    }
    Ok((revision, dropped))
}

// Explicit maintenance may enumerate all canonical keyed identities. Routine
// listing never uses this path, nor derives a missing row on demand.
fn canonical_rows(
    connection: &Connection,
) -> Result<Vec<(ProjectId, String, Metadata)>, StoreError> {
    let keys = connection
        .prepare(
            "SELECT DISTINCT json_extract(canonical_json, '$.scope.project'),
                         json_extract(canonical_json, '$.project_key')
         FROM objects INDEXED BY objects_project_memory_key
         WHERE object_kind='memory_version'
           AND json_extract(canonical_json, '$.scope.kind')='project'
           AND json_type(canonical_json, '$.project_key')='text'
         ORDER BY 1, 2",
        )?
        .query_map([], |row| {
            Ok((ProjectId(row.get(0)?), row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let projected_count: i64 = connection.query_row(
        "SELECT COUNT(*) FROM memory_heads AS head
         JOIN objects AS object ON object.object_id=head.version_id
         WHERE object.object_kind='memory_version'
           AND json_extract(object.canonical_json, '$.scope.kind')='project'
           AND json_type(object.canonical_json, '$.project_key')='text'",
        [],
        |row| row.get(0),
    )?;
    if usize::try_from(projected_count).ok() != Some(keys.len()) {
        return Err(invalid("canonical keys and durable heads differ"));
    }
    keys.into_iter()
        .map(|(project, key)| {
            let history =
                super::read::lookup_project_memory_history_on(connection, &project, &key)?;
            let head = history
                .last()
                .ok_or_else(|| invalid("has no durable head"))?;
            let assertion_id: String = connection.query_row(
                "SELECT assertion_id FROM memory_heads WHERE memory_id=?1",
                [head.version.memory_id.0.to_string()],
                |row| row.get(0),
            )?;
            let assertion_id = ObjectId::from_stored(assertion_id)
                .ok_or_else(|| invalid("has an invalid assertion id"))?;
            let metadata = Metadata::from_history(&history, &assertion_id)?;
            Ok((project, key, metadata))
        })
        .collect()
}

pub(in crate::storage) fn rebuild(connection: &Connection) -> Result<usize, StoreError> {
    // Validate every chain and durable head before changing even disposable rows.
    let rows = canonical_rows(connection)?;
    connection.execute("DELETE FROM project_memory_list_metadata", [])?;
    for (project, key, metadata) in &rows {
        metadata.write(connection, project, key)?;
    }
    Ok(rows.len())
}

pub(in crate::storage) fn verify(
    connection: &Connection,
    checked: &mut usize,
    invalid_rows: &mut Vec<String>,
) -> Result<(), StoreError> {
    let expected = match canonical_rows(connection) {
        Ok(rows) => rows,
        Err(error @ StoreError::Sqlite(_)) => return Err(error),
        Err(_) => {
            invalid_rows.push("project_memory_list_metadata:canonical_binding".into());
            return Ok(());
        }
    };
    let mut valid = true;
    for (project, key, metadata) in &expected {
        *checked += 1;
        match read(connection, project, key) {
            Ok(stored) => valid &= stored == *metadata,
            Err(error @ StoreError::Sqlite(_)) => return Err(error),
            Err(_) => valid = false,
        }
    }
    let count = connection.query_row(
        "SELECT COUNT(*) FROM project_memory_list_metadata",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    if !valid || usize::try_from(count).ok() != Some(expected.len()) {
        invalid_rows.push("project_memory_list_metadata:projection_binding".into());
    }
    Ok(())
}

impl SqliteStore {
    /// Build only the absent memory-list metadata after all old consumers exit.
    /// Checks keyed memory chains and schema shapes, not a full store audit.
    ///
    /// # Errors
    /// Refuses an existing target, unrelated schema damage or invalid durable
    /// memory heads. All changes roll back on refusal.
    pub fn build_project_memory_list_metadata(path: &std::path::Path) -> Result<usize, StoreError> {
        let connection = Self::open_projection_maintenance(path, false)?;
        connection.execute_batch("BEGIN IMMEDIATE;")?;
        Self::preflight_projection_maintenance(&connection)?;
        let exists = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name=?1)",
            [TABLE],
            |row| row.get::<_, bool>(0),
        )?;
        if exists {
            return Err(invalid(
                "already exists; use full projection repair for a damaged target",
            ));
        }
        connection.execute_batch(DDL)?;
        for owner in [
            super::super::SchemaOwner::Core,
            super::super::SchemaOwner::Work,
        ] {
            for durability in [
                super::super::SchemaDurability::Durable,
                super::super::SchemaDurability::Rebuildable,
            ] {
                if let Some(issue) =
                    super::super::current_schema_definition_issue(&connection, owner, durability)?
                {
                    return Err(StoreError::InvalidMemoryProjection(issue));
                }
            }
        }
        let count = rebuild(&connection)?;
        let mut checked = 0;
        let mut findings = Vec::new();
        verify(&connection, &mut checked, &mut findings)?;
        if !findings.is_empty() {
            return Err(invalid("failed scoped verification"));
        }
        connection.execute_batch("COMMIT;")?;
        Ok(count)
    }
}
