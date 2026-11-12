//! Canonical originals and enrolled foreign namespaces; no native activation.

use std::collections::{BTreeMap, BTreeSet};

use crate::task_sync::origin::{Enrollment, PRODUCING_ORIGIN_KIND};

use super::{
    CanonicalObject, Connection, EvidenceError, EvidencePack, Member, ObjectId, OptionalExtension,
    RecordEnvelope, RecordKind, Result, SqliteStore, invalid, object_on, params,
};

pub(super) fn compare_on(connection: &Connection, record: &RecordEnvelope) -> Result<bool> {
    match object_on(connection, record.object_id())? {
        None => Ok(false),
        Some((kind, bytes))
            if kind == record.kind().as_str() && bytes == record.canonical_bytes() =>
        {
            Ok(true)
        }
        Some(_) => Err(EvidenceError::RecordConflict(record.object_id().clone()).into()),
    }
}

pub(super) fn load_on(connection: &Connection, member: &Member) -> Result<RecordEnvelope> {
    let (kind, bytes) = object_on(connection, &member.object_id)?
        .ok_or_else(|| invalid("retained evidence original is missing"))?;
    if kind != member.object_kind.as_str() {
        return Err(EvidenceError::RecordConflict(member.object_id.clone()).into());
    }
    Ok(RecordEnvelope::from_canonical(
        member.origin.clone(),
        member.object_id.clone(),
        &kind,
        bytes,
    )?)
}

pub(super) fn insert_on(connection: &Connection, record: &RecordEnvelope) -> Result<()> {
    let original = CanonicalObject::stored(record.object_id(), record.canonical_bytes().to_vec())?;
    SqliteStore::insert_object(connection, record.kind().as_str(), &original)?;
    Ok(())
}

fn enrollment_on(
    connection: &Connection,
    incoming: &BTreeMap<&ObjectId, &RecordEnvelope>,
    origin: &ObjectId,
) -> Result<Enrollment> {
    let bytes = if let Some(record) = incoming.get(origin) {
        if record.kind() != RecordKind::ProducingOrigin {
            return Err(invalid("origin reference does not name an enrollment"));
        }
        record.canonical_bytes().to_vec()
    } else {
        let (kind, bytes) = object_on(connection, origin)?
            .ok_or_else(|| invalid("origin enrollment lineage is unavailable"))?;
        if kind != PRODUCING_ORIGIN_KIND {
            return Err(invalid("origin reference does not name an enrollment"));
        }
        bytes
    };
    let enrollment: Enrollment = serde_json::from_slice(&bytes)?;
    if enrollment.project_id.0.trim().is_empty()
        || enrollment.enrollment_operation_id.is_nil()
        || enrollment.actor.actor_id.trim().is_empty()
    {
        return Err(invalid("invalid retained origin enrollment"));
    }
    Ok(enrollment)
}

/// Validate the namespace against original enrollment objects, including incoming
/// ancestors. An already indexed prefix is read by key, not rewalked in deltas.
pub(super) fn validate_origins(connection: &Connection, pack: &EvidencePack) -> Result<()> {
    let incoming = pack
        .records()
        .iter()
        .map(|record| (record.object_id(), record))
        .collect::<BTreeMap<_, _>>();
    let mut validated = BTreeSet::new();
    for origin in std::iter::once(pack.publishing_origin())
        .chain(pack.records().iter().map(RecordEnvelope::origin))
        .chain(
            pack.records()
                .iter()
                .filter(|record| record.kind() == RecordKind::ProducingOrigin)
                .map(RecordEnvelope::object_id),
        )
    {
        let mut current = Some(origin.clone());
        let mut path = BTreeSet::new();
        while let Some(id) = current {
            if validated.contains(&id) {
                break;
            }
            if !path.insert(id.clone()) {
                return Err(invalid("origin enrollment lineage contains a cycle"));
            }
            let enrollment = enrollment_on(connection, &incoming, &id)?;
            if &enrollment.project_id != pack.project_id() {
                return Err(invalid("origin enrollment belongs to another project"));
            }
            let indexed: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM work_task_sync_evidence_origins
                     WHERE origin_id=?1 AND project_id=?2)
                 OR EXISTS(SELECT 1 FROM work_task_sync_producing_origin_members
                     WHERE origin_id=?1 AND project_id=?2)",
                params![id.as_str(), pack.project_id().0],
                |row| row.get(0),
            )?;
            if indexed {
                break;
            }
            current = enrollment.predecessor_origin_id;
        }
        validated.extend(path);
    }
    Ok(())
}

pub(super) fn validate_profile(connection: &Connection, pack: &EvidencePack) -> Result<()> {
    for record in pack.records() {
        match record.kind() {
            RecordKind::MemoryVersion => {
                let memory: crate::MemoryVersion =
                    serde_json::from_slice(record.canonical_bytes())?;
                if !matches!(memory.scope,crate::Scope::Work { project,.. } if project==*pack.project_id())
                {
                    return Err(invalid(
                        "only selected work-scoped memory belongs in evidence exchange",
                    ));
                }
            }
            RecordKind::MemoryAssertionEvent => {
                let assertion: crate::domain::MemoryAssertionEvent =
                    serde_json::from_slice(record.canonical_bytes())?;
                let bytes = if let Some(version) = pack
                    .records()
                    .iter()
                    .find(|candidate| candidate.object_id() == &assertion.version)
                {
                    if version.kind() != RecordKind::MemoryVersion {
                        return Err(invalid("memory assertion version kind differs"));
                    }
                    version.canonical_bytes().to_vec()
                } else {
                    let (kind, bytes) = object_on(connection, &assertion.version)?
                        .ok_or_else(|| invalid("memory assertion version is missing"))?;
                    if kind != RecordKind::MemoryVersion.as_str() {
                        return Err(invalid("memory assertion version kind differs"));
                    }
                    bytes
                };
                let memory: crate::MemoryVersion = serde_json::from_slice(&bytes)?;
                if assertion.memory_id != memory.memory_id
                    || !matches!(memory.scope,crate::Scope::Work { project,.. } if project==*pack.project_id())
                {
                    return Err(invalid(
                        "memory assertion does not name selected work-scoped memory",
                    ));
                }
            }
            _ => {}
        }
    }
    Ok(())
}

pub(super) fn index_origins(connection: &Connection, pack: &EvidencePack) -> Result<()> {
    for record in pack
        .records()
        .iter()
        .filter(|record| record.kind() == RecordKind::ProducingOrigin)
    {
        let enrollment: Enrollment = serde_json::from_slice(record.canonical_bytes())?;
        let actual: Option<(String,Option<String>)> = connection.query_row(
            "SELECT project_id,predecessor_id FROM work_task_sync_evidence_origins WHERE origin_id=?1",
            [record.object_id().as_str()],|row| Ok((row.get(0)?,row.get(1)?))).optional()?;
        if actual.is_some_and(|actual| {
            actual
                != (
                    enrollment.project_id.0.clone(),
                    enrollment
                        .predecessor_origin_id
                        .as_ref()
                        .map(ToString::to_string),
                )
        }) {
            return Err(invalid(
                "foreign origin projection differs from its enrollment",
            ));
        }
        connection.execute(
            "INSERT INTO work_task_sync_evidence_origins(origin_id,project_id,predecessor_id)
             VALUES (?1,?2,?3) ON CONFLICT(origin_id) DO NOTHING",
            params![
                record.object_id().as_str(),
                enrollment.project_id.0,
                enrollment
                    .predecessor_origin_id
                    .as_ref()
                    .map(ObjectId::as_str)
            ],
        )?;
    }
    Ok(())
}
