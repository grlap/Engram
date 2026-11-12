//! Every row here is derived from immutable local admissions and originals.

use super::{
    ADMISSION_KIND, Admission, Connection, MANIFEST_KIND, Manifest, ObjectId, PersistenceError,
    Result, StoreError, invalid, params, records, store_error, stored_id, witnesses,
};

pub(in crate::storage) fn create_schema(
    connection: &Connection,
) -> std::result::Result<(), StoreError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS work_task_sync_evidence_packs (
             pack_id TEXT PRIMARY KEY REFERENCES objects(object_id),
             project_id TEXT NOT NULL,
             admission_id TEXT NOT NULL UNIQUE REFERENCES objects(object_id),
             publishing_origin TEXT NOT NULL REFERENCES objects(object_id),
             position INTEGER NOT NULL CHECK(position>0),
             UNIQUE(project_id,position)
         ) STRICT;
         CREATE TABLE IF NOT EXISTS work_task_sync_evidence_members (
             pack_id TEXT NOT NULL REFERENCES work_task_sync_evidence_packs(pack_id),
             ordinal INTEGER NOT NULL CHECK(ordinal>=0),
             object_id TEXT NOT NULL REFERENCES objects(object_id),
             object_kind TEXT NOT NULL,
             origin_id TEXT NOT NULL REFERENCES objects(object_id),
             PRIMARY KEY(pack_id,ordinal), UNIQUE(pack_id,object_id)
         ) STRICT;
         CREATE INDEX IF NOT EXISTS work_task_sync_evidence_members_original
             ON work_task_sync_evidence_members(object_id,origin_id);
         CREATE TABLE IF NOT EXISTS work_task_sync_evidence_imported (
             object_id TEXT PRIMARY KEY REFERENCES objects(object_id),
             admission_id TEXT NOT NULL REFERENCES objects(object_id)
         ) STRICT;
         CREATE TABLE IF NOT EXISTS work_task_sync_evidence_origins (
             origin_id TEXT PRIMARY KEY REFERENCES objects(object_id),
             project_id TEXT NOT NULL,
             predecessor_id TEXT REFERENCES objects(object_id)
         ) STRICT;",
    )?;
    witnesses::create_schema(connection)?;
    super::verification::create_schema(connection)?;
    super::acceptance::create_schema(connection)?;
    Ok(())
}

pub(super) fn append_on(
    connection: &Connection,
    id: &ObjectId,
    admission: &Admission,
    manifest: &Manifest,
) -> Result<()> {
    if admission.position <= 0 || admission.project_id != manifest.project_id {
        return Err(invalid("invalid evidence admission position or scope"));
    }
    connection.execute(
        "INSERT INTO work_task_sync_evidence_packs(pack_id,project_id,admission_id,publishing_origin,position)
         VALUES (?1,?2,?3,?4,?5)",
        params![admission.pack_id.as_str(), admission.project_id.0, id.as_str(), manifest.publishing_origin.as_str(), admission.position],
    )?;
    for (ordinal, member) in manifest.members.iter().enumerate() {
        let ordinal = i64::try_from(ordinal)
            .map_err(|_| invalid("evidence member ordinal exceeds SQLite range"))?;
        connection.execute(
            "INSERT INTO work_task_sync_evidence_members(pack_id,ordinal,object_id,object_kind,origin_id) VALUES (?1,?2,?3,?4,?5)",
            params![admission.pack_id.as_str(), ordinal, member.object_id.as_str(), member.object_kind.as_str(), member.origin.as_str()],
        )?;
    }
    let mut seen = std::collections::BTreeSet::new();
    for original in &admission.imported_objects {
        if !seen.insert(original)
            || !manifest
                .members
                .iter()
                .any(|member| &member.object_id == original)
        {
            return Err(invalid(
                "evidence admission contains a duplicate or unrelated original",
            ));
        }
        connection.execute(
            "INSERT INTO work_task_sync_evidence_imported(object_id,admission_id) VALUES (?1,?2)",
            params![original.as_str(), id.as_str()],
        )?;
    }
    Ok(())
}

fn admissions(connection: &Connection) -> Result<Vec<(ObjectId, Admission)>> {
    let mut statement = connection.prepare("SELECT object_id,canonical_json FROM objects WHERE object_kind=?1 ORDER BY json_extract(canonical_json,'$.project_id'),json_extract(canonical_json,'$.position')")?;
    let mut result = Vec::new();
    for row in statement.query_map([ADMISSION_KIND], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
    })? {
        let (id, bytes) = row?;
        result.push((stored_id(id)?, serde_json::from_slice(&bytes)?));
    }
    Ok(result)
}

pub(in crate::storage) fn rebuild(connection: &Connection) -> std::result::Result<(), StoreError> {
    rebuild_on(connection).map_err(store_error)
}

fn rebuild_on(connection: &Connection) -> Result<()> {
    super::acceptance::rebuild(connection)?;
    super::verification::rebuild_on(connection)?;
    witnesses::clear(connection)?;
    connection.execute_batch(
        "DELETE FROM work_task_sync_evidence_members;
        DELETE FROM work_task_sync_evidence_imported;
        DELETE FROM work_task_sync_evidence_origins;
        DELETE FROM work_task_sync_evidence_packs;",
    )?;
    let mut positions = std::collections::BTreeMap::new();
    let admissions = admissions(connection)?;
    for (id, admission) in &admissions {
        let next = positions
            .entry(admission.project_id.0.clone())
            .or_insert(0_i64);
        *next += 1;
        if admission.position != *next {
            return Err(invalid("evidence admission history contains a gap"));
        }
        let pack =
            super::load_retained_pack_on(connection, &admission.project_id, &admission.pack_id)?;
        append_on(connection, id, admission, &Manifest::from_pack(&pack))?;
    }
    // Membership is complete across every retained pack before any dependency
    // check. No transport ordering is an origin or feed ordering guarantee.
    for (_, admission) in &admissions {
        let pack =
            super::load_retained_pack_on(connection, &admission.project_id, &admission.pack_id)?;
        records::validate_origins(connection, &pack)?;
        records::validate_profile(connection, &pack)?;
    }
    for (_, admission) in &admissions {
        let pack =
            super::load_retained_pack_on(connection, &admission.project_id, &admission.pack_id)?;
        records::index_origins(connection, &pack)?;
    }
    witnesses::rebuild_on(connection)?;
    super::consumption::rebuild(connection)?;
    Ok(())
}

/// Exhaustive verification belongs to doctor, never a hot delta or replay.
pub(in crate::storage) fn verify(
    connection: &Connection,
    checked: &mut usize,
    findings: &mut Vec<String>,
) -> std::result::Result<(), StoreError> {
    if let Err(error) = verify_on(connection, checked) {
        match error {
            PersistenceError::Storage(StoreError::Sqlite(rusqlite::Error::QueryReturnedNoRows)) => {
                findings.push("task_sync_evidence:required projection row is missing".into());
            }
            PersistenceError::Storage(StoreError::Sqlite(error)) => return Err(error.into()),
            error => findings.push(format!("task_sync_evidence:{}", store_error(error))),
        }
    }
    Ok(())
}

fn verify_on(connection: &Connection, checked: &mut usize) -> Result<()> {
    let (mut packs, mut members, mut imported) = (0_i64, 0_i64, 0_i64);
    let mut positions = std::collections::BTreeMap::new();
    for (id, admission) in admissions(connection)? {
        *checked += 1;
        let next = positions
            .entry(admission.project_id.0.clone())
            .or_insert(0_i64);
        *next += 1;
        if admission.position != *next {
            return Err(invalid("evidence admission history contains a gap"));
        }
        let pack =
            super::load_retained_pack_on(connection, &admission.project_id, &admission.pack_id)?;
        let actual: (String,String,String,i64) = connection.query_row(
            "SELECT project_id,admission_id,publishing_origin,position FROM work_task_sync_evidence_packs WHERE pack_id=?1",
            [admission.pack_id.as_str()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)))?;
        if actual
            != (
                admission.project_id.0.clone(),
                id.to_string(),
                pack.publishing_origin().to_string(),
                admission.position,
            )
        {
            return Err(invalid("evidence pack projection differs"));
        }
        for (ordinal, member) in pack.records().iter().enumerate() {
            let ordinal = i64::try_from(ordinal)
                .map_err(|_| invalid("evidence member ordinal exceeds SQLite range"))?;
            let actual: (String,String,String) = connection.query_row(
                "SELECT object_id,object_kind,origin_id FROM work_task_sync_evidence_members WHERE pack_id=?1 AND ordinal=?2",
                params![pack.pack_id().as_str(),ordinal], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?;
            if actual
                != (
                    member.object_id().to_string(),
                    member.kind().as_str().to_string(),
                    member.origin().to_string(),
                )
            {
                return Err(invalid("evidence membership projection differs"));
            }
            members += 1;
        }
        let mut seen = std::collections::BTreeSet::new();
        for original in &admission.imported_objects {
            if !seen.insert(original)
                || !pack
                    .records()
                    .iter()
                    .any(|record| record.object_id() == original)
            {
                return Err(invalid("invalid imported original partition"));
            }
            let owner: String = connection.query_row(
                "SELECT admission_id FROM work_task_sync_evidence_imported WHERE object_id=?1",
                [original.as_str()],
                |row| row.get(0),
            )?;
            if owner != id.as_str() {
                return Err(invalid("imported original partition differs"));
            }
            let native: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM work_feed_entries WHERE object_id=?1)",
                [original.as_str()],
                |row| row.get(0),
            )?;
            if native {
                return Err(invalid("foreign original appears in a native feed"));
            }
            imported += 1;
        }
        records::validate_origins(connection, &pack)?;
        records::validate_profile(connection, &pack)?;
        witnesses::verify_pack(connection, &pack)?;
        packs += 1;
    }
    for (table, expected) in [
        ("work_task_sync_evidence_packs", packs),
        ("work_task_sync_evidence_members", members),
        ("work_task_sync_evidence_imported", imported),
    ] {
        let count: i64 =
            connection.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })?;
        if count != expected {
            return Err(invalid(
                "evidence projection contains missing or extra rows",
            ));
        }
    }
    let manifests: i64 = connection.query_row(
        "SELECT count(*) FROM objects WHERE object_kind=?1",
        [MANIFEST_KIND],
        |row| row.get(0),
    )?;
    if manifests != packs {
        return Err(invalid("evidence manifest lacks a local admission"));
    }
    let origin_difference: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM work_task_sync_evidence_origins origin LEFT JOIN objects original ON original.object_id=origin.origin_id WHERE original.object_kind IS NOT 'task_sync_producing_origin' OR origin.project_id IS NOT json_extract(original.canonical_json,'$.project_id') OR origin.predecessor_id IS NOT json_extract(original.canonical_json,'$.predecessor_origin_id') OR NOT EXISTS(SELECT 1 FROM work_task_sync_evidence_members member WHERE member.object_id=origin.origin_id AND member.object_kind='task_sync_producing_origin')) OR EXISTS(SELECT 1 FROM work_task_sync_evidence_members member WHERE member.object_kind='task_sync_producing_origin' AND NOT EXISTS(SELECT 1 FROM work_task_sync_evidence_origins origin WHERE origin.origin_id=member.object_id))",[],|row| row.get(0))?;
    if origin_difference {
        return Err(invalid("foreign origin projection differs"));
    }
    witnesses::verify_counts(connection)?;
    super::verification::verify_on(connection)?;
    super::acceptance::verify(connection)?;
    super::consumption::verify(connection)?;
    Ok(())
}
