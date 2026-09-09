//! Reconstruct the supported source layout before applying a format transform.
//! This is a transport primitive, not the completed versioned importer.

use std::{fs, path::Path};

use rusqlite::{Connection, params_from_iter};

use super::{
    ExportManifest, MigrationError, SchemaEntry, TableManifest, export, quoted, read_only, refused,
    rows, verify,
};

/// Restores every raw category into an absent scratch file using only DDL
/// derived from the compiled current schema or its explicit aggregate-root
/// predecessor. Source SQL is compared as data, never executed as code.
///
/// The result STILL HAS THE SOURCE FORMAT. It is not activated, upgraded, or
/// installed, and this operation does not authorize using its claims or grants.
///
/// # Errors
/// Refuses unknown schema profiles, integrity failures, content differences or
/// an existing destination. Refusal leaves no published scratch database.
pub fn restore_source_layout(
    archive: &Path,
    destination: &Path,
) -> Result<ExportManifest, MigrationError> {
    if destination.try_exists()? {
        return Err(refused("scratch destination already exists"));
    }
    let archive = read_only(archive)?;
    let snapshot = archive.unchecked_transaction()?;
    let manifest = verify::verify_on(&snapshot)?;
    let blueprint = source_blueprint(&manifest)?;
    let stage = export::StagedArchive::create(destination)?;
    let mut target = Connection::open(&stage.path)?;
    target.execute_batch(
        "PRAGMA journal_mode = DELETE; PRAGMA synchronous = FULL; PRAGMA foreign_keys = OFF;",
    )?;
    target.pragma_update(None, "user_version", manifest.source_user_version)?;
    target.pragma_update(None, "application_id", manifest.source_application_id)?;
    let transaction = target.transaction()?;
    create_tables(&transaction, &blueprint, &manifest.tables)?;
    let mut tables: Vec<_> = manifest
        .tables
        .iter()
        .filter(|table| table.kind != "virtual")
        .collect();
    // AUTOINCREMENT inserts can advance these values; restore the exact saved
    // high-water marks last, never reconstruct them from surviving rows.
    tables.sort_by_key(|table| table.name == "sqlite_sequence");
    for table in tables {
        restore_table(&snapshot, &transaction, table)?;
    }
    for entry in blueprint.iter().filter(|entry| entry.kind != "table") {
        if let Some(sql) = &entry.sql {
            transaction.execute_batch(sql)?;
        }
    }
    let violations: i64 =
        transaction.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if violations != 0 {
        return Err(refused("restored source has foreign-key violations"));
    }
    let schema = export::schema_on(&transaction)?;
    let restored = export::export_on(&transaction, None, schema)?;
    if logical_manifest(restored) != logical_manifest(manifest.clone()) {
        return Err(refused("restored source differs from raw archive"));
    }
    transaction.commit()?;
    snapshot.commit()?;
    target.close().map_err(|(_, error)| error)?;
    fs::OpenOptions::new()
        .write(true)
        .open(&stage.path)?
        .sync_all()?;
    fs::hard_link(&stage.path, destination)?;
    Ok(manifest)
}

pub(super) fn source_blueprint(
    manifest: &ExportManifest,
) -> Result<Vec<SchemaEntry>, MigrationError> {
    if manifest.source_encoding != "UTF-8" {
        return Err(refused(
            "source layout profile requires UTF-8 SQLite encoding",
        ));
    }
    let reference = crate::SqliteStore::open_in_memory_with_host_path_identity(None)?;
    let mut blueprint = export::schema_on(&reference.connection)?;
    // The supported pre-migration source has none of these durable provenance
    // tables. A partially present set is not that profile and must refuse.
    if !manifest
        .tables
        .iter()
        .any(|table| super::schema::TABLES.contains(&table.name.as_str()))
    {
        blueprint.retain(|entry| !super::schema::TABLES.contains(&entry.table.as_str()));
    }
    if manifest.tables.iter().any(|table| {
        table.name == "work_root_executions"
            && table
                .columns
                .iter()
                .any(|column| column.name == "execution_json")
    }) {
        blueprint.retain(|entry| entry.table != "work_root_members");
        let root = blueprint
            .iter_mut()
            .find(|entry| entry.kind == "table" && entry.name == "work_root_executions")
            .ok_or_else(|| refused("compiled root schema is missing"))?;
        let sql = root
            .sql
            .as_mut()
            .ok_or_else(|| refused("compiled root DDL is missing"))?;
        let previous = "header_json BLOB NOT NULL,\n             head_hash TEXT NOT NULL REFERENCES objects(object_hash),";
        if !sql.contains(previous) {
            return Err(refused(
                "compiled root schema no longer supports this source profile",
            ));
        }
        *sql = sql.replace(previous, "execution_json BLOB NOT NULL,");
    }
    if logical_schema(&blueprint) != logical_schema(&manifest.schema) {
        let differences: Vec<_> = blueprint
            .iter()
            .filter(|expected| {
                !manifest.schema.iter().any(|actual| {
                    actual.kind == expected.kind
                        && actual.name == expected.name
                        && actual.table == expected.table
                        && actual.sql == expected.sql
                })
            })
            .map(|entry| format!("{}:{}", entry.kind, entry.name))
            .collect();
        let unexpected: Vec<_> = manifest
            .schema
            .iter()
            .filter(|actual| {
                !blueprint
                    .iter()
                    .any(|expected| expected.kind == actual.kind && expected.name == actual.name)
            })
            .map(|entry| format!("{}:{}", entry.kind, entry.name))
            .collect();
        return Err(refused(format!(
            "source schema is not an explicitly supported migration profile; differing or missing: {differences:?}; unexpected: {unexpected:?}"
        )));
    }
    Ok(blueprint)
}

pub(super) fn create_tables(
    target: &Connection,
    blueprint: &[SchemaEntry],
    tables: &[TableManifest],
) -> Result<(), MigrationError> {
    for entry in blueprint
        .iter()
        .filter(|entry| entry.kind == "table" && entry.name != "sqlite_sequence")
    {
        let table = tables
            .iter()
            .find(|table| table.name == entry.name)
            .ok_or_else(|| refused("missing table metadata"))?;
        if table.kind == "shadow" {
            continue;
        }
        target.execute_batch(
            entry
                .sql
                .as_deref()
                .ok_or_else(|| refused("missing table DDL"))?,
        )?;
    }
    Ok(())
}

fn restore_table(
    archive: &Connection,
    target: &Connection,
    table: &TableManifest,
) -> Result<(), MigrationError> {
    target.execute(&format!("DELETE FROM {}", quoted(&table.name)), [])?;
    let mut columns = Vec::new();
    if let Some(alias) = &table.rowid_alias {
        columns.push(quoted(alias));
    }
    columns.extend(
        table
            .columns
            .iter()
            .filter(|column| column.hidden == 0)
            .map(|column| quoted(&column.name)),
    );
    let placeholders = (1..=columns.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    let mut insert = target.prepare(&format!(
        "INSERT INTO {} ({}) VALUES ({placeholders})",
        quoted(&table.name),
        columns.join(", ")
    ))?;
    let mut query = archive
        .prepare("SELECT cells FROM migration_rows WHERE table_name = ?1 ORDER BY row_number")?;
    let mut selected = query.query([&table.name])?;
    let encoded_count = table
        .columns
        .iter()
        .filter(|column| column.hidden != 1)
        .count()
        + usize::from(table.rowid_alias.is_some());
    while let Some(row) = selected.next()? {
        let rusqlite::types::ValueRef::Blob(bytes) = row.get_ref(0)? else {
            return Err(refused("row cells are not a BLOB"));
        };
        let cells = rows::decode(bytes, encoded_count)?;
        let retained = table.rowid_alias.iter().map(|_| true).chain(
            table
                .columns
                .iter()
                .filter(|column| column.hidden != 1)
                .map(|column| column.hidden == 0),
        );
        insert.execute(params_from_iter(
            cells
                .iter()
                .zip(retained)
                .filter_map(|(cell, keep)| keep.then_some(cell)),
        ))?;
    }
    Ok(())
}

fn logical_schema(schema: &[SchemaEntry]) -> Vec<(String, String, String, Option<String>)> {
    schema
        .iter()
        .filter(|entry| !super::schema_compare::rebuildable(entry))
        .map(|entry| {
            (
                entry.kind.clone(),
                entry.name.clone(),
                entry.table.clone(),
                comparable_sql(entry),
            )
        })
        .collect()
}

fn logical_manifest(mut manifest: ExportManifest) -> ExportManifest {
    // Raw staging compares all table rows. Disposable index/trigger definitions
    // are reconstructed from the compiled schema, not source-format identity.
    manifest
        .schema
        .retain(|entry| entry.kind == "table" || !super::schema_compare::rebuildable(entry));
    for entry in &mut manifest.schema {
        entry.root_page = 0;
        entry.sql = comparable_sql(entry);
    }
    manifest
}

fn comparable_sql(entry: &SchemaEntry) -> Option<String> {
    entry
        .sql
        .as_ref()
        .map(|sql| serde_json::json!(super::schema_compare::tokens(sql)).to_string())
}
