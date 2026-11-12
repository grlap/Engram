//! Keyed terminal union and destination decisions, separate from native credit.

use super::{
    Connection, ObjectId, OptionalExtension, RecordKind, Result, StoreError, invalid, object_on,
    params, stored_id,
};
use crate::task_sync::evidence::EvidenceError;
use crate::task_sync::evidence::verification::{ExecutionKey, TerminalFact};

pub(crate) mod applicability;

#[cfg(test)]
mod tests;

pub(super) fn create_schema(connection: &Connection) -> std::result::Result<(), StoreError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS work_task_sync_evidence_execution_keys (
            project_id TEXT NOT NULL, host_id TEXT NOT NULL, run_id TEXT NOT NULL,
            representative_id TEXT NOT NULL REFERENCES objects(object_id),
            conflicted INTEGER NOT NULL CHECK(conflicted IN (0,1)),
            PRIMARY KEY(project_id,host_id,run_id)
         ) STRICT;
         CREATE TABLE IF NOT EXISTS work_task_sync_evidence_terminals (
            object_id TEXT PRIMARY KEY REFERENCES objects(object_id),
            project_id TEXT NOT NULL, host_id TEXT NOT NULL, run_id TEXT NOT NULL,
            FOREIGN KEY(project_id,host_id,run_id) REFERENCES work_task_sync_evidence_execution_keys(project_id,host_id,run_id)
         ) STRICT;
         CREATE INDEX IF NOT EXISTS work_task_sync_evidence_terminal_key
            ON work_task_sync_evidence_terminals(project_id,host_id,run_id,object_id);",
    )?;
    applicability::create_schema(connection)
}

fn fact_on(connection: &Connection, id: &ObjectId) -> Result<Option<TerminalFact>> {
    let (kind, bytes) =
        object_on(connection, id)?.ok_or_else(|| invalid("terminal union original is missing"))?;
    let kind = RecordKind::parse(&kind)?;
    Ok(TerminalFact::from_bytes(id.clone(), kind, &bytes)?)
}

/// One known execution's state, without complete terminal or source payloads.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ExecutionUnion {
    pub contributors: Vec<ObjectId>,
    pub conflicted: bool,
}

/// Bounded decision basis; the original union remains keyed and untruncated.
pub(crate) fn union_on(connection: &Connection, key: &ExecutionKey) -> Result<ExecutionUnion> {
    let conflicted: Option<bool> = connection.query_row(
        "SELECT conflicted FROM work_task_sync_evidence_execution_keys WHERE project_id=?1 AND host_id=?2 AND run_id=?3",
        params![key.project.0,key.host,key.run], |row| row.get(0),
    ).optional()?;
    let mut statement = connection.prepare(
        "SELECT object_id FROM work_task_sync_evidence_terminals WHERE project_id=?1 AND host_id=?2 AND run_id=?3 ORDER BY object_id")?;
    let mut contributors = Vec::new();
    for row in statement.query_map(params![key.project.0, key.host, key.run], |row| {
        row.get::<_, String>(0)
    })? {
        contributors.push(stored_id(row?)?);
        crate::task_sync::evidence::limits::enforce(
            crate::task_sync::evidence::limits::LimitKind::ComponentTooLarge,
            None,
            crate::task_sync::evidence::limits::Dimension::Objects,
            contributors.len(),
            crate::task_sync::evidence::limits::MAX_COMPONENT_OBJECTS,
        )
        .map_err(EvidenceError::from)?;
    }
    Ok(ExecutionUnion {
        contributors,
        conflicted: conflicted.unwrap_or(false),
    })
}

fn append_fact(connection: &Connection, fact: &TerminalFact) -> Result<()> {
    let key = &fact.key;
    let representative: Option<String> = connection.query_row(
        "SELECT representative_id FROM work_task_sync_evidence_execution_keys WHERE project_id=?1 AND host_id=?2 AND run_id=?3",
        params![key.project.0,key.host,key.run], |row| row.get(0),
    ).optional()?;
    let conflict = if let Some(id) = representative {
        let other = fact_on(connection, &stored_id(id)?)?
            .ok_or_else(|| invalid("terminal representative is not authoritative"))?;
        !fact.same_terminal(&other)
    } else {
        connection.execute(
            "INSERT INTO work_task_sync_evidence_execution_keys(project_id,host_id,run_id,representative_id,conflicted) VALUES (?1,?2,?3,?4,0)",
            params![key.project.0,key.host,key.run,fact.record.as_str()],
        )?;
        false
    };
    connection.execute(
        "INSERT INTO work_task_sync_evidence_terminals(object_id,project_id,host_id,run_id) VALUES (?1,?2,?3,?4) ON CONFLICT(object_id) DO NOTHING",
        params![fact.record.as_str(),key.project.0,key.host,key.run],
    )?;
    if conflict {
        connection.execute(
            "UPDATE work_task_sync_evidence_execution_keys SET conflicted=1 WHERE project_id=?1 AND host_id=?2 AND run_id=?3",
            params![key.project.0,key.host,key.run],
        )?;
        // Historical decisions and consumed credit are immutable. Only the
        // derived current eligibility changes here, including a decision that
        // cannot be used again after its earlier consumption.
        connection.execute(
            "UPDATE work_task_sync_evidence_applicability SET invalidated=1 WHERE decision_id IN (
               SELECT decision_id FROM work_task_sync_evidence_applicability_keys WHERE project_id=?1 AND host_id=?2 AND run_id=?3)",
            params![key.project.0,key.host,key.run],
        )?;
    }
    Ok(())
}

fn index_record(
    connection: &Connection,
    id: &ObjectId,
    kind: RecordKind,
    bytes: &[u8],
) -> Result<()> {
    let Some(fact) = TerminalFact::from_bytes(id.clone(), kind, bytes)? else {
        return Ok(());
    };
    if kind == RecordKind::RemoteVerificationAssociation {
        let association: crate::domain::RemoteVerificationAssociationRecord =
            serde_json::from_slice(bytes)?;
        let origin = association
            .origin
            .as_ref()
            .ok_or_else(|| invalid("terminal association lacks its origin"))?;
        let original = fact_on(connection, origin)?
            .filter(|original| original.key == fact.key)
            .ok_or_else(|| invalid("terminal association origin differs from its execution"))?;
        let origin_kind = object_on(connection, origin)?.map(|(kind, _)| kind);
        if origin_kind.as_deref() != Some(crate::domain::REMOTE_TERMINAL_ORIGIN_KIND) {
            return Err(invalid("terminal association does not link an origin"));
        }
        append_fact(connection, &original)?;
    }
    append_fact(connection, &fact)
}

/// Both native recording and foreign retention call this inside their writer.
pub(in crate::storage) fn index_remote_record_on(
    connection: &Connection,
    id: &ObjectId,
    kind: &str,
    bytes: &[u8],
) -> std::result::Result<(), StoreError> {
    if !matches!(
        kind,
        crate::domain::REMOTE_TERMINAL_ORIGIN_KIND
            | crate::domain::REMOTE_VERIFICATION_ASSOCIATION_KIND
    ) {
        return Ok(());
    }
    index_record(
        connection,
        id,
        RecordKind::parse(kind).map_err(|error| super::store_error(error.into()))?,
        bytes,
    )
    .map_err(super::store_error)
}

pub(super) fn retain_on(connection: &Connection, pack: &super::EvidencePack) -> Result<()> {
    for record in pack.records() {
        if matches!(
            record.kind(),
            RecordKind::RemoteTerminalOrigin | RecordKind::RemoteVerificationAssociation
        ) {
            let fact = TerminalFact::from_record(record)?;
            if fact
                .as_ref()
                .is_some_and(|fact| &fact.key.project != pack.project_id())
            {
                return Err(invalid("terminal fact belongs to another project"));
            }
            index_record(
                connection,
                record.object_id(),
                record.kind(),
                record.canonical_bytes(),
            )?;
        }
    }
    Ok(())
}

/// Explicit projection repair is allowed to enumerate retained history once.
pub(super) fn rebuild_on(connection: &Connection) -> Result<()> {
    applicability::clear(connection)?;
    connection.execute_batch("DELETE FROM work_task_sync_evidence_terminals; DELETE FROM work_task_sync_evidence_execution_keys;")?;
    let mut statement = connection.prepare(
        "SELECT object_id,object_kind,canonical_json FROM objects WHERE object_kind IN ('remote_terminal_origin','remote_verification_association') ORDER BY object_id")?;
    for row in statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Vec<u8>>(2)?,
        ))
    })? {
        let (id, kind, bytes) = row?;
        index_record(
            connection,
            &stored_id(id)?,
            RecordKind::parse(&kind)?,
            &bytes,
        )?;
    }
    applicability::rebuild(connection)
}

pub(super) fn verify_on(connection: &Connection) -> Result<()> {
    let mut count = 0_i64;
    let mut statement = connection.prepare("SELECT object_id,object_kind,canonical_json FROM objects WHERE object_kind IN ('remote_terminal_origin','remote_verification_association')")?;
    for row in statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Vec<u8>>(2)?,
        ))
    })? {
        let (id, kind, bytes) = row?;
        if let Some(fact) =
            TerminalFact::from_bytes(stored_id(id)?, RecordKind::parse(&kind)?, &bytes)?
        {
            let actual: (String,String,String) = connection.query_row("SELECT project_id,host_id,run_id FROM work_task_sync_evidence_terminals WHERE object_id=?1",[fact.record.as_str()],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?;
            if actual != (fact.key.project.0, fact.key.host, fact.key.run) {
                return Err(invalid("terminal contributor projection differs"));
            }
            count += 1;
        }
    }
    let actual: i64 = connection.query_row(
        "SELECT count(*) FROM work_task_sync_evidence_terminals",
        [],
        |row| row.get(0),
    )?;
    if actual != count {
        return Err(invalid("terminal contributor projection has extra rows"));
    }
    let mut statement = connection.prepare("SELECT project_id,host_id,run_id,representative_id,conflicted FROM work_task_sync_evidence_execution_keys")?;
    for row in statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, bool>(4)?,
        ))
    })? {
        let (project, host, run, representative, conflicted) = row?;
        let key = ExecutionKey {
            project: crate::ProjectId(project),
            host,
            run,
        };
        let original = fact_on(connection, &stored_id(representative)?)?
            .ok_or_else(|| invalid("invalid terminal representative"))?;
        let mut contributors = connection.prepare("SELECT object_id FROM work_task_sync_evidence_terminals WHERE project_id=?1 AND host_id=?2 AND run_id=?3")?;
        let mut different = false;
        let mut found = false;
        for row in contributors.query_map(params![key.project.0, key.host, key.run], |row| {
            row.get::<_, String>(0)
        })? {
            found = true;
            let fact = fact_on(connection, &stored_id(row?)?)?
                .ok_or_else(|| invalid("unsupported terminal contributor"))?;
            different |= !fact.same_terminal(&original);
        }
        if !found || original.key != key || conflicted != different {
            return Err(invalid("terminal union projection differs"));
        }
    }
    applicability::verify(connection)
}
