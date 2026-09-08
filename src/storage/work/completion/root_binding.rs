//! Exact pre-seal root addresses and their canonical completion-event binding.

use super::{
    CompletionSeal, Connection, ObjectHash, RootExecution, SqliteStore, StoreError, WorkEvent,
    WorkRunState, WorkTransition, load_typed_work_object, params,
};

fn invalid() -> StoreError {
    StoreError::InvalidWorkProjection(
        "completion seal differs from its canonical event or exact pre-seal root binding".into(),
    )
}

pub(in crate::storage::work) fn validate_seal_root_event(
    connection: &Connection,
    seal: &CompletionSeal,
    seal_hash: &ObjectHash,
    event: &WorkEvent,
) -> Result<(), StoreError> {
    let after = event.root_execution.as_ref().ok_or_else(invalid)?;
    if !matches!(&event.transition, WorkTransition::Completed { seal } if seal == seal_hash)
        || event.work_id != seal.work_id
        || event.root_id != seal.root_id
        || event.project_id != seal.root_execution.project_id
        || event.run_id != Some(seal.run_id)
        || event.actor != seal.actor
        || event.created_at != seal.completed_at
        || seal.accepted_work_revision.checked_add(1) != Some(event.revision)
        || seal.root_execution.root_execution_id != seal.root_execution_id
        || seal.root_execution.root_id != seal.root_id
        || !event.run.as_ref().is_some_and(|run| {
            run.run_id == seal.run_id
                && run.root_execution_id == seal.root_execution_id
                && run.generation == seal.run_generation
                && run.state == WorkRunState::Completed
                && run.completion_seal.as_ref() == Some(seal_hash)
        })
    {
        return Err(invalid());
    }
    super::super::root_state::verify_completion_predecessor(connection, &seal.root_execution, after)
}

/// A canonical seal alone does not prove that its address was the state at
/// completion. Select its exact completion event, never the item's latest
/// event or a later current root projection. Decode only the selected event.
pub(in crate::storage::work) fn validate_stored_seal_root(
    connection: &Connection,
    seal: &CompletionSeal,
    seal_hash: &ObjectHash,
) -> Result<(), StoreError> {
    let mut statement = connection.prepare(
        "SELECT object.object_hash
         FROM objects object INDEXED BY objects_work_event_work_id
         JOIN work_feed_entries entry ON entry.object_hash = object.object_hash
         WHERE object.object_kind = 'work_event'
           AND json_extract(object.canonical_json, '$.work_id') = ?1
           AND json_extract(object.canonical_json, '$.transition.kind') = 'completed'
           AND json_extract(object.canonical_json, '$.transition.seal') = ?2
           AND entry.feed_kind = 'root_work' AND entry.feed_id = ?3
           AND entry.object_kind = 'work_event' AND entry.work_id = ?1
         LIMIT 2",
    )?;
    let hashes = statement
        .query_map(
            params![
                seal.work_id.0.to_string(),
                seal_hash.as_str(),
                seal.root_id.0.to_string()
            ],
            |row| row.get::<_, String>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let [stored] = hashes.as_slice() else {
        return Err(invalid());
    };
    let event_hash = ObjectHash::from_stored(stored.clone())
        .ok_or_else(|| StoreError::InvalidStoredHash(stored.clone()))?;
    let event = load_typed_work_object(connection, &event_hash, "work_event")?;
    validate_seal_root_event(connection, seal, seal_hash, &event)
}

impl SqliteStore {
    /// Reads the full root accounting frozen by a native completion seal.
    /// This historical read checks the exact completion binding and replays
    /// every state checksum. It never substitutes the later current root.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] if the seal, completion event or root history is
    /// missing, corrupt or bound to a different state.
    pub fn completion_root_execution(
        &self,
        seal_hash: &ObjectHash,
    ) -> Result<RootExecution, StoreError> {
        self.work_read_snapshot(|store| {
            let connection = &store.connection;
            let seal: CompletionSeal =
                load_typed_work_object(connection, seal_hash, "completion_seal")?;
            validate_stored_seal_root(connection, &seal, seal_hash)?;
            super::super::root_state::resolve(connection, &seal.root_execution)
        })
    }
}
