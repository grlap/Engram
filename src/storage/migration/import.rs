//! Offline, no-replace importer for the explicit aggregate-root source profile.

use std::{collections::HashMap, fs, path::Path};

use rusqlite::{Connection, params};
use serde::Serialize;

use crate::storage::work::ROOT_DELTA_KIND;
use crate::{CanonicalObject, SqliteStore};

use super::{
    ConversionCounts, ExportManifest, MigrationError, convert, export, import_rows::CopyContext,
    read_only, refused, restore, verify,
};

#[derive(Debug, Serialize)]
pub struct ImportReport {
    pub source: ExportManifest,
    pub conversion: ConversionCounts,
    pub reexpressed_completion_results: i64,
    pub installed: bool,
}

/// Imports the known aggregate-root profile into an absent file, never the source.
/// Full originals, operational state and private data stay inside the new store.
/// This is an offline same-host representation change, not portable authority.
///
/// # Errors
/// Refuses unsupported profiles, damaged history, missing mappings or failed
/// validation. No destination is published until the whole import is validated.
pub fn import_aggregate_archive(
    archive: &Path,
    destination: &Path,
) -> Result<ImportReport, MigrationError> {
    if destination.try_exists()? {
        return Err(refused("import destination already exists"));
    }
    let source_stage = export::StagedArchive::create(destination)?;
    // Remove only our just-reserved zero-byte file so the no-replace unpacker
    // can publish into this unique, private staging name.
    fs::remove_file(&source_stage.path)?;
    let unpacked_manifest = restore::restore_source_layout(archive, &source_stage.path)?;
    let archive = read_only(archive)?;
    let archive_snapshot = archive.unchecked_transaction()?;
    let manifest = verify::verify_on(&archive_snapshot)?;
    if manifest != unpacked_manifest {
        return Err(refused(
            "archive changed between source restoration and import",
        ));
    }
    restore::source_blueprint(&manifest)?;
    if !manifest.tables.iter().any(|table| {
        table.name == "work_root_executions"
            && table
                .columns
                .iter()
                .any(|column| column.name == "execution_json")
    }) {
        return Err(refused(
            "this importer accepts only the aggregate-root source profile",
        ));
    }
    let source = read_only(&source_stage.path)?;
    let source_snapshot = source.unchecked_transaction()?;
    let stage = export::StagedArchive::create(destination)?;
    let mut target = Connection::open(&stage.path)?;
    target.execute_batch(
        "PRAGMA journal_mode=DELETE; PRAGMA synchronous=FULL; PRAGMA foreign_keys=OFF;",
    )?;
    target.pragma_update(None, "user_version", manifest.source_user_version)?;
    target.pragma_update(None, "application_id", manifest.source_application_id)?;
    let transaction = target.transaction()?;
    let template = SqliteStore::open_in_memory_with_host_path_identity(None)?;
    let schema = export::schema_on(&template.connection)?;
    let current = export::export_on(&template.connection, None, schema.clone())?;
    restore::create_tables(&transaction, &schema, &current.tables)?;
    let conversion = convert::convert_on(&source_snapshot, &transaction)?;
    let document = CanonicalObject::freeze(&manifest)?;
    transaction.execute(
        "INSERT INTO migration_source_manifest VALUES (?1,'aggregate-root-v1',?2,?1)",
        params![document.hash().as_str(), document.bytes()],
    )?;
    let context = context(&archive_snapshot, &transaction)?;
    context.retain_rows(document.hash().as_str())?;
    context.copy_all(&manifest)?;
    super::delivery::record_attribution(&transaction, &manifest, document.hash().as_str())?;
    for entry in schema.iter().filter(|entry| entry.kind != "table") {
        if let Some(sql) = &entry.sql {
            transaction.execute_batch(sql)?;
        }
    }
    SqliteStore::rebuild_object_fts_from_heads_on(&transaction)?;
    transaction.execute("INSERT INTO work_catalog_fts(work_id,search_text) SELECT work_id,search_text_key FROM work_items ORDER BY work_id",[])?;
    compare_fts(&source_snapshot, &transaction)?;
    let violations: i64 =
        transaction.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if violations != 0 {
        return Err(refused("import has foreign-key violations"));
    }
    let reexpressed = transaction.query_row(
        "SELECT COUNT(*) FROM migration_reexpressed_results",
        [],
        |row| row.get(0),
    )?;
    transaction.commit()?;
    source_snapshot.commit()?;
    archive_snapshot.commit()?;
    target.close().map_err(|(_, error)| error)?;
    // Ordinary strict opening and doctor are validation, not repair. The stored
    // host-path policy remains data; unresolved opening does not replace it.
    let validated = SqliteStore::open_unresolved(&stage.path)?;
    let report = validated.verify_all()?;
    if !report.is_healthy() {
        return Err(refused(format!("import doctor refused: {report:?}")));
    }
    super::resolution::verify_pending_deliveries(&validated)?;
    validated
        .connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")?;
    drop(validated);
    fs::OpenOptions::new()
        .write(true)
        .open(&stage.path)?
        .sync_all()?;
    fs::hard_link(&stage.path, destination)?;
    Ok(ImportReport {
        source: manifest,
        conversion,
        reexpressed_completion_results: reexpressed,
        installed: false,
    })
}

fn context<'a>(
    archive: &'a Connection,
    target: &'a Connection,
) -> Result<CopyContext<'a>, MigrationError> {
    let mapping = target
        .prepare("SELECT source_hash,target_hash FROM migration_object_map")?
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<_, _>>()?;
    let mut heads: HashMap<String, (String, crate::domain::RootExecutionDelta)> = HashMap::new();
    let mut query =
        target.prepare("SELECT object_hash,canonical_json FROM objects WHERE object_kind=?1")?;
    let mut rows = query.query([ROOT_DELTA_KIND])?;
    while let Some(row) = rows.next()? {
        let hash: String = row.get(0)?;
        let delta: crate::domain::RootExecutionDelta =
            serde_json::from_slice(&row.get::<_, Vec<u8>>(1)?)?;
        let key = delta.header.root_execution_id.0.to_string();
        if heads
            .get(&key)
            .is_none_or(|(_, head)| head.sequence < delta.sequence)
        {
            heads.insert(key, (hash, delta));
        }
    }
    Ok(CopyContext {
        archive,
        target,
        mapping,
        heads,
    })
}

fn compare_fts(source: &Connection, target: &Connection) -> Result<(), MigrationError> {
    for sql in [
        "SELECT object_hash,title,body FROM object_fts ORDER BY object_hash",
        "SELECT work_id,search_text FROM work_catalog_fts ORDER BY work_id",
    ] {
        let mut left = source.prepare(sql)?;
        let mut right = target.prepare(sql)?;
        let count = left.column_count();
        let mut left = left.query([])?;
        let mut right = right.query([])?;
        loop {
            match (left.next()?, right.next()?) {
                (None, None) => break,
                (Some(left), Some(right))
                    if super::rows::encode(left, count)? == super::rows::encode(right, count)? => {}
                _ => return Err(refused("rebuilt FTS logical contents differ from source")),
            }
        }
    }
    Ok(())
}
