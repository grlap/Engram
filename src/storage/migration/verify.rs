use std::{collections::BTreeSet, path::Path};

use rusqlite::Connection;
use sha2::{Digest, Sha256};

use super::{ExportManifest, MigrationError, TableManifest, export, read_only, refused, rows};

/// Checks archive structure, category coverage, cell frames, counts and bytes.
/// This detects damage relative to the embedded manifest. A separately anchored
/// source comparison is still required: rewriting both data and manifest can
/// make a self-consistent but incomplete archive. No domain import is implied.
///
/// # Errors
/// Refuses a different archive format, missing/extra categories or changed data.
pub fn verify_export(path: &Path) -> Result<ExportManifest, MigrationError> {
    let archive = read_only(path)?;
    let snapshot = archive.unchecked_transaction()?;
    let manifest = verify_on(&snapshot)?;
    snapshot.commit()?;
    Ok(manifest)
}

pub(super) fn verify_on(snapshot: &Connection) -> Result<ExportManifest, MigrationError> {
    verify_schema(snapshot)?;
    let (document, identity): (Vec<u8>, String) = snapshot.query_row(
        "SELECT document, document_sha256 FROM migration_manifest WHERE singleton = 1 AND length(document) <= 16777216",
        [], |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if format!("{:x}", Sha256::digest(&document)) != identity {
        return Err(refused("manifest content identity differs"));
    }
    let manifest: ExportManifest = serde_json::from_slice(&document)?;
    if manifest.format != "engram-full-sqlite-export" || manifest.version != 1 {
        return Err(refused("unsupported migration archive format"));
    }
    verify_coverage(snapshot, &manifest)?;
    for table in &manifest.tables {
        verify_table(snapshot, table)?;
    }
    Ok(manifest)
}

fn verify_schema(archive: &Connection) -> Result<(), MigrationError> {
    let reference = Connection::open_in_memory()?;
    reference.execute_batch(export::ARCHIVE_SCHEMA)?;
    let logical = |connection: &Connection| -> Result<_, MigrationError> {
        Ok(export::schema_on(connection)?
            .into_iter()
            .map(|entry| (entry.kind, entry.name, entry.table, entry.sql))
            .collect::<Vec<_>>())
    };
    if logical(archive)? != logical(&reference)? {
        return Err(refused("archive schema differs from this format"));
    }
    Ok(())
}

fn verify_coverage(archive: &Connection, manifest: &ExportManifest) -> Result<(), MigrationError> {
    let declared: BTreeSet<_> = manifest
        .schema
        .iter()
        .filter(|entry| entry.kind == "table")
        .map(|entry| entry.name.as_str())
        .collect();
    let exported: BTreeSet<_> = manifest
        .tables
        .iter()
        .map(|table| table.name.as_str())
        .collect();
    if declared != exported || exported.len() != manifest.tables.len() {
        return Err(refused("table category set differs from source schema"));
    }
    let populated: Vec<String> = archive
        .prepare("SELECT DISTINCT table_name FROM migration_rows ORDER BY table_name")?
        .query_map([], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    let expected: BTreeSet<_> = manifest
        .tables
        .iter()
        .filter(|table| table.rows > 0)
        .map(|table| table.name.as_str())
        .collect();
    if populated
        .iter()
        .map(String::as_str)
        .collect::<BTreeSet<_>>()
        != expected
    {
        return Err(refused("populated category set differs from manifest"));
    }
    let empty: Vec<_> = manifest
        .tables
        .iter()
        .filter(|table| table.rows == 0)
        .map(|table| table.name.clone())
        .collect();
    let total = manifest.tables.iter().try_fold(0_u64, |sum, table| {
        sum.checked_add(table.rows)
            .ok_or_else(|| refused("total row count overflow"))
    })?;
    if empty != manifest.empty_tables || total != manifest.total_rows {
        return Err(refused("manifest totals differ"));
    }
    Ok(())
}

fn verify_table(archive: &Connection, table: &TableManifest) -> Result<(), MigrationError> {
    let count = table
        .columns
        .iter()
        .filter(|column| column.hidden != 1)
        .count()
        + usize::from(table.rowid_alias.is_some());
    let mut query = archive.prepare(
        "SELECT row_number, cells FROM migration_rows WHERE table_name = ?1 ORDER BY row_number",
    )?;
    let mut selected = query.query([&table.name])?;
    let mut position = 0_i64;
    let mut length = 0_u64;
    let mut digest = Sha256::new();
    while let Some(row) = selected.next()? {
        position = position
            .checked_add(1)
            .ok_or_else(|| refused("row count overflow"))?;
        if row.get::<_, i64>(0)? != position {
            return Err(refused(format!("row position gap in {}", table.name)));
        }
        let rusqlite::types::ValueRef::Blob(cells) = row.get_ref(1)? else {
            return Err(refused("archive row is not a BLOB"));
        };
        rows::validate(cells, count)?;
        let size = u64::try_from(cells.len()).map_err(|_| refused("row length overflow"))?;
        length = length
            .checked_add(size)
            .ok_or_else(|| refused("table length overflow"))?;
        digest.update(size.to_be_bytes());
        digest.update(cells);
    }
    let rows = u64::try_from(position).map_err(|_| refused("invalid row count"))?;
    if rows != table.rows
        || length != table.encoded_bytes
        || format!("{:x}", digest.finalize()) != table.rows_sha256
    {
        return Err(refused(format!(
            "row content differs from manifest in {}",
            table.name
        )));
    }
    Ok(())
}

/// Compares a verified export to a separate read cut of the complete source.
/// Keep the source snapshot fixed: live changes after export cause refusal.
/// This anchors category and byte coverage outside the editable archive.
///
/// # Errors
/// Refuses any metadata, category or typed-row difference, including an export
/// whose data and embedded manifest were both consistently truncated.
pub fn compare_export_to_source(
    source: &Path,
    archive: &Path,
) -> Result<ExportManifest, MigrationError> {
    let exported = verify_export(archive)?;
    let source = read_only(source)?;
    let snapshot = source.unchecked_transaction()?;
    let schema = export::schema_on(&snapshot)?;
    let observed = export::export_on(&snapshot, None, schema)?;
    if observed != exported {
        return Err(refused(
            "export differs from the independently read source cut",
        ));
    }
    snapshot.commit()?;
    Ok(exported)
}
