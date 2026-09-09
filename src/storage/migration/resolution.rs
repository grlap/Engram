//! Narrow historical-reference resolution; ordinary canonical get stays exact.

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use crate::{CanonicalObject, ObjectHash, SqliteStore, StoreError};

#[cfg(test)]
mod tests;

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ObjectBinding {
    pub profile: String,
    pub source: ObjectHash,
    pub target: ObjectHash,
    pub kind: String,
}

fn invalid(message: &str) -> StoreError {
    StoreError::InvalidWorkProjection(format!("migration provenance: {message}"))
}

impl SqliteStore {
    pub(crate) fn migrated_note_prefix(
        &self,
        project: &crate::ProjectId,
        work: crate::WorkId,
        prefix: &str,
    ) -> Result<Vec<ObjectHash>, StoreError> {
        let mut query = self.connection.prepare("SELECT object_hash FROM migration_original_objects WHERE object_kind='work_observation' AND object_hash>=?1 AND object_hash<?2 AND json_extract(canonical_json,'$.project_id')=?3 AND json_extract(canonical_json,'$.work_id')=?4")?;
        let mut found = Vec::new();
        for row in query.query_map(
            params![prefix, format!("{prefix}g"), project.0, work.0.to_string()],
            |row| row.get::<_, String>(0),
        )? {
            let hash: ObjectHash = row?
                .parse()
                .map_err(|_| invalid("invalid original observation address"))?;
            found.push(resolve_on(&self.connection, &hash)?);
        }
        Ok(found)
    }

    /// Resolves a retained historical address at an explicitly chosen boundary.
    /// `get` itself never aliases addresses or accepts bytes under the wrong hash.
    pub(crate) fn resolve_migrated_reference(
        &self,
        hash: &ObjectHash,
    ) -> Result<ObjectHash, StoreError> {
        resolve_on(&self.connection, hash)
    }
}

pub(super) fn verify_pending_deliveries(store: &SqliteStore) -> Result<(), StoreError> {
    store.work_read_snapshot(|store| {
        let mut query = store.connection.prepare("SELECT project_id,session_id,project_cursor,tentative_project_cursor,tentative_delivery_payload_hash,tentative_delivery_payload FROM work_session_state WHERE tentative_project_cursor IS NOT NULL")?;
        let mut rows = query.query([])?;
        while let Some(row) = rows.next()? {
            let project = crate::ProjectId(row.get(0)?);
            let session = crate::SessionId(row.get(1)?);
            let hash: ObjectHash = row.get::<_,String>(4)?.parse().map_err(|_| invalid("invalid staged payload hash"))?;
            let payload = CanonicalObject::verify(&hash,row.get(5)?)?;
            crate::work_service::validate_migration_delivery(store,&session,&project,row.get(2)?,row.get(3)?,&payload)?;
        }
        Ok(())
    })
}

pub(super) fn resolve_on(
    connection: &Connection,
    hash: &ObjectHash,
) -> Result<ObjectHash, StoreError> {
    let original: Option<String> = connection
        .query_row(
            "SELECT object_kind FROM migration_original_objects WHERE object_hash=?1",
            [hash.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    let Some(kind) = original else {
        return Ok(hash.clone());
    };
    // Identity entries are mandatory too: absent never means unchanged.
    let row: Option<(String, String)> = connection
        .query_row(
            "SELECT target_hash,binding_hash FROM migration_object_map WHERE source_hash=?1",
            [hash.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (target, binding_hash) = row.ok_or_else(|| invalid("historical address has no mapping"))?;
    let binding_hash: ObjectHash = binding_hash
        .parse()
        .map_err(|_| invalid("invalid binding address"))?;
    let bytes: Vec<u8> = connection.query_row("SELECT canonical_json FROM objects WHERE object_hash=?1 AND object_kind='migration_object_binding'",[binding_hash.as_str()],|row| row.get(0)).optional()?.ok_or_else(|| invalid("canonical binding is missing"))?;
    let binding: ObjectBinding = CanonicalObject::verify(&binding_hash, bytes)
        .and_then(|object| object.decode())
        .map_err(|error| invalid(&format!("canonical binding failed verification: {error}")))?;
    if binding.profile != "aggregate-root-v1"
        || binding.source != *hash
        || binding.target.as_str() != target
        || binding.kind != kind
    {
        return Err(invalid("mapping row differs from its canonical binding"));
    }
    let target_kind: String = connection
        .query_row(
            "SELECT object_kind FROM objects WHERE object_hash=?1",
            [&target],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| invalid("mapped canonical object is missing"))?;
    if target_kind != kind {
        return Err(invalid("mapping changes canonical object kind"));
    }
    Ok(binding.target)
}

type ReexpressedAudit = (Vec<u8>, String, Vec<u8>, String, String, String, String);

/// The sole replay exception: pre-migration `complete_work` results in this source
/// profile were expressed in the current shape. All original bytes remain here.
/// Native results and every other operation keep their ordinary exact-byte rule.
pub(in crate::storage) fn validate_reexpressed_result_on(
    connection: &Connection,
    operation: &str,
    key: &str,
    result: &[u8],
) -> Result<(), StoreError> {
    if operation != "complete_work" {
        return Ok(());
    }
    let count: i64 = connection.query_row("SELECT COUNT(*) FROM migration_reexpressed_results WHERE operation=?1 AND idempotency_key=?2", params![operation,key], |row| row.get(0))?;
    if count > 1 {
        return Err(invalid("ambiguous per-key replay audit"));
    }
    let row: Option<ReexpressedAudit> = connection.query_row("SELECT source_result,source_result_hash,target_result,target_result_hash,source_seal,target_seal,project_id FROM migration_reexpressed_results WHERE operation=?1 AND idempotency_key=?2",params![operation,key],|row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?,row.get(4)?,row.get(5)?,row.get(6)?))).optional()?;
    let Some((source, source_digest, target, target_digest, source_seal, target_seal, project)) =
        row
    else {
        let value: serde_json::Value = serde_json::from_slice(result)?;
        let hash = CanonicalObject::freeze(&value)?;
        let migrated: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM migration_object_map map JOIN migration_original_objects original ON original.object_hash=map.source_hash WHERE map.target_hash=?1 AND original.object_kind='completion_seal')",[hash.hash().as_str()],|row|row.get(0))?;
        return if migrated {
            Err(invalid("re-expressed completion has no per-key audit"))
        } else {
            Ok(())
        };
    };
    if ObjectHash::from_canonical_bytes(&source).as_str() != source_digest
        || ObjectHash::from_canonical_bytes(&target).as_str() != target_digest
        || target != result
    {
        return Err(invalid(
            "re-expressed result differs from its retained audit",
        ));
    }
    let original: serde_json::Value = serde_json::from_slice(&source)?;
    let original = CanonicalObject::freeze(&original)?;
    if original.hash().as_str() != source_seal {
        return Err(invalid("original result is not the recorded original seal"));
    }
    let mapped = resolve_on(connection, original.hash())?;
    let value: serde_json::Value = serde_json::from_slice(&target)?;
    if value
        .pointer("/root_execution/project_id")
        .and_then(serde_json::Value::as_str)
        != Some(project.as_str())
    {
        return Err(invalid("per-key replay audit has a different project"));
    }
    if mapped.as_str() != target_seal || CanonicalObject::freeze(&value)?.hash() != &mapped {
        return Err(invalid(
            "re-expressed completion lacks its exact seal mapping",
        ));
    }
    Ok(())
}
