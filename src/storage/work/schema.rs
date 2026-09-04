use rusqlite::{Connection, OptionalExtension, TransactionBehavior};

use super::super::{SchemaDurability, SchemaOwner, StoreError};
use super::CURRENT_WORK_SCHEMA_VERSION;
use super::integrity::verify_work_catalog_projections;

#[cfg(test)]
mod tests;

const REBUILDABLE_WORK_SCHEMA_OBJECTS: &[&str] = &[
    "work_catalog_fts",
    "objects_work_evidence_gate_name",
    "objects_work_event_work_id",
    "work_feed_entries_work_event_item",
    "work_feed_entries_environment_cut",
    "work_blockers_active",
    "work_claims_holder_live",
    "work_claims_live",
    "work_handoff_offer_active",
    "work_handoff_offer_from_live",
    "work_handoff_offer_to_live",
    "work_items_parent",
    "work_items_assigned",
    "work_items_catalog_after",
    "work_items_ready",
    "work_items_root",
    "work_item_labels_lookup",
    "work_prerequisites_reverse",
    "work_root_execution_active",
    "work_run_active",
    "work_run_evidence_run",
    "work_run_evidence_work",
    "work_run_obligations_run",
    "work_session_state_retention",
    "work_feed_entries_require_work_id",
];

pub(in crate::storage) fn owns_schema_object(name: &str) -> bool {
    name.starts_with("work_") || name.starts_with("objects_work_")
}

pub(in crate::storage) fn is_rebuildable_schema_object(name: &str) -> bool {
    name == "work_catalog_fts"
        || name.starts_with("work_catalog_fts_")
        || REBUILDABLE_WORK_SCHEMA_OBJECTS.contains(&name)
}

pub(in crate::storage) fn preflight_schema(
    connection: &Connection,
    allow_initialization: bool,
) -> Result<(), StoreError> {
    let metadata_exists = connection.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM sqlite_master
             WHERE type = 'table' AND name = 'work_schema_metadata'
         )",
        [],
        |row| row.get::<_, bool>(0),
    )?;
    if !metadata_exists {
        let existing_work_tables = connection.query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name GLOB 'work_*'",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        if existing_work_tables == 0 && allow_initialization {
            return Ok(());
        }
        if existing_work_tables == 0 {
            return Err(super::super::different_build_store_error());
        }
        return Err(StoreError::InvalidWorkProjection(
            "local-work tables exist without schema metadata".into(),
        ));
    }
    if current_work_durable_schema_issue(connection)?.is_some() {
        return Err(super::super::different_build_store_error());
    }
    let version = connection.query_row(
        "SELECT schema_version FROM work_schema_metadata WHERE singleton = 1",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    super::super::require_current_schema_marker(version, CURRENT_WORK_SCHEMA_VERSION)
}

pub(super) fn current_work_durable_schema_issue(
    connection: &Connection,
) -> Result<Option<String>, StoreError> {
    super::super::current_schema_definition_issue(
        connection,
        SchemaOwner::Work,
        SchemaDurability::Durable,
    )
}

fn current_work_rebuildable_schema_issue(
    connection: &Connection,
) -> Result<Option<String>, StoreError> {
    super::super::current_schema_definition_issue(
        connection,
        SchemaOwner::Work,
        SchemaDurability::Rebuildable,
    )
}

pub(in crate::storage) fn schema_version(connection: &Connection) -> Result<i64, StoreError> {
    connection
        .query_row(
            "SELECT schema_version FROM work_schema_metadata WHERE singleton = 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .ok_or_else(|| {
            StoreError::InvalidWorkProjection(
                "local-work schema metadata has no singleton row".into(),
            )
        })
}

pub(in crate::storage) fn require_work_schema_version(
    connection: &Connection,
    expected_version: i64,
) -> Result<(), StoreError> {
    let version = schema_version(connection)?;
    super::super::require_current_schema_marker(version, expected_version)
}

pub(in crate::storage) fn initialize_schema(
    connection: &mut Connection,
    allow_initialization: bool,
) -> Result<(), StoreError> {
    preflight_schema(connection, allow_initialization)?;
    if current_work_schema_is_complete(connection)? {
        return Ok(());
    }
    let metadata_exists = connection.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM sqlite_master
             WHERE type = 'table' AND name = 'work_schema_metadata'
         )",
        [],
        |row| row.get::<_, bool>(0),
    )?;
    if metadata_exists {
        return Err(StoreError::InvalidWorkProjection(
            "current local-work schema is missing rebuildable projections; run `engram doctor --repair-projections` explicitly"
                .into(),
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    preflight_schema(&transaction, allow_initialization)?;
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS work_schema_metadata (
             singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
             schema_version INTEGER NOT NULL CHECK(schema_version > 0)
         ) STRICT;
         CREATE TABLE IF NOT EXISTS work_items (
             work_id TEXT PRIMARY KEY,
             project_id TEXT NOT NULL,
             short_ref TEXT NOT NULL,
             root_id TEXT NOT NULL REFERENCES work_items(work_id),
             parent_id TEXT REFERENCES work_items(work_id),
             child_requirement TEXT NOT NULL,
             lifecycle TEXT NOT NULL,
             priority INTEGER NOT NULL,
             assigned_to TEXT,
             assigned_to_key TEXT,
             search_text_key TEXT NOT NULL DEFAULT '',
             deferred_until_ms INTEGER,
             revision INTEGER NOT NULL,
             active_run_id TEXT,
             superseded_by TEXT REFERENCES work_items(work_id),
             source_snapshot_hash TEXT REFERENCES objects(object_hash),
             latest_event_hash TEXT REFERENCES objects(object_hash),
             created_at_ms INTEGER NOT NULL,
             updated_at_ms INTEGER NOT NULL,
             item_json BLOB NOT NULL,
             UNIQUE(project_id, short_ref),
             CHECK(priority BETWEEN 0 AND 4),
             CHECK(revision > 0)
         ) STRICT;
         CREATE INDEX IF NOT EXISTS work_items_ready
             ON work_items(project_id, lifecycle, priority, deferred_until_ms, created_at_ms);
         CREATE INDEX IF NOT EXISTS work_items_parent
             ON work_items(parent_id, lifecycle);
         CREATE INDEX IF NOT EXISTS work_items_root
             ON work_items(project_id, root_id, work_id);
         CREATE TABLE IF NOT EXISTS work_item_labels (
             work_id TEXT NOT NULL REFERENCES work_items(work_id) ON DELETE CASCADE,
             label_key TEXT NOT NULL,
             PRIMARY KEY(work_id, label_key)
         ) STRICT;
         CREATE INDEX IF NOT EXISTS work_item_labels_lookup
             ON work_item_labels(label_key, work_id);
         CREATE VIRTUAL TABLE IF NOT EXISTS work_catalog_fts USING fts5(
             work_id UNINDEXED,
             search_text,
             tokenize='trigram'
         );
         CREATE TABLE IF NOT EXISTS work_root_executions (
             root_execution_id TEXT PRIMARY KEY,
             project_id TEXT NOT NULL,
             root_id TEXT NOT NULL REFERENCES work_items(work_id),
             generation INTEGER NOT NULL,
             state TEXT NOT NULL,
             revision INTEGER NOT NULL,
             created_at_ms INTEGER NOT NULL,
             updated_at_ms INTEGER NOT NULL,
             execution_json BLOB NOT NULL,
             UNIQUE(root_id, generation)
         ) STRICT;
         CREATE UNIQUE INDEX IF NOT EXISTS work_root_execution_active
             ON work_root_executions(root_id) WHERE state = 'active';
         CREATE TABLE IF NOT EXISTS work_runs (
             run_id TEXT PRIMARY KEY,
             root_execution_id TEXT NOT NULL REFERENCES work_root_executions(root_execution_id),
             work_id TEXT NOT NULL REFERENCES work_items(work_id),
             generation INTEGER NOT NULL,
             executor_session_id TEXT,
             state TEXT NOT NULL,
             revision INTEGER NOT NULL,
             claim_fence_head INTEGER NOT NULL DEFAULT 0,
             last_checkpoint_hash TEXT REFERENCES objects(object_hash),
             completion_seal_hash TEXT REFERENCES objects(object_hash),
             created_at_ms INTEGER NOT NULL,
             updated_at_ms INTEGER NOT NULL,
             run_json BLOB NOT NULL,
             UNIQUE(work_id, generation),
             CHECK(revision > 0),
             CHECK(claim_fence_head >= 0)
         ) STRICT;
         CREATE UNIQUE INDEX IF NOT EXISTS work_run_active
             ON work_runs(work_id) WHERE state != 'completed' AND state != 'cancelled';
         CREATE TABLE IF NOT EXISTS work_claims (
             run_id TEXT PRIMARY KEY REFERENCES work_runs(run_id),
             work_id TEXT NOT NULL REFERENCES work_items(work_id),
             claim_id TEXT NOT NULL UNIQUE,
             holder_session_id TEXT NOT NULL,
             state TEXT NOT NULL,
             expires_at_ms INTEGER NOT NULL,
             revision INTEGER NOT NULL,
             fence INTEGER NOT NULL,
             claim_json BLOB NOT NULL,
             CHECK(revision > 0),
             CHECK(fence > 0)
         ) STRICT;
         CREATE INDEX IF NOT EXISTS work_claims_live
             ON work_claims(work_id, state, expires_at_ms);
         CREATE INDEX IF NOT EXISTS work_claims_holder_live
             ON work_claims(holder_session_id, state, expires_at_ms, work_id);
         CREATE TABLE IF NOT EXISTS work_handoff_offers (
             offer_id TEXT PRIMARY KEY,
             run_id TEXT NOT NULL REFERENCES work_runs(run_id),
             work_id TEXT NOT NULL REFERENCES work_items(work_id),
             state TEXT NOT NULL,
             expires_at_ms INTEGER NOT NULL,
             offer_hash TEXT REFERENCES objects(object_hash),
             offer_json BLOB NOT NULL
         ) STRICT;
         CREATE UNIQUE INDEX IF NOT EXISTS work_handoff_offer_active
             ON work_handoff_offers(run_id) WHERE state = 'offered';
         CREATE INDEX IF NOT EXISTS work_handoff_offer_from_live
             ON work_handoff_offers(
                 json_extract(offer_json, '$.from'), expires_at_ms, work_id
             ) WHERE state = 'offered';
         CREATE INDEX IF NOT EXISTS work_handoff_offer_to_live
             ON work_handoff_offers(
                 json_extract(offer_json, '$.to'), expires_at_ms, work_id
             ) WHERE state = 'offered';
         CREATE TABLE IF NOT EXISTS work_prerequisites (
             work_id TEXT NOT NULL REFERENCES work_items(work_id),
             prerequisite_id TEXT NOT NULL REFERENCES work_items(work_id),
             event_hash TEXT NOT NULL REFERENCES objects(object_hash),
             PRIMARY KEY(work_id, prerequisite_id),
             CHECK(work_id != prerequisite_id)
         ) STRICT;
         CREATE INDEX IF NOT EXISTS work_prerequisites_reverse
             ON work_prerequisites(prerequisite_id, work_id);
         CREATE TABLE IF NOT EXISTS work_blockers (
             blocker_id TEXT PRIMARY KEY,
             work_id TEXT NOT NULL REFERENCES work_items(work_id),
             state TEXT NOT NULL,
             blocker_json BLOB NOT NULL,
             created_event_hash TEXT NOT NULL REFERENCES objects(object_hash),
             cleared_event_hash TEXT REFERENCES objects(object_hash)
         ) STRICT;
         CREATE INDEX IF NOT EXISTS work_blockers_active
             ON work_blockers(work_id, state);
         CREATE TABLE IF NOT EXISTS work_run_evidence (
             evidence_hash TEXT PRIMARY KEY REFERENCES objects(object_hash),
             work_id TEXT NOT NULL REFERENCES work_items(work_id),
             run_id TEXT NOT NULL REFERENCES work_runs(run_id),
             evidence_kind TEXT NOT NULL DEFAULT 'generic',
             workspace_id TEXT,
             source_revision TEXT,
             producer_session_id TEXT,
             producer_observation_hash TEXT REFERENCES objects(object_hash),
             check_fingerprint TEXT,
             verification_result TEXT,
             observed_at_ms INTEGER,
             environment_fingerprint TEXT,
             environment_evidence_hash TEXT REFERENCES objects(object_hash),
             components_json BLOB
         ) STRICT;
         CREATE INDEX IF NOT EXISTS work_run_evidence_run
             ON work_run_evidence(run_id, evidence_hash);
         CREATE INDEX IF NOT EXISTS work_run_evidence_work
             ON work_run_evidence(work_id, evidence_hash);
         CREATE TABLE IF NOT EXISTS work_run_obligations (
             obligation_id TEXT PRIMARY KEY,
             definition_hash TEXT NOT NULL UNIQUE REFERENCES objects(object_hash),
             project_id TEXT NOT NULL,
             root_execution_id TEXT NOT NULL REFERENCES work_root_executions(root_execution_id),
             root_id TEXT NOT NULL REFERENCES work_items(work_id),
             work_id TEXT NOT NULL REFERENCES work_items(work_id),
             run_id TEXT NOT NULL REFERENCES work_runs(run_id),
             work_revision INTEGER NOT NULL,
             rule_set_hash TEXT NOT NULL REFERENCES objects(object_hash),
             rule_id TEXT NOT NULL,
             rule_version INTEGER NOT NULL,
             triggering_observation_hash TEXT NOT NULL REFERENCES objects(object_hash),
             trigger_position INTEGER NOT NULL,
             check_kind TEXT NOT NULL,
             check_fingerprint TEXT,
             state TEXT NOT NULL,
             resolution_hash TEXT UNIQUE REFERENCES objects(object_hash),
             resolution_kind TEXT,
             evidence_hash TEXT REFERENCES objects(object_hash),
             opened_at_ms INTEGER NOT NULL,
             resolved_at_ms INTEGER,
             UNIQUE(run_id, rule_id, rule_version, triggering_observation_hash),
             CHECK(work_revision > 0),
             CHECK(rule_version > 0),
             CHECK(trigger_position > 0)
         ) STRICT;
         CREATE INDEX IF NOT EXISTS work_run_obligations_run
             ON work_run_obligations(run_id, state, trigger_position, obligation_id);
         CREATE TABLE IF NOT EXISTS work_completion_seals (
             seal_hash TEXT PRIMARY KEY REFERENCES objects(object_hash),
             work_id TEXT NOT NULL REFERENCES work_items(work_id),
             run_id TEXT NOT NULL UNIQUE REFERENCES work_runs(run_id),
             root_execution_id TEXT NOT NULL REFERENCES work_root_executions(root_execution_id),
             seal_json BLOB NOT NULL
         ) STRICT;
         CREATE TABLE IF NOT EXISTS work_feed_heads (
             feed_kind TEXT NOT NULL,
             feed_id TEXT NOT NULL,
             position INTEGER NOT NULL,
             PRIMARY KEY(feed_kind, feed_id),
             CHECK(position >= 0)
         ) STRICT;
         CREATE TABLE IF NOT EXISTS work_feed_entries (
             feed_kind TEXT NOT NULL,
             feed_id TEXT NOT NULL,
             position INTEGER NOT NULL,
             object_kind TEXT NOT NULL,
             object_hash TEXT NOT NULL REFERENCES objects(object_hash),
             work_id TEXT REFERENCES work_items(work_id),
             PRIMARY KEY(feed_kind, feed_id, position),
             UNIQUE(feed_kind, feed_id, object_hash),
             FOREIGN KEY(feed_kind, feed_id)
                 REFERENCES work_feed_heads(feed_kind, feed_id)
         ) STRICT;
         CREATE INDEX IF NOT EXISTS objects_work_event_work_id
             ON objects(json_extract(canonical_json, '$.work_id'))
             WHERE object_kind = 'work_event';
         CREATE INDEX IF NOT EXISTS objects_work_evidence_gate_name
             ON objects(
                 json_extract(canonical_json, '$.run_id'),
                 json_extract(canonical_json, '$.gate.name')
             )
             WHERE object_kind = 'work_evidence'
               AND json_type(canonical_json, '$.gate') = 'object';
         CREATE TABLE IF NOT EXISTS work_operation_results (
             operation TEXT NOT NULL,
             idempotency_key TEXT NOT NULL,
             request_hash TEXT NOT NULL,
             result_json BLOB NOT NULL,
             PRIMARY KEY(operation, idempotency_key)
         ) STRICT;
         CREATE TABLE IF NOT EXISTS work_session_state (
             project_id TEXT NOT NULL,
             session_id TEXT NOT NULL,
             focused_work_id TEXT REFERENCES work_items(work_id),
             project_cursor INTEGER NOT NULL DEFAULT 0 CHECK(project_cursor >= 0),
             tentative_project_cursor INTEGER CHECK(tentative_project_cursor >= 0),
             tentative_delivery_token TEXT,
             tentative_delivery_payload_hash TEXT,
             tentative_delivery_payload BLOB,
             updated_at_ms INTEGER NOT NULL,
             PRIMARY KEY(project_id, session_id)
         ) STRICT;
         CREATE TABLE IF NOT EXISTS work_protocol_attempts (
             project_id TEXT NOT NULL,
             session_id TEXT NOT NULL,
             operation TEXT NOT NULL,
             idempotency_key TEXT NOT NULL,
             request_hash TEXT NOT NULL,
             basis_hash TEXT,
             basis_json BLOB,
             initiated_at_ms INTEGER NOT NULL,
             result_hash TEXT REFERENCES objects(object_hash),
             result_json BLOB,
             PRIMARY KEY(project_id, session_id, operation, idempotency_key)
         ) STRICT;",
    )?;
    transaction.execute(
        "INSERT INTO work_schema_metadata (singleton, schema_version)
         VALUES (1, ?1) ON CONFLICT(singleton) DO NOTHING",
        [CURRENT_WORK_SCHEMA_VERSION],
    )?;
    transaction.execute_batch(
        "CREATE INDEX IF NOT EXISTS work_feed_entries_work_event_item
             ON work_feed_entries(feed_kind, work_id, position DESC)
             WHERE object_kind = 'work_event' AND work_id IS NOT NULL;
         CREATE INDEX IF NOT EXISTS work_feed_entries_environment_cut
             ON work_feed_entries(feed_id, position, object_hash)
             WHERE feed_kind = 'run_execution'
               AND object_kind = 'environment_evidence';
         CREATE TRIGGER IF NOT EXISTS work_feed_entries_require_work_id
             BEFORE INSERT ON work_feed_entries
             WHEN NEW.object_kind = 'work_event' AND NEW.work_id IS NULL
             BEGIN
                 SELECT RAISE(ABORT, 'work-event feed entry requires work_id');
             END;
         CREATE INDEX IF NOT EXISTS work_items_assigned
             ON work_items(project_id, assigned_to_key, work_id);
         CREATE INDEX IF NOT EXISTS work_items_catalog_after
             ON work_items(project_id, work_id);
         CREATE INDEX IF NOT EXISTS work_session_state_retention
             ON work_session_state(project_id, updated_at_ms, session_id);",
    )?;
    transaction.execute(
        "UPDATE work_schema_metadata SET schema_version = ?1 WHERE singleton = 1",
        [CURRENT_WORK_SCHEMA_VERSION],
    )?;
    transaction.commit()?;
    Ok(())
}

pub(in crate::storage) fn repair_rebuildable_schema_on(
    connection: &Connection,
) -> Result<bool, StoreError> {
    preflight_schema(connection, false)?;
    for object in REBUILDABLE_WORK_SCHEMA_OBJECTS {
        super::super::drop_schema_object(connection, object)?;
    }
    connection.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS work_catalog_fts USING fts5(
             work_id UNINDEXED,
             search_text,
             tokenize='trigram'
         );
         CREATE INDEX IF NOT EXISTS work_items_ready
             ON work_items(project_id, lifecycle, priority, deferred_until_ms, created_at_ms);
         CREATE INDEX IF NOT EXISTS work_items_parent
             ON work_items(parent_id, lifecycle);
         CREATE INDEX IF NOT EXISTS work_items_root
             ON work_items(project_id, root_id, work_id);
         CREATE INDEX IF NOT EXISTS work_item_labels_lookup
             ON work_item_labels(label_key, work_id);
         CREATE UNIQUE INDEX IF NOT EXISTS work_root_execution_active
             ON work_root_executions(root_id) WHERE state = 'active';
         CREATE UNIQUE INDEX IF NOT EXISTS work_run_active
             ON work_runs(work_id) WHERE state != 'completed' AND state != 'cancelled';
         CREATE INDEX IF NOT EXISTS work_claims_live
             ON work_claims(work_id, state, expires_at_ms);
         CREATE INDEX IF NOT EXISTS work_claims_holder_live
             ON work_claims(holder_session_id, state, expires_at_ms, work_id);
         CREATE UNIQUE INDEX IF NOT EXISTS work_handoff_offer_active
             ON work_handoff_offers(run_id) WHERE state = 'offered';
         CREATE INDEX IF NOT EXISTS work_handoff_offer_from_live
             ON work_handoff_offers(
                 json_extract(offer_json, '$.from'), expires_at_ms, work_id
             ) WHERE state = 'offered';
         CREATE INDEX IF NOT EXISTS work_handoff_offer_to_live
             ON work_handoff_offers(
                 json_extract(offer_json, '$.to'), expires_at_ms, work_id
             ) WHERE state = 'offered';
         CREATE INDEX IF NOT EXISTS work_prerequisites_reverse
             ON work_prerequisites(prerequisite_id, work_id);
         CREATE INDEX IF NOT EXISTS work_blockers_active
             ON work_blockers(work_id, state);
         CREATE INDEX IF NOT EXISTS work_run_evidence_run
             ON work_run_evidence(run_id, evidence_hash);
         CREATE INDEX IF NOT EXISTS work_run_evidence_work
             ON work_run_evidence(work_id, evidence_hash);
         CREATE INDEX IF NOT EXISTS work_run_obligations_run
             ON work_run_obligations(run_id, state, trigger_position, obligation_id);
         CREATE INDEX IF NOT EXISTS objects_work_event_work_id
             ON objects(json_extract(canonical_json, '$.work_id'))
             WHERE object_kind = 'work_event';
         CREATE INDEX IF NOT EXISTS objects_work_evidence_gate_name
             ON objects(
                 json_extract(canonical_json, '$.run_id'),
                 json_extract(canonical_json, '$.gate.name')
             )
             WHERE object_kind = 'work_evidence'
               AND json_type(canonical_json, '$.gate') = 'object';
         CREATE INDEX IF NOT EXISTS work_feed_entries_work_event_item
             ON work_feed_entries(feed_kind, work_id, position DESC)
             WHERE object_kind = 'work_event' AND work_id IS NOT NULL;
         CREATE INDEX IF NOT EXISTS work_feed_entries_environment_cut
             ON work_feed_entries(feed_id, position, object_hash)
             WHERE feed_kind = 'run_execution'
               AND object_kind = 'environment_evidence';
         CREATE INDEX IF NOT EXISTS work_session_state_retention
             ON work_session_state(project_id, updated_at_ms, session_id);
         CREATE TRIGGER IF NOT EXISTS work_feed_entries_require_work_id
             BEFORE INSERT ON work_feed_entries
             WHEN NEW.object_kind = 'work_event' AND NEW.work_id IS NULL
             BEGIN
                 SELECT RAISE(ABORT, 'work-event feed entry requires work_id');
             END;
         CREATE INDEX IF NOT EXISTS work_items_assigned
             ON work_items(project_id, assigned_to_key, work_id);
          CREATE INDEX IF NOT EXISTS work_items_catalog_after
             ON work_items(project_id, work_id);",
    )?;
    connection.execute("DELETE FROM work_catalog_fts", [])?;
    connection.execute(
        "INSERT INTO work_catalog_fts (work_id, search_text)
         SELECT work_id, search_text_key FROM work_items ORDER BY work_id",
        [],
    )?;
    let mut checked = 0;
    let mut invalid = Vec::new();
    verify_work_catalog_projections(connection, &mut checked, &mut invalid)?;
    if !invalid.is_empty() {
        return Err(StoreError::InvalidWorkProjection(format!(
            "explicit projection repair did not rebuild work_catalog_fts: {}",
            invalid.join(", ")
        )));
    }
    if current_work_rebuildable_schema_issue(connection)?.is_some() {
        return Err(StoreError::InvalidWorkProjection(
            "explicit local-work projection repair did not restore the current schema".into(),
        ));
    }
    Ok(true)
}

fn current_work_schema_is_complete(connection: &Connection) -> Result<bool, StoreError> {
    let metadata_exists = connection.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM sqlite_master
             WHERE type = 'table' AND name = 'work_schema_metadata'
         )",
        [],
        |row| row.get::<_, bool>(0),
    )?;
    if !metadata_exists {
        return Ok(false);
    }
    let version = connection
        .query_row(
            "SELECT schema_version FROM work_schema_metadata WHERE singleton = 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    if version != Some(CURRENT_WORK_SCHEMA_VERSION) {
        return Ok(false);
    }
    Ok(current_work_durable_schema_issue(connection)?.is_none()
        && current_work_rebuildable_schema_issue(connection)?.is_none())
}
