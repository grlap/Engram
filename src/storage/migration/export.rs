use std::{
    fs::{self, OpenOptions},
    path::{Path, PathBuf},
};

use rusqlite::{Connection, params};
use sha2::{Digest, Sha256};

use super::{
    Column, ExportManifest, ForeignKey, MigrationError, SchemaEntry, TableManifest, quoted,
    read_only, refused, rows,
};

pub(super) const ARCHIVE_SCHEMA: &str = "
    CREATE TABLE migration_manifest (
        singleton INTEGER PRIMARY KEY CHECK(singleton = 1), document BLOB NOT NULL,
        document_sha256 TEXT NOT NULL
    );
    CREATE TABLE migration_rows (
        table_name TEXT NOT NULL, row_number INTEGER NOT NULL, cells BLOB NOT NULL,
        PRIMARY KEY(table_name, row_number)
    ) WITHOUT ROWID;
";

/// Exports every table from one read transaction, including unknown and empty
/// categories. Canonical bodies, private data and authority records are opaque
/// cells. Nothing is redacted, rebound to this host, or installed.
///
/// # Errors
/// Refuses unsupported table representations, SQLite/I/O errors and any
/// existing destination. A completed archive appears only after a successful
/// transaction and file sync. Memory use is bounded by one source row plus
/// schema metadata, not the size of the store.
pub fn export_store(source: &Path, destination: &Path) -> Result<ExportManifest, MigrationError> {
    if destination.try_exists()? {
        return Err(refused("export destination already exists"));
    }
    let source = read_only(source)?;
    let snapshot = source.unchecked_transaction()?;
    // This first read fixes the WAL snapshot before metadata or data is read.
    let schema = schema_on(&snapshot)?;
    let stage = StagedArchive::create(destination)?;
    let mut archive = Connection::open(&stage.path)?;
    archive.execute_batch("PRAGMA journal_mode = DELETE; PRAGMA synchronous = FULL;")?;
    let transaction = archive.transaction()?;
    transaction.execute_batch(ARCHIVE_SCHEMA)?;
    let manifest = export_on(&snapshot, Some(&transaction), schema)?;
    let document = serde_json::to_vec(&manifest)?;
    let identity = format!("{:x}", Sha256::digest(&document));
    transaction.execute(
        "INSERT INTO migration_manifest VALUES (1, ?1, ?2)",
        params![document, identity],
    )?;
    transaction.commit()?;
    snapshot.commit()?;
    archive.close().map_err(|(_, error)| error)?;
    if super::verify_export(&stage.path)? != manifest {
        return Err(refused("staged archive differs from the export manifest"));
    }
    OpenOptions::new()
        .write(true)
        .open(&stage.path)?
        .sync_all()?;
    // Same-directory hard linking is atomic and refuses an existing target.
    // No copy fallback: a partially copied output must never look complete.
    fs::hard_link(&stage.path, destination)?;
    Ok(manifest)
}

pub(super) fn schema_on(connection: &Connection) -> Result<Vec<SchemaEntry>, MigrationError> {
    Ok(connection
        .prepare(
            "SELECT type, name, tbl_name, rootpage, sql FROM sqlite_schema ORDER BY type, name",
        )?
        .query_map([], |row| {
            Ok(SchemaEntry {
                kind: row.get(0)?,
                name: row.get(1)?,
                table: row.get(2)?,
                root_page: row.get(3)?,
                sql: row.get(4)?,
            })
        })?
        .collect::<Result<_, _>>()?)
}

pub(super) fn export_on(
    source: &Connection,
    archive: Option<&Connection>,
    schema: Vec<SchemaEntry>,
) -> Result<ExportManifest, MigrationError> {
    let mut tables = Vec::new();
    let mut total_rows = 0_u64;
    for entry in schema.iter().filter(|entry| entry.kind == "table") {
        let mut table = table_on(source, &entry.name)?;
        export_table(source, archive, &mut table)?;
        total_rows = total_rows
            .checked_add(table.rows)
            .ok_or_else(|| refused("total row count overflow"))?;
        tables.push(table);
    }
    Ok(ExportManifest {
        format: "engram-full-sqlite-export".into(),
        version: 1,
        source_encoding: source.query_row("PRAGMA encoding", [], |row| row.get(0))?,
        source_user_version: source.query_row("PRAGMA user_version", [], |row| row.get(0))?,
        source_application_id: source.query_row("PRAGMA application_id", [], |row| row.get(0))?,
        schema,
        empty_tables: tables
            .iter()
            .filter(|table| table.rows == 0)
            .map(|table| table.name.clone())
            .collect(),
        tables,
        total_rows,
    })
}

pub(super) fn table_on(
    connection: &Connection,
    name: &str,
) -> Result<TableManifest, MigrationError> {
    let (kind, without_rowid, strict): (String, bool, bool) = connection.query_row(
        "SELECT type, wr, strict FROM pragma_table_list WHERE schema = 'main' AND name = ?1",
        [name],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    let columns: Vec<Column> = connection
        .prepare(&format!("PRAGMA main.table_xinfo({})", quoted(name)))?
        .query_map([], |row| {
            Ok(Column {
                position: row.get(0)?,
                name: row.get(1)?,
                declared_type: row.get(2)?,
                not_null: row.get(3)?,
                default_sql: row.get(4)?,
                primary_key_position: row.get(5)?,
                hidden: row.get(6)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    if columns.is_empty()
        || columns
            .iter()
            .any(|column| !(0..=3).contains(&column.hidden))
    {
        return Err(refused(format!("unsupported columns in table {name}")));
    }
    let rowid_alias = if without_rowid {
        None
    } else {
        Some(
            ["_rowid_", "rowid", "oid"]
                .into_iter()
                .find(|alias| {
                    !columns
                        .iter()
                        .any(|column| column.name.eq_ignore_ascii_case(alias))
                })
                .ok_or_else(|| refused(format!("all rowid aliases are shadowed in {name}")))?
                .into(),
        )
    };
    let foreign_keys = connection
        .prepare(&format!("PRAGMA main.foreign_key_list({})", quoted(name)))?
        .query_map([], |row| {
            Ok(ForeignKey {
                id: row.get(0)?,
                sequence: row.get(1)?,
                target_table: row.get(2)?,
                from_column: row.get(3)?,
                to_column: row.get(4)?,
                on_update: row.get(5)?,
                on_delete: row.get(6)?,
                match_rule: row.get(7)?,
            })
        })?
        .collect::<Result<_, _>>()?;
    Ok(TableManifest {
        name: name.into(),
        kind,
        without_rowid,
        strict,
        columns,
        foreign_keys,
        rowid_alias,
        rows: 0,
        encoded_bytes: 0,
        rows_sha256: String::new(),
    })
}

fn export_table(
    source: &Connection,
    archive: Option<&Connection>,
    table: &mut TableManifest,
) -> Result<(), MigrationError> {
    let mut selected: Vec<String> = table.rowid_alias.iter().map(|name| quoted(name)).collect();
    selected.extend(
        table
            .columns
            .iter()
            .filter(|column| column.hidden != 1)
            .map(|column| quoted(&column.name)),
    );
    let order = if let Some(alias) = &table.rowid_alias {
        quoted(alias)
    } else {
        let mut key: Vec<_> = table
            .columns
            .iter()
            .filter(|column| column.primary_key_position > 0)
            .collect();
        key.sort_by_key(|column| column.primary_key_position);
        if key.is_empty() {
            return Err(refused(format!("no stable row order for {}", table.name)));
        }
        key.iter()
            .map(|column| quoted(&column.name))
            .collect::<Vec<_>>()
            .join(", ")
    };
    let mut query = source.prepare(&format!(
        "SELECT {} FROM {} ORDER BY {order}",
        selected.join(", "),
        quoted(&table.name)
    ))?;
    let mut insert = archive
        .map(|connection| connection.prepare("INSERT INTO migration_rows VALUES (?1, ?2, ?3)"))
        .transpose()?;
    let mut rows = query.query([])?;
    let mut digest = Sha256::new();
    while let Some(row) = rows.next()? {
        let cells = rows::encode(row, selected.len())?;
        let length = u64::try_from(cells.len()).map_err(|_| refused("row length overflow"))?;
        table.rows = table
            .rows
            .checked_add(1)
            .ok_or_else(|| refused("row count overflow"))?;
        table.encoded_bytes = table
            .encoded_bytes
            .checked_add(length)
            .ok_or_else(|| refused("table length overflow"))?;
        digest.update(length.to_be_bytes());
        digest.update(&cells);
        let position = i64::try_from(table.rows)
            .map_err(|_| refused("row position exceeds SQLite integer range"))?;
        if let Some(insert) = &mut insert {
            insert.execute(params![table.name, position, cells])?;
        }
    }
    table.rows_sha256 = format!("{:x}", digest.finalize());
    Ok(())
}

pub(super) struct StagedArchive {
    pub(super) path: PathBuf,
}

impl StagedArchive {
    pub(super) fn create(destination: &Path) -> Result<Self, MigrationError> {
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
}

impl Drop for StagedArchive {
    fn drop(&mut self) {
        // Only our uniquely reserved staging file and its SQLite journal.
        let _ = fs::remove_file(&self.path);
        let mut journal = self.path.as_os_str().to_os_string();
        journal.push("-journal");
        let _ = fs::remove_file(PathBuf::from(journal));
    }
}
