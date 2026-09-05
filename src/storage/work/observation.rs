//! Non-holder notes: work-scoped evidence without execution authority.

use std::collections::BTreeMap;

use rusqlite::{Connection, Transaction, params};

use super::super::{SqliteStore, StoreError};
use super::WorkNoteCapture;
use super::feeds::{
    append_to_work_feeds, inspect_work_request, load_typed_work_object, replay_operation,
    request_object,
};
use super::planning::{
    assert_actor_session, assert_revision, normalize_strings, normalize_text,
    persist_operation_result,
};
use super::query::{latest_restored_record, load_work_claim_optional, load_work_item};
use crate::domain::{
    RecordWorkObservationRequest, SCHEMA_VERSION, WorkClaimState, WorkEvent, WorkId, WorkLifecycle,
    WorkObservation, WorkObservationBasis, is_non_holder_note_marker,
};
use crate::{CanonicalObject, ObjectHash, RestoredRecord, memory::Redactor};

pub(super) fn create_schema(connection: &Connection) -> Result<(), StoreError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS work_observations (
             observation_hash TEXT PRIMARY KEY REFERENCES objects(object_hash),
             work_id TEXT NOT NULL REFERENCES work_items(work_id) ON DELETE CASCADE,
             sequence INTEGER NOT NULL CHECK(sequence > 0),
             created_at_ms INTEGER NOT NULL,
             UNIQUE(work_id, sequence)
         ) STRICT;",
    )?;
    Ok(())
}

impl SqliteStore {
    pub(crate) fn record_work_observation<R: Redactor>(
        &mut self,
        request: &RecordWorkObservationRequest,
        redactor: &R,
    ) -> Result<WorkNoteCapture, StoreError> {
        inspect_work_request(redactor, request, &request.actor)?;
        assert_actor_session(&request.actor, &request.session_id)?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        // Same result namespace as holder notes: recovery after a committed
        // append must not choose a different authority path on retry.
        if let Some(capture) = replay_operation(
            &transaction,
            "record_work_note",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(capture);
        }
        let capture = append_work_observation_on(&transaction, request)?;
        persist_operation_result(
            &transaction,
            "record_work_note",
            &request.idempotency_key,
            request_object.hash(),
            &capture,
        )?;
        transaction.commit()?;
        Ok(capture)
    }

    pub(crate) fn work_observation_tail(
        &self,
        work_id: WorkId,
        limit: usize,
    ) -> Result<(usize, Vec<(ObjectHash, WorkObservation)>), StoreError> {
        let total: i64 = self.connection.query_row(
            "SELECT COUNT(*) FROM work_observations WHERE work_id = ?1",
            [work_id.0.to_string()],
            |row| row.get(0),
        )?;
        let observations = observations_on(&self.connection, work_id, limit)?;
        let total = usize::try_from(total)
            .map_err(|_| invalid("observation count exceeds its representable range"))?;
        Ok((total, observations))
    }
}

/// The caller owns atomicity and replay, including creation with initial notes.
fn append_work_observation_on(
    transaction: &Transaction<'_>,
    request: &RecordWorkObservationRequest,
) -> Result<WorkNoteCapture, StoreError> {
    let observation = prepare_work_observation_on(transaction, request)?;
    persist_work_observation_on(transaction, &observation)
}

fn prepare_work_observation_on(
    transaction: &Transaction<'_>,
    request: &RecordWorkObservationRequest,
) -> Result<WorkObservation, StoreError> {
    assert_actor_session(&request.actor, &request.session_id)?;
    let item = load_work_item(transaction, request.work_id)?;
    assert_revision(&item, request.expected_work_revision)?;
    if item.project_id != request.project_id || item.lifecycle != WorkLifecycle::Open {
        return Err(StoreError::InvalidWork(
            "non-holder notes require open work in this project".into(),
        ));
    }
    if let Some(run_id) = item.active_run_id
        && let Some(claim) = load_work_claim_optional(transaction, run_id)?
        && claim.holder == request.session_id
        && claim.state == WorkClaimState::Active
        && claim.expires_at > request.recorded_at
    {
        return Err(StoreError::WorkClaimMismatch { work: item.work_id });
    }
    // load_work_item verified this projected hash against the canonical
    // feed head. Keep its original bytes' identity, never re-freeze it.
    let event_hash: Option<String> = transaction.query_row(
        "SELECT latest_event_hash FROM work_items WHERE work_id = ?1",
        [item.work_id.0.to_string()],
        |row| row.get(0),
    )?;
    let basis = if let Some(hash) = event_hash {
        WorkObservationBasis::NativeEvent {
            event: parse_hash(hash)?,
        }
    } else {
        let (record, _) = latest_restored_record(transaction, item.work_id)?
            .ok_or_else(|| invalid("observation has no canonical planning basis"))?;
        WorkObservationBasis::RestoredRecord { record }
    };
    let head: i64 = transaction.query_row(
        "SELECT COALESCE(MAX(sequence), 0) FROM work_observations WHERE work_id = ?1",
        [item.work_id.0.to_string()],
        |row| row.get(0),
    )?;
    let observation = WorkObservation {
        schema_version: SCHEMA_VERSION,
        project_id: item.project_id,
        root_id: item.root_id,
        work_id: item.work_id,
        work_revision: item.revision,
        basis,
        sequence: head
            .checked_add(1)
            .ok_or_else(|| invalid("observation sequence overflow"))?,
        summary: normalize_text(&request.summary, "note summary")?,
        refs: normalize_strings(&request.refs),
        actor: request.actor.clone(),
        created_at: request.recorded_at,
    };
    validate(transaction, &observation)?;
    Ok(observation)
}

/// Persists a validated observation; callers own the transaction and replay.
fn persist_work_observation_on(
    transaction: &Transaction<'_>,
    observation: &WorkObservation,
) -> Result<WorkNoteCapture, StoreError> {
    let object = CanonicalObject::freeze(observation)?;
    SqliteStore::insert_object(transaction, "work_observation", &object)?;
    insert_projection(transaction, object.hash(), observation)?;
    append_to_work_feeds(
        transaction,
        &observation.project_id,
        observation.root_id,
        None,
        None,
        "work_observation",
        &object,
    )?;
    Ok(WorkNoteCapture {
        non_holder: true,
        evidence: object.hash().clone(),
        checkpoint: None,
    })
}

pub(super) fn append_initial_notes_on<R: Redactor>(
    transaction: &Transaction<'_>,
    item: &crate::domain::WorkItem,
    notes: &[String],
    redactor: &R,
) -> Result<(), StoreError> {
    if notes.is_empty() {
        return Ok(());
    }
    let actor = crate::domain::non_holder_note_actor(item.created_by.clone());
    let session_id = actor.session_id.clone().ok_or_else(|| {
        StoreError::InvalidWork("initial notes require an attributed session".into())
    })?;
    // The caller has normalized and bounded the entire plan before writing.
    // Item, claim, canonical basis and starting sequence cannot change during
    // this batch. Validate that basis once, then validate each note's shape.
    let mut previous: Option<WorkObservation> = None;
    for summary in notes {
        let request = RecordWorkObservationRequest {
            project_id: item.project_id.clone(),
            work_id: item.work_id,
            expected_work_revision: item.revision,
            session_id: session_id.clone(),
            summary: summary.clone(),
            refs: Vec::new(),
            actor: actor.clone(),
            // The outer creation attempt owns replay for the entire ordered list.
            idempotency_key: String::new(),
            recorded_at: item.created_at,
        };
        inspect_work_request(redactor, &request, &actor)?;
        let observation = if let Some(mut observation) = previous {
            observation.sequence = observation
                .sequence
                .checked_add(1)
                .ok_or_else(|| invalid("observation sequence overflow"))?;
            observation.summary.clone_from(summary);
            validate_shape(&observation)?;
            observation
        } else {
            prepare_work_observation_on(transaction, &request)?
        };
        persist_work_observation_on(transaction, &observation)?;
        previous = Some(observation);
    }
    Ok(())
}

pub(in crate::storage) fn observations_on(
    connection: &Connection,
    work_id: WorkId,
    limit: usize,
) -> Result<Vec<(ObjectHash, WorkObservation)>, StoreError> {
    let rows = connection
        .prepare(
            "SELECT observation_hash, sequence, created_at_ms FROM work_observations
         WHERE work_id = ?1 ORDER BY sequence DESC LIMIT ?2",
        )?
        .query_map(
            params![
                work_id.0.to_string(),
                i64::try_from(limit).unwrap_or(i64::MAX)
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let mut observations = Vec::with_capacity(rows.len());
    for (hash, sequence, created_at_ms) in rows.into_iter().rev() {
        let hash = parse_hash(hash)?;
        let observation: WorkObservation =
            load_typed_work_object(connection, &hash, "work_observation")?;
        validate(connection, &observation)?;
        if observation.work_id != work_id
            || observation.sequence != sequence
            || observation.created_at.timestamp_millis() != created_at_ms
        {
            return Err(invalid("work observation differs from its projection"));
        }
        observations.push((hash, observation));
    }
    Ok(observations)
}

pub(super) fn validate(connection: &Connection, value: &WorkObservation) -> Result<(), StoreError> {
    validate_shape(value)?;
    validate_basis(connection, value)
}

fn validate_shape(value: &WorkObservation) -> Result<(), StoreError> {
    let marker_count = value
        .actor
        .provenance_chain
        .iter()
        .filter(|link| is_non_holder_note_marker(link))
        .count();
    if value.schema_version != SCHEMA_VERSION
        || value.sequence <= 0
        || value.work_revision <= 0
        || value.actor.session_id.is_none()
        || marker_count != 1
        || normalize_text(&value.summary, "note summary")? != value.summary
        || normalize_strings(&value.refs) != value.refs
    {
        return Err(invalid("work observation has invalid shape or provenance"));
    }
    Ok(())
}

fn validate_basis(connection: &Connection, value: &WorkObservation) -> Result<(), StoreError> {
    let bound = match &value.basis {
        WorkObservationBasis::NativeEvent { event } => {
            let event: WorkEvent = load_typed_work_object(connection, event, "work_event")?;
            event.schema_version == SCHEMA_VERSION
                && event.work_id == value.work_id
                && event.work.work_id == value.work_id
                && event.root_id == value.root_id
                && event.work.root_id == value.root_id
                && event.project_id == value.project_id
                && event.work.project_id == value.project_id
                && event.revision == value.work_revision
                && event.work.revision == value.work_revision
                && event.work.lifecycle == WorkLifecycle::Open
        }
        WorkObservationBasis::RestoredRecord { record } => {
            let record: RestoredRecord =
                load_typed_work_object(connection, record, "work_restored_record")?;
            record.work_id == value.work_id
                && record.project_id == value.project_id
                && record.item.root_id == value.root_id
                && record.item.lifecycle == WorkLifecycle::Open
                && value.work_revision == 1
        }
    };
    if !bound {
        return Err(invalid(
            "work observation crosses its canonical planning basis",
        ));
    }
    Ok(())
}

fn insert_projection(
    connection: &Connection,
    hash: &ObjectHash,
    value: &WorkObservation,
) -> Result<(), StoreError> {
    connection.execute(
        "INSERT INTO work_observations
        (observation_hash, work_id, sequence, created_at_ms) VALUES (?1, ?2, ?3, ?4)",
        params![
            hash.as_str(),
            value.work_id.0.to_string(),
            value.sequence,
            value.created_at.timestamp_millis()
        ],
    )?;
    Ok(())
}

pub(super) fn rebuild(connection: &Connection) -> Result<(), StoreError> {
    let rows = connection
        .prepare(
            "SELECT object_hash, canonical_json FROM objects
        WHERE object_kind = 'work_observation' ORDER BY object_hash",
        )?
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (hash, bytes) in rows {
        let hash = parse_hash(hash)?;
        let value: WorkObservation = CanonicalObject::verify(&hash, bytes)?.decode()?;
        validate(connection, &value)?;
        insert_projection(connection, &hash, &value)?;
    }
    Ok(())
}

pub(super) fn verify_rows(
    connection: &Connection,
    checked: &mut usize,
    invalid_rows: &mut Vec<String>,
) -> Result<(), StoreError> {
    let rows = connection
        .prepare(
            "SELECT object_hash FROM objects WHERE object_kind = 'work_observation'
        UNION SELECT observation_hash FROM work_observations ORDER BY 1",
        )?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut ordered = BTreeMap::<String, BTreeMap<i64, [i64; 2]>>::new();
    for stored_hash in rows {
        *checked += 1;
        let valid = (|| -> Result<bool, StoreError> {
            let hash = parse_hash(stored_hash.clone())?;
            let value: WorkObservation =
                load_typed_work_object(connection, &hash, "work_observation")?;
            validate(connection, &value)?;
            let positions = observation_feed_positions(connection, &hash, &value)?;
            ordered
                .entry(value.work_id.0.to_string())
                .or_default()
                .insert(value.sequence, positions);
            connection.query_row("SELECT EXISTS(SELECT 1 FROM work_observations
                WHERE observation_hash = ?1 AND work_id = ?2 AND sequence = ?3 AND created_at_ms = ?4)",
                params![hash.as_str(), value.work_id.0.to_string(), value.sequence, value.created_at.timestamp_millis()],
                |row| row.get(0)).map_err(StoreError::from)
        })();
        if !matches!(valid, Ok(true)) {
            invalid_rows.push(format!("work_observation:{stored_hash}"));
        }
    }
    for (work_id, positions) in ordered {
        let positions = positions.into_values().collect::<Vec<_>>();
        if positions
            .windows(2)
            .any(|pair| pair[0][0] >= pair[1][0] || pair[0][1] >= pair[1][1])
        {
            invalid_rows.push(format!("work_observation:{work_id}:feed_order"));
        }
    }
    let gaps = connection
        .prepare(
            "SELECT work_id FROM work_observations GROUP BY work_id
        HAVING MIN(sequence) != 1 OR MAX(sequence) != COUNT(*)",
        )?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for work_id in gaps {
        invalid_rows.push(format!("work_observation:{work_id}:sequence"));
    }
    Ok(())
}

fn observation_feed_positions(
    connection: &Connection,
    hash: &ObjectHash,
    value: &WorkObservation,
) -> Result<[i64; 2], StoreError> {
    let mut positions = [0; 2];
    for (index, (kind, id)) in [
        ("project", value.project_id.0.clone()),
        ("root_work", value.root_id.0.to_string()),
    ]
    .into_iter()
    .enumerate()
    {
        let position = |hash: &ObjectHash| -> Result<i64, StoreError> {
            connection
                .query_row(
                    "SELECT position FROM work_feed_entries
                 WHERE feed_kind = ?1 AND feed_id = ?2 AND object_hash = ?3",
                    params![kind, id, hash.as_str()],
                    |row| row.get(0),
                )
                .map_err(StoreError::from)
        };
        positions[index] = position(hash)?;
        if let WorkObservationBasis::NativeEvent { event } = &value.basis
            && position(event)? >= positions[index]
        {
            return Err(invalid("observation must follow its native planning basis"));
        }
    }
    Ok(positions)
}

fn parse_hash(value: String) -> Result<ObjectHash, StoreError> {
    ObjectHash::from_stored(value.clone()).ok_or(StoreError::InvalidStoredHash(value))
}

fn invalid(message: &str) -> StoreError {
    StoreError::InvalidWorkProjection(message.into())
}
