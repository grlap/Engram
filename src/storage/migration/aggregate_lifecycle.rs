//! Test-only builder: a real current lifecycle, then predecessor-shaped rows.
//! Not a production importer path.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;

use chrono::{Duration, TimeZone, Utc};
use rusqlite::{Connection, OptionalExtension, params};
use serde_json::Value;

use crate::domain::{
    CompletionSeal, RootExecution, RootExecutionDelta, RootExecutionMember, RootExecutionRef,
    WorkId,
};
use crate::work_service::{
    LocalWorkService, WorkCompleteInput, WorkCompleteResult, WorkCompletionCaptureInput,
    WorkProposeInput, WorkUpdateInput,
};
use crate::{CanonicalObject, ObjectHash, ProjectId, RootExecutionId, SessionId};

use super::quoted;

pub(super) fn populate_completed_aggregate(path: &Path) -> ObjectHash {
    complete_current(path);
    derive_predecessor(path).expect("derive predecessor from completed current store")
}

fn at(second: i64) -> chrono::DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 8, 27, 3, 0, 0)
        .single()
        .expect("fixed timestamp")
        + Duration::seconds(second)
}

fn complete_current(path: &Path) {
    let service = LocalWorkService::new(
        path.to_path_buf(),
        ProjectId("migration-aggregate-lifecycle".into()),
        "agent".into(),
        SessionId("migration-aggregate-session".into()),
        Some("protocol-test".into()),
    );
    service
        .work_propose(
            WorkProposeInput::Root {
                evaluation_mode: None,
                external_ref: None,
                notes: Vec::new(),
                title: "Aggregate lifecycle fixture".into(),
                outcome: "completed predecessor with real projections".into(),
                acceptance: vec!["imported seal remaps and replays".into()],
                work_kind: None,
                priority: Some(1),
                labels: vec!["migration".into()],
                assigned_to: None,
                deferred_until: None,
                idempotency_key: "aggregate-lifecycle-root".into(),
            },
            at(0),
        )
        .expect("root");
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "aggregate-lifecycle-claim".into(),
            },
            at(1),
        )
        .expect("claim");
    service
        .work_update(
            WorkUpdateInput::Evidence {
                summary: "finding".into(),
                refs: vec!["test:aggregate-lifecycle".into()],
                attach: None,
                idempotency_key: "aggregate-lifecycle-evidence".into(),
            },
            at(2),
        )
        .expect("evidence");
    service
        .work_update(
            WorkUpdateInput::Checkpoint {
                summary: "progress".into(),
                evidence: None,
                idempotency_key: "aggregate-lifecycle-checkpoint".into(),
            },
            at(3),
        )
        .expect("checkpoint");
    let completed = service
        .work_complete(
            WorkCompleteInput {
                source_fingerprint: None,
                links: Vec::new(),
                link_basis: None,
                capture: Some(WorkCompletionCaptureInput {
                    summary: "delivered".into(),
                    refs: Vec::new(),
                }),
                evidence: Vec::new(),
                acceptance: None,
                note: None,
                idempotency_key: "aggregate-lifecycle-complete".into(),
            },
            at(4),
        )
        .expect("complete");
    match completed {
        WorkCompleteResult::Completed(_) => {}
        WorkCompleteResult::Refused(refusal) => panic!("completion refused: {refusal:?}"),
    }
    let store = crate::SqliteStore::open_unresolved(path).expect("current");
    assert!(store.verify_all().expect("current doctor").is_healthy());
}

fn derive_predecessor(path: &Path) -> Result<ObjectHash, super::MigrationError> {
    let connection = Connection::open(path)?;
    connection.execute_batch("PRAGMA foreign_keys=OFF")?;
    let seals: Vec<String> = connection
        .prepare(
            "SELECT object_hash FROM objects WHERE object_kind = 'completion_seal' ORDER BY rowid",
        )?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    let mut renamed = HashMap::new();
    let mut last_seal = None;
    for hash in seals {
        let rewritten = rewrite_seal(&connection, &hash)?;
        last_seal = Some(rewritten.clone());
        renamed.insert(hash, rewritten);
    }
    let events: Vec<String> = connection
        .prepare("SELECT object_hash FROM objects WHERE object_kind = 'work_event' ORDER BY rowid")?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    for hash in events {
        let rewritten = rewrite_event(&connection, &hash, &renamed)?;
        renamed.insert(hash, rewritten);
    }
    remap_identity_objects(&connection, &renamed)?;
    remap_attempt_bodies(&connection, &renamed)?;
    assert_source_reference_closure(&connection)?;
    let roots = load_projected_roots(&connection)?;
    downgrade_root_table(&connection, &roots)?;
    connection.execute(
        "DELETE FROM objects WHERE object_kind = 'work_root_delta'",
        [],
    )?;
    connection.execute_batch(
        "DROP INDEX IF EXISTS objects_work_source_key;
         DROP INDEX IF EXISTS objects_work_source_proposal_work;
         DROP INDEX IF EXISTS work_items_source_snapshot;
         DROP INDEX IF EXISTS control_work_leases_task_state;
         CREATE INDEX control_work_leases_task_state
                   ON control_work_leases(task_id, state, expires_at_ms);",
    )?;
    assert_source_reference_closure(&connection)?;
    let seal = last_seal.ok_or_else(|| super::refused("completed current store has no seal"))?;
    drop(connection);
    Ok(seal)
}

fn rewrite_event(
    connection: &Connection,
    hash: &str,
    seals: &HashMap<String, ObjectHash>,
) -> Result<ObjectHash, super::MigrationError> {
    let (kind, bytes, created): (String, Vec<u8>, String) = connection.query_row(
        "SELECT object_kind, canonical_json, created_at FROM objects WHERE object_hash = ?1",
        [hash],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let mut event: Value = serde_json::from_slice(&bytes)?;
    let root_value = event
        .get("root_execution")
        .cloned()
        .ok_or_else(|| super::refused("event has no root_execution"))?;
    if !root_value.is_null() {
        let address: RootExecutionRef = serde_json::from_value(root_value)?;
        let root = root_at(connection, &address)?;
        event["root_execution"] = serde_json::to_value(&root)?;
    }
    if let Some(Value::String(seal)) = event.pointer("/transition/seal").cloned()
        && let Some(rewritten) = seals.get(&seal)
        && let Some(slot) = event.pointer_mut("/transition/seal")
    {
        *slot = Value::String(rewritten.as_str().into());
    }
    if let Some(Value::String(seal)) = event.pointer("/run/completion_seal").cloned()
        && let Some(rewritten) = seals.get(&seal)
        && let Some(slot) = event.pointer_mut("/run/completion_seal")
    {
        *slot = Value::String(rewritten.as_str().into());
    }
    replace_object(connection, hash, &kind, &event, &created)
}

fn remap_identity_objects(
    connection: &Connection,
    renamed: &HashMap<String, ObjectHash>,
) -> Result<(), super::MigrationError> {
    let rows: Vec<(String, String, Vec<u8>, String)> = connection
        .prepare(
            "SELECT object_hash, object_kind, canonical_json, created_at FROM objects ORDER BY rowid",
        )?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)))?
        .collect::<Result<_, _>>()?;
    for (hash, kind, bytes, created) in rows {
        if kind == "work_event" || kind == "completion_seal" || kind == "work_root_delta" {
            continue;
        }
        let mut value: Value = serde_json::from_slice(&bytes)?;
        if !remap_value(&mut value, renamed) {
            continue;
        }
        replace_object(connection, &hash, &kind, &value, &created)?;
    }
    Ok(())
}

fn remap_attempt_bodies(
    connection: &Connection,
    renamed: &HashMap<String, ObjectHash>,
) -> Result<(), super::MigrationError> {
    let rows = connection
        .prepare("SELECT rowid, basis_json, result_json FROM work_protocol_attempts")?
        .query_map([], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, Option<Vec<u8>>>(1)?,
                row.get::<_, Option<Vec<u8>>>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (rowid, basis, result) in rows {
        let basis = remap_blob(basis, renamed)?;
        let result = remap_blob(result, renamed)?;
        connection.execute(
            "UPDATE work_protocol_attempts SET basis_json = ?1, result_json = ?2 WHERE rowid = ?3",
            params![basis, result, rowid],
        )?;
    }
    let rows: Vec<(i64, Option<String>)> = connection
        .prepare(
            "SELECT rowid, result_hash FROM work_protocol_attempts WHERE result_hash IS NOT NULL",
        )?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?;
    for (rowid, hash) in rows {
        let Some(hash) = hash else {
            continue;
        };
        let bytes: Vec<u8> = connection.query_row(
            "SELECT canonical_json FROM objects WHERE object_hash = ?1",
            [&hash],
            |row| row.get(0),
        )?;
        connection.execute(
            "UPDATE work_protocol_attempts SET result_json = ?1 WHERE rowid = ?2",
            params![bytes, rowid],
        )?;
    }
    Ok(())
}

fn remap_blob(
    bytes: Option<Vec<u8>>,
    renamed: &HashMap<String, ObjectHash>,
) -> Result<Option<Vec<u8>>, super::MigrationError> {
    let Some(bytes) = bytes else {
        return Ok(None);
    };
    let mut value: Value = serde_json::from_slice(&bytes)?;
    if remap_value(&mut value, renamed) {
        Ok(Some(serde_json::to_vec(&value)?))
    } else {
        Ok(Some(bytes))
    }
}

fn remap_value(value: &mut Value, renamed: &HashMap<String, ObjectHash>) -> bool {
    match value {
        Value::String(text) => {
            if let Some(rewritten) = renamed.get(text.as_str()) {
                *text = rewritten.as_str().into();
                true
            } else {
                false
            }
        }
        Value::Array(items) => {
            let mut changed = false;
            for item in items {
                changed |= remap_value(item, renamed);
            }
            changed
        }
        Value::Object(fields) => {
            let mut changed = false;
            for item in fields.values_mut() {
                changed |= remap_value(item, renamed);
            }
            changed
        }
        _ => false,
    }
}

fn assert_source_reference_closure(connection: &Connection) -> Result<(), super::MigrationError> {
    let hashes: HashSet<String> = connection
        .prepare("SELECT object_hash FROM objects")?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    let rows: Vec<(String, String, Vec<u8>)> = connection
        .prepare("SELECT object_hash, object_kind, canonical_json FROM objects")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<Result<_, _>>()?;
    for (hash, kind, bytes) in rows {
        let value: Value = serde_json::from_slice(&bytes)?;
        for reference in declared_canonical_references(&kind, &value) {
            if !hashes.contains(&reference) {
                return Err(super::refused(format!(
                    "predecessor {kind} {hash} cites missing {reference}"
                )));
            }
        }
    }
    let violations: i64 =
        connection.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if violations != 0 {
        return Err(super::refused(
            "predecessor source has foreign-key violations",
        ));
    }
    Ok(())
}

fn declared_canonical_references(kind: &str, value: &Value) -> Vec<String> {
    let mut refs = Vec::new();
    match kind {
        "work_event" => {
            push_pointer(value, "/work/source_snapshot_id", &mut refs);
            push_pointer(value, "/run/last_checkpoint", &mut refs);
            push_pointer(value, "/run/completion_seal", &mut refs);
            push_pointer(value, "/transition/seal", &mut refs);
            push_pointer(value, "/transition/checkpoint", &mut refs);
            push_pointer(value, "/transition/evidence", &mut refs);
            push_pointer(value, "/transition/version", &mut refs);
            push_pointer(value, "/transition/assertion", &mut refs);
            push_pointer(value, "/transition/offer", &mut refs);
        }
        "completion_seal" => {
            push_pointer(value, "/accepted_work_revision_hash", &mut refs);
            push_pointer(value, "/checkpoint", &mut refs);
            push_hashes(value.get("evidence"), &mut refs);
            push_hashes(value.get("environment"), &mut refs);
            push_hashes(value.get("required_child_seals"), &mut refs);
        }
        "work_observation" => {
            push_pointer(value, "/basis/event", &mut refs);
            push_pointer(value, "/basis/record", &mut refs);
        }
        "work_checkpoint" => push_hashes(value.get("evidence"), &mut refs),
        "work_protocol_result" => {
            push_pointer(value, "/seal", &mut refs);
            push_pointer(value, "/receipt/result", &mut refs);
            if let Some(Value::Array(items)) = value.pointer("/focus/history/items") {
                for item in items {
                    push_pointer(item, "/entry/object_hash", &mut refs);
                }
            }
            push_pointer(value, "/focus/run/last_checkpoint", &mut refs);
            push_pointer(value, "/focus/run/completion_seal", &mut refs);
        }
        _ => {}
    }
    refs
}

fn push_pointer(value: &Value, pointer: &str, refs: &mut Vec<String>) {
    if let Some(Value::String(hash)) = value.pointer(pointer) {
        refs.push(hash.clone());
    }
}

fn push_hashes(value: Option<&Value>, refs: &mut Vec<String>) {
    match value {
        Some(Value::String(hash)) => refs.push(hash.clone()),
        Some(Value::Array(items)) => {
            for item in items {
                if let Value::String(hash) = item {
                    refs.push(hash.clone());
                }
            }
        }
        _ => {}
    }
}

fn rewrite_seal(connection: &Connection, hash: &str) -> Result<ObjectHash, super::MigrationError> {
    let (kind, bytes, created): (String, Vec<u8>, String) = connection.query_row(
        "SELECT object_kind, canonical_json, created_at FROM objects WHERE object_hash = ?1",
        [hash],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let seal: CompletionSeal = serde_json::from_slice(&bytes)?;
    let root = root_at(connection, &seal.root_execution)?;
    let mut value = serde_json::to_value(&seal)?;
    let fields = value
        .as_object_mut()
        .ok_or_else(|| super::refused("seal is not an object"))?;
    fields.remove("root_execution");
    fields.insert(
        "expected_contributors".into(),
        serde_json::to_value(&root.expected_contributors)?,
    );
    fields.insert(
        "contributions".into(),
        serde_json::to_value(&root.contributions)?,
    );
    fields.insert("waivers".into(), serde_json::to_value(&root.waivers)?);
    replace_object(connection, hash, &kind, &value, &created)?;
    let new_hash = CanonicalObject::freeze(&value)?.hash().clone();
    connection.execute(
        "UPDATE work_completion_seals SET seal_json = ?1 WHERE seal_hash = ?2",
        params![serde_json::to_vec(&value)?, new_hash.as_str()],
    )?;
    let run_bytes: Option<Vec<u8>> = connection
        .query_row(
            "SELECT run_json FROM work_runs WHERE completion_seal_hash = ?1",
            [new_hash.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(run_bytes) = run_bytes {
        let mut run: Value = serde_json::from_slice(&run_bytes)?;
        run["completion_seal"] = Value::String(new_hash.as_str().into());
        connection.execute(
            "UPDATE work_runs SET run_json = ?1 WHERE completion_seal_hash = ?2",
            params![serde_json::to_vec(&run)?, new_hash.as_str()],
        )?;
    }
    connection.execute(
        "UPDATE work_operation_results SET result_json = ?1 WHERE operation = 'complete_work'",
        params![serde_json::to_vec(&value)?],
    )?;
    Ok(new_hash)
}

fn replace_object(
    connection: &Connection,
    old: &str,
    kind: &str,
    value: &Value,
    created: &str,
) -> Result<ObjectHash, super::MigrationError> {
    let object = CanonicalObject::freeze(value)?;
    let new = object.hash().clone();
    if new.as_str() == old {
        connection.execute(
            "UPDATE objects SET canonical_json = ?1 WHERE object_hash = ?2",
            params![object.bytes(), old],
        )?;
        return Ok(new);
    }
    connection.execute(
        "INSERT INTO objects(object_hash, object_kind, canonical_json, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![new.as_str(), kind, object.bytes(), created],
    )?;
    retarget_object_hash(connection, old, new.as_str())?;
    connection.execute("DELETE FROM objects WHERE object_hash = ?1", [old])?;
    Ok(new)
}

fn retarget_object_hash(
    connection: &Connection,
    old: &str,
    new: &str,
) -> Result<(), super::MigrationError> {
    let tables: Vec<String> = connection
        .prepare(
            "SELECT name FROM pragma_table_list WHERE type = 'table' AND name NOT LIKE 'sqlite_%'",
        )?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    for table in tables {
        if table == "objects" {
            continue;
        }
        let targets: Vec<(String, String)> = connection
            .prepare(&format!(
                "SELECT \"from\", \"table\" FROM pragma_foreign_key_list({})",
                quoted(&table)
            ))?
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
            .collect::<Result<_, _>>()?;
        for (column, target) in targets {
            if target != "objects" {
                continue;
            }
            connection.execute(
                &format!(
                    "UPDATE {} SET {} = ?1 WHERE {} = ?2",
                    quoted(&table),
                    quoted(&column),
                    quoted(&column)
                ),
                params![new, old],
            )?;
        }
    }
    Ok(())
}

fn load_delta(
    connection: &Connection,
    hash: &ObjectHash,
) -> Result<RootExecutionDelta, super::MigrationError> {
    let bytes: Vec<u8> = connection.query_row(
        "SELECT canonical_json FROM objects WHERE object_hash = ?1 AND object_kind = 'work_root_delta'",
        [hash.as_str()],
        |row| row.get(0),
    )?;
    CanonicalObject::verify(hash, bytes)?
        .decode()
        .map_err(Into::into)
}

fn root_at(
    connection: &Connection,
    address: &RootExecutionRef,
) -> Result<RootExecution, super::MigrationError> {
    let mut chain = Vec::new();
    let mut head = address.head.clone();
    loop {
        let delta = load_delta(connection, &head)?;
        let predecessor = delta.predecessor.clone();
        chain.push(delta);
        match predecessor {
            Some(previous) => head = previous,
            None => break,
        }
    }
    chain.reverse();
    let mut members: BTreeMap<ObjectHash, RootExecutionMember> = BTreeMap::new();
    let mut header = chain
        .first()
        .ok_or_else(|| super::refused("empty root delta chain"))?
        .header
        .clone();
    for delta in &chain {
        for member in &delta.removed {
            let hash = CanonicalObject::freeze(member)?.hash().clone();
            members.remove(&hash);
        }
        for member in &delta.added {
            members.insert(
                CanonicalObject::freeze(member)?.hash().clone(),
                member.clone(),
            );
        }
        header = delta.header.clone();
    }
    let mut root = RootExecution {
        schema_version: header.schema_version,
        root_execution_id: header.root_execution_id,
        project_id: header.project_id,
        root_id: header.root_id,
        generation: header.generation,
        state: header.state,
        revision: header.revision,
        run_ids: Vec::new(),
        required_child_seals: Vec::new(),
        required_child_waivers: Vec::new(),
        expected_contributors: Vec::new(),
        contributions: Vec::new(),
        waivers: Vec::new(),
        created_at: header.created_at,
        updated_at: header.updated_at,
    };
    for member in members.values() {
        match member {
            RootExecutionMember::Run(id) => root.run_ids.push(*id),
            RootExecutionMember::ChildSeal(hash) => root.required_child_seals.push(hash.clone()),
            RootExecutionMember::ChildWaiver(waiver) => {
                root.required_child_waivers.push(waiver.clone());
            }
            RootExecutionMember::Contributor(session) => {
                root.expected_contributors.push(session.clone());
            }
            RootExecutionMember::Contribution(contribution) => {
                root.contributions.push(contribution.clone());
            }
            RootExecutionMember::Waiver(waiver) => root.waivers.push(waiver.clone()),
        }
    }
    super::roots::normalize(&mut root)?;
    Ok(root)
}

fn load_projected_roots(
    connection: &Connection,
) -> Result<Vec<RootExecution>, super::MigrationError> {
    let ids: Vec<String> = connection
        .prepare("SELECT root_execution_id FROM work_root_executions")?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    let mut roots = Vec::new();
    for id in ids {
        let (project, root_id, generation): (String, String, i64) = connection.query_row(
            "SELECT project_id, root_id, generation FROM work_root_executions WHERE root_execution_id = ?1",
            [&id],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )?;
        let head: String = connection.query_row(
            "SELECT head_hash FROM work_root_executions WHERE root_execution_id = ?1",
            [&id],
            |row| row.get(0),
        )?;
        let address = RootExecutionRef {
            root_execution_id: RootExecutionId(id.parse().map_err(|_| super::refused("root id"))?),
            project_id: ProjectId(project),
            root_id: WorkId(root_id.parse().map_err(|_| super::refused("work id"))?),
            generation,
            head: head.parse().map_err(|_| super::refused("head hash"))?,
        };
        roots.push(root_at(connection, &address)?);
    }
    Ok(roots)
}

fn downgrade_root_table(
    connection: &Connection,
    roots: &[RootExecution],
) -> Result<(), super::MigrationError> {
    connection.execute_batch(
        "DROP TABLE work_root_members;
         DROP INDEX IF EXISTS work_root_execution_active;
         DROP TABLE work_root_executions;
         CREATE TABLE work_root_executions (
              root_execution_id TEXT PRIMARY KEY,
              project_id TEXT NOT NULL,
              root_id TEXT NOT NULL REFERENCES work_items(work_id),
              generation INTEGER NOT NULL,
              state TEXT NOT NULL,
              revision INTEGER NOT NULL,
              created_at_ms INTEGER NOT NULL,
              updated_at_ms INTEGER NOT NULL,
              execution_json BLOB NOT NULL,
              UNIQUE(root_id, generation)
          ) STRICT;
         CREATE UNIQUE INDEX work_root_execution_active
              ON work_root_executions(root_id) WHERE state = 'active';",
    )?;
    for root in roots {
        let state: String = serde_json::from_value(serde_json::to_value(root.state)?)?;
        connection.execute(
            "INSERT INTO work_root_executions(
                 root_execution_id, project_id, root_id, generation, state, revision,
                 created_at_ms, updated_at_ms, execution_json
             ) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)",
            params![
                root.root_execution_id.0.to_string(),
                root.project_id.0,
                root.root_id.0.to_string(),
                root.generation,
                state,
                root.revision,
                root.created_at.timestamp_millis(),
                root.updated_at.timestamp_millis(),
                serde_json::to_vec(root)?,
            ],
        )?;
    }
    Ok(())
}
