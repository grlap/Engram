//! Durable provenance for the one explicit aggregate-root format conversion.
//! These tables are not rebuildable, a live compatibility mode, or a second ledger.

pub(super) const TABLES: &[&str] = &[
    "migration_source_manifest",
    "migration_original_objects",
    "migration_object_map",
    "migration_original_rows",
    "migration_reexpressed_results",
    "migration_delivery_attribution",
];

pub(crate) const MIGRATION_SCHEMA: &str = "
CREATE TABLE IF NOT EXISTS migration_delivery_attribution (
    project_id TEXT NOT NULL,
    session_id TEXT NOT NULL,
    payload_hash TEXT NOT NULL,
    audit_json BLOB NOT NULL,
    audit_hash TEXT NOT NULL,
    PRIMARY KEY (project_id, session_id, payload_hash)
) WITHOUT ROWID, STRICT;
CREATE TABLE IF NOT EXISTS migration_source_manifest (
    source_id TEXT PRIMARY KEY,
    profile TEXT NOT NULL,
    document BLOB NOT NULL,
    document_hash TEXT NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS migration_original_objects (
    object_hash TEXT PRIMARY KEY,
    object_kind TEXT NOT NULL,
    canonical_json BLOB NOT NULL,
    created_at TEXT NOT NULL,
    source_rowid INTEGER NOT NULL
) STRICT;
CREATE TABLE IF NOT EXISTS migration_object_map (
    source_hash TEXT PRIMARY KEY REFERENCES migration_original_objects(object_hash),
    target_hash TEXT NOT NULL REFERENCES objects(object_hash),
    binding_hash TEXT NOT NULL REFERENCES objects(object_hash)
) STRICT;
CREATE TABLE IF NOT EXISTS migration_original_rows (
    source_id TEXT NOT NULL REFERENCES migration_source_manifest(source_id),
    table_name TEXT NOT NULL,
    row_number INTEGER NOT NULL,
    cells BLOB NOT NULL,
    PRIMARY KEY (source_id, table_name, row_number)
) WITHOUT ROWID, STRICT;
CREATE TABLE IF NOT EXISTS migration_reexpressed_results (
    project_id TEXT NOT NULL,
    operation TEXT NOT NULL CHECK (operation = 'complete_work'),
    idempotency_key TEXT NOT NULL,
    source_result BLOB NOT NULL,
    source_result_hash TEXT NOT NULL,
    target_result BLOB NOT NULL,
    target_result_hash TEXT NOT NULL,
    source_seal TEXT NOT NULL REFERENCES migration_original_objects(object_hash),
    target_seal TEXT NOT NULL REFERENCES objects(object_hash),
    PRIMARY KEY (project_id, operation, idempotency_key)
) WITHOUT ROWID, STRICT;
";
