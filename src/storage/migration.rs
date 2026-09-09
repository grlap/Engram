//! Full, lossless SQLite export for an explicitly versioned store migration.
//!
//! This reader deliberately does not open `SqliteStore` or decode domain objects.
//! The archive is data, not executable SQL and not an installable Engram store.

mod audit;
mod convert;
mod delivery;
mod export;
mod import;
mod import_rows;
mod predecessors;
mod resolution;
mod restore;
mod roots;
mod rows;
mod schema;
mod schema_compare;
mod transform;
mod verify;

#[cfg(test)]
mod tests;

use std::path::Path;

use serde::{Deserialize, Serialize};

pub use convert::{ConversionCounts, convert_aggregate_store_objects};
pub use export::export_store;
pub use import::{ImportReport, import_aggregate_archive};
pub use predecessors::{PreSealBinding, PreSealPlan, inspect_pre_seal_history};
pub use restore::restore_source_layout;
pub use roots::{EncodedRoot, RootHistoryEncoder};
pub use transform::{convert_event, convert_observation, convert_seal, map_root_references};
pub use verify::{compare_export_to_source, verify_export};

pub(super) use audit::verify_provenance_on;
pub(super) use resolution::validate_reexpressed_result_on;
pub(super) use schema::MIGRATION_SCHEMA;

/// Failures do not authorize projection repair or replacement of the source.
#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    #[error("migration store: {0}")]
    Store(#[from] crate::StoreError),
    #[error("migration I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("migration SQLite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("migration metadata: {0}")]
    Json(#[from] serde_json::Error),
    #[error("migration refused: {0}")]
    Refused(String),
}

/// Exact source schema, including indexes, views, triggers and empty tables.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SchemaEntry {
    pub kind: String,
    pub name: String,
    pub table: String,
    /// Physical source metadata; not a logical identity after migration.
    pub root_page: i64,
    pub sql: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Column {
    pub position: i64,
    pub name: String,
    pub declared_type: String,
    pub not_null: bool,
    pub default_sql: Option<String>,
    pub primary_key_position: i64,
    /// SQLite `table_xinfo`: 1 is a virtual-table control column, 2/3 generated.
    pub hidden: i64,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForeignKey {
    pub id: i64,
    pub sequence: i64,
    pub target_table: String,
    pub from_column: String,
    pub to_column: Option<String>,
    pub on_update: String,
    pub on_delete: String,
    pub match_rule: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TableManifest {
    pub name: String,
    /// SQLite's own `table_list` classification, not a name-prefix guess.
    pub kind: String,
    pub without_rowid: bool,
    pub strict: bool,
    pub columns: Vec<Column>,
    pub foreign_keys: Vec<ForeignKey>,
    /// First encoded cell is this rowid alias when present. Remaining cells
    /// follow columns in position order, excluding virtual control columns.
    pub rowid_alias: Option<String>,
    pub rows: u64,
    pub encoded_bytes: u64,
    /// Runtime content identity of length-framed encoded rows in archive order.
    pub rows_sha256: String,
}

/// A complete inventory is not proof of domain integrity or of import success.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExportManifest {
    pub format: String,
    pub version: u32,
    pub source_encoding: String,
    pub source_user_version: i64,
    pub source_application_id: i64,
    pub schema: Vec<SchemaEntry>,
    pub tables: Vec<TableManifest>,
    pub total_rows: u64,
    /// Schema coverage is complete; these categories have no source row data
    /// with which to demonstrate a transform's correctness.
    pub empty_tables: Vec<String>,
}

fn refused(message: impl Into<String>) -> MigrationError {
    MigrationError::Refused(message.into())
}

fn quoted(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

fn read_only(path: &Path) -> Result<rusqlite::Connection, MigrationError> {
    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    connection.execute_batch("PRAGMA query_only = ON; PRAGMA trusted_schema = OFF;")?;
    Ok(connection)
}
