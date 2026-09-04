use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Serialize, de::DeserializeOwned};

use super::super::{SqliteStore, StoreError};
use super::execution::latest_canonical_handoff_offer;
use super::integrity::expected_environment_projection;
use super::planning::{
    apply_work_relation_transition, assert_actor_session, projected_work_relation_basis,
    renew_holder_claim, validate_live_claim_on, validated_current_work_relation_basis,
    work_relation_fingerprint,
};
use super::query::{
    feed_parts, latest_canonical_work_event_for_item_optional, load_root_execution,
    load_work_claim_optional, load_work_item, load_work_run, parse_work_id, verified_work_identity,
};
use super::{CHECKPOINT_APPEND_COUNT, MAX_WORK_SOURCE_SNAPSHOT_BYTES, WorkEventDraft};
use crate::{
    CanonicalObject, ObjectHash,
    domain::{
        ActorContext, EnvironmentEvidence, ExecutionObservation, FeedId, FeedPosition,
        MemoryAssertionEvent, MemoryVersion, SCHEMA_VERSION, SessionId, WorkHandoffOffer,
        WorkHandoffState, WorkId, WorkRunId, WorkSourceSnapshot, WorkTransition,
    },
    memory::Redactor,
};

#[cfg(test)]
mod tests;

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
             feed_kind, feed_id, position, object_kind, object_hash, work_id
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            feed_kind,
            feed_id,
            position.position,
            object_kind,
            object.hash().as_str(),
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

#[allow(
    clippy::too_many_arguments,
    reason = "the exact work, holder, time, actor, typed memory, and canonical objects form one audited capture binding"
)]
pub(in crate::storage) fn append_memory_capture_to_work_feeds(
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
        || assertion.version != *version_object.hash()
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
            version: version_object.hash().clone(),
            assertion: assertion_object.hash().clone(),
        },
        actor: actor.clone(),
        created_at: captured_at,
    };
    let (_, event_positions) = append_work_event(transaction, &event)?;
    positions.extend(event_positions);
    Ok(positions)
}

pub(in crate::storage) fn append_context_object_to_work_feeds(
    transaction: &Transaction<'_>,
    work_id: WorkId,
    object_kind: &str,
    object: &CanonicalObject,
) -> Result<Vec<FeedPosition>, StoreError> {
    let item = load_work_item(transaction, work_id)?;
    append_to_work_feeds(
        transaction,
        &item.project_id,
        item.root_id,
        item.active_run_id,
        None,
        object_kind,
        object,
    )
}

pub(in crate::storage) fn load_control_execution_observation_on(
    connection: &Connection,
    hash: &ObjectHash,
) -> Result<Option<ExecutionObservation>, StoreError> {
    let stored = connection
        .query_row(
            "SELECT object_kind, canonical_json FROM objects WHERE object_hash = ?1",
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
    Ok(Some(CanonicalObject::verify(hash, bytes)?.decode()?))
}

pub(in crate::storage) fn load_control_environment_evidence_on(
    connection: &Connection,
    hash: &ObjectHash,
) -> Result<Option<EnvironmentEvidence>, StoreError> {
    let stored = connection
        .query_row(
            "SELECT object_kind, canonical_json FROM objects WHERE object_hash = ?1",
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
    let evidence = CanonicalObject::verify(hash, bytes)?.decode()?;
    expected_environment_projection(connection, hash)?;
    Ok(Some(evidence))
}

pub(super) fn verify_anchored_memory_feeds(
    connection: &Connection,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let feed_has = |object_hash: &ObjectHash, feed_kind: &str, feed_id: &str| {
        connection
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM work_feed_entries
                     WHERE feed_kind = ?1 AND feed_id = ?2 AND object_hash = ?3
                 )",
                params![feed_kind, feed_id, object_hash.as_str()],
                |row| row.get::<_, bool>(0),
            )
            .map_err(StoreError::from)
    };
    let mut statement = connection.prepare(
        "SELECT object_hash, object_kind, canonical_json FROM objects
         WHERE object_kind IN ('memory_contradiction_event', 'memory_version')
         ORDER BY object_hash",
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
        let Some(hash) = ObjectHash::from_stored(stored_hash) else {
            invalid.push(label);
            continue;
        };
        let Ok(object) = CanonicalObject::verify(&hash, bytes) else {
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
        let anchor = if object_kind == "memory_contradiction_event" {
            object
                .decode::<crate::domain::MemoryContradictionEvent>()
                .ok()
                .and_then(|event| {
                    event
                        .work_root_id
                        .map(|root_id| (event.project_id, root_id))
                })
        } else if let Ok(crate::domain::MemoryVersion {
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
    object_hash: &ObjectHash,
) -> Result<FeedPosition, StoreError> {
    let position = connection
        .query_row(
            "SELECT position FROM work_feed_entries
             WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND object_hash = ?2",
            params![run_id.0.to_string(), object_hash.as_str()],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .ok_or_else(|| {
            StoreError::InvalidWorkProjection(format!(
                "object {object_hash} is missing from run {run_id:?} feed"
            ))
        })?;
    Ok(FeedPosition {
        feed: FeedId::RunExecution(run_id),
        position,
    })
}

pub(super) fn current_run_feed_cut_on(
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

pub(super) fn latest_source_mutation_on(
    connection: &Connection,
    run_id: WorkRunId,
    through: i64,
) -> Result<Option<(i64, ExecutionObservation)>, StoreError> {
    let stored = connection
        .query_row(
            "SELECT entry.position, entry.object_hash, object.canonical_json
             FROM work_feed_entries entry
             JOIN objects object ON object.object_hash = entry.object_hash
             WHERE entry.feed_kind = 'run_execution' AND entry.feed_id = ?1
               AND entry.position <= ?2
               AND entry.object_kind = 'execution_observation'
               AND json_extract(object.canonical_json, '$.source_changed') = 1
             ORDER BY entry.position DESC LIMIT 1",
            params![run_id.0.to_string(), through],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            },
        )
        .optional()?;
    stored
        .map(|(position, stored_hash, bytes)| {
            let hash = ObjectHash::from_stored(stored_hash.clone())
                .ok_or(StoreError::InvalidStoredHash(stored_hash))?;
            let observation = CanonicalObject::verify(&hash, bytes)?.decode()?;
            Ok((position, observation))
        })
        .transpose()
}

pub(super) fn append_work_event(
    transaction: &Transaction<'_>,
    event: &WorkEventDraft,
) -> Result<(ObjectHash, Vec<FeedPosition>), StoreError> {
    if event.actor.actor_id.trim().is_empty()
        || event
            .actor
            .session_id
            .as_ref()
            .is_none_or(|session| session.0.trim().is_empty())
    {
        return Err(StoreError::InvalidWork(
            "local work requires a non-empty asserted actor and session binding".into(),
        ));
    }
    let mut relation_basis =
        if latest_canonical_work_event_for_item_optional(transaction, event.work_id)?.is_some() {
            validated_current_work_relation_basis(transaction, event.work_id)?
        } else {
            projected_work_relation_basis(transaction, event.work_id)?
        };
    apply_work_relation_transition(
        &mut relation_basis,
        &event.transition,
        event.blocker.as_ref(),
    )?;
    let event = event
        .clone()
        .finalize(work_relation_fingerprint(&relation_basis)?);
    let object = CanonicalObject::freeze(&event)?;
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
        "UPDATE work_items SET latest_event_hash = ?2 WHERE work_id = ?1",
        params![event.work_id.0.to_string(), object.hash().as_str()],
    )?;
    if changed != 1 {
        return Err(StoreError::InvalidWorkProjection(format!(
            "work event append lost item {:?}",
            event.work_id
        )));
    }
    Ok((object.hash().clone(), positions))
}

pub(super) fn request_object<T: Serialize>(request: &T) -> Result<CanonicalObject, StoreError> {
    CanonicalObject::freeze(request)
}

pub(in crate::storage) fn load_typed_work_object<T: DeserializeOwned>(
    connection: &Connection,
    hash: &ObjectHash,
    object_kind: &str,
) -> Result<T, StoreError> {
    let stored: Option<(String, Vec<u8>)> = connection
        .query_row(
            "SELECT object_kind, canonical_json FROM objects WHERE object_hash = ?1",
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
    CanonicalObject::verify(hash, bytes)?.decode()
}

pub(super) fn load_handoff_offer_projection(
    connection: &Connection,
    row: (Option<String>, Vec<u8>),
) -> Result<WorkHandoffOffer, StoreError> {
    let (stored_hash, projection_bytes) = row;
    let stored_hash = stored_hash
        .and_then(ObjectHash::from_stored)
        .ok_or_else(|| {
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
        "work_propose:decompose" => {
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
        || snapshot.captured_at > imported_at
    {
        return Err(StoreError::InvalidWork(
            "work source snapshot has invalid schema, canonical text, or capture time".into(),
        ));
    }
    let object = CanonicalObject::freeze(snapshot)?;
    if object.bytes().len() > MAX_WORK_SOURCE_SNAPSHOT_BYTES {
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
    actor
        .validate_attribution_context()
        .map_err(|detail| StoreError::InvalidWork(format!("invalid actor context: {detail}")))?;
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
        "SELECT offer_hash, offer_json FROM work_handoff_offers
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
        let offer_object = CanonicalObject::freeze(&offer)?;
        SqliteStore::insert_object(transaction, "work_handoff_offer", &offer_object)?;
        let changed = transaction.execute(
            "UPDATE work_handoff_offers
             SET state = 'expired', offer_hash = ?2, offer_json = ?3
              WHERE offer_id = ?1 AND state = 'offered'",
            params![
                offer.offer_id.0.to_string(),
                offer_object.hash().as_str(),
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
                    offer: offer_object.hash().clone(),
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
    request_hash: &ObjectHash,
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
