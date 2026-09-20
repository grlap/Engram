//! Whole-store transfer as plain JSON.
//!
//! `export_json` writes every row of a store to a JSON Lines file.
//! `import_json` creates a new store in the current format and inserts those
//! rows by column name. Record ids travel as they are: nothing is recomputed,
//! and no link is rewritten because a record changed shape.
//!
//! Search indexes are left out and rebuilt. SQLite guards the bytes on disk;
//! after the rows are in, the ordinary doctor checks what they mean.

use std::{
    collections::{BTreeMap, HashMap},
    fs::{self, File, OpenOptions},
    io::{BufRead, BufReader, BufWriter, Write},
    path::{Path, PathBuf},
};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, types::Value as SqlValue, types::ValueRef};
use serde::{Deserialize, Serialize};
use serde_json::Value as Json;

use crate::SqliteStore;

#[cfg(test)]
mod tests;

const FORMAT: &str = "engram-json-export";

/// Tables whose single row describes the format of the store that holds it.
/// The new store keeps its own.
const FORMAT_MARKER_TABLES: &[&str] = &["work_schema_metadata"];

/// Columns the current format retired, as (table, column). Export writes them
/// as the source holds them; import names each one it met, with the number of
/// values it carried, and stores nothing for it. Any other column the current
/// format lacks is refused by name.
const RETIRED_COLUMNS: &[(&str, &str)] = &[
    // A fingerprint of the staged delivery page that nothing ever compared;
    // the page itself and its delivery token are what a session needs.
    ("work_session_state", "tentative_delivery_payload_hash"),
];

fn is_retired_column(table: &str, column: &str) -> bool {
    RETIRED_COLUMNS
        .iter()
        .any(|(retired_table, retired_column)| *retired_table == table && *retired_column == column)
}

/// Explicitly retired operational scaffolding. This is not an old schema or
/// migration chain: only these named tables may be omitted on fresh import,
/// and only when every declared column is in the corresponding allowed set.
/// Canonical objects and task-feed history are still copied unchanged.
fn retired_table_columns(table: &str) -> Option<&'static [&'static str]> {
    match table {
        "task_claims" => Some(&[
            "task_id",
            "lease_id",
            "holder_session_id",
            "idempotency_key",
            "expires_at_ms",
            "revision",
        ]),
        "task_claim_intents" => Some(&[
            "idempotency_key",
            "task_id",
            "holder_session_id",
            "lease_json",
        ]),
        "publication_intents" => Some(&[
            "idempotency_key",
            "report_hash",
            "external_ref",
            "state",
            "last_error",
            "attempt_count",
            "receipt_json",
        ]),
        _ => None,
    }
}

/// Validate the complete row even when its table is explicitly retired.
/// Retirement never excuses a malformed value or an undeclared column.
fn validate_omitted_row(
    table: &str,
    columns: &[String],
    mut values: serde_json::Map<String, Json>,
) -> Result<(), MigrationError> {
    for column in columns {
        let value = values
            .remove(column)
            .ok_or_else(|| refused(format!("a row of table {table} lacks column {column}")))?;
        decode(value).map_err(|error| match error {
            MigrationError::Refused(reason) => refused(format!(
                "column {column} of a row of table {table}: {reason}"
            )),
            other => other,
        })?;
    }
    if let Some(extra) = values.keys().next() {
        return Err(refused(format!(
            "a row of table {table} has undeclared column {extra}"
        )));
    }
    Ok(())
}

#[derive(Debug, thiserror::Error)]
pub enum MigrationError {
    #[error("migration store: {0}")]
    Store(#[from] crate::StoreError),
    #[error("migration I/O: {0}")]
    Io(#[from] std::io::Error),
    #[error("migration SQLite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("migration JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("migration refused: {0}")]
    Refused(String),
}

fn refused(message: impl Into<String>) -> MigrationError {
    MigrationError::Refused(message.into())
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct TableRows {
    pub name: String,
    pub columns: Vec<String>,
    pub rows: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct LeftOut {
    pub name: String,
    pub rows: u64,
    pub reason: String,
}

/// A retired column the file carried: its rows went in without it.
#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
pub struct RetiredField {
    pub table: String,
    pub column: String,
    /// Rows that carried a value in it, as distinct from rows left out.
    pub values: u64,
}

#[derive(Clone, Debug, Deserialize, PartialEq, Eq, Serialize)]
struct Header {
    format: String,
    exported_at: DateTime<Utc>,
    tables: Vec<TableRows>,
    /// `AUTOINCREMENT` high-water marks, so that no id is ever handed out twice.
    sequences: BTreeMap<String, i64>,
    left_out: Vec<LeftOut>,
}

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
enum Line {
    #[serde(rename = "engram_export")]
    Header(Header),
    #[serde(rename = "row")]
    Row {
        table: String,
        values: serde_json::Map<String, Json>,
    },
    #[serde(rename = "end")]
    End { rows: u64 },
}

#[derive(Debug, Serialize)]
pub struct ExportReport {
    pub database_bytes: u64,
    /// Size of the write-ahead log beside the source, or zero. The export
    /// read the committed frames it holds; a backup of the source is the
    /// database file together with that log and its `-shm` file.
    pub wal_bytes: u64,
    pub file_bytes: u64,
    pub rows: u64,
    pub tables: Vec<TableRows>,
    pub left_out: Vec<LeftOut>,
}

#[derive(Debug, Serialize)]
pub struct ImportReport {
    pub file_bytes: u64,
    pub database_bytes: u64,
    pub rows: u64,
    pub tables: Vec<TableRows>,
    pub left_out: Vec<LeftOut>,
    pub checked_objects: usize,
    pub checked_control_records: usize,
    pub checked_work_records: usize,
    /// Session rows carrying any part of a pending delivery, each read before
    /// publication the way the next retry reads it.
    pub checked_pending_deliveries: u64,
    /// Retired columns the file carried, each with the values it held.
    pub retired_fields: Vec<RetiredField>,
}

fn quoted(identifier: &str) -> String {
    format!("\"{}\"", identifier.replace('"', "\"\""))
}

/// A private file that is removed unless it is published.
struct Staged {
    path: PathBuf,
}

impl Staged {
    fn beside(destination: &Path) -> Result<Self, MigrationError> {
        let parent = destination
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        let path = parent.join(format!(".engram-migration-{}.tmp", uuid::Uuid::new_v4()));
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        options.open(&path)?;
        Ok(Self { path })
    }

    /// Publishes without ever replacing an existing destination. The
    /// non-replacing step is a hard link; see [`publish_failure`].
    fn publish(self, destination: &Path) -> Result<(), MigrationError> {
        fs::hard_link(&self.path, destination).map_err(|error| publish_failure(destination, &error))
    }
}

/// What a failed publication reports. An existing destination is the refusal it
/// always was. A filesystem that reports it cannot make the link is named as
/// such, since the link is how an existing file is never replaced. Every other
/// failure keeps its kind and its cause, and says what was being done, rather
/// than arriving as a bare I/O error at the very last step. Nothing here is a
/// claim about which filesystems fail in which way.
fn publish_failure(destination: &Path, error: &std::io::Error) -> MigrationError {
    match error.kind() {
        std::io::ErrorKind::AlreadyExists => refused("destination already exists"),
        std::io::ErrorKind::Unsupported | std::io::ErrorKind::PermissionDenied => refused(format!(
            "{} cannot be published by a hard link, which is how an existing file is never replaced; write to a filesystem that supports one: {error}",
            destination.display()
        )),
        kind => MigrationError::Io(std::io::Error::new(
            kind,
            format!(
                "publishing {} by a hard link to the staged store: {error}",
                destination.display()
            ),
        )),
    }
}

impl Drop for Staged {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.path);
        for sidecar in super::store_sidecars(&self.path) {
            let _ = fs::remove_file(sidecar);
        }
    }
}

/// Refuses a destination that has a write-ahead log, its shared-memory file or
/// a rollback journal beside it. Such a file belongs to the database it was
/// written for; SQLite would apply it to whatever database it finds at that
/// name, so one left behind by the old file must move with the old file. This
/// runs once, before anything is staged: the swap of the old file for the new
/// one is the operator's step and outside what any check here can see.
fn refuse_sidecars(destination: &Path) -> Result<(), MigrationError> {
    for sidecar in super::store_sidecars(destination) {
        if sidecar.try_exists()? {
            return Err(refused(format!(
                "{} exists beside the destination; a write-ahead log or journal belongs to the database it was written for and must move with that file, never sit beside another",
                sidecar.display()
            )));
        }
    }
    Ok(())
}

/// A derived table export names as left out, with the reason it is.
struct DerivedTable {
    name: String,
    reason: &'static str,
}

struct SourceTable {
    name: String,
    columns: Vec<String>,
    order_by: String,
}

/// The search indexes this build declares. Their rows are derived from the
/// records they index, so export reports them and import rebuilds them.
const SUPPORTED_SEARCH_INDEXES: &[&str] = &["object_fts", "work_catalog_fts"];

const SEARCH_INDEX: &str = "search index; import rebuilds it";
const REBUILT_PROJECTION: &str =
    "rebuilt projection; import starts it empty and repair derives it again";
const DELIVERY_BOOKKEEPING: &str =
    "delivery bookkeeping; import starts it empty and each session re-announces once";

/// An ordinary table this build drops and recreates whenever it repairs a
/// store, in the core schema or the work schema, so its rows are derived
/// state: export reports them, and import starts them empty rather than count
/// rows the published store will not hold. A search index is classified by
/// SQLite's own kind, not by these lists.
fn rebuilt_projection(name: &str) -> bool {
    !SUPPORTED_SEARCH_INDEXES.contains(&name)
        && (super::CORE_REBUILDABLE_SCHEMA_OBJECTS
            .iter()
            .any(|(kind, object)| *kind == "table" && *object == name)
            || super::work::is_rebuilt_projection_table(name))
}

/// Why a rebuilt projection is left out. Repair derives every such table
/// again from the records it projects, except the delivery bookkeeping, which
/// it starts empty and the sessions refill.
fn rebuilt_reason(name: &str) -> &'static str {
    if name == "project_memory_advertisements" {
        DELIVERY_BOOKKEEPING
    } else {
        REBUILT_PROJECTION
    }
}

/// Tables to copy, and the derived tables import rebuilds instead.
///
/// The classification is SQLite's own (`pragma_table_list`), never a guess from
/// a name: an ordinary table that merely looks like part of a search index,
/// such as `object_fts_notes` beside `object_fts`, is copied like any other
/// table. Anything this build can neither copy nor rebuild — an unknown virtual
/// table, its shadow tables, a view — is refused by name rather than dropped.
fn source_tables(
    connection: &Connection,
) -> Result<(Vec<SourceTable>, Vec<DerivedTable>), MigrationError> {
    let mut statement = connection.prepare(
        "SELECT name, type FROM pragma_table_list
         WHERE schema = 'main' AND substr(name, 1, 7) COLLATE NOCASE != 'sqlite_'
         ORDER BY name",
    )?;
    let all = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    // A shadow table is only derived if the search index it belongs to is
    // really one of this build's, and is really present here as a virtual table.
    let present = |index: &str| {
        all.iter()
            .any(|(name, kind)| name == index && kind == "virtual")
    };
    let mut tables = Vec::new();
    let mut rebuilt = Vec::new();
    for (name, kind) in &all {
        match kind.as_str() {
            "table" if rebuilt_projection(name) => rebuilt.push(DerivedTable {
                name: name.clone(),
                reason: rebuilt_reason(name),
            }),
            "table" => tables.push(ordinary_table(connection, name.clone())?),
            // A virtual table must be one of the search indexes by name. A name
            // that merely starts with one, such as `object_fts_extra`, is not.
            "virtual" if SUPPORTED_SEARCH_INDEXES.contains(&name.as_str()) => {
                rebuilt.push(DerivedTable {
                    name: name.clone(),
                    reason: SEARCH_INDEX,
                });
            }
            "shadow"
                if SUPPORTED_SEARCH_INDEXES.iter().any(|index| {
                    name.strip_prefix(index)
                        .is_some_and(|rest| rest.starts_with('_'))
                        && present(index)
                }) =>
            {
                rebuilt.push(DerivedTable {
                    name: name.clone(),
                    reason: SEARCH_INDEX,
                });
            }
            kind => {
                return Err(refused(format!(
                    "{kind} table {name} is not one this build copies or rebuilds"
                )));
            }
        }
    }
    Ok((tables, rebuilt))
}

fn ordinary_table(connection: &Connection, name: String) -> Result<SourceTable, MigrationError> {
    let mut columns = Vec::new();
    let mut key = Vec::new();
    let mut info = connection.prepare(&format!("PRAGMA table_xinfo({})", quoted(&name)))?;
    let mut rows = info.query([])?;
    while let Some(row) = rows.next()? {
        // Generated and hidden columns are derived, never stored input.
        if row.get::<_, i64>(6)? != 0 {
            continue;
        }
        let column: String = row.get(1)?;
        let position: i64 = row.get(5)?;
        if position > 0 {
            key.push((position, column.clone()));
        }
        columns.push(column);
    }
    key.sort();
    drop(rows);
    drop(info);
    // Insertion order is kept where a table has it, so a query that reads
    // "the latest row" still finds the same one.
    let has_rowid = connection
        .prepare(&format!("SELECT rowid FROM {} LIMIT 0", quoted(&name)))
        .is_ok();
    let order_by = if has_rowid {
        "rowid".to_owned()
    } else {
        key.iter()
            .map(|(_, column)| quoted(column))
            .collect::<Vec<_>>()
            .join(", ")
    };
    Ok(SourceTable {
        name,
        columns,
        order_by,
    })
}

/// The source's indexes and triggers, reported rather than carried.
///
/// The current schema is authoritative and no source DDL is ever executed, so
/// an index or trigger this build declares is simply recreated by the new
/// store. One it does not declare is named: an undeclared index is derived
/// data and is reported as left out, while an undeclared trigger is behaviour
/// the imported store would not reproduce, so it is refused.
fn non_table_objects(connection: &Connection) -> Result<Vec<LeftOut>, MigrationError> {
    let declared = super::current_schema_reference()?;
    let mut statement = connection.prepare(
        "SELECT type, name FROM sqlite_schema
         WHERE type IN ('index', 'trigger')
           AND substr(name, 1, 7) COLLATE NOCASE != 'sqlite_'
         ORDER BY type, name",
    )?;
    let all = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut left_out = Vec::new();
    for (kind, name) in all {
        if declared
            .iter()
            .any(|declared| declared.object_type == kind && declared.name == name)
        {
            continue;
        }
        if kind == "trigger" {
            return Err(refused(format!(
                "trigger {name} is not declared by this build; the imported store would not reproduce what it enforces"
            )));
        }
        left_out.push(LeftOut {
            name,
            rows: 0,
            reason: "index this build does not declare; the new store has the current indexes"
                .into(),
        });
    }
    Ok(left_out)
}

fn count(connection: &Connection, table: &str) -> Result<u64, MigrationError> {
    let rows: i64 = connection.query_row(
        &format!("SELECT COUNT(*) FROM {}", quoted(table)),
        [],
        |row| row.get(0),
    )?;
    Ok(u64::try_from(rows).unwrap_or_default())
}

/// A stored value as JSON. Text is a string. A blob is an object naming how it
/// is written: nested `json` when re-serializing it yields the same bytes,
/// else `text`, else `hex`.
fn encode(value: ValueRef<'_>) -> Result<Json, MigrationError> {
    Ok(match value {
        ValueRef::Null => Json::Null,
        ValueRef::Integer(integer) => Json::from(integer),
        ValueRef::Real(real) => serde_json::Number::from_f64(real)
            .map(Json::Number)
            .ok_or_else(|| refused("a stored number is not finite"))?,
        ValueRef::Text(text) => Json::String(
            std::str::from_utf8(text)
                .map_err(|_| refused("a stored text value is not UTF-8"))?
                .to_owned(),
        ),
        ValueRef::Blob(bytes) => {
            if let Ok(nested) = serde_json::from_slice::<Json>(bytes)
                && serde_json_canonicalizer::to_vec(&nested).is_ok_and(|again| again == bytes)
            {
                serde_json::json!({ "json": nested })
            } else if let Ok(text) = std::str::from_utf8(bytes) {
                serde_json::json!({ "text": text })
            } else {
                let mut hex = String::with_capacity(bytes.len() * 2);
                for byte in bytes {
                    use std::fmt::Write as _;
                    let _ = write!(hex, "{byte:02x}");
                }
                serde_json::json!({ "hex": hex })
            }
        }
    })
}

fn decode(value: Json) -> Result<SqlValue, MigrationError> {
    Ok(match value {
        Json::Null => SqlValue::Null,
        Json::Number(number) => match (number.as_i64(), number.as_f64()) {
            (Some(integer), _) => SqlValue::Integer(integer),
            (None, Some(real)) if number.is_f64() => SqlValue::Real(real),
            _ => {
                return Err(refused(
                    "a number outside the range of a stored integer or real",
                ));
            }
        },
        Json::String(text) => SqlValue::Text(text),
        // A refusal names the shape it met and never the value: a cell can hold
        // a private body, and a refusal reaches the operator's terminal.
        Json::Object(mut blob) => {
            if blob.len() != 1 {
                return Err(refused(format!(
                    "a blob is written as exactly one of json, text, or hex; this one has {} keys",
                    blob.len()
                )));
            }
            if let Some(nested) = blob.remove("json") {
                SqlValue::Blob(
                    serde_json_canonicalizer::to_vec(&nested)
                        .map_err(|_| refused("nested JSON cannot be written canonically"))?,
                )
            } else if let Some(text) = blob.remove("text") {
                match text {
                    Json::String(text) => SqlValue::Blob(text.into_bytes()),
                    _ => return Err(refused("a text blob is written as a string")),
                }
            } else if let Some(hex) = blob.remove("hex") {
                match hex {
                    Json::String(hex) => SqlValue::Blob(unhex(&hex)?),
                    _ => return Err(refused("a hex blob is written as a string")),
                }
            } else {
                return Err(refused("a blob is written as json, text, or hex"));
            }
        }
        Json::Array(_) => return Err(refused("an array is not a stored value")),
        Json::Bool(_) => return Err(refused("a boolean is not a stored value")),
    })
}

/// Reads a blob written as hex. The alphabet is exactly `0-9`, `a-f` and
/// `A-F`, two digits per byte; export writes the lowercase form. Every byte is
/// checked against that alphabet first, because a number parser would accept a
/// leading sign, and `+f` is not a byte.
fn unhex(hex: &str) -> Result<Vec<u8>, MigrationError> {
    if !hex.len().is_multiple_of(2) {
        return Err(refused("hex blob has an odd length"));
    }
    if !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(refused("hex blob has a character outside 0-9, a-f and A-F"));
    }
    hex.as_bytes()
        .chunks(2)
        .map(|pair| {
            let digit = |byte: u8| match byte {
                b'0'..=b'9' => byte - b'0',
                b'a'..=b'f' => byte - b'a' + 10,
                _ => byte - b'A' + 10,
            };
            Ok((digit(pair[0]) << 4) | digit(pair[1]))
        })
        .collect()
}

/// Writes every row of `database` to a new JSON Lines file at `out`.
///
/// The source is opened read-only and read inside one transaction, so the file
/// is one coherent moment of the store. It holds private and restricted data.
///
/// # Errors
/// Refuses an existing `out`, a source that is not a store, and a value that
/// cannot be written as JSON. No partial file is left behind.
pub fn export_json(database: &Path, out: &Path) -> Result<ExportReport, MigrationError> {
    if out.try_exists()? {
        return Err(refused("export destination already exists"));
    }
    let connection =
        Connection::open_with_flags(database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    connection.busy_timeout(std::time::Duration::from_secs(5))?;
    connection.execute_batch("PRAGMA query_only = ON; PRAGMA trusted_schema = OFF; BEGIN;")?;
    let (tables, rebuilt) = source_tables(&connection)?;
    if tables.is_empty() {
        return Err(refused("the source has no tables; it is not a store"));
    }
    let mut left_out = non_table_objects(&connection)?;
    for DerivedTable { name, reason } in rebuilt {
        left_out.push(LeftOut {
            rows: count(&connection, &name)?,
            name,
            reason: reason.into(),
        });
    }
    let copied = tables;
    let has_sequences: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name = 'sqlite_sequence')",
        [],
        |row| row.get(0),
    )?;
    let mut sequences = BTreeMap::new();
    if has_sequences {
        let mut statement = connection.prepare("SELECT name, seq FROM sqlite_sequence")?;
        for row in statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))? {
            let (name, sequence): (String, i64) = row?;
            sequences.insert(name, sequence);
        }
    }
    let header = Header {
        format: FORMAT.into(),
        exported_at: Utc::now(),
        tables: copied
            .iter()
            .map(|table| {
                Ok(TableRows {
                    name: table.name.clone(),
                    columns: table.columns.clone(),
                    rows: count(&connection, &table.name)?,
                })
            })
            .collect::<Result<_, MigrationError>>()?,
        sequences,
        left_out,
    };

    let staged = Staged::beside(out)?;
    let mut writer = BufWriter::new(OpenOptions::new().write(true).open(&staged.path)?);
    serde_json::to_writer(&mut writer, &Line::Header(header.clone()))?;
    writer.write_all(b"\n")?;
    let mut total = 0_u64;
    for (table, expected) in copied.iter().zip(&header.tables) {
        let select = format!(
            "SELECT {} FROM {} ORDER BY {}",
            table
                .columns
                .iter()
                .map(|column| quoted(column))
                .collect::<Vec<_>>()
                .join(", "),
            quoted(&table.name),
            table.order_by
        );
        let mut statement = connection.prepare(&select)?;
        let mut rows = statement.query([])?;
        let mut written = 0_u64;
        while let Some(row) = rows.next()? {
            let mut values = serde_json::Map::new();
            for (index, column) in table.columns.iter().enumerate() {
                values.insert(column.clone(), encode(row.get_ref(index)?)?);
            }
            serde_json::to_writer(
                &mut writer,
                &Line::Row {
                    table: table.name.clone(),
                    values,
                },
            )?;
            writer.write_all(b"\n")?;
            written += 1;
        }
        if written != expected.rows {
            return Err(refused(format!(
                "table {} changed while it was read",
                table.name
            )));
        }
        total += written;
    }
    serde_json::to_writer(&mut writer, &Line::End { rows: total })?;
    writer.write_all(b"\n")?;
    writer
        .into_inner()
        .map_err(|error| MigrationError::Io(error.into_error()))?
        .sync_all()?;
    connection.execute_batch("COMMIT;")?;
    let file_bytes = fs::metadata(&staged.path)?.len();
    staged.publish(out)?;
    Ok(ExportReport {
        database_bytes: fs::metadata(database)?.len(),
        wal_bytes: fs::metadata(super::sidecar(database, "-wal")).map_or(0, |wal| wal.len()),
        file_bytes,
        rows: total,
        tables: header.tables,
        left_out: header.left_out,
    })
}

fn lines(
    file: &Path,
) -> Result<impl Iterator<Item = Result<Line, MigrationError>>, MigrationError> {
    Ok(BufReader::new(File::open(file)?)
        .lines()
        .filter(|line| line.as_ref().map_or(true, |line| !line.trim().is_empty()))
        .map(|line| Ok(serde_json::from_str::<Line>(&line?)?)))
}

fn header_of(file: &Path) -> Result<Header, MigrationError> {
    match lines(file)?.next() {
        Some(Ok(Line::Header(header))) if header.format == FORMAT => Ok(header),
        Some(Err(error)) => Err(error),
        _ => Err(refused(
            "the file does not start with an engram export header",
        )),
    }
}

/// Creates a new store at `out` in the current format from an export file.
///
/// Rows go in by column name under the ids they already have. A table or
/// column the current format has no place for is refused by name, except the
/// explicitly retired lists reported as omissions. The doctor must find the
/// result healthy before it is published, and an existing `out` is never replaced.
///
/// # Errors
/// Refuses a damaged or truncated file, an unknown table or column, a broken
/// reference between rows, and a result the doctor finds unhealthy.
pub fn import_json(file: &Path, out: &Path) -> Result<ImportReport, MigrationError> {
    if out.try_exists()? {
        return Err(refused("import destination already exists"));
    }
    refuse_sidecars(out)?;
    let header = header_of(file)?;

    // The reserved file is private from the moment it exists, and SQLite keeps
    // an existing file's permissions — including on the journals it creates
    // beside it. The empty reserved file is an empty database, so opening it in
    // place keeps every sensitive row private through schema creation, the
    // inserts, the journals and the published link. Deleting it first would let
    // SQLite create the replacement at 0644 subject to umask.
    let staged = Staged::beside(out)?;
    drop(SqliteStore::open_unresolved(&staged.path)?);
    let mut connection = Connection::open(&staged.path)?;
    connection.execute_batch("PRAGMA foreign_keys = ON; PRAGMA synchronous = FULL;")?;
    let transaction = connection.transaction()?;
    // Rows arrive table by table; references are checked once, at commit.
    transaction.execute_batch("PRAGMA defer_foreign_keys = ON;")?;

    let mut inserts = HashMap::new();
    let mut omitted = HashMap::new();
    let mut declared_names = std::collections::HashSet::new();
    let mut tables = Vec::new();
    let mut left_out = header.left_out.clone();
    let mut retired_fields: Vec<RetiredField> = Vec::new();
    for table in &header.tables {
        if !declared_names.insert(&table.name) {
            return Err(refused(format!(
                "duplicate table {} in the header",
                table.name
            )));
        }
        let mut declared_columns = std::collections::HashSet::new();
        for column in &table.columns {
            if !declared_columns.insert(column) {
                return Err(refused(format!(
                    "duplicate column {column} of table {}",
                    table.name
                )));
            }
        }
        if let Some(known) = retired_table_columns(&table.name) {
            for column in &table.columns {
                if !known.contains(&column.as_str()) {
                    return Err(refused(format!(
                        "column {column} of retired table {} has no place in the current format",
                        table.name
                    )));
                }
            }
            left_out.push(LeftOut {
                name: table.name.clone(),
                rows: table.rows,
                reason: "explicitly retired operational table; rows remain in the source export, not the new store".into(),
            });
            omitted.insert(table.name.clone(), (table.columns.clone(), 0_u64));
            continue;
        }
        if FORMAT_MARKER_TABLES.contains(&table.name.as_str()) {
            left_out.push(LeftOut {
                name: table.name.clone(),
                rows: table.rows,
                reason: "format marker; the new store keeps its own".into(),
            });
            omitted.insert(table.name.clone(), (table.columns.clone(), 0_u64));
            continue;
        }
        let (columns, retired): (Vec<String>, Vec<String>) = table
            .columns
            .iter()
            .cloned()
            .partition(|column| !is_retired_column(&table.name, column));
        for column in &retired {
            retired_fields.push(RetiredField {
                table: table.name.clone(),
                column: column.clone(),
                values: 0,
            });
        }
        admit_destination(&transaction, &table.name, &columns)?;
        transaction.execute(&format!("DELETE FROM {}", quoted(&table.name)), [])?;
        let insert = format!(
            "INSERT INTO {} ({}) VALUES ({})",
            quoted(&table.name),
            columns
                .iter()
                .map(|column| quoted(column))
                .collect::<Vec<_>>()
                .join(", "),
            (1..=columns.len())
                .map(|index| format!("?{index}"))
                .collect::<Vec<_>>()
                .join(", ")
        );
        inserts.insert(
            table.name.clone(),
            (transaction.prepare(&insert)?, columns, retired, 0_u64),
        );
        tables.push(table.clone());
    }

    let mut seen = 0_u64;
    let mut ended = None;
    for line in lines(file)?.skip(1) {
        match line? {
            Line::Header(_) => return Err(refused("the file has a second header")),
            Line::End { .. } if ended.is_some() => {
                return Err(refused("the file has a second end line"));
            }
            Line::End { rows } => ended = Some(rows),
            Line::Row { .. } if ended.is_some() => {
                return Err(refused("the file has rows after its end line"));
            }
            Line::Row { table, mut values } => {
                seen += 1;
                if let Some((columns, count)) = omitted.get_mut(&table) {
                    validate_omitted_row(&table, columns, values)?;
                    *count += 1;
                    continue;
                }
                let Some((insert, columns, retired, inserted)) = inserts.get_mut(&table) else {
                    return Err(refused(format!(
                        "the file has a row for undeclared table {table}"
                    )));
                };
                for column in retired.iter() {
                    if values.remove(column).is_some_and(|value| !value.is_null())
                        && let Some(field) = retired_fields
                            .iter_mut()
                            .find(|field| field.table == table && &field.column == column)
                    {
                        field.values += 1;
                    }
                }
                let mut row = Vec::with_capacity(columns.len());
                for column in columns.iter() {
                    let value = values.remove(column).ok_or_else(|| {
                        refused(format!("a row of table {table} lacks column {column}"))
                    })?;
                    row.push(decode(value).map_err(|error| match error {
                        MigrationError::Refused(reason) => refused(format!(
                            "column {column} of a row of table {table}: {reason}"
                        )),
                        other => other,
                    })?);
                }
                if let Some(extra) = values.keys().next() {
                    return Err(refused(format!(
                        "a row of table {table} has undeclared column {extra}"
                    )));
                }
                insert.execute(rusqlite::params_from_iter(row))?;
                *inserted += 1;
            }
        }
    }
    let expected = header.tables.iter().map(|table| table.rows).sum::<u64>();
    if ended != Some(seen) || seen != expected {
        return Err(refused(
            "the file is truncated or its row counts do not match its header",
        ));
    }
    let inserted = inserts
        .into_iter()
        .map(|(name, (_, _, _, rows))| (name, rows))
        .collect::<HashMap<_, _>>();
    for table in &header.tables {
        let inserted = inserted
            .get(&table.name)
            .copied()
            .or_else(|| omitted.get(&table.name).map(|(_, count)| *count))
            .ok_or_else(|| refused(format!("unaccounted table {}", table.name)))?;
        if inserted != table.rows {
            return Err(refused(format!(
                "table {} declares {} rows but the file holds {}",
                table.name, table.rows, inserted
            )));
        }
    }
    restore_sequences(&transaction, &header.sequences, &inserted)?;
    transaction.commit()?;
    let damage: String = connection.query_row("PRAGMA integrity_check", [], |row| row.get(0))?;
    if damage != "ok" {
        return Err(refused(format!("SQLite integrity check: {damage}")));
    }
    connection.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    connection.close().map_err(|(_, error)| error)?;

    // Rebuilds the search indexes and runs the full doctor; an unhealthy
    // result is an error that names the records.
    let report = SqliteStore::repair_rebuildable_projections(&staged.path)?;
    let pending = admit_pending_deliveries(&staged.path)?;
    let checkpoint = Connection::open(&staged.path)?;
    checkpoint.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;
    checkpoint.close().map_err(|(_, error)| error)?;

    let database_bytes = fs::metadata(&staged.path)?.len();
    staged.publish(out)?;
    Ok(ImportReport {
        file_bytes: fs::metadata(file)?.len(),
        database_bytes,
        rows: tables.iter().map(|table| table.rows).sum(),
        tables,
        left_out,
        checked_objects: report.checked_objects,
        checked_control_records: report.checked_control_records,
        checked_work_records: report.checked_work_records,
        checked_pending_deliveries: pending,
        retired_fields,
    })
}

/// Admits a file table into the destination that carries its name.
///
/// The destination must be an ordinary table, since only one holds copied rows:
/// a search index or one of its shadow tables is derived from other rows and
/// rebuilt after the copy, so rows copied into one would be thrown away while
/// the report counted them — matching columns are no admission. Every column
/// the file declares must then have a place in that table.
fn admit_destination(
    transaction: &rusqlite::Transaction<'_>,
    table: &str,
    columns: &[String],
) -> Result<(), MigrationError> {
    let destination: Option<String> = transaction
        .query_row(
            "SELECT type FROM pragma_table_list WHERE schema = 'main' AND name = ?1",
            [table],
            |row| row.get(0),
        )
        .optional()?;
    match destination.as_deref() {
        Some("table") => {}
        Some(derived) => {
            return Err(refused(format!(
                "table {table} is a derived {derived} table in the current format and takes no copied rows"
            )));
        }
        None => {
            return Err(refused(format!(
                "table {table} has no place in the current format"
            )));
        }
    }
    let mut known = Vec::new();
    let mut info = transaction.prepare(&format!("PRAGMA table_xinfo({})", quoted(table)))?;
    let mut rows = info.query([])?;
    while let Some(row) = rows.next()? {
        if row.get::<_, i64>(6)? == 0 {
            known.push(row.get::<_, String>(1)?);
        }
    }
    if let Some(column) = columns.iter().find(|column| !known.contains(column)) {
        return Err(refused(format!(
            "column {column} of table {table} has no place in the current format"
        )));
    }
    Ok(())
}

/// Carries each `AUTOINCREMENT` high-water mark across, so that no row id is
/// ever handed out twice in the new store.
fn restore_sequences(
    transaction: &rusqlite::Transaction<'_>,
    sequences: &BTreeMap<String, i64>,
    inserted: &HashMap<String, u64>,
) -> Result<(), MigrationError> {
    for (name, sequence) in sequences {
        if !inserted.contains_key(name) {
            continue;
        }
        let updated = transaction.execute(
            "UPDATE sqlite_sequence SET seq = max(seq, ?2) WHERE name = ?1",
            rusqlite::params![name, sequence],
        )?;
        if updated == 0 {
            transaction.execute(
                "INSERT INTO sqlite_sequence (name, seq) VALUES (?1, ?2)",
                rusqlite::params![name, sequence],
            )?;
        }
    }
    Ok(())
}

/// Admits every retained pending delivery in the new store.
///
/// A page staged before the transfer is replayed by the next core retry at the
/// cursor it already confirmed, and the doctor does not look at one. Every
/// session row that carries any part of a pending delivery is read the way that
/// retry reads it, so a row the file left with its cursor, token and payload
/// not present together, or a page that disagrees with current source state
/// about what the session's own changes are, is refused here, by session, and
/// never reaches a published store where that retry would fail. Nothing is
/// supplied or rewritten on a page's behalf.
fn admit_pending_deliveries(path: &Path) -> Result<u64, MigrationError> {
    let store = SqliteStore::open_unresolved(path)?;
    let pending: Vec<(String, String)> = store
        .connection
        .prepare(
            "SELECT project_id, session_id
             FROM work_session_state
             WHERE tentative_project_cursor IS NOT NULL
                OR tentative_delivery_token IS NOT NULL
                OR tentative_delivery_payload IS NOT NULL
             ORDER BY project_id, session_id",
        )?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<_, _>>()?;
    let checked = u64::try_from(pending.len()).unwrap_or(u64::MAX);
    for (project, session) in pending {
        let project_id = crate::ProjectId(project.clone());
        let session_id = crate::SessionId(session.clone());
        let state = store
            .work_session_state(&project_id, &session_id, Utc::now())
            .map_err(|error| {
                refused(format!(
                    "the pending delivery of session {session} of project {project} is not one this build can admit: {}",
                    admission_reason(&error)
                ))
            })?;
        let (Some(through), Some(payload)) = (
            state.tentative_project_cursor,
            store.staged_work_session_delivery_payload(&project_id, &session_id)?,
        ) else {
            continue;
        };
        crate::work_service::read_pending_delivery(
            &store,
            &session_id,
            &project_id,
            state.project_cursor,
            through,
            &payload,
        )
        .map_err(|error| {
            refused(format!(
                "the delivery page staged for session {session} of project {project} is not one this build can admit: {}",
                admission_reason(&error)
            ))
        })?;
    }
    Ok(checked)
}

/// What a refused pending delivery may say to the operator: the shape of the
/// problem and where it is, never a value from the page, which carries work
/// titles and actor context. Every string this returns is written here: a
/// decoding error is reduced to its category and position, and a projection
/// reason is recognised and restated, never repeated.
fn admission_reason(error: &crate::StoreError) -> String {
    const PROJECTION_REASONS: &[(&str, &str)] = &[
        (
            "present together",
            "its cursor, delivery token and page are not present together",
        ),
        (
            "attribution differs",
            "its attribution differs from the receiving session",
        ),
        (
            "dense source interval",
            "it does not bind its exact dense source interval",
        ),
        ("schema", "its schema version is not one this build reads"),
        ("is missing", "it names a record this store does not hold"),
        ("timestamp", "its session timestamp is invalid"),
    ];
    match error {
        crate::StoreError::Json(error) => {
            let category = match error.classify() {
                serde_json::error::Category::Syntax => "malformed JSON",
                serde_json::error::Category::Eof => "truncated JSON",
                serde_json::error::Category::Data => "a field of the wrong type or a missing field",
                serde_json::error::Category::Io => "unreadable JSON",
            };
            format!(
                "the page is not a delivery page ({category} at line {} column {})",
                error.line(),
                error.column()
            )
        }
        crate::StoreError::InvalidWorkProjection(reason) => PROJECTION_REASONS
            .iter()
            .find(|(marker, _)| reason.contains(marker))
            .map_or("the row cannot be read by this build", |(_, wording)| {
                wording
            })
            .to_owned(),
        _ => "the row cannot be read by this build".into(),
    }
}
