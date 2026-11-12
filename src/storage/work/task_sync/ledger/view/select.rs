//! Indexed discovery of canonical bindings and historical publication starts.

use std::path::Path;

use rusqlite::{Connection, OptionalExtension, params, types::Value};

use super::super::{MAX_RECORDS, ObjectId, StoreError, require};
use crate::ProjectId;

const PROJECT: &str =
    "CASE WHEN json_valid(canonical_json) THEN json_extract(canonical_json,'$.project') END";
const ORIGIN: &str =
    "CASE WHEN json_valid(canonical_json) THEN json_extract(canonical_json,'$.store.origin') END";
const SUCCESSOR: &str =
    "CASE WHEN json_valid(canonical_json) THEN json_extract(canonical_json,'$.supersedes') END";
const ROOT: &str = "CASE WHEN json_valid(canonical_json) THEN json_extract(canonical_json,'$.repository.git_directory') END";
const RETRY_PUSH: &str = "CASE WHEN json_valid(canonical_json) THEN json_extract(canonical_json,'$.transition.transition')||':'||json_extract(canonical_json,'$.transition.reconciliation.step') END='outgoing_reconciled:push_started'";

// One portable coarse key keeps schema/backup identity independent of the OS.
// Exact native store_path_text equality is applied after keyed discovery.
fn root_expression(value: &str) -> String {
    format!(
        "CASE WHEN substr({value},1,8)=char(92,92,63,92)||'UNC'||char(92)
                 THEN char(92,92)||substr({value},9)
                 WHEN substr({value},1,4)=char(92,92,63,92) THEN substr({value},5)
                 ELSE {value} END"
    )
}

pub(in crate::storage::work::task_sync::ledger) fn create_schema(
    connection: &Connection,
) -> Result<(), StoreError> {
    connection.execute_batch(&format!(
        "CREATE INDEX IF NOT EXISTS objects_task_sync_binding_scope
             ON objects({PROJECT},{ORIGIN},object_id) WHERE object_kind='task_sync_binding';
         CREATE INDEX IF NOT EXISTS objects_task_sync_binding_successor
             ON objects({SUCCESSOR},object_id) WHERE object_kind='task_sync_binding';
         CREATE INDEX IF NOT EXISTS objects_task_sync_binding_root
             ON objects({},object_id) WHERE object_kind='task_sync_binding';
         CREATE INDEX IF NOT EXISTS objects_task_sync_exchange_push_started
             ON objects(json_extract(canonical_json,'$.binding'),json_extract(canonical_json,'$.sequence'),object_id)
             WHERE object_kind='task_sync_exchange_event'
               AND CASE WHEN json_valid(canonical_json) THEN json_extract(canonical_json,'$.transition.transition') END='push_started';
         CREATE INDEX IF NOT EXISTS objects_task_sync_exchange_retry_push_started
             ON objects(json_extract(canonical_json,'$.binding'),json_extract(canonical_json,'$.sequence'),object_id)
             WHERE object_kind='task_sync_exchange_event' AND {RETRY_PUSH};",
        root_expression(ROOT),
    ))?;
    Ok(())
}

#[derive(Clone, Copy)]
pub(in crate::storage::work::task_sync::ledger) enum Bindings<'a> {
    Project(&'a ProjectId),
    Origin(&'a ProjectId, &'a ObjectId),
    Root(&'a Path),
}

fn key(value: String) -> Result<ObjectId, StoreError> {
    ObjectId::from_stored(value)
        .ok_or_else(|| super::super::super::invalid("invalid exchange binding identity"))
}

pub(in crate::storage::work::task_sync::ledger) fn bindings(
    connection: &Connection,
    selector: Bindings<'_>,
) -> Result<Vec<ObjectId>, StoreError> {
    let (index, predicate, mut values) = match selector {
        Bindings::Project(project) => (
            "objects_task_sync_binding_scope",
            format!("{PROJECT}=?"),
            vec![Value::Text(project.0.clone())],
        ),
        Bindings::Origin(project, origin) => (
            "objects_task_sync_binding_scope",
            format!("{PROJECT}=? AND {ORIGIN}=?"),
            vec![
                Value::Text(project.0.clone()),
                Value::Text(origin.to_string()),
            ],
        ),
        Bindings::Root(root) => (
            "objects_task_sync_binding_root",
            format!("{}={}", root_expression(ROOT), root_expression("?1")),
            vec![Value::Text(crate::storage::store_path_text(root))],
        ),
    };
    let limit = i64::try_from(MAX_RECORDS)
        .ok()
        .and_then(|limit| limit.checked_add(1))
        .ok_or_else(|| {
            super::super::super::invalid("exchange selector bound exceeds SQLite integer range")
        })?;
    values.push(Value::Integer(limit));
    let mut statement = connection.prepare(&format!(
        "SELECT object_id,{ROOT} FROM objects INDEXED BY {index}
         WHERE object_kind='task_sync_binding' AND {predicate} ORDER BY object_id LIMIT ?"
    ))?;
    let rows = statement
        .query_map(rusqlite::params_from_iter(values), |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    require(
        rows.len() <= MAX_RECORDS,
        "exchange binding selection exceeds supported bound",
    )?;
    rows.into_iter()
        .filter(|(_, stored)| match selector {
            Bindings::Root(root) => stored.as_ref().is_some_and(|stored| {
                crate::storage::store_path_text(Path::new(stored))
                    == crate::storage::store_path_text(root)
            }),
            Bindings::Project(_) | Bindings::Origin(_, _) => true,
        })
        .map(|(id, _)| key(id))
        .collect()
}

pub(in crate::storage::work::task_sync::ledger) fn successor(
    connection: &Connection,
    binding: &ObjectId,
) -> Result<Option<ObjectId>, StoreError> {
    let mut statement = connection.prepare(&format!(
        "SELECT object_id FROM objects INDEXED BY objects_task_sync_binding_successor
         WHERE object_kind='task_sync_binding' AND {SUCCESSOR}=?1 ORDER BY object_id LIMIT 2"
    ))?;
    let ids = statement
        .query_map([binding.as_str()], |row| row.get::<_, String>(0))?
        .map(|row| key(row?))
        .collect::<Result<Vec<_>, StoreError>>()?;
    require(ids.len() <= 1, "exchange binding activation forks")?;
    let projected: Option<Option<String>> = connection
        .query_row(
            "SELECT superseded_by FROM work_task_sync_exchange_heads WHERE binding_id=?1",
            [binding.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    require(
        projected.as_ref().map(Option::as_deref) == Some(ids.first().map(ObjectId::as_str)),
        "exchange supersession projection differs; repair projections explicitly",
    )?;
    Ok(ids.into_iter().next())
}

pub(in crate::storage::work::task_sync::ledger) fn pushes(
    connection: &Connection,
    binding: &ObjectId,
) -> Result<Vec<ObjectId>, StoreError> {
    let limit = i64::try_from(MAX_RECORDS)
        .ok()
        .and_then(|limit| limit.checked_add(1))
        .ok_or_else(|| {
            super::super::super::invalid("exchange selector bound exceeds SQLite integer range")
        })?;
    let mut statement = connection.prepare(&format!(
        "SELECT object_id FROM (
         SELECT object_id,json_extract(canonical_json,'$.sequence') AS sequence FROM objects INDEXED BY objects_task_sync_exchange_push_started
         WHERE object_kind='task_sync_exchange_event'
           AND CASE WHEN json_valid(canonical_json) THEN json_extract(canonical_json,'$.transition.transition') END='push_started'
           AND json_extract(canonical_json,'$.binding')=?1
         UNION ALL
         SELECT object_id,json_extract(canonical_json,'$.sequence') AS sequence FROM objects INDEXED BY objects_task_sync_exchange_retry_push_started
         WHERE object_kind='task_sync_exchange_event' AND {RETRY_PUSH}
           AND json_extract(canonical_json,'$.binding')=?1)
         ORDER BY sequence,object_id LIMIT ?2"))?;
    let ids = statement
        .query_map(params![binding.as_str(), limit], |row| {
            row.get::<_, String>(0)
        })?
        .map(|row| key(row?))
        .collect::<Result<Vec<_>, StoreError>>()?;
    require(
        ids.len() <= MAX_RECORDS,
        "historical publication selection exceeds supported bound",
    )?;
    Ok(ids)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_index_uses_portable_coarse_key_and_exact_native_path_filter() {
        let connection = rusqlite::Connection::open_in_memory().unwrap();
        connection
            .execute(
                "CREATE TABLE objects(object_id TEXT,object_kind TEXT,canonical_json BLOB)",
                [],
            )
            .unwrap();
        create_schema(&connection).unwrap();
        let schema: String = connection
            .query_row(
                "SELECT sql FROM sqlite_schema WHERE name='objects_task_sync_binding_root'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        // Prefix folding is present on every platform; DDL has no host branch.
        assert!(schema.contains("substr("), "{schema}");
        let values = [
            r"C:\repo\.git",
            r"\\?\C:\repo\.git",
            r"\\?\UNC\server\share\.git",
            r"\\?\unc\server\share\.git",
            "/a/../repo/.git",
        ];
        let mut records = Vec::new();
        for value in values {
            let id = ObjectId::mint();
            connection
                .execute(
                    "INSERT INTO objects VALUES (?1,'task_sync_binding',?2)",
                    params![
                        id.as_str(),
                        serde_json::to_vec(
                            &serde_json::json!({"repository":{"git_directory":value}}),
                        )
                        .unwrap()
                    ],
                )
                .unwrap();
            let actual: String = connection
                .query_row(
                    &format!(
                        "SELECT {} FROM objects WHERE object_id=?1",
                        root_expression(ROOT)
                    ),
                    [id.as_str()],
                    |row| row.get(0),
                )
                .unwrap();
            let expected = value
                .strip_prefix(r"\\?\UNC\")
                .map(|rest| format!(r"\\{rest}"))
                .or_else(|| value.strip_prefix(r"\\?\").map(str::to_owned))
                .unwrap_or_else(|| value.to_owned());
            assert_eq!(actual, expected);
            records.push((id, value));
        }
        for selected in values {
            let actual = bindings(&connection, Bindings::Root(Path::new(selected))).unwrap();
            let mut expected = records
                .iter()
                .filter(|(_, value)| {
                    crate::storage::store_path_text(Path::new(value))
                        == crate::storage::store_path_text(Path::new(selected))
                })
                .map(|(id, _)| id.clone())
                .collect::<Vec<_>>();
            expected.sort();
            assert_eq!(actual, expected, "{selected}");
        }
    }
}
