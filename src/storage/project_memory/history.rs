use std::collections::{BTreeMap, BTreeSet};

use super::{
    Connection, MemoryAssertionEvent, MemoryStatus, MemoryVersion, ObjectHash, ProjectMemoryFull,
    SCHEMA_VERSION, Scope, SqliteStore, StoreError, StoredProjectMemory, params,
    validate_keyed_project_memory_shape,
};

/// Canonical parent edges, not timestamps or projection order, number revisions.
/// Every keyed version must belong to this one root and linear chain. This is
/// shared by reads, live admission, integrity verification and projection replay.
pub(in crate::storage) fn project_memory_history_on(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
    key: &str,
) -> Result<Vec<StoredProjectMemory>, StoreError> {
    let hashes = connection
        .prepare(
            "SELECT object_hash FROM objects INDEXED BY objects_project_memory_key
         WHERE object_kind = 'memory_version'
           AND json_extract(canonical_json, '$.scope.kind') = 'project'
           AND json_type(canonical_json, '$.project_key') = 'text'
           AND json_extract(canonical_json, '$.scope.project') = ?1
           AND json_extract(canonical_json, '$.project_key') = ?2",
        )?
        .query_map(params![project_id.0, key], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let invalid = || {
        StoreError::InvalidMemoryProjection(format!(
            "project memory {key:?} has an invalid revision chain"
        ))
    };
    let mut versions = BTreeMap::new();
    let mut children = BTreeMap::new();
    let mut roots = Vec::new();
    for stored_hash in hashes {
        let hash = ObjectHash::from_stored(stored_hash.clone())
            .ok_or(StoreError::InvalidStoredHash(stored_hash))?;
        let version: MemoryVersion =
            SqliteStore::get_typed_object_on(connection, &hash, "memory_version")?
                .ok_or_else(&invalid)?;
        if version.project_key.as_deref() != Some(key)
            || !matches!(&version.scope, Scope::Project { project } if project == project_id)
        {
            return Err(invalid());
        }
        let assertions = connection
            .prepare(
                "SELECT object_hash FROM objects INDEXED BY objects_memory_assertion_version
             WHERE object_kind = 'memory_assertion_event'
               AND json_extract(canonical_json, '$.version') = ?1",
            )?
            .query_map([hash.as_str()], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let mut active = None;
        let mut terminal = None;
        for stored_assertion in assertions {
            let assertion_hash = ObjectHash::from_stored(stored_assertion.clone())
                .ok_or(StoreError::InvalidStoredHash(stored_assertion))?;
            let assertion: MemoryAssertionEvent = SqliteStore::get_typed_object_on(
                connection,
                &assertion_hash,
                "memory_assertion_event",
            )?
            .ok_or_else(&invalid)?;
            validate_keyed_project_memory_shape(&version, &assertion)?;
            if assertion.memory_id != version.memory_id
                || assertion.version != hash
                || assertion.schema_version != SCHEMA_VERSION
                || version.schema_version != SCHEMA_VERSION
            {
                return Err(invalid());
            }
            let slot = match assertion.status {
                MemoryStatus::Active => &mut active,
                MemoryStatus::Tombstoned => &mut terminal,
                _ => return Err(invalid()),
            };
            if slot.replace(assertion).is_some() {
                return Err(invalid());
            }
        }
        // A restored terminal key intentionally retains no old body. Native
        // captures always retain their original active assertion as well.
        if active.is_none() && !(version.source_snapshot.is_some() && terminal.is_some()) {
            return Err(invalid());
        }
        let assertion = terminal.or(active).ok_or_else(&invalid)?;
        match version.parents.as_slice() {
            [] => roots.push(hash.clone()),
            [parent] => {
                if children.insert(parent.clone(), hash.clone()).is_some() {
                    return Err(invalid());
                }
            }
            _ => return Err(invalid()),
        }
        versions.insert(
            hash.clone(),
            StoredProjectMemory {
                version_hash: hash,
                version,
                assertion,
            },
        );
    }
    if versions.is_empty() {
        return Ok(Vec::new());
    }
    let [root] = roots.as_slice() else {
        return Err(invalid());
    };
    let mut next = Some(root.clone());
    let mut seen = BTreeSet::new();
    let mut history: Vec<StoredProjectMemory> = Vec::with_capacity(versions.len());
    while let Some(hash) = next {
        if !seen.insert(hash.clone()) {
            return Err(invalid());
        }
        let entry = versions.remove(&hash).ok_or_else(&invalid)?;
        if let Some(previous) = history.last()
            && (entry.version.memory_id != previous.version.memory_id
                || entry.version.created_at < previous.version.created_at
                || previous.assertion.status == MemoryStatus::Tombstoned)
        {
            return Err(invalid());
        }
        next = children.remove(&hash);
        history.push(entry);
    }
    if !versions.is_empty() || !children.is_empty() {
        return Err(invalid());
    }
    Ok(history)
}

pub(super) fn memory_full(
    key: &str,
    entry: &StoredProjectMemory,
    revision: u64,
    current_revision: u64,
) -> ProjectMemoryFull {
    ProjectMemoryFull {
        key: key.into(),
        revision,
        current_revision,
        body: entry.version.body.clone(),
        remembered_at: entry.version.created_at,
        actor_id: entry.version.actor.actor_id.clone(),
        actor_context: entry.version.actor.attribution_context().map(str::to_owned),
        session_id: entry.version.actor.session_id.clone(),
    }
}
