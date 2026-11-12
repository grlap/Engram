//! Rebuildable enrollment links; canonical records remain the identity owner.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Connection, OptionalExtension, params};

use super::{Enrollment, MAX_LINEAGE_RECORDS, MAX_RECORD_BYTES, ProducingOrigin};
use crate::{CanonicalObject, ObjectId, ProjectId, storage::StoreError};

#[cfg(test)]
mod tests;

#[cfg(test)]
thread_local! {
    static DECODES: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static ANCESTRY_VISITS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(in crate::storage::work) fn create_schema(connection: &Connection) -> Result<(), StoreError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS work_task_sync_producing_origin_members (
             origin_id TEXT PRIMARY KEY REFERENCES objects(object_id),
             project_id TEXT NOT NULL,
             predecessor_id TEXT REFERENCES objects(object_id),
             operation_id TEXT NOT NULL UNIQUE,
             feed_position INTEGER NOT NULL CHECK(feed_position > 0)
         ) STRICT;
         CREATE TABLE IF NOT EXISTS work_task_sync_producing_origin_heads (
             project_id TEXT PRIMARY KEY,
             origin_id TEXT NOT NULL REFERENCES work_task_sync_producing_origin_members(origin_id)
         ) STRICT;
         CREATE INDEX IF NOT EXISTS objects_task_sync_producing_origin_project
             ON objects(
                 CASE WHEN json_valid(canonical_json) THEN json_extract(canonical_json,'$.project_id') END,
                 CASE WHEN json_valid(canonical_json) THEN json_extract(canonical_json,'$.predecessor_origin_id') END)
             WHERE object_kind='task_sync_producing_origin';
         CREATE INDEX IF NOT EXISTS objects_task_sync_producing_origin_operation
             ON objects(CASE WHEN json_valid(canonical_json)
                            THEN json_extract(canonical_json,'$.enrollment_operation_id') END)
             WHERE object_kind='task_sync_producing_origin';",
    )?;
    Ok(())
}

fn invalid(reason: &str) -> StoreError {
    super::super::invalid(reason)
}

fn repair() -> StoreError {
    invalid("origin enrollment projection differs; repair projections explicitly")
}

fn byte_limit() -> Result<i64, StoreError> {
    i64::try_from(MAX_RECORD_BYTES)
        .map_err(|_| invalid("origin enrollment byte bound exceeds SQLite integer range"))
}

fn id(value: String) -> Result<ObjectId, StoreError> {
    ObjectId::from_stored(value.clone()).ok_or(StoreError::InvalidStoredKey(value))
}

fn decode(object: &CanonicalObject) -> Result<Enrollment, StoreError> {
    #[cfg(test)]
    DECODES.with(|count| count.set(count.get() + 1));
    if object.bytes().len() > MAX_RECORD_BYTES {
        return Err(invalid("origin enrollment record exceeds its byte bound"));
    }
    let record: Enrollment = object.decode()?;
    if record.project_id.0.trim().is_empty()
        || record.enrollment_operation_id.is_nil()
        || record.actor.actor_id.trim().is_empty()
    {
        return Err(invalid("invalid or duplicate origin enrollment operation"));
    }
    Ok(record)
}

#[derive(Debug, Eq, PartialEq)]
struct Member {
    project: String,
    predecessor: Option<String>,
    operation: String,
    position: i64,
}

impl Member {
    fn from_record(record: &Enrollment, position: i64) -> Self {
        Self {
            project: record.project_id.0.clone(),
            predecessor: record
                .predecessor_origin_id
                .as_ref()
                .map(ToString::to_string),
            operation: record.enrollment_operation_id.to_string(),
            position,
        }
    }

    fn read(connection: &Connection, origin: &ObjectId) -> Result<Option<Self>, StoreError> {
        Ok(connection
            .query_row(
                "SELECT project_id, predecessor_id, operation_id, feed_position
             FROM work_task_sync_producing_origin_members WHERE origin_id=?1",
                [origin.as_str()],
                |row| {
                    Ok(Self {
                        project: row.get(0)?,
                        predecessor: row.get(1)?,
                        operation: row.get(2)?,
                        position: row.get(3)?,
                    })
                },
            )
            .optional()?)
    }

    fn write(&self, connection: &Connection, origin: &ObjectId) -> Result<(), StoreError> {
        connection.execute(
            "INSERT INTO work_task_sync_producing_origin_members
             (origin_id,project_id,predecessor_id,operation_id,feed_position) VALUES (?1,?2,?3,?4,?5)",
            params![origin.as_str(), self.project, self.predecessor, self.operation, self.position],
        )?;
        Ok(())
    }
}

fn position(
    connection: &Connection,
    origin: &ObjectId,
    project: &ProjectId,
) -> Result<i64, StoreError> {
    connection
        .query_row(
            "SELECT position FROM work_feed_entries
         WHERE feed_kind='project' AND feed_id=?1 AND object_id=?2
           AND object_kind='task_sync_producing_origin' AND work_id IS NULL",
            params![project.0, origin.as_str()],
            |row| row.get(0),
        )
        .optional()?
        .filter(|position| *position > 0)
        .ok_or_else(|| invalid("origin enrollment project feed provenance differs"))
}

fn head(connection: &Connection, project: &ProjectId) -> Result<Option<ObjectId>, StoreError> {
    connection
        .query_row(
            "SELECT origin_id FROM work_task_sync_producing_origin_heads WHERE project_id=?1",
            [&project.0],
            |row| row.get(0),
        )
        .optional()?
        .map(id)
        .transpose()
}

/// Selected reads validate their record and feed proof. Doctor owns the
/// exhaustive check of disconnected canonical history, without a hot fallback.
pub(super) fn load(
    connection: &Connection,
    project: &ProjectId,
) -> Result<Option<ProducingOrigin>, StoreError> {
    let Some(origin) = head(connection, project)? else {
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM objects INDEXED BY objects_task_sync_producing_origin_project
             WHERE object_kind='task_sync_producing_origin'
               AND NOT EXISTS(SELECT 1 FROM work_task_sync_evidence_imported foreign_record WHERE foreign_record.object_id=objects.object_id)
               AND CASE WHEN json_valid(canonical_json) THEN json_extract(canonical_json,'$.project_id') END=?1)",
            [&project.0], |row| row.get(0),
        )?;
        return if exists { Err(repair()) } else { Ok(None) };
    };
    let bytes: Option<Vec<u8>> = connection
        .query_row(
            "SELECT CASE WHEN length(canonical_json)<=?2 THEN canonical_json END FROM objects
         WHERE object_id=?1 AND object_kind='task_sync_producing_origin'",
            params![origin.as_str(), byte_limit()?],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    let bytes =
        bytes.ok_or_else(|| invalid("origin enrollment canonical record missing or oversized"))?;
    let record = decode(&CanonicalObject::stored(&origin, bytes)?)?;
    let expected = Member::from_record(&record, position(connection, &origin, project)?);
    if record.project_id != *project
        || Member::read(connection, &origin)?.as_ref() != Some(&expected)
    {
        return Err(repair());
    }
    let successor: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM objects INDEXED BY objects_task_sync_producing_origin_project
         WHERE object_kind='task_sync_producing_origin'
           AND NOT EXISTS(SELECT 1 FROM work_task_sync_evidence_imported foreign_record WHERE foreign_record.object_id=objects.object_id)
           AND CASE WHEN json_valid(canonical_json) THEN json_extract(canonical_json,'$.project_id') END=?1
           AND CASE WHEN json_valid(canonical_json) THEN json_extract(canonical_json,'$.predecessor_origin_id') END=?2)",
        params![project.0, origin.as_str()], |row| row.get(0),
    )?;
    if successor {
        return Err(repair());
    }
    let roots: i64 = connection.query_row(
        "SELECT count(*) FROM (SELECT object_id FROM objects
         INDEXED BY objects_task_sync_producing_origin_project
         WHERE object_kind='task_sync_producing_origin'
           AND NOT EXISTS(SELECT 1 FROM work_task_sync_evidence_imported foreign_record WHERE foreign_record.object_id=objects.object_id)
           AND CASE WHEN json_valid(canonical_json) THEN json_extract(canonical_json,'$.project_id') END=?1
           AND CASE WHEN json_valid(canonical_json) THEN json_extract(canonical_json,'$.predecessor_origin_id') END IS NULL LIMIT 2)",
        [&project.0], |row| row.get(0),
    )?;
    if roots != 1 {
        return Err(invalid("multiple or missing origin enrollment roots"));
    }
    Ok(Some(ProducingOrigin {
        origin_id: origin,
        enrollment: record,
    }))
}

pub(super) fn operation_exists(
    connection: &Connection,
    operation: uuid::Uuid,
) -> Result<bool, StoreError> {
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM objects INDEXED BY objects_task_sync_producing_origin_operation
         WHERE object_kind='task_sync_producing_origin'
           AND NOT EXISTS(SELECT 1 FROM work_task_sync_evidence_imported foreign_record WHERE foreign_record.object_id=objects.object_id)
           AND CASE WHEN json_valid(canonical_json) THEN json_extract(canonical_json,'$.enrollment_operation_id') END=?1)",
        [operation.to_string()], |row| row.get(0),
    )?)
}

/// The enrollment owner has checked the predecessor before inserting the
/// canonical object and feed entry. Update its links inside that same writer.
pub(super) fn record(
    connection: &Connection,
    origin: &ObjectId,
    record: &Enrollment,
) -> Result<(), StoreError> {
    if connection.is_autocommit()
        || head(connection, &record.project_id)? != record.predecessor_origin_id
    {
        return Err(repair());
    }
    Member::from_record(record, position(connection, origin, &record.project_id)?)
        .write(connection, origin)?;
    connection.execute(
        "INSERT INTO work_task_sync_producing_origin_heads(project_id,origin_id) VALUES (?1,?2)
         ON CONFLICT(project_id) DO UPDATE SET origin_id=excluded.origin_id",
        params![record.project_id.0, origin.as_str()],
    )?;
    Ok(())
}

#[derive(Default, Eq, PartialEq)]
struct Rows {
    members: BTreeMap<ObjectId, Member>,
    heads: BTreeMap<String, ObjectId>,
}

fn derive(connection: &Connection, checked: &mut usize) -> Result<Rows, StoreError> {
    let mut statement = connection.prepare(
        "SELECT object_id, CASE WHEN length(canonical_json)<=?1 THEN canonical_json END
         FROM objects WHERE object_kind='task_sync_producing_origin'
         AND NOT EXISTS(SELECT 1 FROM work_task_sync_evidence_imported foreign_record WHERE foreign_record.object_id=objects.object_id) ORDER BY object_id",
    )?;
    let mut rows = statement.query([byte_limit()?])?;
    let mut records = BTreeMap::new();
    let mut operations = BTreeSet::new();
    while let Some(row) = rows.next()? {
        *checked += 1;
        if records.len() >= MAX_LINEAGE_RECORDS {
            return Err(invalid(
                "origin enrollment lineage exceeds its record bound",
            ));
        }
        let origin = id(row.get(0)?)?;
        let bytes: Option<Vec<u8>> = row.get(1)?;
        let record = decode(&CanonicalObject::stored(
            &origin,
            bytes.ok_or_else(|| invalid("origin enrollment record exceeds its byte bound"))?,
        )?)?;
        if !operations.insert(record.enrollment_operation_id) {
            return Err(invalid("invalid or duplicate origin enrollment operation"));
        }
        records.insert(origin, record);
    }
    let mut superseded = BTreeSet::new();
    for record in records.values() {
        if let Some(prior) = &record.predecessor_origin_id {
            let predecessor = records
                .get(prior)
                .ok_or_else(|| invalid("origin predecessor is missing"))?;
            if predecessor.project_id != record.project_id || !superseded.insert(prior.clone()) {
                return Err(invalid(
                    "origin lineage has foreign or competing successors",
                ));
            }
        }
    }
    // A completed predecessor suffix is visited only once across all lineages.
    let mut complete = BTreeSet::new();
    for origin in records.keys() {
        let mut path = BTreeSet::new();
        let mut cursor = Some(origin);
        while let Some(current) = cursor {
            if complete.contains(current) {
                break;
            }
            if !path.insert(current.clone()) {
                return Err(invalid("origin enrollment lineage contains a cycle"));
            }
            #[cfg(test)]
            ANCESTRY_VISITS.with(|count| count.set(count.get() + 1));
            cursor = records
                .get(current)
                .ok_or_else(|| invalid("origin predecessor is missing"))?
                .predecessor_origin_id
                .as_ref();
        }
        complete.extend(path);
    }
    let mut result = Rows::default();
    for (origin, record) in records {
        result.members.insert(
            origin.clone(),
            Member::from_record(&record, position(connection, &origin, &record.project_id)?),
        );
        if !superseded.contains(&origin)
            && result.heads.insert(record.project_id.0, origin).is_some()
        {
            return Err(invalid("multiple active origin enrollment heads"));
        }
    }
    Ok(result)
}

fn failure(error: &StoreError) -> String {
    let reason =
        crate::storage::undecodable_record_reason(error).unwrap_or_else(|| error.to_string());
    format!("task_sync_producing_origin:{reason}")
}

pub(in crate::storage::work) fn rebuild(
    connection: &Connection,
) -> Result<Vec<String>, StoreError> {
    let expected = match derive(connection, &mut 0) {
        Ok(rows) => rows,
        Err(
            error @ (StoreError::InvalidWork(_)
            | StoreError::InvalidStoredKey(_)
            | StoreError::Json(_)),
        ) => return Ok(vec![failure(&error)]),
        Err(error) => return Err(error),
    };
    connection.execute("DELETE FROM work_task_sync_producing_origin_heads", [])?;
    connection.execute("DELETE FROM work_task_sync_producing_origin_members", [])?;
    for (origin, member) in expected.members {
        member.write(connection, &origin)?;
    }
    for (project, origin) in expected.heads {
        connection.execute("INSERT INTO work_task_sync_producing_origin_heads(project_id,origin_id) VALUES (?1,?2)", params![project,origin.as_str()])?;
    }
    Ok(Vec::new())
}

pub(super) fn verify(
    connection: &Connection,
    checked: &mut usize,
    invalid_records: &mut Vec<String>,
) -> Result<(), StoreError> {
    let expected = match derive(connection, checked) {
        Ok(rows) => rows,
        Err(
            error @ (StoreError::InvalidWork(_)
            | StoreError::InvalidStoredKey(_)
            | StoreError::Json(_)),
        ) => {
            invalid_records.push(failure(&error));
            return Ok(());
        }
        Err(error) => return Err(error),
    };
    let mut actual = Rows::default();
    let mut statement = connection.prepare("SELECT origin_id,project_id,predecessor_id,operation_id,feed_position FROM work_task_sync_producing_origin_members ORDER BY origin_id")?;
    for row in statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            Member {
                project: row.get(1)?,
                predecessor: row.get(2)?,
                operation: row.get(3)?,
                position: row.get(4)?,
            },
        ))
    })? {
        let (origin, member) = row?;
        actual.members.insert(id(origin)?, member);
    }
    let mut statement = connection.prepare("SELECT project_id,origin_id FROM work_task_sync_producing_origin_heads ORDER BY project_id")?;
    for row in statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (project, origin) = row?;
        actual.heads.insert(project, id(origin)?);
    }
    *checked += 1;
    if actual != expected {
        invalid_records.push("task_sync_producing_origin:projection differs".into());
    }
    Ok(())
}
