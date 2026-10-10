#[cfg(test)]
use super::planning::{assert_actor_session, renew_holder_claim, validate_live_claim_on};
#[cfg(test)]
use crate::domain::{MemoryAssertionEvent, MemoryVersion, SessionId};

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Serialize, de::DeserializeOwned};

use super::super::{SqliteStore, StoreError};
use super::execution::latest_canonical_handoff_offer;
use super::integrity::expected_environment_projection;
use super::planning::{
    apply_work_relation_transition, projected_work_relation_basis,
    validated_current_work_relation_basis, work_relation_fingerprint,
};
use super::query::{
    feed_parts, latest_canonical_work_event_for_item_optional, load_root_execution,
    load_work_claim_optional, load_work_item, load_work_run, parse_work_id, verified_work_identity,
};
use super::{CHECKPOINT_APPEND_COUNT, MAX_WORK_SOURCE_SNAPSHOT_BYTES, WorkEventDraft};
use crate::{
    CanonicalObject, ObjectId,
    domain::{
        ActorContext, EnvironmentEvidence, ExecutionObservation, FeedId, FeedPosition,
        NamedRootBindingEvent, SCHEMA_VERSION, SourceObservation, WorkClaimId, WorkHandoffOffer,
        WorkHandoffState, WorkId, WorkRunId, WorkSourceSnapshot, WorkTransition,
    },
    memory::Redactor,
};

#[cfg(test)]
mod tests;

impl SqliteStore {
    /// The source record stored as `record`, read as accounting reads it.
    ///
    /// # Errors
    ///
    /// [`StoreError::InvalidWorkProjection`] when it is missing or is no
    /// source record; other [`StoreError`] values when it cannot be read.
    pub(crate) fn source_observation(
        &self,
        record: &ObjectId,
    ) -> Result<SourceObservation, StoreError> {
        load_source_observation_on(&self.connection, record)
    }

    /// Orders note families in their shared dense root feed, never by the
    /// caller-asserted observation timestamp.
    pub(crate) fn work_root_object_position(
        &self,
        root_id: WorkId,
        hash: &ObjectId,
    ) -> Result<i64, StoreError> {
        self.connection
            .query_row(
                "SELECT position FROM work_feed_entries
             WHERE feed_kind = 'root_work' AND feed_id = ?1 AND object_id = ?2",
                params![root_id.0.to_string(), hash.as_str()],
                |row| row.get(0),
            )
            .map_err(StoreError::from)
    }
}

pub(super) fn reserve_feed_position(
    transaction: &Transaction<'_>,
    feed: &FeedId,
) -> Result<FeedPosition, StoreError> {
    let (feed_kind, feed_id) = feed_parts(feed);
    let current = transaction
        .query_row(
            "SELECT position FROM work_feed_heads
             WHERE feed_kind = ?1 AND feed_id = ?2",
            params![feed_kind, feed_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    let position = if let Some(current) = current {
        let next = current.checked_add(1).ok_or_else(|| {
            StoreError::InvalidWorkProjection(format!("work feed {feed:?} position overflowed"))
        })?;
        let changed = transaction.execute(
            "UPDATE work_feed_heads SET position = ?3
             WHERE feed_kind = ?1 AND feed_id = ?2 AND position = ?4",
            params![feed_kind, feed_id, next, current],
        )?;
        if changed != 1 {
            return Err(StoreError::InvalidWorkProjection(format!(
                "work feed {feed:?} head changed during allocation"
            )));
        }
        next
    } else {
        transaction.execute(
            "INSERT INTO work_feed_heads (feed_kind, feed_id, position)
             VALUES (?1, ?2, 1)",
            params![feed_kind, feed_id],
        )?;
        1
    };
    Ok(FeedPosition {
        feed: feed.clone(),
        position,
    })
}

pub(super) fn checkpoint_feed_end(position: i64) -> Result<i64, StoreError> {
    position
        .checked_add(CHECKPOINT_APPEND_COUNT)
        .ok_or_else(|| {
            StoreError::InvalidWorkProjection(
                "checkpoint run-feed position arithmetic overflowed".into(),
            )
        })
}

/// The run-feed position `checkpoint`'s own appends end at: the head
/// completion requires when it begins, before completion appends anything.
pub(crate) fn checkpoint_run_feed_end(
    checkpoint: &crate::domain::WorkCheckpoint,
) -> Result<FeedPosition, StoreError> {
    Ok(FeedPosition {
        feed: checkpoint.acknowledged_run_position.feed.clone(),
        position: checkpoint_feed_end(checkpoint.acknowledged_run_position.position)?,
    })
}

fn insert_reserved_feed_entry(
    transaction: &Transaction<'_>,
    position: &FeedPosition,
    object_kind: &str,
    object: &CanonicalObject,
    work_id: Option<WorkId>,
) -> Result<(), StoreError> {
    let (feed_kind, feed_id) = feed_parts(&position.feed);
    transaction.execute(
        "INSERT INTO work_feed_entries (
             feed_kind, feed_id, position, object_kind, object_id, work_id
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            feed_kind,
            feed_id,
            position.position,
            object_kind,
            object.key().as_str(),
            work_id.map(|work_id| work_id.0.to_string())
        ],
    )?;
    Ok(())
}

pub(super) fn append_to_work_feeds(
    transaction: &Transaction<'_>,
    project_id: &crate::domain::ProjectId,
    root_id: WorkId,
    run_id: Option<WorkRunId>,
    work_id: Option<WorkId>,
    object_kind: &str,
    object: &CanonicalObject,
) -> Result<Vec<FeedPosition>, StoreError> {
    let mut feeds = vec![
        FeedId::Project(project_id.clone()),
        FeedId::RootWork(root_id),
    ];
    if let Some(run_id) = run_id {
        feeds.push(FeedId::RunExecution(run_id));
    }
    feeds
        .into_iter()
        .map(|feed| {
            let position = reserve_feed_position(transaction, &feed)?;
            insert_reserved_feed_entry(transaction, &position, object_kind, object, work_id)?;
            Ok(position)
        })
        .collect()
}

#[cfg(test)]
#[allow(
    clippy::too_many_arguments,
    reason = "the exact work, holder, time, actor, typed memory, and canonical objects form one audited capture binding"
)]
pub(in crate::storage) fn append_fixture_memory_to_work_feeds(
    transaction: &Transaction<'_>,
    work_id: WorkId,
    holder: &SessionId,
    captured_at: DateTime<Utc>,
    actor: &crate::domain::ActorContext,
    version: &MemoryVersion,
    assertion: &MemoryAssertionEvent,
    version_object: &CanonicalObject,
    assertion_object: &CanonicalObject,
) -> Result<Vec<FeedPosition>, StoreError> {
    let crate::domain::Scope::Work { project, work } = &version.scope else {
        return Err(StoreError::InvalidMemoryProjection(
            "shared work capture must carry work scope".into(),
        ));
    };
    if *work != work_id
        || assertion.memory_id != version.memory_id
        || assertion.version != *version_object.key()
        || version.actor != *actor
        || assertion.actor != *actor
        || version.created_at != captured_at
        || assertion.created_at != captured_at
    {
        return Err(StoreError::InvalidMemoryProjection(
            "shared work capture is not bound to its note, actor, and timestamp".into(),
        ));
    }
    assert_actor_session(actor, holder)?;
    let projected_item = load_work_item(transaction, work_id)?;
    if project != &projected_item.project_id {
        return Err(StoreError::InvalidMemoryProjection(
            "shared work capture project differs from the focused work".into(),
        ));
    }
    let run_id = projected_item
        .active_run_id
        .ok_or(StoreError::WorkClaimMismatch { work: work_id })?;
    let projected_claim = load_work_claim_optional(transaction, run_id)?
        .ok_or(StoreError::WorkClaimMismatch { work: work_id })?;
    let (item, run, mut claim) = validate_live_claim_on(
        transaction,
        work_id,
        run_id,
        projected_item.revision,
        holder,
        projected_claim.claim_id,
        projected_claim.fence,
        captured_at,
        false,
    )?;
    renew_holder_claim(transaction, &mut claim, captured_at)?;
    let root_execution = load_root_execution(transaction, run.root_execution_id)?;
    let mut positions = append_to_work_feeds(
        transaction,
        &item.project_id,
        item.root_id,
        item.active_run_id,
        None,
        "memory_version",
        version_object,
    )?;
    positions.extend(append_to_work_feeds(
        transaction,
        &item.project_id,
        item.root_id,
        item.active_run_id,
        None,
        "memory_assertion_event",
        assertion_object,
    )?);
    let event = WorkEventDraft {
        schema_version: SCHEMA_VERSION,
        project_id: item.project_id.clone(),
        root_id: item.root_id,
        work_id: item.work_id,
        run_id: Some(run.run_id),
        revision: item.revision,
        work: item,
        run: Some(run),
        root_execution: Some(root_execution),
        claim: Some(claim),
        handoff_offer: None,
        blocker: None,
        transition: WorkTransition::MemoryCaptured {
            version: version_object.key().clone(),
            assertion: assertion_object.key().clone(),
        },
        actor: actor.clone(),
        created_at: captured_at,
    };
    let (_, event_positions) = append_work_event(transaction, &event)?;
    positions.extend(event_positions);
    Ok(positions)
}

pub(in crate::storage) fn load_control_execution_observation_on(
    connection: &Connection,
    hash: &ObjectId,
) -> Result<Option<ExecutionObservation>, StoreError> {
    let stored = connection
        .query_row(
            "SELECT object_kind, canonical_json FROM objects WHERE object_id = ?1",
            [hash.as_str()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()?;
    let Some((kind, bytes)) = stored else {
        return Ok(None);
    };
    if kind != "execution_observation" {
        return Ok(None);
    }
    Ok(Some(CanonicalObject::stored(hash, bytes)?.decode()?))
}

pub(in crate::storage) fn load_control_environment_evidence_on(
    connection: &Connection,
    hash: &ObjectId,
) -> Result<Option<EnvironmentEvidence>, StoreError> {
    let stored = connection
        .query_row(
            "SELECT object_kind, canonical_json FROM objects WHERE object_id = ?1",
            [hash.as_str()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()?;
    let Some((kind, bytes)) = stored else {
        return Ok(None);
    };
    if kind != "environment_evidence" {
        return Ok(None);
    }
    let evidence = CanonicalObject::stored(hash, bytes)?.decode()?;
    expected_environment_projection(connection, hash)?;
    Ok(Some(evidence))
}

pub(super) fn verify_anchored_memory_feeds(
    connection: &Connection,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let feed_has = |object_id: &ObjectId, feed_kind: &str, feed_id: &str| {
        connection
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM work_feed_entries
                     WHERE feed_kind = ?1 AND feed_id = ?2 AND object_id = ?3
                 )",
                params![feed_kind, feed_id, object_id.as_str()],
                |row| row.get::<_, bool>(0),
            )
            .map_err(StoreError::from)
    };
    let mut statement = connection.prepare(
        "SELECT object_id, object_kind, canonical_json FROM objects
         WHERE object_kind = 'memory_version'
         ORDER BY object_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Vec<u8>>(2)?,
        ))
    })?;
    for row in rows {
        let (stored_hash, object_kind, bytes) = row?;
        let label = format!("{object_kind}:{stored_hash}:work-feed");
        let Some(hash) = ObjectId::from_stored(stored_hash) else {
            invalid.push(label);
            continue;
        };
        let Ok(object) = CanonicalObject::stored(&hash, bytes) else {
            invalid.push(label);
            continue;
        };
        // Objects retained under another schema are never activated or fed;
        // only the current schema carries feed expectations.
        let current_schema = serde_json::from_slice::<serde_json::Value>(object.bytes())
            .ok()
            .and_then(|value| {
                value
                    .get("schema_version")
                    .and_then(serde_json::Value::as_u64)
            })
            == Some(u64::from(SCHEMA_VERSION));
        if !current_schema {
            continue;
        }
        let anchor = if let Ok(crate::domain::MemoryVersion {
            scope: crate::domain::Scope::Work { project, work },
            ..
        }) = object.decode::<crate::domain::MemoryVersion>()
        {
            let Ok((_, root)) = verified_work_identity(connection, work) else {
                invalid.push(label);
                continue;
            };
            Some((project, root))
        } else {
            None
        };
        let Some((project_id, root_id)) = anchor else {
            continue;
        };
        *checked += 1;
        if !feed_has(&hash, "project", &project_id.0)?
            || !feed_has(&hash, "root_work", &root_id.0.to_string())?
        {
            invalid.push(label);
        }
    }
    Ok(())
}

pub(super) fn run_feed_position_for_object_on(
    connection: &Connection,
    run_id: WorkRunId,
    object_id: &ObjectId,
) -> Result<FeedPosition, StoreError> {
    let position = connection
        .query_row(
            "SELECT position FROM work_feed_entries
             WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND object_id = ?2",
            params![run_id.0.to_string(), object_id.as_str()],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .ok_or_else(|| {
            StoreError::InvalidWorkProjection(format!(
                "object {object_id} is missing from run {run_id:?} feed"
            ))
        })?;
    Ok(FeedPosition {
        feed: FeedId::RunExecution(run_id),
        position,
    })
}

pub(in crate::storage) fn current_run_feed_cut_on(
    connection: &Connection,
    run_id: WorkRunId,
) -> Result<FeedPosition, StoreError> {
    let position = connection.query_row(
        "SELECT position FROM work_feed_heads
         WHERE feed_kind = 'run_execution' AND feed_id = ?1",
        [run_id.0.to_string()],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(FeedPosition {
        feed: FeedId::RunExecution(run_id),
        position,
    })
}

/// Appends a host-authorized named-root selection to its bound run feed.
pub(in crate::storage) fn append_named_root_binding_on(
    transaction: &Transaction<'_>,
    event: &NamedRootBindingEvent,
) -> Result<(ObjectId, FeedPosition), StoreError> {
    let item = load_work_item(transaction, event.work_id)?;
    let run = load_work_run(transaction, event.run_id)?;
    let root = load_root_execution(transaction, event.root_execution_id)?;
    if item.project_id != event.project_id
        || item.root_id != root.root_id
        || run.work_id != item.work_id
        || run.root_execution_id != root.root_execution_id
    {
        return Err(StoreError::InvalidWorkProjection(
            "named-root binding crosses its canonical work run".into(),
        ));
    }
    let object = CanonicalObject::mint(event)?;
    SqliteStore::insert_object(transaction, "named_root_binding", &object)?;
    let position = append_to_work_feeds(
        transaction,
        &event.project_id,
        item.root_id,
        Some(run.run_id),
        None,
        "named_root_binding",
        &object,
    )?
    .into_iter()
    .find(|position| position.feed == FeedId::RunExecution(run.run_id))
    .ok_or_else(|| {
        StoreError::InvalidWorkProjection(
            "named-root binding did not receive a run-feed position".into(),
        )
    })?;
    Ok((object.key().clone(), position))
}

pub(super) fn latest_named_root_binding_on(
    connection: &Connection,
    run_id: WorkRunId,
    claim_id: WorkClaimId,
    through: i64,
) -> Result<Option<(FeedPosition, ObjectId, NamedRootBindingEvent)>, StoreError> {
    // A release ends the claim, and with it every binding recorded before
    // it, even though a later claim of the run reuses the claim id.
    let released = latest_claim_release_on(connection, run_id, claim_id, through)?;
    connection
        .query_row(
            "SELECT entry.position, entry.object_id, object.canonical_json
             FROM work_feed_entries entry
             JOIN objects object ON object.object_id = entry.object_id
             WHERE entry.feed_kind = 'run_execution' AND entry.feed_id = ?1
               AND entry.position <= ?2 AND entry.position > ?4
               AND entry.object_kind = 'named_root_binding'
               AND json_extract(object.canonical_json, '$.claim_id') = ?3
             ORDER BY entry.position DESC LIMIT 1",
            params![
                run_id.0.to_string(),
                through,
                claim_id.0.to_string(),
                released.unwrap_or(0)
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            },
        )
        .optional()?
        .map(|(position, stored_id, bytes)| {
            let id = ObjectId::from_stored(stored_id.clone())
                .ok_or(StoreError::InvalidStoredKey(stored_id))?;
            let event = CanonicalObject::stored(&id, bytes)?.decode()?;
            Ok((
                FeedPosition {
                    feed: FeedId::RunExecution(run_id),
                    position,
                },
                id,
                event,
            ))
        })
        .transpose()
}

/// The run-feed position of the newest release of `claim_id` on the run at
/// or before `through`: the latest point at which that claim ended while the
/// run went on.
pub(super) fn latest_claim_release_on(
    connection: &Connection,
    run_id: WorkRunId,
    claim_id: WorkClaimId,
    through: i64,
) -> Result<Option<i64>, StoreError> {
    Ok(connection.query_row(
        "SELECT MAX(entry.position)
         FROM work_feed_entries entry
         JOIN objects object ON object.object_id = entry.object_id
         WHERE entry.feed_kind = 'run_execution' AND entry.feed_id = ?1
           AND entry.position <= ?2 AND entry.object_kind = 'work_event'
           AND json_extract(object.canonical_json, '$.transition.kind') = 'released'
           AND json_extract(object.canonical_json, '$.transition.claim_id') = ?3",
        params![run_id.0.to_string(), through, claim_id.0.to_string()],
        |row| row.get(0),
    )?)
}

/// The claim's newest `named_root_binding` event at or before `through`, bound
/// or ended, whether or not a release came after it, with its run-feed
/// position.
pub(in crate::storage) fn latest_named_root_event_on(
    connection: &Connection,
    run_id: WorkRunId,
    claim_id: WorkClaimId,
    through: i64,
) -> Result<Option<(i64, NamedRootBindingEvent)>, StoreError> {
    Ok(
        latest_named_root_event_record_on(connection, run_id, claim_id, through)?
            .map(|(position, _, event)| (position, event)),
    )
}

/// The same newest event with the real record id it is stored under.
pub(in crate::storage) fn latest_named_root_event_record_on(
    connection: &Connection,
    run_id: WorkRunId,
    claim_id: WorkClaimId,
    through: i64,
) -> Result<Option<(i64, ObjectId, NamedRootBindingEvent)>, StoreError> {
    connection
        .query_row(
            "SELECT entry.position, entry.object_id, object.canonical_json
             FROM work_feed_entries entry
             JOIN objects object ON object.object_id = entry.object_id
             WHERE entry.feed_kind = 'run_execution' AND entry.feed_id = ?1
               AND entry.position <= ?2
               AND entry.object_kind = 'named_root_binding'
               AND json_extract(object.canonical_json, '$.claim_id') = ?3
             ORDER BY entry.position DESC LIMIT 1",
            params![run_id.0.to_string(), through, claim_id.0.to_string()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            },
        )
        .optional()?
        .map(|(position, stored_id, bytes)| {
            let id = ObjectId::from_stored(stored_id.clone())
                .ok_or(StoreError::InvalidStoredKey(stored_id))?;
            let event = CanonicalObject::stored(&id, bytes)?.decode()?;
            Ok((position, id, event))
        })
        .transpose()
}

/// The run-feed position of the run's newest terminal transition, its
/// completion or disposal, at or before `through`. The run's claim ends there.
fn latest_run_end_on(
    connection: &Connection,
    run_id: WorkRunId,
    through: i64,
) -> Result<Option<i64>, StoreError> {
    Ok(connection.query_row(
        "SELECT MAX(entry.position)
         FROM work_feed_entries entry
         JOIN objects object ON object.object_id = entry.object_id
         WHERE entry.feed_kind = 'run_execution' AND entry.feed_id = ?1
           AND entry.position <= ?2 AND entry.object_kind = 'work_event'
           AND json_extract(object.canonical_json, '$.transition.kind')
               IN ('completed', 'disposed')",
        params![run_id.0.to_string(), through],
        |row| row.get(0),
    )?)
}

/// The claim's named-root state at `through`, derived from its recorded
/// `named_root_bind` events and its own lifecycle, never from path text or
/// fence changes. An ended root is `none`, even when a release follows it, and
/// so is a run that completed or was disposed after the name. Otherwise, of
/// the newest name and the claim's newest release, the later one decides: a
/// release after the name leaves the claim unbound until a later name. A
/// handoff or a recovery records no release, so the binding stays.
pub(in crate::storage) fn named_root_state_on(
    connection: &Connection,
    run_id: WorkRunId,
    claim_id: WorkClaimId,
    through: i64,
) -> Result<crate::domain::NamedRootState, StoreError> {
    use crate::domain::NamedRootState;
    let Some((position, event)) =
        latest_named_root_event_on(connection, run_id, claim_id, through)?
    else {
        return Ok(NamedRootState::NoRoot);
    };
    if event.kind == crate::domain::NamedRootBindingKind::Ended
        || latest_run_end_on(connection, run_id, through)?.is_some_and(|end| end > position)
    {
        return Ok(NamedRootState::NoRoot);
    }
    Ok(
        match latest_claim_release_on(connection, run_id, claim_id, through)?
            .filter(|released| *released > position)
        {
            Some(released_at_position) => NamedRootState::UnboundByRelease {
                last_generation: event.generation,
                released_at_position,
            },
            None => NamedRootState::Bound {
                workspace_id: event.workspace_id,
                generation: event.generation,
                named_at: event.named_at,
            },
        },
    )
}

/// The run-feed position and workspace of the claim's first recorded `kind`
/// event for `generation` at or before `through`, if there is one.
pub(super) fn named_root_event_on(
    connection: &Connection,
    run_id: WorkRunId,
    claim_id: WorkClaimId,
    generation: i64,
    kind: &str,
    through: i64,
) -> Result<Option<(i64, String)>, StoreError> {
    Ok(connection
        .query_row(
            "SELECT entry.position, json_extract(object.canonical_json, '$.workspace_id')
             FROM work_feed_entries entry
             JOIN objects object ON object.object_id = entry.object_id
             WHERE entry.feed_kind = 'run_execution' AND entry.feed_id = ?1
               AND entry.position <= ?2 AND entry.object_kind = 'named_root_binding'
               AND json_extract(object.canonical_json, '$.claim_id') = ?3
               AND json_extract(object.canonical_json, '$.generation') = ?4
               AND json_extract(object.canonical_json, '$.kind') = ?5
             ORDER BY entry.position LIMIT 1",
            params![
                run_id.0.to_string(),
                through,
                claim_id.0.to_string(),
                generation,
                kind
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?)
}

/// SQL over `entry` and `object` that is true for a source record of either
/// kind: an admitted turn's observation, or an unadmitted record accounted as a
/// new change or a repeat. An audit-only record, or one that reported no
/// change, is never a source record.
pub(in crate::storage) const SOURCE_RECORD_SQL: &str = "(entry.object_kind = 'execution_observation'
     OR (entry.object_kind = 'unadmitted_execution_observation'
         AND json_extract(object.canonical_json, '$.accounting.kind') IN ('source_change', 'repeat')))";

/// SQL that is true for a source record read as a change.
pub(in crate::storage) const SOURCE_CHANGED_SQL: &str = "(CASE entry.object_kind
     WHEN 'execution_observation' THEN json_extract(object.canonical_json, '$.source_changed') = 1
     ELSE json_extract(object.canonical_json, '$.accounting.kind') = 'source_change' END)";

/// SQL for one field of a host record's source basis: an unadmitted record's
/// closing sighting, or the `source_basis` an admitted observation or
/// environment evidence carries; NULL when the record carries none.
pub(in crate::storage) fn source_basis_sql(field: &str) -> String {
    format!(
        "(CASE entry.object_kind
             WHEN 'unadmitted_execution_observation'
                 THEN json_extract(object.canonical_json,
                     '$.occurrence.source_change.sighting.source_basis.{field}')
             ELSE json_extract(object.canonical_json, '$.source_basis.{field}') END)"
    )
}

/// The source record stored as `record`, read as accounting reads it.
///
/// # Errors
///
/// [`StoreError::InvalidWorkProjection`] when the record is neither an
/// admitted observation nor an accounted unadmitted one; other
/// [`StoreError`] values when it cannot be read.
pub(in crate::storage) fn load_source_observation_on(
    connection: &Connection,
    record: &ObjectId,
) -> Result<SourceObservation, StoreError> {
    source_observation_if_accounted_on(connection, record)?.ok_or_else(|| {
        StoreError::InvalidWorkProjection(format!(
            "unadmitted record {record} was not accounted as a source record"
        ))
    })
}

/// The record stored as `record` read as accounting reads it, or `None` for
/// an unadmitted record accounting did not read as a source record.
///
/// # Errors
///
/// [`StoreError::InvalidWorkProjection`] when the record is missing or of
/// neither kind; other [`StoreError`] values when it cannot be read.
pub(in crate::storage) fn source_observation_if_accounted_on(
    connection: &Connection,
    record: &ObjectId,
) -> Result<Option<SourceObservation>, StoreError> {
    let (kind, bytes): (String, Vec<u8>) = connection
        .query_row(
            "SELECT object_kind, canonical_json FROM objects WHERE object_id = ?1",
            [record.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?
        .ok_or_else(|| {
            StoreError::InvalidWorkProjection(format!("source record {record} is missing"))
        })?;
    let object = CanonicalObject::stored(record, bytes)?;
    match kind.as_str() {
        "execution_observation" => Ok(Some(SourceObservation::admitted(
            record.clone(),
            &object.decode::<ExecutionObservation>()?,
        ))),
        super::UNADMITTED_OBSERVATION_KIND => Ok(SourceObservation::unadmitted(
            record.clone(),
            &object.decode::<crate::domain::UnadmittedExecutionObservation>()?,
        )),
        other => Err(StoreError::InvalidWorkProjection(format!(
            "record {record} of kind {other} is not a source record"
        ))),
    }
}

fn source_observation_row(
    connection: &Connection,
    (position, stored): (i64, String),
) -> Result<(i64, SourceObservation), StoreError> {
    let record =
        ObjectId::from_stored(stored.clone()).ok_or(StoreError::InvalidStoredKey(stored))?;
    Ok((position, load_source_observation_on(connection, &record)?))
}

pub(super) fn latest_source_mutation_on(
    connection: &Connection,
    run_id: WorkRunId,
    through: i64,
) -> Result<Option<(i64, SourceObservation)>, StoreError> {
    latest_source_mutation_in_on(connection, run_id, None, through)
}

/// The newest source change on the run at or before `through`, only among
/// changes the host recorded in `workspace` when one is given.
pub(super) fn latest_source_mutation_in_on(
    connection: &Connection,
    run_id: WorkRunId,
    workspace: Option<&str>,
    through: i64,
) -> Result<Option<(i64, SourceObservation)>, StoreError> {
    let workspace_sql = source_basis_sql("workspace_id");
    let row = connection
        .query_row(
            &format!(
                "SELECT entry.position, entry.object_id
                 FROM work_feed_entries entry
                 JOIN objects object ON object.object_id = entry.object_id
                 WHERE entry.feed_kind = 'run_execution' AND entry.feed_id = ?1
                   AND entry.position <= ?2
                   AND {SOURCE_RECORD_SQL} AND {SOURCE_CHANGED_SQL}
                   AND (?3 IS NULL OR {workspace_sql} = ?3)
                 ORDER BY entry.position DESC LIMIT 1"
            ),
            params![run_id.0.to_string(), through, workspace],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    row.map(|row| source_observation_row(connection, row))
        .transpose()
}

/// Last capture in the active named root at the bound generation. A quiet
/// capture matters: it can reveal a changed revision without opening a new
/// source-change obligation.
pub(super) fn latest_named_root_sighting_on(
    connection: &Connection,
    run_id: WorkRunId,
    workspace_id: &str,
    generation: i64,
    through: i64,
    changed_only: bool,
) -> Result<Option<(i64, SourceObservation)>, StoreError> {
    let (workspace_sql, generation_sql, state_sql) = (
        source_basis_sql("workspace_id"),
        source_basis_sql("source_root_generation"),
        source_basis_sql("source_root_state"),
    );
    let row = connection
        .query_row(
            &format!(
                "SELECT entry.position, entry.object_id
                 FROM work_feed_entries entry
                 JOIN objects object ON object.object_id = entry.object_id
                 WHERE entry.feed_kind = 'run_execution' AND entry.feed_id = ?1
                   AND entry.position <= ?2 AND {SOURCE_RECORD_SQL}
                   AND {workspace_sql} = ?3
                   AND {generation_sql} = ?4
                   AND {state_sql} = 'named'
                   AND (?5 = 0 OR {SOURCE_CHANGED_SQL})
                 ORDER BY entry.position DESC LIMIT 1"
            ),
            params![
                run_id.0.to_string(),
                through,
                workspace_id,
                generation,
                changed_only
            ],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    row.map(|row| source_observation_row(connection, row))
        .transpose()
}

/// The newest measured sighting on the run at or before `through` in
/// `workspace`, with its run-feed position: an admitted observation, quiet or
/// not (a check's producer among them), environment evidence, or an accounted
/// unadmitted source record. An audit-only record, one that reported no
/// change, and every check nested in an unadmitted record never count.
pub(super) fn newest_measured_sighting_on(
    connection: &Connection,
    run_id: WorkRunId,
    workspace: &str,
    through: i64,
) -> Result<Option<(i64, crate::domain::ExecutionSourceBasis)>, StoreError> {
    let (workspace_sql, revision_sql) = (
        source_basis_sql("workspace_id"),
        source_basis_sql("source_revision"),
    );
    let row = connection
        .query_row(
            &format!(
                "SELECT entry.position, entry.object_kind, entry.object_id
                 FROM work_feed_entries entry
                 JOIN objects object ON object.object_id = entry.object_id
                 WHERE entry.feed_kind = 'run_execution' AND entry.feed_id = ?1
                   AND entry.position <= ?2
                   AND (entry.object_kind = 'environment_evidence' OR {SOURCE_RECORD_SQL})
                   AND {workspace_sql} = ?3 AND {revision_sql} IS NOT NULL
                 ORDER BY entry.position DESC LIMIT 1"
            ),
            params![run_id.0.to_string(), through, workspace],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((position, kind, stored)) = row else {
        return Ok(None);
    };
    let record =
        ObjectId::from_stored(stored.clone()).ok_or(StoreError::InvalidStoredKey(stored))?;
    let basis = if kind == "environment_evidence" {
        Some(
            load_typed_work_object::<EnvironmentEvidence>(
                connection,
                &record,
                "environment_evidence",
            )?
            .source_basis,
        )
    } else {
        load_source_observation_on(connection, &record)?.source_basis
    };
    Ok(basis.map(|basis| (position, basis)))
}

/// The first accounted unadmitted change on the run at or before `through`
/// that a check does not follow, with whether only its time failed: a check
/// follows such a change only when its producer (at `producer_position`) and
/// its record (at `evidence_position`) are both after the change and it
/// completed no earlier than the change was recorded. Under a named root
/// (`root`: workspace and generation) a change sighted outside that root does
/// not count; a watcher-only change, which carries no located source, always
/// counts, since nothing shows it was elsewhere. `None` when the check follows
/// every one.
pub(in crate::storage) fn unadmitted_barrier_on(
    connection: &Connection,
    run_id: WorkRunId,
    (producer_position, evidence_position): (i64, i64),
    completed_at: DateTime<Utc>,
    through: i64,
    root: Option<(&str, i64)>,
) -> Result<Option<(SourceObservation, bool)>, StoreError> {
    let changes = connection
        .prepare(
            "SELECT entry.position, entry.object_id FROM work_feed_entries entry
             JOIN objects object ON object.object_id = entry.object_id
             WHERE entry.feed_kind = 'run_execution' AND entry.feed_id = ?1
               AND entry.position <= ?2
               AND entry.object_kind = 'unadmitted_execution_observation'
               AND json_extract(object.canonical_json, '$.accounting.kind') = 'source_change'
             ORDER BY entry.position",
        )?
        .query_map(params![run_id.0.to_string(), through], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (position, stored) in changes {
        let record =
            ObjectId::from_stored(stored.clone()).ok_or(StoreError::InvalidStoredKey(stored))?;
        let change = load_source_observation_on(connection, &record)?;
        if let (Some(basis), Some((workspace, generation))) = (change.source_basis.as_ref(), root)
            && (basis.workspace_id != workspace
                || basis.source_root_generation != Some(generation)
                || basis.source_root_state != Some(crate::domain::SourceRootState::Named))
        {
            continue;
        }
        let follows = producer_position > position && evidence_position > position;
        if !follows || completed_at < change.recorded_at {
            return Ok(Some((change, follows)));
        }
    }
    Ok(None)
}

/// An unlocated source change after the name cannot be attributed safely to
/// another workspace. A subsequent named-root check must outrun it.
pub(super) fn latest_unlocated_source_change_on(
    connection: &Connection,
    run_id: WorkRunId,
    binding_position: i64,
    through: i64,
) -> Result<Option<i64>, StoreError> {
    let workspace_sql = source_basis_sql("workspace_id");
    connection
        .query_row(
            &format!(
                "SELECT entry.position FROM work_feed_entries entry
                 JOIN objects object ON object.object_id = entry.object_id
                 WHERE entry.feed_kind = 'run_execution' AND entry.feed_id = ?1
                   AND entry.position > ?2 AND entry.position <= ?3
                   AND {SOURCE_RECORD_SQL} AND {SOURCE_CHANGED_SQL}
                   AND {workspace_sql} IS NULL
                 ORDER BY entry.position DESC LIMIT 1"
            ),
            params![run_id.0.to_string(), binding_position, through],
            |row| row.get(0),
        )
        .optional()
        .map_err(Into::into)
}

pub(super) fn append_work_event(
    transaction: &Transaction<'_>,
    event: &WorkEventDraft,
) -> Result<(ObjectId, Vec<FeedPosition>), StoreError> {
    append_work_event_on(transaction, event, None, None)
}

pub(super) fn append_work_event_with_root(
    transaction: &Transaction<'_>,
    event: &WorkEventDraft,
    root: &super::root_state::WrittenRoot<'_>,
) -> Result<(ObjectId, Vec<FeedPosition>), StoreError> {
    append_work_event_on(transaction, event, Some(root), None)
}

/// Only the atomic-plan writer supplies this transaction-local relation basis.
/// It must verify all resulting edge bindings before committing the plan.
pub(super) fn append_planned_prerequisite_event(
    transaction: &Transaction<'_>,
    event: &WorkEventDraft,
    basis: &super::WorkRelationBasis,
) -> Result<(ObjectId, Vec<FeedPosition>), StoreError> {
    if !matches!(
        event.transition,
        crate::domain::WorkTransition::PrerequisiteAdded { .. }
    ) {
        return Err(StoreError::InvalidWorkProjection(
            "planned relation basis requires a prerequisite addition".into(),
        ));
    }
    append_work_event_on(transaction, event, None, Some(basis))
}

/// Only the ordinary prerequisite transition supplies this basis: the one it
/// validated for the same item earlier in this transaction, so the append
/// does not validate it a second time. It is refused for another item or any
/// other transition.
pub(super) fn append_checked_relation_event(
    transaction: &Transaction<'_>,
    event: &WorkEventDraft,
    checked: super::planning::CheckedRelationBasis,
) -> Result<(ObjectId, Vec<FeedPosition>), StoreError> {
    let (work_id, basis) = checked.into_parts();
    if work_id != event.work_id
        || !matches!(
            event.transition,
            crate::domain::WorkTransition::PrerequisiteAdded { .. }
                | crate::domain::WorkTransition::PrerequisiteRemoved { .. }
        )
    {
        return Err(StoreError::InvalidWorkProjection(
            "a checked relation basis belongs to one item's prerequisite change".into(),
        ));
    }
    append_work_event_on(transaction, event, None, Some(&basis))
}

fn append_work_event_on(
    transaction: &Transaction<'_>,
    event: &WorkEventDraft,
    written_root: Option<&super::root_state::WrittenRoot<'_>>,
    planned_relations: Option<&super::WorkRelationBasis>,
) -> Result<(ObjectId, Vec<FeedPosition>), StoreError> {
    if event.actor.actor_id.trim().is_empty()
        || event
            .actor
            .session_id
            .as_ref()
            .is_none_or(|session| session.0.trim().is_empty())
    {
        return Err(StoreError::InvalidWork(
            crate::storage::refusal_labels::UNBOUND_LOCAL_ACTOR.into(),
        ));
    }
    let mut relation_basis = if let Some(basis) = planned_relations {
        basis.clone()
    } else if latest_canonical_work_event_for_item_optional(transaction, event.work_id)?.is_some() {
        validated_current_work_relation_basis(transaction, event.work_id)?
    } else {
        projected_work_relation_basis(transaction, event.work_id)?
    };
    apply_work_relation_transition(
        &mut relation_basis,
        &event.transition,
        event.blocker.as_ref(),
    )?;
    let root = match written_root {
        Some(root) => Some(root.event_ref(transaction, event.root_execution.as_ref())?),
        None => event
            .root_execution
            .as_ref()
            .map(|value| super::root_state::current_ref(transaction, value))
            .transpose()?,
    };
    let event = event
        .clone()
        .finalize(work_relation_fingerprint(&relation_basis)?, root);
    let object = CanonicalObject::mint(&event)?;
    SqliteStore::insert_object(transaction, "work_event", &object)?;
    let positions = append_to_work_feeds(
        transaction,
        &event.project_id,
        event.root_id,
        event.run_id,
        Some(event.work_id),
        "work_event",
        &object,
    )?;
    let changed = transaction.execute(
        "UPDATE work_items SET latest_event_id = ?2 WHERE work_id = ?1",
        params![event.work_id.0.to_string(), object.key().as_str()],
    )?;
    if changed != 1 {
        return Err(StoreError::InvalidWorkProjection(format!(
            "work event append lost item {:?}",
            event.work_id
        )));
    }
    Ok((object.key().clone(), positions))
}

pub(super) fn request_object<T: Serialize>(request: &T) -> Result<CanonicalObject, StoreError> {
    CanonicalObject::freeze(request)
}

pub(in crate::storage) fn load_typed_work_object<T: DeserializeOwned>(
    connection: &Connection,
    hash: &ObjectId,
    object_kind: &str,
) -> Result<T, StoreError> {
    let stored: Option<(String, Vec<u8>)> = connection
        .query_row(
            "SELECT object_kind, canonical_json FROM objects WHERE object_id = ?1",
            [hash.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (stored_kind, bytes) = stored
        .ok_or_else(|| StoreError::InvalidWorkProjection(format!("object {hash} is missing")))?;
    if stored_kind != object_kind {
        return Err(StoreError::ObjectKindMismatch {
            hash: hash.clone(),
            stored: stored_kind,
            requested: object_kind.into(),
        });
    }
    let decoded = CanonicalObject::stored(hash, bytes)?.decode()?;
    #[cfg(test)]
    super::cost::typed_work_object_decoded(object_kind);
    Ok(decoded)
}

pub(super) fn load_handoff_offer_projection(
    connection: &Connection,
    row: (Option<String>, Vec<u8>),
) -> Result<WorkHandoffOffer, StoreError> {
    let (stored_hash, projection_bytes) = row;
    let stored_hash = stored_hash.and_then(ObjectId::from_stored).ok_or_else(|| {
        StoreError::InvalidWorkProjection(
            "handoff offer projection has no valid canonical hash".into(),
        )
    })?;
    let canonical =
        load_typed_work_object::<WorkHandoffOffer>(connection, &stored_hash, "work_handoff_offer")?;
    let projection: WorkHandoffOffer = serde_json::from_slice(&projection_bytes)?;
    if projection != canonical {
        return Err(StoreError::InvalidWorkProjection(format!(
            "handoff offer {} differs from canonical object {stored_hash}",
            projection.offer_id.0
        )));
    }
    if latest_canonical_handoff_offer(
        connection,
        &projection.offer_id.0.to_string(),
        &projection.work_id.0.to_string(),
    )?
    .as_ref()
        != Some(&canonical)
    {
        return Err(StoreError::InvalidWorkProjection(format!(
            "handoff offer {} differs from the latest canonical work event",
            projection.offer_id.0
        )));
    }
    Ok(canonical)
}

pub(super) fn require_work_protocol_result_object(
    result: serde_json::Value,
) -> Result<serde_json::Value, StoreError> {
    result.as_object().ok_or_else(|| {
        StoreError::InvalidWorkProjection("work-protocol result must be a JSON object".into())
    })?;
    Ok(result)
}

pub(super) fn validate_work_protocol_result_binding(
    connection: &Connection,
    project_id: &str,
    operation: &str,
    result: &serde_json::Value,
) -> Result<(), StoreError> {
    let mut bound_items = Vec::new();
    match operation {
        crate::storage::PLAN_PROTOCOL_OPERATION => {
            let receipt: crate::domain::WorkPlanReceipt = serde_json::from_value(result.clone())?;
            if result.get("kind").and_then(serde_json::Value::as_str) != Some("plan")
                || receipt.tasks.is_empty()
                || receipt.tasks.len() > crate::domain::MAX_WORK_PLAN_TASKS
            {
                return Err(StoreError::InvalidWorkProjection(
                    "invalid plan replay mapping".into(),
                ));
            }
            let mut keys = HashSet::new();
            let mut ids = HashSet::new();
            for mapping in receipt.tasks {
                let item = load_work_item(connection, mapping.work_id)?;
                if !keys.insert(mapping.key)
                    || !ids.insert(mapping.work_id)
                    || mapping.short_ref != item.short_ref
                    || mapping.revision < 1
                    || mapping.revision > item.revision
                {
                    return Err(StoreError::InvalidWorkProjection(
                        "invalid plan replay identity".into(),
                    ));
                }
                bound_items.push(item);
            }
        }
        "work_propose:root" => {
            let work_id = result
                .pointer("/work/work_id")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "root proposal replay has no work identity".into(),
                    )
                })?;
            bound_items.push(load_work_item(connection, parse_work_id(work_id)?)?);
        }
        crate::storage::DECOMPOSE_PROTOCOL_OPERATION => {
            let parent_id = result
                .pointer("/parent/work_id")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "decomposition replay has no parent identity".into(),
                    )
                })?;
            bound_items.push(load_work_item(connection, parse_work_id(parent_id)?)?);
            let children = result
                .get("children")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "decomposition replay has no child identities".into(),
                    )
                })?;
            for child in children {
                let work_id = child
                    .get("work_id")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        StoreError::InvalidWorkProjection(
                            "decomposition replay child has no work identity".into(),
                        )
                    })?;
                bound_items.push(load_work_item(connection, parse_work_id(work_id)?)?);
            }
        }
        "work_complete" => {
            let work_id = result
                .get("work_id")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "completion replay has no work identity".into(),
                    )
                })?;
            bound_items.push(load_work_item(connection, parse_work_id(work_id)?)?);
        }
        operation
            if operation.starts_with("work_update:") || operation.starts_with("work_handoff:") =>
        {
            let work_id = result
                .pointer("/receipt/work_id")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    StoreError::InvalidWorkProjection("ambient replay has no work identity".into())
                })?;
            bound_items.push(load_work_item(connection, parse_work_id(work_id)?)?);
        }
        _ => {
            return Err(StoreError::InvalidWorkProjection(format!(
                "unknown durable work-protocol operation {operation}"
            )));
        }
    }
    if bound_items
        .iter()
        .any(|item| item.project_id.0 != project_id)
    {
        return Err(StoreError::InvalidWorkProjection(format!(
            "work-protocol result {operation} crosses its project binding"
        )));
    }
    Ok(())
}

pub(super) fn validate_work_source_snapshot(
    snapshot: &WorkSourceSnapshot,
    imported_at: DateTime<Utc>,
) -> Result<(), StoreError> {
    validate_work_source_snapshot_shape(snapshot)?;
    if snapshot.captured_at > imported_at {
        return Err(StoreError::InvalidWork(
            "work source snapshot capture time is in the future".into(),
        ));
    }
    Ok(())
}

pub(super) fn validate_work_source_snapshot_shape(
    snapshot: &WorkSourceSnapshot,
) -> Result<(), StoreError> {
    super::import::validate_key(&super::import::key(snapshot))?;
    let required_text_is_valid = [
        &snapshot.adapter_kind,
        &snapshot.canonical_ref,
        &snapshot.fingerprint,
    ]
    .into_iter()
    .all(|value| !value.trim().is_empty() && value.trim() == value);
    let optional_text_is_valid = snapshot
        .source_revision
        .as_ref()
        .into_iter()
        .chain(snapshot.canonical_url.as_ref())
        .chain(snapshot.projected.title.as_ref())
        .chain(snapshot.projected.status.as_ref())
        .chain(snapshot.projected.owner.as_ref())
        .all(|value| !value.trim().is_empty() && value.trim() == value);
    if snapshot.schema_version != SCHEMA_VERSION
        || !required_text_is_valid
        || !optional_text_is_valid
    {
        return Err(StoreError::InvalidWork(
            "work source snapshot has invalid schema or canonical text".into(),
        ));
    }
    let snapshot_bytes = crate::canonical::canonical_bytes(snapshot)?;
    if snapshot_bytes.len() > MAX_WORK_SOURCE_SNAPSHOT_BYTES {
        return Err(StoreError::InvalidWork(format!(
            "work source snapshot exceeds the {MAX_WORK_SOURCE_SNAPSHOT_BYTES}-byte canonical limit"
        )));
    }
    Ok(())
}

pub(super) fn inspect_work_request<R: Redactor, T: Serialize>(
    redactor: &R,
    request: &T,
    actor: &ActorContext,
) -> Result<(), StoreError> {
    crate::domain::validate_status_capture_actor(actor).map_err(StoreError::InvalidWork)?;
    actor
        .validate_attribution_context()
        .map_err(|detail| StoreError::InvalidWork(format!("invalid actor context: {detail}")))?;
    if let Some(session) = &actor.session_id {
        crate::storage::admit_session_id(session)?;
    }
    let candidate = serde_json::to_string(request)?;
    redactor
        .inspect(&candidate)
        .map_err(StoreError::RedactionRefused)
}

pub(super) fn expire_handoff_offers(
    transaction: &Transaction<'_>,
    run_id: WorkRunId,
    now: DateTime<Utc>,
    actor: &crate::domain::ActorContext,
) -> Result<Vec<WorkHandoffOffer>, StoreError> {
    let mut statement = transaction.prepare(
        "SELECT offer_object_id, offer_json FROM work_handoff_offers
         WHERE run_id = ?1 AND state = 'offered' AND expires_at_ms <= ?2
         ORDER BY offer_id",
    )?;
    let rows = statement
        .query_map(
            params![run_id.0.to_string(), now.timestamp_millis()],
            |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let item_run = if rows.is_empty() {
        None
    } else {
        let run = load_work_run(transaction, run_id)?;
        let item = load_work_item(transaction, run.work_id)?;
        let root_execution = load_root_execution(transaction, run.root_execution_id)?;
        let claim = load_work_claim_optional(transaction, run_id)?;
        Some((item, run, root_execution, claim))
    };
    let mut expired = Vec::with_capacity(rows.len());
    for row in rows {
        let mut offer = load_handoff_offer_projection(transaction, row)?;
        offer.state = WorkHandoffState::Expired;
        let offer_object = CanonicalObject::mint(&offer)?;
        SqliteStore::insert_object(transaction, "work_handoff_offer", &offer_object)?;
        let changed = transaction.execute(
            "UPDATE work_handoff_offers
             SET state = 'expired', offer_object_id = ?2, offer_json = ?3
              WHERE offer_id = ?1 AND state = 'offered'",
            params![
                offer.offer_id.0.to_string(),
                offer_object.key().as_str(),
                serde_json::to_vec(&offer)?
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::InvalidWorkProjection(format!(
                "handoff offer {:?} was not offered during expiry",
                offer.offer_id
            )));
        }
        if let Some((item, run, root_execution, claim)) = item_run.as_ref() {
            let event = WorkEventDraft {
                schema_version: SCHEMA_VERSION,
                project_id: item.project_id.clone(),
                root_id: item.root_id,
                work_id: item.work_id,
                run_id: Some(run.run_id),
                revision: item.revision,
                work: item.clone(),
                run: Some(run.clone()),
                root_execution: Some(root_execution.clone()),
                claim: claim.clone(),
                handoff_offer: Some(offer.clone()),
                blocker: None,
                transition: WorkTransition::HandoffExpired {
                    offer_id: offer.offer_id,
                    offer: offer_object.key().clone(),
                },
                actor: actor.clone(),
                created_at: now,
            };
            append_work_event(transaction, &event)?;
        }
        expired.push(offer);
    }
    Ok(expired)
}

pub(super) fn replay_operation<T: DeserializeOwned>(
    transaction: &Transaction<'_>,
    operation: &str,
    key: &str,
    request_hash: &ObjectId,
) -> Result<Option<T>, StoreError> {
    let stored: Option<(String, Vec<u8>)> = transaction
        .query_row(
            "SELECT request_hash, result_json FROM work_operation_results
             WHERE operation = ?1 AND idempotency_key = ?2",
            params![operation, key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((stored_hash, result)) = stored else {
        return Ok(None);
    };
    if stored_hash != request_hash.as_str() {
        return Err(StoreError::WorkOperationIdempotencyConflict {
            operation: operation.into(),
            key: key.into(),
        });
    }
    serde_json::from_slice(&result)
        .map(Some)
        .map_err(StoreError::from)
}
