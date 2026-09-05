use std::collections::{BTreeMap, HashSet};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior};
use serde_json::Value;

use super::{
    MAX_PROJECT_MEMORY_ATTRIBUTION_BYTES, MAX_PROJECT_MEMORY_ATTRIBUTION_TEXT_BYTES,
    MAX_PROJECT_MEMORY_PROVENANCE_LINKS, MemoryHeadProjectionRow, SqliteStore, StoreError,
    derived_project_memory_state_on, project_memory_state_on, validate_keyed_project_memory_shape,
    validate_stored_project_memory_key,
};
use crate::{
    ActorContext, CanonicalObject, CompletionSeal, EnvironmentEvidence, MemoryStatus,
    MemoryVersion, ObjectHash, ProjectId, RestoredRecord, RestoredWorkEvidence, Scope, Sensitivity,
    VerificationEvidence, WorkCheckpoint, WorkEvidence, WorkEvidenceKind, WorkGraphSnapshotBlocker,
    WorkGraphSnapshotBody, WorkGraphSnapshotCompletion, WorkGraphSnapshotCut,
    WorkGraphSnapshotDestinationKind, WorkGraphSnapshotDocument, WorkGraphSnapshotEvent,
    WorkGraphSnapshotExport, WorkGraphSnapshotGate, WorkGraphSnapshotHistory,
    WorkGraphSnapshotItem, WorkGraphSnapshotLoadedEvent, WorkGraphSnapshotManifest,
    WorkGraphSnapshotMemory, WorkGraphSnapshotMemoryState, WorkGraphSnapshotNote,
    WorkGraphSnapshotRecord, WorkGraphSnapshotRecordPayload, WorkGraphSnapshotRedactedCounts,
    WorkGraphSnapshotSavedEvent, WorkGraphSnapshotSectionCounts, WorkGraphSnapshotSource,
    WorkGraphSnapshotSummary, WorkGraphSnapshotText, WorkId, WorkLifecycle, WorkSourceSnapshot,
    WorkTransition,
    domain::{MemoryAssertionEvent, is_unsafe_rendered_text_char},
    memory::Redactor,
    work_graph_snapshot_exporting_build, work_graph_snapshot_format_fingerprint,
};

mod load;

pub(super) const REDACTED_MEMORY_PLACEHOLDER: &str = "[redacted in work-graph snapshot]";
pub(super) const RESTORED_MEMORY_SOURCE: &str = "engram:work-graph-snapshot";
pub(super) const RESTORED_REDACTED_MEMORY_SOURCE: &str = "engram:work-graph-snapshot:redacted";

impl SqliteStore {
    /// Freezes one deterministic work-graph body and commits its disclosure
    /// audit before returning any bytes to the caller.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when canonical state is corrupt, redaction
    /// refuses a carried string, or either transaction cannot complete.
    pub fn save_work_graph_snapshot<R: Redactor>(
        &mut self,
        project_id: &ProjectId,
        actor: &ActorContext,
        widening_reason: Option<&str>,
        destination_kind: WorkGraphSnapshotDestinationKind,
        exported_at: DateTime<Utc>,
        redactor: &R,
    ) -> Result<WorkGraphSnapshotExport, StoreError> {
        let widening_reason = validate_widening_reason(widening_reason)?;
        validate_snapshot_audit_actor_shape(actor).map_err(StoreError::InvalidWork)?;
        let widened = widening_reason.is_some();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Deferred)?;
        let body = build_snapshot_body_on(
            &transaction,
            project_id,
            widening_reason.as_deref(),
            redactor,
        )?;
        let body_object = CanonicalObject::freeze(&body)?;
        let body_sha256 = body_object.hash().clone();
        let manifest = WorkGraphSnapshotManifest {
            exported_at,
            exporting_build: work_graph_snapshot_exporting_build(),
            body_sha256: body_sha256.clone(),
            summary: body.summary.clone(),
        };
        let document = WorkGraphSnapshotDocument { body, manifest };
        load::validate_generated_snapshot_document(&document)?;
        inspect_serialized_strings(redactor, &serde_json::to_value(&document)?)?;
        transaction.commit()?;

        let saved = WorkGraphSnapshotSavedEvent {
            attempt_id: uuid::Uuid::now_v7().to_string(),
            schema_version: crate::WORK_GRAPH_SNAPSHOT_SCHEMA_VERSION,
            project_id: project_id.clone(),
            as_of: document.body.summary.as_of.clone(),
            widened,
            widening_reason,
            redacted: document.body.summary.redacted.clone(),
            body_sha256: body_sha256.clone(),
            destination_kind,
            actor: actor.clone(),
            attempted_at: exported_at,
        };
        inspect_serialized_strings(redactor, &serde_json::to_value(&saved)?)?;
        let saved_object = CanonicalObject::freeze(&saved)?;
        let audit = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::insert_object(&audit, "work_graph_snapshot_saved", &saved_object)?;
        audit.commit()?;

        Ok(WorkGraphSnapshotExport {
            document,
            body_sha256,
            redactor_status: redactor.description().into(),
        })
    }

    /// Returns the complete verified disclosure-attempt audit for one project
    /// in stable attempt order. This host/operator query is intentionally
    /// unbounded; routine diagnostics use the bounded recent-page method.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when a matching canonical audit object is
    /// missing, malformed, or disagrees with its project binding.
    pub fn work_graph_snapshot_save_audits(
        &self,
        project_id: &ProjectId,
    ) -> Result<Vec<WorkGraphSnapshotSavedEvent>, StoreError> {
        let rows = self
            .connection
            .prepare(
                "SELECT object_hash, canonical_json
                 FROM objects INDEXED BY objects_graph_snapshot_audit
                 WHERE object_kind = 'work_graph_snapshot_saved'
                   AND json_extract(canonical_json, '$.project_id') = ?1
                 ORDER BY json_extract(canonical_json, '$.attempt_id'), object_hash",
            )?
            .query_map([project_id.0.as_str()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|(stored_hash, bytes)| {
                let hash = ObjectHash::from_stored(stored_hash.clone())
                    .ok_or(StoreError::InvalidStoredHash(stored_hash))?;
                let event: WorkGraphSnapshotSavedEvent =
                    CanonicalObject::verify(&hash, bytes)?.decode()?;
                validate_saved_event(&event, Some(project_id))?;
                Ok(event)
            })
            .collect()
    }

    /// Returns the total disclosure-attempt count and the most recent bounded
    /// page in stable chronological order.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the limit is invalid or a matching audit
    /// object is missing, malformed, or disagrees with its project binding.
    pub fn recent_work_graph_snapshot_save_audits(
        &self,
        project_id: &ProjectId,
        limit: usize,
    ) -> Result<(usize, Vec<WorkGraphSnapshotSavedEvent>), StoreError> {
        let limit = i64::try_from(limit).map_err(|_| {
            StoreError::InvalidWorkProjection(
                "graph snapshot audit page limit exceeds SQLite range".into(),
            )
        })?;
        let total = self.connection.query_row(
            "SELECT COUNT(*)
             FROM objects INDEXED BY objects_graph_snapshot_audit
             WHERE object_kind = 'work_graph_snapshot_saved'
               AND json_extract(canonical_json, '$.project_id') = ?1",
            [project_id.0.as_str()],
            |row| row.get::<_, i64>(0),
        )?;
        let mut rows = self
            .connection
            .prepare(
                "SELECT object_hash, canonical_json
                 FROM objects INDEXED BY objects_graph_snapshot_audit
                 WHERE object_kind = 'work_graph_snapshot_saved'
                   AND json_extract(canonical_json, '$.project_id') = ?1
                 ORDER BY json_extract(canonical_json, '$.attempt_id') DESC, object_hash DESC
                 LIMIT ?2",
            )?
            .query_map(rusqlite::params![project_id.0, limit], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|(stored_hash, bytes)| {
                let hash = ObjectHash::from_stored(stored_hash.clone())
                    .ok_or(StoreError::InvalidStoredHash(stored_hash))?;
                let event: WorkGraphSnapshotSavedEvent =
                    CanonicalObject::verify(&hash, bytes)?.decode()?;
                validate_saved_event(&event, Some(project_id))?;
                Ok(event)
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        rows.reverse();
        let total = usize::try_from(total).map_err(|_| {
            StoreError::InvalidWorkProjection(
                "graph snapshot audit count exceeds process range".into(),
            )
        })?;
        Ok((total, rows))
    }

    /// Returns the total successful-load count and the most recent bounded
    /// page in stable chronological order.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the limit is invalid or a matching audit
    /// object is missing, malformed, or disagrees with its project binding.
    pub fn recent_work_graph_snapshot_load_audits(
        &self,
        project_id: &ProjectId,
        limit: usize,
    ) -> Result<(usize, Vec<WorkGraphSnapshotLoadedEvent>), StoreError> {
        let limit = i64::try_from(limit).map_err(|_| {
            StoreError::InvalidWorkProjection(
                "graph snapshot load-audit page limit exceeds SQLite range".into(),
            )
        })?;
        let total = self.connection.query_row(
            "SELECT COUNT(*)
             FROM objects INDEXED BY objects_graph_snapshot_load_audit
             WHERE object_kind = 'work_graph_snapshot_loaded'
               AND json_extract(canonical_json, '$.project_id') = ?1",
            [project_id.0.as_str()],
            |row| row.get::<_, i64>(0),
        )?;
        let mut rows = self
            .connection
            .prepare(
                "SELECT object_hash, canonical_json
                 FROM objects INDEXED BY objects_graph_snapshot_load_audit
                 WHERE object_kind = 'work_graph_snapshot_loaded'
                   AND json_extract(canonical_json, '$.project_id') = ?1
                 ORDER BY json_extract(canonical_json, '$.attempt_id') DESC, object_hash DESC
                 LIMIT ?2",
            )?
            .query_map(rusqlite::params![project_id.0, limit], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
            .into_iter()
            .map(|(stored_hash, bytes)| {
                let hash = ObjectHash::from_stored(stored_hash.clone())
                    .ok_or(StoreError::InvalidStoredHash(stored_hash))?;
                let event: WorkGraphSnapshotLoadedEvent =
                    CanonicalObject::verify(&hash, bytes)?.decode()?;
                validate_loaded_event(&event, Some(project_id))?;
                Ok(event)
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        rows.reverse();
        let total = usize::try_from(total).map_err(|_| {
            StoreError::InvalidWorkProjection(
                "graph snapshot load-audit count exceeds process range".into(),
            )
        })?;
        Ok((total, rows))
    }
}

pub(super) fn verify_work_graph_snapshot_saved_events_on(
    connection: &Connection,
) -> Result<(usize, Vec<String>), StoreError> {
    let rows = connection
        .prepare(
            "SELECT object_hash, canonical_json
             FROM objects WHERE object_kind = 'work_graph_snapshot_saved'
             ORDER BY object_hash",
        )?
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut invalid = Vec::new();
    let mut attempt_ids = HashSet::new();
    for (stored_hash, bytes) in &rows {
        let valid = ObjectHash::from_stored(stored_hash.clone())
            .ok_or_else(|| StoreError::InvalidStoredHash(stored_hash.clone()))
            .and_then(|hash| CanonicalObject::verify(&hash, bytes.clone()))
            .and_then(|object| object.decode::<WorkGraphSnapshotSavedEvent>())
            .and_then(|event| {
                validate_saved_event(&event, None)?;
                if !attempt_ids.insert(event.attempt_id) {
                    return Err(StoreError::InvalidWorkProjection(
                        "duplicate graph snapshot save attempt id".into(),
                    ));
                }
                Ok(())
            })
            .is_ok();
        if !valid {
            invalid.push(format!("work_graph_snapshot_saved:{stored_hash}"));
        }
    }
    let mut checked = rows.len();
    let loaded_rows = connection
        .prepare(
            "SELECT object_hash, canonical_json
             FROM objects WHERE object_kind = 'work_graph_snapshot_loaded'
             ORDER BY object_hash",
        )?
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    checked += loaded_rows.len();
    let mut load_attempt_ids = HashSet::new();
    for (stored_hash, bytes) in loaded_rows {
        let valid = ObjectHash::from_stored(stored_hash.clone())
            .ok_or_else(|| StoreError::InvalidStoredHash(stored_hash.clone()))
            .and_then(|hash| CanonicalObject::verify(&hash, bytes))
            .and_then(|object| object.decode::<WorkGraphSnapshotLoadedEvent>())
            .and_then(|event| {
                validate_loaded_event(&event, None)?;
                if !load_attempt_ids.insert(event.attempt_id) {
                    return Err(StoreError::InvalidWorkProjection(
                        "duplicate graph snapshot load attempt id".into(),
                    ));
                }
                Ok(())
            })
            .is_ok();
        if !valid {
            invalid.push(format!("work_graph_snapshot_loaded:{stored_hash}"));
        }
    }
    Ok((checked, invalid))
}

fn validate_loaded_event(
    event: &WorkGraphSnapshotLoadedEvent,
    project_id: Option<&ProjectId>,
) -> Result<(), StoreError> {
    validate_snapshot_audit_actor_shape(&event.actor).map_err(|detail| {
        StoreError::InvalidWorkProjection(format!(
            "snapshot load audit has invalid attribution: {detail}"
        ))
    })?;
    let attempt_id = uuid::Uuid::parse_str(&event.attempt_id).map_err(|error| {
        StoreError::InvalidWorkProjection(format!(
            "snapshot load audit has invalid attempt id: {error}"
        ))
    })?;
    validate_snapshot_audit_text(&event.exporting_build, "exporting build").map_err(|detail| {
        StoreError::InvalidWorkProjection(format!(
            "snapshot load audit has invalid exporting build: {detail}"
        ))
    })?;
    if attempt_id.get_version_num() != 7
        || event.schema_version != crate::WORK_GRAPH_SNAPSHOT_SCHEMA_VERSION
        || event.as_of.work_feed < 0
        || event.as_of.project_memory < 0
        || event.widened != event.widening_reason.is_some()
        || !snapshot_redacted_counts_are_current(&event.redacted)
        || project_id.is_some_and(|expected| expected != &event.project_id)
    {
        return Err(StoreError::InvalidWorkProjection(
            "snapshot load audit disagrees with its binding".into(),
        ));
    }
    Ok(())
}

pub(in crate::storage) fn work_graph_snapshot_load_origin_on(
    connection: &Connection,
    project_id: &ProjectId,
) -> Result<WorkGraphSnapshotLoadedEvent, StoreError> {
    // Empty graph loads leave the destination eligible for a later load. The
    // newest attempt is the one that created its work, even with a supplied
    // operation clock that moves backwards between loads.
    let row = connection
        .query_row(
            "SELECT object_hash, canonical_json
             FROM objects INDEXED BY objects_graph_snapshot_load_audit
             WHERE object_kind = 'work_graph_snapshot_loaded'
               AND json_extract(canonical_json, '$.project_id') = ?1
             ORDER BY json_extract(canonical_json, '$.attempt_id') DESC LIMIT 1",
            [project_id.0.as_str()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()?;
    let (stored_hash, bytes) = row.ok_or_else(|| {
        StoreError::InvalidWorkProjection(format!(
            "restored project {:?} has no load audit",
            project_id.0
        ))
    })?;
    let hash = ObjectHash::from_stored(stored_hash.clone())
        .ok_or(StoreError::InvalidStoredHash(stored_hash))?;
    let event: WorkGraphSnapshotLoadedEvent = CanonicalObject::verify(&hash, bytes)?.decode()?;
    validate_loaded_event(&event, Some(project_id))?;
    Ok(event)
}

fn validate_saved_event(
    event: &WorkGraphSnapshotSavedEvent,
    project_id: Option<&ProjectId>,
) -> Result<(), StoreError> {
    let widening_reason = validate_widening_reason(event.widening_reason.as_deref())?;
    validate_snapshot_audit_actor_shape(&event.actor).map_err(|detail| {
        StoreError::InvalidWorkProjection(format!(
            "snapshot save audit has invalid attribution: {detail}"
        ))
    })?;
    let attempt_id = uuid::Uuid::parse_str(&event.attempt_id).map_err(|error| {
        StoreError::InvalidWorkProjection(format!(
            "snapshot save audit has invalid attempt id: {error}"
        ))
    })?;
    if attempt_id.get_version_num() != 7
        || event.schema_version != crate::WORK_GRAPH_SNAPSHOT_SCHEMA_VERSION
        || event.widened != widening_reason.is_some()
        || event.as_of.work_feed < 0
        || event.as_of.project_memory < 0
        || !snapshot_redacted_counts_are_current(&event.redacted)
        || project_id.is_some_and(|expected| expected != &event.project_id)
    {
        return Err(StoreError::InvalidWorkProjection(
            "snapshot save audit disagrees with its binding".into(),
        ));
    }
    Ok(())
}

fn snapshot_redacted_counts_are_current(counts: &WorkGraphSnapshotRedactedCounts) -> bool {
    counts.items == 0 && counts.blockers == 0 && counts.sources == 0 && counts.records == 0
}

fn validate_snapshot_audit_actor_shape(actor: &ActorContext) -> Result<(), String> {
    actor
        .validate_attribution_context()
        .map_err(str::to_owned)?;
    validate_snapshot_audit_text(&actor.actor_id, "actor id")?;
    validate_snapshot_audit_text(&actor.actor_kind, "actor kind")?;
    validate_snapshot_audit_text(&actor.reason, "reason")?;
    for (label, value) in [
        ("run id", actor.run_id.as_deref()),
        ("source tool", actor.source_tool.as_deref()),
        ("source skill", actor.source_skill.as_deref()),
    ] {
        if let Some(value) = value {
            validate_snapshot_audit_text(value, label)?;
        }
    }
    let session = actor
        .session_id
        .as_ref()
        .ok_or_else(|| "session id is required".to_owned())?;
    validate_snapshot_audit_text(&session.0, "session id")?;
    if actor.provenance_chain.len() > MAX_PROJECT_MEMORY_PROVENANCE_LINKS {
        return Err(format!(
            "provenance must contain at most {MAX_PROJECT_MEMORY_PROVENANCE_LINKS} links"
        ));
    }
    for link in &actor.provenance_chain {
        validate_snapshot_audit_text(&link.source, "provenance source")?;
        if let Some(reference) = link.reference.as_deref() {
            validate_snapshot_audit_text(reference, "provenance reference")?;
        }
    }
    let bytes = serde_json::to_vec(actor).map_err(|error| error.to_string())?;
    if bytes.len() > MAX_PROJECT_MEMORY_ATTRIBUTION_BYTES {
        return Err(format!(
            "serialized attribution exceeds {MAX_PROJECT_MEMORY_ATTRIBUTION_BYTES} bytes"
        ));
    }
    Ok(())
}

fn validate_snapshot_audit_text(value: &str, label: &str) -> Result<(), String> {
    if value.trim().is_empty()
        || value.len() > MAX_PROJECT_MEMORY_ATTRIBUTION_TEXT_BYTES
        || value.chars().any(is_unsafe_rendered_text_char)
    {
        return Err(format!(
            "{label} must contain 1 through {MAX_PROJECT_MEMORY_ATTRIBUTION_TEXT_BYTES} UTF-8 bytes without control or format characters"
        ));
    }
    Ok(())
}

fn build_snapshot_body_on<R: Redactor>(
    connection: &Connection,
    project_id: &ProjectId,
    widening_reason: Option<&str>,
    redactor: &R,
) -> Result<WorkGraphSnapshotBody, StoreError> {
    let widened = widening_reason.is_some();
    let (_, invalid_work_records) = SqliteStore::verify_work_projections_on(connection)?;
    if !invalid_work_records.is_empty() {
        return Err(StoreError::InvalidWorkProjection(format!(
            "snapshot refused because work integrity verification found {} invalid record(s)",
            invalid_work_records.len()
        )));
    }
    let (active_memory_count, project_memory_position) =
        project_memory_state_on(connection, project_id)?;
    let derived_memory_state = derived_project_memory_state_on(connection, project_id)?;
    let stored_active_count = i64::try_from(active_memory_count).map_err(|_| {
        StoreError::InvalidMemoryProjection(
            "snapshot project-memory count exceeds its stored representation".into(),
        )
    })?;
    if derived_memory_state != (stored_active_count, project_memory_position) {
        return Err(StoreError::InvalidMemoryProjection(
            "snapshot project-memory state does not match its canonical history".into(),
        ));
    }
    let as_of = WorkGraphSnapshotCut {
        work_feed: connection
            .query_row(
                "SELECT position FROM work_feed_heads
                 WHERE feed_kind = 'project' AND feed_id = ?1",
                [project_id.0.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .optional()?
            .unwrap_or(0),
        project_memory: project_memory_position,
    };
    let sections = work_sections_on(connection, project_id)?;
    let (memories, memory_redactions) =
        memories_on(connection, project_id, widened, active_memory_count)?;
    let redacted = WorkGraphSnapshotRedactedCounts {
        memories: memory_redactions,
        ..WorkGraphSnapshotRedactedCounts::default()
    };
    let section_counts = WorkGraphSnapshotSectionCounts {
        items: sections.items.len(),
        blockers: sections.blockers.len(),
        sources: sections.sources.len(),
        records: sections.records.len(),
        memories: memories.len(),
    };
    let summary = WorkGraphSnapshotSummary {
        schema_version: crate::WORK_GRAPH_SNAPSHOT_SCHEMA_VERSION,
        format_fingerprint: work_graph_snapshot_format_fingerprint()?,
        project_id: project_id.clone(),
        as_of,
        widened,
        widening_reason: widening_reason.map(str::to_owned),
        redacted,
        redactor_status: redactor.description().into(),
        section_counts,
    };
    Ok(WorkGraphSnapshotBody {
        summary,
        items: sections.items,
        blockers: sections.blockers,
        sources: sections.sources,
        records: sections.records,
        memories,
    })
}

fn validate_widening_reason(reason: Option<&str>) -> Result<Option<String>, StoreError> {
    const MAX_REASON_BYTES: usize = 4_096;

    reason
        .map(|reason| {
            if reason.trim().is_empty()
                || reason.len() > MAX_REASON_BYTES
                || reason
                    .chars()
                    .any(crate::domain::is_unsafe_rendered_text_char)
            {
                return Err(StoreError::InvalidWork(format!(
                    "snapshot widening reason must contain from 1 through {MAX_REASON_BYTES} bytes without control or format characters"
                )));
            }
            Ok(reason.to_owned())
        })
        .transpose()
}

struct SnapshotWorkSections {
    items: Vec<WorkGraphSnapshotItem>,
    blockers: Vec<WorkGraphSnapshotBlocker>,
    sources: Vec<WorkGraphSnapshotSource>,
    records: Vec<WorkGraphSnapshotRecord>,
}

fn work_sections_on(
    connection: &Connection,
    project_id: &ProjectId,
) -> Result<SnapshotWorkSections, StoreError> {
    let item_ids = snapshot_work_ids_on(connection, project_id)?;
    let mut items = Vec::with_capacity(item_ids.len());
    let mut blockers = Vec::new();
    let mut sources = BTreeMap::<ObjectHash, WorkGraphSnapshotSource>::new();
    let mut records = Vec::with_capacity(item_ids.len());
    for work_id in item_ids {
        let item = super::work::load_work_item(connection, work_id)?;
        if &item.project_id != project_id {
            return Err(StoreError::InvalidWorkProjection(format!(
                "snapshot item {:?} crosses its project binding",
                item.work_id
            )));
        }
        let restored_records = restored_records_on(connection, work_id)?;
        let events = super::work::canonical_work_events_for_item(connection, work_id)?;
        let prerequisites = super::work::load_prerequisite_projection_ids(connection, work_id)?;
        let snapshot = snapshot_item(&item, prerequisites, &events, &restored_records)?;
        items.push(snapshot.clone());
        blockers.extend(
            super::work::load_active_blocker_projections(connection, work_id)?
                .into_iter()
                .map(|blocker| WorkGraphSnapshotBlocker {
                    work_id,
                    blocker_id: blocker.blocker_id,
                    kind: blocker.kind,
                    detail: blocker.detail,
                    created_by: blocker.created_by,
                    created_at: blocker.created_at,
                }),
        );
        if let Some(hash) = item.source_snapshot_id.as_ref()
            && !sources.contains_key(hash)
        {
            sources.insert(hash.clone(), load_source_on(connection, hash)?);
        }
        for (hash, canonical_json, record) in &restored_records {
            records.push(WorkGraphSnapshotRecord {
                work_id,
                generation_index: record.generation_index,
                payload: WorkGraphSnapshotRecordPayload::Restored {
                    object_hash: hash.clone(),
                    canonical_json: canonical_json.clone(),
                },
            });
        }
        let notes = notes_for_item_on(connection, work_id)?;
        if !events.is_empty() || !notes.is_empty() {
            records.push(WorkGraphSnapshotRecord {
                work_id,
                generation_index: restored_records.len(),
                payload: WorkGraphSnapshotRecordPayload::Native {
                    history: Box::new(WorkGraphSnapshotHistory {
                        notes,
                        events: snapshot_events_on(connection, &events)?,
                        completion: completion_from_events(
                            connection,
                            &item,
                            &events,
                            &restored_records,
                        )?,
                    }),
                },
            });
        }
    }
    Ok(SnapshotWorkSections {
        items,
        blockers,
        sources: sources.into_values().collect(),
        records,
    })
}

fn snapshot_work_ids_on(
    connection: &Connection,
    project_id: &ProjectId,
) -> Result<Vec<WorkId>, StoreError> {
    connection
        .prepare(
            "SELECT work_id FROM work_items
             WHERE project_id = ?1 ORDER BY short_ref",
        )?
        .query_map([project_id.0.as_str()], |row| row.get::<_, String>(0))?
        .map(|row| parse_snapshot_work_id(&row?))
        .collect()
}

fn snapshot_item(
    item: &crate::WorkItem,
    prerequisites: Vec<WorkId>,
    events: &[crate::WorkEvent],
    restored_records: &[(ObjectHash, Value, RestoredRecord)],
) -> Result<WorkGraphSnapshotItem, StoreError> {
    Ok(WorkGraphSnapshotItem {
        work_id: item.work_id,
        short_ref: item.short_ref.clone(),
        root_id: item.root_id,
        parent_id: item.parent_id,
        child_requirement: item.child_requirement,
        title: item.title.clone(),
        outcome: item.outcome.clone(),
        acceptance: item.acceptance.clone(),
        kind: item.kind,
        priority: item.priority,
        labels: item.labels.clone(),
        origin: item.origin,
        source_snapshot_id: item.source_snapshot_id.clone(),
        lifecycle: item.lifecycle,
        prerequisites,
        superseded_by: item.superseded_by,
        assigned_to: item.assigned_to.clone(),
        deferred_until: item.deferred_until,
        disposal_reason: current_disposal_reason(item.lifecycle, events, restored_records)?,
    })
}

fn current_disposal_reason(
    lifecycle: WorkLifecycle,
    events: &[crate::WorkEvent],
    restored_records: &[(ObjectHash, Value, RestoredRecord)],
) -> Result<Option<String>, StoreError> {
    if !matches!(
        lifecycle,
        WorkLifecycle::Cancelled | WorkLifecycle::Superseded
    ) {
        return Ok(None);
    }
    let native_reason = events
        .iter()
        .rev()
        .find_map(|event| match &event.transition {
            WorkTransition::Disposed {
                lifecycle: disposed,
                reason,
                ..
            } if *disposed == lifecycle => Some(reason.clone()),
            _ => None,
        });
    let restored_reason = restored_records.iter().rev().find_map(|(_, _, record)| {
        record.history.events.iter().rev().find_map(|event| {
            (event.kind == "disposed" && event.lifecycle == Some(lifecycle))
                .then(|| event.reason.clone())
                .flatten()
        })
    });
    native_reason.or(restored_reason).map(Some).ok_or_else(|| {
        StoreError::InvalidWorkProjection(
            "terminal snapshot item has no matching disposal event".into(),
        )
    })
}

fn restored_records_on(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Vec<(ObjectHash, Value, RestoredRecord)>, StoreError> {
    let rows = connection
        .prepare(
            "SELECT generation_index, record_hash FROM work_restored_records
             WHERE work_id = ?1 ORDER BY generation_index",
        )?
        .query_map([work_id.0.to_string()], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut records = Vec::with_capacity(rows.len());
    for (expected, (generation_index, stored_hash)) in rows.into_iter().enumerate() {
        if i64::try_from(expected).ok() != Some(generation_index) {
            return Err(StoreError::InvalidWorkProjection(format!(
                "restored history for {work_id:?} is not dense"
            )));
        }
        let hash = ObjectHash::from_stored(stored_hash.clone())
            .ok_or(StoreError::InvalidStoredHash(stored_hash))?;
        let canonical_json = load_verified_value_on(connection, &hash, "work_restored_record")?;
        let record: RestoredRecord = serde_json::from_value(canonical_json.clone())?;
        if record.work_id != work_id || record.generation_index != expected {
            return Err(StoreError::InvalidWorkProjection(format!(
                "restored history for {work_id:?} differs from its projection binding"
            )));
        }
        records.push((hash, canonical_json, record));
    }
    Ok(records)
}

fn load_source_on(
    connection: &Connection,
    hash: &ObjectHash,
) -> Result<WorkGraphSnapshotSource, StoreError> {
    let value = load_verified_value_on(connection, hash, "work_source_snapshot")?;
    let _: WorkSourceSnapshot = serde_json::from_value(value.clone())?;
    Ok(WorkGraphSnapshotSource {
        hash: hash.clone(),
        canonical_json: value,
    })
}

fn load_verified_value_on(
    connection: &Connection,
    hash: &ObjectHash,
    required_kind: &str,
) -> Result<Value, StoreError> {
    let stored = connection
        .query_row(
            "SELECT object_kind, canonical_json FROM objects WHERE object_hash = ?1",
            [hash.as_str()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()?
        .ok_or_else(|| StoreError::InvalidWorkProjection(format!("object {hash} is missing")))?;
    if stored.0 != required_kind {
        return Err(StoreError::ObjectKindMismatch {
            hash: hash.clone(),
            stored: stored.0,
            requested: required_kind.into(),
        });
    }
    CanonicalObject::verify(hash, stored.1)?.decode()
}

fn notes_for_item_on(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Vec<WorkGraphSnapshotNote>, StoreError> {
    let rows = connection
        .prepare(
            "SELECT evidence.evidence_kind, object.object_hash,
                    object.object_kind, object.canonical_json
             FROM work_run_evidence AS evidence INDEXED BY work_run_evidence_work
             JOIN objects AS object ON object.object_hash = evidence.evidence_hash
             WHERE evidence.work_id = ?1",
        )?
        .query_map([work_id.0.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut notes = Vec::with_capacity(rows.len());
    for (hash, observation) in super::work::observations_on(connection, work_id, usize::MAX)? {
        let note = WorkGraphSnapshotNote {
            evidence_kind: WorkEvidenceKind::Generic,
            summary: observation.summary,
            refs: observation.refs,
            gate: None,
            actor: observation.actor,
            recorded_at: observation.created_at,
        };
        notes.push((note.recorded_at, hash, note));
    }
    for row in rows {
        let (note, hash) = snapshot_note_from_row(work_id, row)?;
        notes.push((note.recorded_at, hash, note));
    }
    let restored_rows = connection
        .prepare(
            "SELECT evidence.evidence_hash, evidence.record_hash,
                    evidence.gate_name, evidence.created_at_ms,
                    object.object_kind, object.canonical_json
             FROM work_restored_evidence AS evidence
                  INDEXED BY work_restored_evidence_work
             JOIN objects AS object ON object.object_hash = evidence.evidence_hash
             WHERE evidence.work_id = ?1",
        )?
        .query_map([work_id.0.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Vec<u8>>(5)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (stored_hash, record_hash, gate_name, created_at_ms, object_kind, bytes) in restored_rows {
        let hash = ObjectHash::from_stored(stored_hash.clone())
            .ok_or(StoreError::InvalidStoredHash(stored_hash))?;
        if object_kind != "work_restored_evidence" {
            return Err(StoreError::ObjectKindMismatch {
                hash,
                stored: object_kind,
                requested: "work_restored_evidence".into(),
            });
        }
        let evidence: RestoredWorkEvidence = CanonicalObject::verify(&hash, bytes)?.decode()?;
        let projection_matches = evidence.work_id == work_id
            && evidence.restored_record.as_str() == record_hash
            && evidence.gate.as_ref().map(|gate| gate.name.as_str()) == gate_name.as_deref()
            && evidence.created_at.timestamp_millis() == created_at_ms
            && connection.query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM work_restored_records
                     WHERE work_id = ?1 AND record_hash = ?2
                 )",
                rusqlite::params![work_id.0.to_string(), evidence.restored_record.as_str()],
                |row| row.get::<_, bool>(0),
            )?;
        if !projection_matches {
            return Err(StoreError::InvalidWorkProjection(format!(
                "restored work evidence {hash} differs from its projection binding"
            )));
        }
        let gate = evidence.gate.map(|gate| WorkGraphSnapshotGate {
            name: gate.name,
            passed: gate.passed,
            failed: gate.failed,
            evidence_ref: evidence.refs.first().cloned(),
        });
        let note = WorkGraphSnapshotNote {
            evidence_kind: WorkEvidenceKind::Generic,
            summary: evidence.summary,
            refs: evidence.refs,
            gate,
            actor: evidence.actor,
            recorded_at: evidence.created_at,
        };
        notes.push((note.recorded_at, hash, note));
    }
    notes.sort_by(|left, right| (left.0, &left.1).cmp(&(right.0, &right.1)));
    Ok(notes.into_iter().map(|(_, _, note)| note).collect())
}

fn snapshot_note_from_row(
    work_id: WorkId,
    row: (String, String, String, Vec<u8>),
) -> Result<(WorkGraphSnapshotNote, ObjectHash), StoreError> {
    let (stored_kind, stored_hash, object_kind, bytes) = row;
    let hash = ObjectHash::from_stored(stored_hash.clone())
        .ok_or(StoreError::InvalidStoredHash(stored_hash))?;
    let object = CanonicalObject::verify(&hash, bytes)?;
    let (note, expected_kind) = match object_kind.as_str() {
        "work_evidence" => {
            let evidence: WorkEvidence = object.decode()?;
            if evidence.work_id != work_id {
                return Err(StoreError::InvalidWorkProjection(
                    "snapshot note crosses its work binding".into(),
                ));
            }
            let gate = evidence.gate.as_ref().map(|gate| WorkGraphSnapshotGate {
                name: gate.name.clone(),
                passed: gate.passed,
                failed: gate.failed.clone(),
                evidence_ref: evidence.refs.first().cloned(),
            });
            (
                WorkGraphSnapshotNote {
                    evidence_kind: WorkEvidenceKind::Generic,
                    summary: evidence.summary,
                    refs: evidence.refs,
                    gate,
                    actor: evidence.actor,
                    recorded_at: evidence.created_at,
                },
                "generic",
            )
        }
        "verification_evidence" => {
            let evidence: VerificationEvidence = object.decode()?;
            if evidence.binding.work_id != work_id {
                return Err(StoreError::InvalidWorkProjection(
                    "snapshot verification crosses its work binding".into(),
                ));
            }
            (
                WorkGraphSnapshotNote {
                    evidence_kind: WorkEvidenceKind::Verification,
                    summary: evidence.summary,
                    refs: evidence.refs,
                    gate: None,
                    actor: evidence.actor,
                    recorded_at: evidence.recorded_at,
                },
                "verification",
            )
        }
        "environment_evidence" => {
            let evidence: EnvironmentEvidence = object.decode()?;
            if evidence.binding.work_id != work_id {
                return Err(StoreError::InvalidWorkProjection(
                    "snapshot environment evidence crosses its work binding".into(),
                ));
            }
            (
                WorkGraphSnapshotNote {
                    evidence_kind: WorkEvidenceKind::Environment,
                    summary: "host-recorded environment identity".into(),
                    refs: Vec::new(),
                    gate: None,
                    actor: evidence.actor,
                    recorded_at: evidence.recorded_at,
                },
                "environment",
            )
        }
        _ => {
            return Err(StoreError::InvalidWorkProjection(format!(
                "run evidence {hash} has unsupported kind {object_kind:?}"
            )));
        }
    };
    if stored_kind != expected_kind {
        return Err(StoreError::InvalidWorkProjection(format!(
            "run evidence {hash} disagrees with its typed kind"
        )));
    }
    Ok((note, hash))
}

fn completion_from_events(
    connection: &Connection,
    item: &crate::WorkItem,
    events: &[crate::WorkEvent],
    restored_records: &[(ObjectHash, Value, RestoredRecord)],
) -> Result<Option<WorkGraphSnapshotCompletion>, StoreError> {
    if item.lifecycle != WorkLifecycle::Completed {
        return Ok(None);
    }
    let event = events
        .iter()
        .rev()
        .find(|event| matches!(event.transition, WorkTransition::Completed { .. }));
    let Some(event) = event else {
        if item.restored {
            return restored_records
                .iter()
                .rev()
                .find_map(|(_, _, record)| record.history.completion.clone())
                .map(Some)
                .ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "completed restored snapshot item has no completion proof".into(),
                    )
                });
        }
        return Err(StoreError::InvalidWorkProjection(
            "completed snapshot item has no completion event".into(),
        ));
    };
    let WorkTransition::Completed { seal } = &event.transition else {
        unreachable!("completion event was selected by its transition");
    };
    let seal: CompletionSeal =
        super::work::load_typed_work_object(connection, seal, "completion_seal")?;
    if seal.work_id != item.work_id || seal.completed_at != event.created_at {
        return Err(StoreError::InvalidWorkProjection(
            "snapshot completion event disagrees with its seal".into(),
        ));
    }
    let summary = seal.checkpoint.as_ref().map_or_else(
        || Ok("completed".into()),
        |checkpoint| {
            let checkpoint: WorkCheckpoint =
                super::work::load_typed_work_object(connection, checkpoint, "work_checkpoint")?;
            if checkpoint.work_id != item.work_id {
                return Err(StoreError::InvalidWorkProjection(
                    "snapshot completion checkpoint crosses its work binding".into(),
                ));
            }
            Ok(checkpoint.summary)
        },
    )?;
    Ok(Some(WorkGraphSnapshotCompletion {
        summary,
        completed_at: seal.completed_at,
        actor: event.actor.clone(),
    }))
}

fn snapshot_events_on(
    connection: &Connection,
    events: &[crate::WorkEvent],
) -> Result<Vec<WorkGraphSnapshotEvent>, StoreError> {
    events
        .iter()
        .map(|event| snapshot_event_on(connection, event))
        .collect()
}

#[allow(
    clippy::too_many_lines,
    reason = "the exhaustive transition-to-inert-history mapping stays together for auditability"
)]
fn snapshot_event_on(
    connection: &Connection,
    event: &crate::WorkEvent,
) -> Result<WorkGraphSnapshotEvent, StoreError> {
    let (kind, reason, lifecycle, related_work_id, related_work_revision) = match &event.transition
    {
        WorkTransition::Created { .. } => ("created", None, None, None, None),
        WorkTransition::Decomposed { .. } => ("decomposed", None, None, None, None),
        WorkTransition::Revised { .. } => ("revised", None, None, None, None),
        WorkTransition::PrerequisiteAdded {
            prerequisite_id, ..
        } => (
            "prerequisite_added",
            None,
            None,
            Some(*prerequisite_id),
            None,
        ),
        WorkTransition::PrerequisiteRemoved {
            prerequisite_id, ..
        } => (
            "prerequisite_removed",
            None,
            None,
            Some(*prerequisite_id),
            None,
        ),
        WorkTransition::Blocked { .. } => (
            "blocked",
            event.blocker.as_ref().map(|blocker| blocker.detail.clone()),
            None,
            None,
            None,
        ),
        WorkTransition::Unblocked { blocker_id } => {
            ("unblocked", Some(blocker_id.clone()), None, None, None)
        }
        WorkTransition::Claimed { recovered, .. } => (
            "claimed",
            recovered.then(|| "recovered lapsed claim".into()),
            None,
            None,
            None,
        ),
        // The compact snapshot vocabulary records claim activity without live
        // claim authority. Keep renewal detail in its existing reason field.
        WorkTransition::ClaimRenewed { .. } => (
            "claimed",
            Some("renewed existing claim".into()),
            None,
            None,
            None,
        ),
        WorkTransition::Released { reason, .. } => {
            ("released", Some(reason.clone()), None, None, None)
        }
        WorkTransition::Checkpointed { checkpoint } => (
            "checkpointed",
            Some(checkpoint_summary_on(
                connection,
                event.work_id,
                checkpoint,
            )?),
            None,
            None,
            None,
        ),
        WorkTransition::HandoffOffered { checkpoint, .. } => (
            "handoff_offered",
            Some(checkpoint_summary_on(
                connection,
                event.work_id,
                checkpoint,
            )?),
            None,
            None,
            None,
        ),
        WorkTransition::HandoffExpired { .. } => ("handoff_expired", None, None, None, None),
        WorkTransition::HandoffCancelled { reason, .. } => {
            ("handoff_cancelled", Some(reason.clone()), None, None, None)
        }
        WorkTransition::HandedOff { checkpoint, .. } => (
            "handed_off",
            Some(checkpoint_summary_on(
                connection,
                event.work_id,
                checkpoint,
            )?),
            None,
            None,
            None,
        ),
        WorkTransition::EvidenceAdded { .. } => ("evidence_added", None, None, None, None),
        WorkTransition::MemoryCaptured { .. } => ("memory_captured", None, None, None, None),
        WorkTransition::TypedEvidenceAdded { .. } => {
            ("typed_evidence_added", None, None, None, None)
        }
        WorkTransition::Completed { .. } => ("completed", None, None, None, None),
        WorkTransition::Disposed {
            lifecycle,
            replacement_id,
            reason,
        } => (
            "disposed",
            Some(reason.clone()),
            Some(*lifecycle),
            *replacement_id,
            None,
        ),
        WorkTransition::RequiredChildWaived {
            child_id,
            child_revision,
            reason,
        } => (
            "required_child_waived",
            Some(reason.clone()),
            None,
            Some(*child_id),
            Some(*child_revision),
        ),
        WorkTransition::Reopened { reason, .. } => {
            ("reopened", Some(reason.clone()), None, None, None)
        }
    };
    Ok(WorkGraphSnapshotEvent {
        kind: kind.into(),
        work_revision: event.revision,
        occurred_at: event.created_at,
        reason,
        lifecycle,
        related_work_id,
        related_work_revision,
        actor: event.actor.clone(),
    })
}

fn checkpoint_summary_on(
    connection: &Connection,
    work_id: WorkId,
    checkpoint: &ObjectHash,
) -> Result<String, StoreError> {
    let checkpoint: WorkCheckpoint =
        super::work::load_typed_work_object(connection, checkpoint, "work_checkpoint")?;
    if checkpoint.work_id != work_id {
        return Err(StoreError::InvalidWorkProjection(
            "snapshot checkpoint crosses its work binding".into(),
        ));
    }
    Ok(checkpoint.summary)
}

fn snapshot_memory_projection_rows_on(
    connection: &Connection,
    project_id: &ProjectId,
) -> Result<Vec<(String, MemoryHeadProjectionRow)>, StoreError> {
    Ok(connection
        .prepare(
            "SELECT json_extract(object.canonical_json, '$.project_key'),
                    head.memory_id, head.version_hash, head.assertion_hash,
                    head.schema_version, head.status, head.scope_kind,
                    head.project_id, head.task_id, head.work_id, head.agent_id,
                    head.memory_kind, head.authority, head.delivery,
                    head.sensitivity, head.title, head.body, head.created_at_ms
             FROM memory_heads AS head
             JOIN objects AS object ON object.object_hash = head.version_hash
             WHERE head.scope_kind = 'project' AND head.project_id = ?1
               AND object.object_kind = 'memory_version'
               AND json_type(object.canonical_json, '$.project_key') = 'text'
             ORDER BY json_extract(object.canonical_json, '$.project_key')",
        )?
        .query_map([project_id.0.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                MemoryHeadProjectionRow {
                    memory_id: row.get(1)?,
                    version_hash: row.get(2)?,
                    assertion_hash: row.get(3)?,
                    schema_version: row.get(4)?,
                    status: row.get(5)?,
                    scope_kind: row.get(6)?,
                    project_id: row.get(7)?,
                    task_id: row.get(8)?,
                    work_id: row.get(9)?,
                    agent_id: row.get(10)?,
                    memory_kind: row.get(11)?,
                    authority: row.get(12)?,
                    delivery: row.get(13)?,
                    sensitivity: row.get(14)?,
                    title: row.get(15)?,
                    body: row.get(16)?,
                    created_at_ms: row.get(17)?,
                },
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?)
}

fn memories_on(
    connection: &Connection,
    project_id: &ProjectId,
    widened: bool,
    expected_active_count: usize,
) -> Result<(Vec<WorkGraphSnapshotMemory>, usize), StoreError> {
    let rows = snapshot_memory_projection_rows_on(connection, project_id)?;
    let mut memories = Vec::with_capacity(rows.len());
    let mut redacted = 0;
    let mut active_count = 0;
    for (key, projected) in rows {
        let key = validate_stored_project_memory_key(&key)?;
        let version_hash = ObjectHash::from_stored(projected.version_hash.clone())
            .ok_or_else(|| StoreError::InvalidStoredHash(projected.version_hash.clone()))?;
        let assertion_hash = ObjectHash::from_stored(projected.assertion_hash.clone())
            .ok_or_else(|| StoreError::InvalidStoredHash(projected.assertion_hash.clone()))?;
        let version: MemoryVersion =
            SqliteStore::get_typed_object_on(connection, &version_hash, "memory_version")?
                .ok_or_else(|| {
                    StoreError::InvalidMemoryProjection(
                        "snapshot project memory version is missing".into(),
                    )
                })?;
        let assertion: MemoryAssertionEvent = SqliteStore::get_typed_object_on(
            connection,
            &assertion_hash,
            "memory_assertion_event",
        )?
        .ok_or_else(|| {
            StoreError::InvalidMemoryProjection(
                "snapshot project memory assertion is missing".into(),
            )
        })?;
        validate_keyed_project_memory_shape(&version, &assertion)?;
        let projected_status = SqliteStore::expected_memory_head_status_on(
            connection,
            &version_hash,
            assertion.status,
        )?;
        let expected = SqliteStore::expected_memory_head_projection_from_canonical(
            &version_hash,
            &assertion_hash,
            &version,
            &assertion,
            projected_status,
        )?;
        if projected != expected
            || version.project_key.as_deref() != Some(&key)
            || !matches!(&version.scope, Scope::Project { project } if project == project_id)
        {
            return Err(StoreError::InvalidMemoryProjection(
                "snapshot project memory projection does not match its canonical objects".into(),
            ));
        }
        let state = match projected_status {
            MemoryStatus::Active => {
                active_count += 1;
                let (state, was_redacted) = snapshot_active_memory(version, widened);
                redacted += usize::from(was_redacted);
                state
            }
            MemoryStatus::Tombstoned => WorkGraphSnapshotMemoryState::Tombstone {
                retired_at: assertion.created_at,
                actor: assertion.actor,
            },
            _ => {
                return Err(StoreError::InvalidMemoryProjection(
                    "snapshot project memory has an unsupported lifecycle".into(),
                ));
            }
        };
        memories.push(WorkGraphSnapshotMemory { key, state });
    }
    if active_count != expected_active_count {
        return Err(StoreError::InvalidMemoryProjection(
            "snapshot project-memory count disagrees with its state head".into(),
        ));
    }
    Ok((memories, redacted))
}

fn snapshot_active_memory(
    version: crate::MemoryVersion,
    widened: bool,
) -> (WorkGraphSnapshotMemoryState, bool) {
    let restored_redaction = version
        .source_snapshot
        .as_ref()
        .is_some_and(|source| source.source_ref == RESTORED_REDACTED_MEMORY_SOURCE);
    let was_redacted =
        version.sensitivity == Sensitivity::Restricted && (!widened || restored_redaction);
    let body = if was_redacted {
        WorkGraphSnapshotText::Redacted {
            sensitivity: version.sensitivity,
        }
    } else {
        WorkGraphSnapshotText::Present {
            value: version.body,
        }
    };
    (
        WorkGraphSnapshotMemoryState::Active {
            body,
            sensitivity: version.sensitivity,
            remembered_at: version.created_at,
            actor: version.actor,
        },
        was_redacted,
    )
}

fn parse_snapshot_work_id(value: &str) -> Result<WorkId, StoreError> {
    uuid::Uuid::parse_str(value).map(WorkId).map_err(|error| {
        StoreError::InvalidWorkProjection(format!("invalid snapshot work id {value:?}: {error}"))
    })
}

fn inspect_serialized_strings<R: Redactor>(redactor: &R, value: &Value) -> Result<(), StoreError> {
    match value {
        Value::String(value) => redactor
            .inspect(value)
            .map_err(StoreError::RedactionRefused),
        Value::Array(values) => {
            for value in values {
                inspect_serialized_strings(redactor, value)?;
            }
            Ok(())
        }
        Value::Object(values) => {
            for (key, value) in values {
                redactor
                    .inspect(key)
                    .map_err(StoreError::RedactionRefused)?;
                inspect_serialized_strings(redactor, value)?;
            }
            Ok(())
        }
        Value::Null | Value::Bool(_) | Value::Number(_) => Ok(()),
    }
}

#[cfg(test)]
mod tests;
