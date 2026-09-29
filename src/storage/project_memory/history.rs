use std::collections::{BTreeMap, BTreeSet};

use super::{
    Connection, MemoryAssertionEvent, MemoryStatus, MemoryVersion, ObjectId, ProjectMemoryFull,
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
            "SELECT object_id FROM objects INDEXED BY objects_project_memory_key
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
        let hash = ObjectId::from_stored(stored_hash.clone())
            .ok_or(StoreError::InvalidStoredKey(stored_hash))?;
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
                "SELECT object_id FROM objects INDEXED BY objects_memory_assertion_version
             WHERE object_kind = 'memory_assertion_event'
               AND json_extract(canonical_json, '$.version') = ?1",
            )?
            .query_map([hash.as_str()], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let mut active = None;
        let mut terminal = None;
        for stored_assertion in assertions {
            let assertion_id = ObjectId::from_stored(stored_assertion.clone())
                .ok_or(StoreError::InvalidStoredKey(stored_assertion))?;
            let assertion: MemoryAssertionEvent = SqliteStore::get_typed_object_on(
                connection,
                &assertion_id,
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
                version_id: hash,
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
                || previous.assertion.status == MemoryStatus::Tombstoned
                || (entry.version.retiring_target_cleared
                    && !has_clearable_retiring_target(&history)))
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

/// The full-read envelope of `history[index]`, whose revision is `index + 1`.
/// The item state of a local target is read separately, when the memory is.
///
/// # Panics
///
/// Never in practice: callers pass an index inside `history`.
pub(super) fn memory_full(
    key: &str,
    history: &[StoredProjectMemory],
    index: usize,
    current_revision: u64,
) -> ProjectMemoryFull {
    let entry = &history[index];
    ProjectMemoryFull {
        key: key.into(),
        revision: index as u64 + 1,
        current_revision,
        body: entry.version.body.clone(),
        remembered_at: entry.version.created_at,
        actor_id: entry.version.actor.actor_id.clone(),
        actor_context: entry.version.actor.attribution_context().map(str::to_owned),
        session_id: entry.version.actor.session_id.clone(),
        retiring_target: entry.version.retiring_target.clone(),
        retiring_state: None,
        retiring_target_dropped: retiring_target_dropped(history, index),
        workaround: entry.version.retiring_target.as_ref().map(|_| true),
    }
}

/// The target `history[index]` lost without an explicit clear: when that
/// version has neither a target nor a clear, the nearest earlier version that
/// has either decides. A target there was dropped by the revision after it;
/// a clear there, or no such version, means nothing was dropped.
pub(super) fn retiring_target_dropped(
    history: &[StoredProjectMemory],
    index: usize,
) -> Option<crate::domain::ProjectMemoryRetiringTargetDropped> {
    let version = &history.get(index)?.version;
    if version.retiring_target.is_some() || version.retiring_target_cleared {
        return None;
    }
    retiring_target_dropped_before(&history[..index])
}

/// What a new version with neither a target nor a clear would drop, given the
/// versions before it.
pub(super) fn retiring_target_dropped_before(
    prior: &[StoredProjectMemory],
) -> Option<crate::domain::ProjectMemoryRetiringTargetDropped> {
    let (position, entry) = prior.iter().enumerate().rev().find(|(_, entry)| {
        entry.version.retiring_target.is_some() || entry.version.retiring_target_cleared
    })?;
    let target = entry.version.retiring_target.clone()?;
    Some(crate::domain::ProjectMemoryRetiringTargetDropped {
        revision: position as u64 + 2,
        target,
    })
}

/// Whether a clear written after `prior` has a target to remove: the
/// newest version's own target, or one that later revisions dropped without
/// a clear. The nearest earlier version with a target or a clear decides.
pub(super) fn has_clearable_retiring_target(prior: &[StoredProjectMemory]) -> bool {
    retiring_target_dropped_before(prior).is_some()
}
