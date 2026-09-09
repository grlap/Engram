//! Full provenance audit. Ordinary opens do not scan original history.

use rusqlite::{Connection, OptionalExtension, params};
use sha2::{Digest, Sha256};

use crate::{CanonicalObject, ObjectHash, StoreError};

use super::{ExportManifest, TableManifest, rows};

#[cfg(test)]
mod tests;

fn invalid(reason: &str) -> StoreError {
    StoreError::InvalidWorkProjection(format!("migration provenance: {reason}"))
}

pub(in crate::storage) fn verify_provenance_on(connection: &Connection) -> Result<(), StoreError> {
    let documents: Vec<(String,String,Vec<u8>,String)> = connection.prepare("SELECT source_id,profile,document,document_hash FROM migration_source_manifest ORDER BY source_id")?.query_map([],|row| Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)))?.collect::<Result<_,_>>()?;
    if documents.is_empty() {
        for table in super::schema::TABLES
            .iter()
            .filter(|table| **table != "migration_source_manifest")
        {
            let populated: bool = connection.query_row(
                &format!("SELECT EXISTS(SELECT 1 FROM {})", super::quoted(table)),
                [],
                |row| row.get(0),
            )?;
            if populated {
                return Err(invalid("retained provenance has no source manifest"));
            }
        }
        return Ok(());
    }
    // Later explicit importers must compose and preserve prior sources. This
    // build does not invent validation rules for an unimplemented next profile.
    if documents.len() != 1 {
        return Err(invalid("unsupported migration source history"));
    }
    for (source_id, profile, document, hash) in documents {
        if source_id != hash || profile != "aggregate-root-v1" {
            return Err(invalid("unsupported or misbound source manifest"));
        }
        let hash: ObjectHash = hash
            .parse()
            .map_err(|_| invalid("invalid manifest address"))?;
        let manifest: ExportManifest = CanonicalObject::verify(&hash, document)?.decode()?;
        let mut total_objects = None;
        for table in &manifest.tables {
            if table.name == "objects" {
                verify_original_objects(connection, table)?;
                total_objects = Some(table.rows);
            } else {
                verify_original_rows(connection, &source_id, table)?;
            }
        }
        super::delivery::verify_attribution(connection, &manifest, &source_id)?;
        let mapped: i64 =
            connection.query_row("SELECT COUNT(*) FROM migration_object_map", [], |row| {
                row.get(0)
            })?;
        if u64::try_from(mapped).ok() != total_objects {
            return Err(invalid("object map is not total over the source set"));
        }
        let mut query = connection.prepare("SELECT object_hash FROM migration_original_objects")?;
        for hash in query.query_map([], |row| row.get::<_, String>(0))? {
            let hash: ObjectHash = hash?
                .parse()
                .map_err(|_| invalid("invalid original address"))?;
            super::resolution::resolve_on(connection, &hash)?;
        }
        let mut query = connection.prepare(
            "SELECT operation,idempotency_key,target_result FROM migration_reexpressed_results",
        )?;
        let mut results = query.query([])?;
        while let Some(row) = results.next()? {
            let operation: String = row.get(0)?;
            let key: String = row.get(1)?;
            let target: Vec<u8> = row.get(2)?;
            super::resolution::validate_reexpressed_result_on(
                connection, &operation, &key, &target,
            )?;
            let stored: Vec<u8> = connection.query_row("SELECT result_json FROM work_operation_results WHERE operation=?1 AND idempotency_key=?2",params![operation,key],|row|row.get(0))?;
            if stored != target {
                return Err(invalid("operation replay differs from its migration audit"));
            }
        }
        let extra: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM migration_original_rows WHERE source_id != ?1)",
            [&source_id],
            |row| row.get(0),
        )?;
        if extra {
            return Err(invalid("retained rows cite an unknown source"));
        }
        let mut names = connection.prepare(
            "SELECT DISTINCT table_name FROM migration_original_rows WHERE source_id=?1",
        )?;
        for name in names.query_map([&source_id], |row| row.get::<_, String>(0))? {
            let name = name?;
            if name == "objects" || !manifest.tables.iter().any(|table| table.name == name) {
                return Err(invalid(
                    "retained row category is absent from the source manifest",
                ));
            }
        }
    }
    Ok(())
}

struct RowDigest {
    hash: Sha256,
    count: u64,
    bytes: u64,
}

impl RowDigest {
    fn new() -> Self {
        Self {
            hash: Sha256::new(),
            count: 0,
            bytes: 0,
        }
    }
    fn row(&mut self, bytes: &[u8]) -> Result<(), StoreError> {
        let length = u64::try_from(bytes.len()).map_err(|_| invalid("row length overflow"))?;
        self.hash.update(length.to_be_bytes());
        self.hash.update(bytes);
        self.count = self
            .count
            .checked_add(1)
            .ok_or_else(|| invalid("row count overflow"))?;
        self.bytes = self
            .bytes
            .checked_add(length)
            .ok_or_else(|| invalid("row bytes overflow"))?;
        Ok(())
    }
    fn check(self, table: &TableManifest) -> Result<(), StoreError> {
        if self.count != table.rows
            || self.bytes != table.encoded_bytes
            || format!("{:x}", self.hash.finalize()) != table.rows_sha256
        {
            return Err(invalid(&format!(
                "retained {} rows differ from the source manifest",
                table.name
            )));
        }
        Ok(())
    }
}

fn verify_original_objects(
    connection: &Connection,
    table: &TableManifest,
) -> Result<(), StoreError> {
    if table.rowid_alias.is_none()
        || table
            .columns
            .iter()
            .map(|column| column.name.as_str())
            .collect::<Vec<_>>()
            != ["object_hash", "object_kind", "canonical_json", "created_at"]
    {
        return Err(invalid("unsupported source object table layout"));
    }
    let mut query = connection.prepare("SELECT source_rowid,object_hash,object_kind,canonical_json,created_at FROM migration_original_objects ORDER BY source_rowid")?;
    let mut selected = query.query([])?;
    let mut digest = RowDigest::new();
    while let Some(row) = selected.next()? {
        let hash: ObjectHash = row
            .get::<_, String>(1)?
            .parse()
            .map_err(|_| invalid("invalid original address"))?;
        CanonicalObject::verify(&hash, row.get(3)?)?;
        digest.row(&rows::encode(row, 5).map_err(|error| invalid(&error.to_string()))?)?;
    }
    digest.check(table)
}

fn verify_original_rows(
    connection: &Connection,
    source: &str,
    table: &TableManifest,
) -> Result<(), StoreError> {
    let mut query = connection.prepare("SELECT row_number,cells FROM migration_original_rows WHERE source_id=?1 AND table_name=?2 ORDER BY row_number")?;
    let mut selected = query.query(params![source, table.name])?;
    let mut digest = RowDigest::new();
    let count = usize::from(table.rowid_alias.is_some())
        + table
            .columns
            .iter()
            .filter(|column| column.hidden != 1)
            .count();
    let mut completions = 0_i64;
    while let Some(row) = selected.next()? {
        let position: i64 = row.get(0)?;
        let cells: Vec<u8> = row.get(1)?;
        rows::validate(&cells, count).map_err(|error| invalid(&error.to_string()))?;
        if table.name == "work_operation_results"
            && verify_original_completion(connection, table, &cells, count)?
        {
            completions += 1;
        }
        digest.row(&cells)?;
        if u64::try_from(position).ok() != Some(digest.count) {
            return Err(invalid("retained row positions are not dense"));
        }
    }
    if table.name == "work_operation_results" {
        let audited: i64 = connection.query_row(
            "SELECT COUNT(*) FROM migration_reexpressed_results",
            [],
            |row| row.get(0),
        )?;
        if audited != completions {
            return Err(invalid(
                "per-key replay audits do not cover the original completion set",
            ));
        }
    }
    digest.check(table)
}

fn verify_original_completion(
    connection: &Connection,
    table: &TableManifest,
    bytes: &[u8],
    count: usize,
) -> Result<bool, StoreError> {
    let cells = rows::decode(bytes, count).map_err(|error| invalid(&error.to_string()))?;
    let position = |name: &str| {
        table
            .columns
            .iter()
            .filter(|column| column.hidden != 1)
            .position(|column| column.name == name)
            .map(|index| index + usize::from(table.rowid_alias.is_some()))
            .ok_or_else(|| invalid("original replay table lacks a required column"))
    };
    let operation = cells[position("operation")?]
        .0
        .as_str()
        .map_err(|_| invalid("original operation is not text"))?;
    if operation != "complete_work" {
        return Ok(false);
    }
    let key = cells[position("idempotency_key")?]
        .0
        .as_str()
        .map_err(|_| invalid("original replay key is not text"))?;
    let (rusqlite::types::ValueRef::Blob(original) | rusqlite::types::ValueRef::Text(original)) =
        cells[position("result_json")?].0
    else {
        return Err(invalid("original replay result is not JSON bytes"));
    };
    let audited: Option<Vec<u8>> = connection.query_row("SELECT source_result FROM migration_reexpressed_results WHERE operation=?1 AND idempotency_key=?2", params![operation,key], |row|row.get(0)).optional()?;
    if audited.as_deref() != Some(original) {
        return Err(invalid(
            "original completion has no exact per-key replay audit",
        ));
    }
    Ok(true)
}
