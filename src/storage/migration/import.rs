//! Offline, no-replace importer for explicitly supported source profiles.

use std::{collections::HashMap, fs, path::Path};

use rusqlite::{Connection, Transaction, params};
use serde::Serialize;

use crate::storage::work::ROOT_DELTA_KIND;
use crate::{CanonicalObject, SqliteStore};

use super::{
    ConversionCounts, ExportManifest, MigrationError, convert, export,
    import_rows::CopyContext,
    profile::{MigrationProfile, TableDisposition},
    read_only, refused, restore, verify,
};

#[derive(Debug, Serialize)]
pub struct ImportedTable {
    pub name: String,
    pub disposition: TableDisposition,
}

#[derive(Debug, Serialize)]
pub struct ImportReport {
    pub profile: MigrationProfile,
    pub dispositions: Vec<ImportedTable>,
    pub source: ExportManifest,
    pub conversion: ConversionCounts,
    pub reexpressed_completion_results: i64,
    pub installed: bool,
}

/// Imports a supported current or aggregate-root archive into an absent file.
/// Full originals, operational state and private data stay inside the new store.
/// This is an offline same-host representation change, not portable authority.
///
/// # Errors
/// Refuses unsupported profiles, unknown tables, damaged history, missing
/// mappings or failed validation. No destination is published until the whole
/// import is validated.
pub fn import_archive(archive: &Path, destination: &Path) -> Result<ImportReport, MigrationError> {
    if destination.try_exists()? {
        return Err(refused("import destination already exists"));
    }
    let source_stage = export::StagedArchive::create(destination)?;
    // Remove only our just-reserved zero-byte file so the no-replace unpacker
    // can publish into this unique, private staging name.
    fs::remove_file(&source_stage.path)?;
    let unpacked_manifest = restore::restore_source_layout(archive, &source_stage.path)?;
    let archive_connection = read_only(archive)?;
    let archive_snapshot = archive_connection.unchecked_transaction()?;
    let manifest = verify::verify_on(&archive_snapshot)?;
    if manifest != unpacked_manifest {
        return Err(refused(
            "archive changed between source restoration and import",
        ));
    }
    restore::source_blueprint(&manifest)?;
    let profile = super::profile::detect_profile(&manifest)?;
    let dispositions = super::profile::table_dispositions(profile, &manifest)?;
    match profile {
        MigrationProfile::AggregateRootV1 => import_aggregate_on(
            &source_stage.path,
            archive_snapshot,
            manifest,
            dispositions,
            destination,
        ),
        MigrationProfile::Current => import_current_on(
            &source_stage.path,
            archive_snapshot,
            manifest,
            dispositions,
            destination,
        ),
    }
}

fn import_aggregate_on(
    unpacked: &Path,
    archive_snapshot: Transaction<'_>,
    manifest: ExportManifest,
    dispositions: Vec<(String, TableDisposition)>,
    destination: &Path,
) -> Result<ImportReport, MigrationError> {
    if !super::profile::is_aggregate_root(&manifest) {
        return Err(refused(
            "this importer accepts only the aggregate-root source profile",
        ));
    }
    let source = read_only(unpacked)?;
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
    let mut context = context(&archive_snapshot, &transaction)?;
    context.retain_rows(document.hash().as_str())?;
    context.copy_all(&manifest, &dispositions)?;
    super::delivery::record_attribution(&transaction, &manifest, document.hash().as_str())?;
    let reexpressed = context.reexpressed;
    drop(context);
    complete_scratch(transaction, &source_snapshot, &schema)?;
    source_snapshot.commit()?;
    archive_snapshot.commit()?;
    target.close().map_err(|(_, error)| error)?;
    publish_imported(
        &stage.path,
        destination,
        manifest,
        MigrationProfile::AggregateRootV1,
        dispositions,
        conversion,
        reexpressed,
    )
}

fn import_current_on(
    unpacked: &Path,
    archive_snapshot: Transaction<'_>,
    manifest: ExportManifest,
    dispositions: Vec<(String, TableDisposition)>,
    destination: &Path,
) -> Result<ImportReport, MigrationError> {
    let source = read_only(unpacked)?;
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
    let mut context = CopyContext {
        archive: &archive_snapshot,
        target: &transaction,
        mapping: HashMap::new(),
        heads: HashMap::new(),
        reexpressed: 0,
    };
    context.copy_all(&manifest, &dispositions)?;
    let reexpressed = context.reexpressed;
    drop(context);
    complete_scratch(transaction, &source_snapshot, &schema)?;
    source_snapshot.commit()?;
    archive_snapshot.commit()?;
    target.close().map_err(|(_, error)| error)?;
    publish_imported(
        &stage.path,
        destination,
        manifest,
        MigrationProfile::Current,
        dispositions,
        ConversionCounts {
            original_objects: 0,
            changed_objects: 0,
            generated_root_deltas: 0,
        },
        reexpressed,
    )
}

fn complete_scratch(
    transaction: Transaction<'_>,
    source_snapshot: &Transaction<'_>,
    schema: &[super::SchemaEntry],
) -> Result<(), MigrationError> {
    for entry in schema.iter().filter(|entry| entry.kind != "table") {
        if let Some(sql) = &entry.sql {
            transaction.execute_batch(sql)?;
        }
    }
    SqliteStore::rebuild_object_fts_from_heads_on(&transaction)?;
    transaction.execute("DELETE FROM work_catalog_fts", [])?;
    transaction.execute("INSERT INTO work_catalog_fts(work_id,search_text) SELECT work_id,search_text_key FROM work_items ORDER BY work_id",[])?;
    compare_fts(source_snapshot, &transaction)?;
    let violations: i64 =
        transaction.query_row("SELECT COUNT(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })?;
    if violations != 0 {
        return Err(refused("import has foreign-key violations"));
    }
    transaction.commit()?;
    Ok(())
}

fn publish_imported(
    stage: &Path,
    destination: &Path,
    manifest: ExportManifest,
    profile: MigrationProfile,
    dispositions: Vec<(String, TableDisposition)>,
    conversion: ConversionCounts,
    reexpressed: i64,
) -> Result<ImportReport, MigrationError> {
    // Ordinary strict opening and doctor are validation, not repair. The stored
    // host-path policy remains data; unresolved opening does not replace it.
    let validated = SqliteStore::open_unresolved(stage)?;
    let report = validated.verify_all()?;
    if !report.is_healthy() {
        return Err(refused(format!("import doctor refused: {report:?}")));
    }
    super::resolution::verify_pending_deliveries(&validated)?;
    validated
        .connection
        .execute_batch("PRAGMA wal_checkpoint(TRUNCATE); PRAGMA journal_mode=DELETE;")?;
    drop(validated);
    fs::OpenOptions::new().write(true).open(stage)?.sync_all()?;
    fs::hard_link(stage, destination)?;
    Ok(ImportReport {
        profile,
        dispositions: dispositions
            .into_iter()
            .map(|(name, disposition)| ImportedTable { name, disposition })
            .collect(),
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
        reexpressed: 0,
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
