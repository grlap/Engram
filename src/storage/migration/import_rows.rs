//! Copy operational state, changing only declared canonical references and shapes.

use std::collections::HashMap;

use rusqlite::{
    Connection, ToSql, params, params_from_iter,
    types::{ToSqlOutput, Value, ValueRef},
};
use serde_json::Value as Json;

use crate::{CanonicalObject, CompletionSeal, ObjectHash, RootExecution, WorkRun};

use super::{ExportManifest, MigrationError, TableManifest, quoted, refused, roots, rows};

#[cfg(test)]
mod tests;

pub(super) struct CopyContext<'a> {
    pub archive: &'a Connection,
    pub target: &'a Connection,
    pub mapping: HashMap<String, String>,
    pub heads: HashMap<String, (String, crate::domain::RootExecutionDelta)>,
}

enum Cell<'a> {
    Raw(rows::Cell<'a>),
    Edited(Value),
}

impl Cell<'_> {
    fn value(&self) -> ValueRef<'_> {
        match self {
            Self::Raw(cell) => cell.0,
            Self::Edited(value) => value.into(),
        }
    }
    fn text(&self) -> Result<&str, MigrationError> {
        self.value()
            .as_str()
            .map_err(|_| refused("expected a text field"))
    }
    fn bytes(&self) -> Result<&[u8], MigrationError> {
        match self.value() {
            ValueRef::Text(bytes) | ValueRef::Blob(bytes) => Ok(bytes),
            _ => Err(refused("expected JSON bytes")),
        }
    }
    fn replace_json(&mut self, bytes: Vec<u8>) -> Result<(), MigrationError> {
        *self = match self.value() {
            ValueRef::Blob(_) => Self::Edited(Value::Blob(bytes)),
            ValueRef::Text(_) => Self::Edited(Value::Text(
                String::from_utf8(bytes).map_err(|_| refused("encoded JSON is not UTF-8"))?,
            )),
            _ => return Err(refused("JSON source cell is neither text nor blob")),
        };
        Ok(())
    }
}

impl ToSql for Cell<'_> {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Borrowed(self.value()))
    }
}

impl CopyContext<'_> {
    pub(super) fn retain_rows(&self, source_id: &str) -> Result<(), MigrationError> {
        let mut query = self.archive.prepare("SELECT table_name,row_number,cells FROM migration_rows WHERE table_name != 'objects' ORDER BY table_name,row_number")?;
        let mut rows = query.query([])?;
        let mut insert = self
            .target
            .prepare("INSERT INTO migration_original_rows VALUES (?1,?2,?3,?4)")?;
        while let Some(row) = rows.next()? {
            insert.execute(params![
                source_id,
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Vec<u8>>(2)?
            ])?;
        }
        Ok(())
    }

    pub(super) fn copy_all(&self, manifest: &ExportManifest) -> Result<(), MigrationError> {
        let mut tables: Vec<_> = manifest
            .tables
            .iter()
            .filter(|table| table.kind == "table" && table.name != "objects")
            .collect();
        tables.sort_by_key(|table| table.name == "sqlite_sequence");
        for table in tables {
            if super::schema::TABLES.contains(&table.name.as_str()) {
                if table.rows != 0 {
                    return Err(refused(
                        "this source profile does not accept an already migrated store",
                    ));
                }
                continue;
            }
            self.copy_table(table)?;
        }
        Ok(())
    }

    fn mapped(&self, hash: &str) -> Result<&str, MigrationError> {
        self.mapping
            .get(hash)
            .map(String::as_str)
            .ok_or_else(|| refused(format!("missing canonical row mapping for {hash}")))
    }

    fn copy_table(&self, table: &TableManifest) -> Result<(), MigrationError> {
        if table.columns.iter().any(|column| column.hidden != 0) {
            return Err(refused("unsupported generated operational column"));
        }
        if table.name == "sqlite_sequence" {
            self.target.execute("DELETE FROM sqlite_sequence", [])?;
        }
        let mut names: Vec<_> = table.rowid_alias.iter().cloned().collect();
        names.extend(table.columns.iter().map(|column| column.name.clone()));
        let count = names.len();
        let position = |name: &str| {
            names
                .iter()
                .position(|column| column == name)
                .ok_or_else(|| refused(format!("missing column {}.{name}", table.name)))
        };
        let mut query = self
            .archive
            .prepare("SELECT cells FROM migration_rows WHERE table_name=?1 ORDER BY row_number")?;
        let mut rows = query.query([&table.name])?;
        while let Some(row) = rows.next()? {
            let encoded: Vec<u8> = row.get(0)?;
            let mut cells: Vec<_> = rows::decode(&encoded, count)?
                .into_iter()
                .map(Cell::Raw)
                .collect();
            // Check the source projection before its hash or body is replaced.
            // Comparing only the converted row would silently repair damage.
            if table.name == "work_completion_seals" {
                self.validate_source_seal(
                    cells[position("seal_hash")?].text()?,
                    cells[position("seal_json")?].bytes()?,
                )?;
            }
            for foreign in &table.foreign_keys {
                if foreign.target_table == "objects" {
                    let index = position(&foreign.from_column)?;
                    if cells[index].value() != ValueRef::Null {
                        cells[index] =
                            Cell::Edited(Value::Text(self.mapped(cells[index].text()?)?.into()));
                    }
                }
            }
            if table.name == "work_root_executions" {
                self.copy_root(&cells, &position)?;
                continue;
            }
            match table.name.as_str() {
                "work_completion_seals" => {
                    let hash = cells[position("seal_hash")?].text()?;
                    let bytes = self.object_bytes(hash)?;
                    cells[position("seal_json")?].replace_json(bytes)?;
                }
                "work_runs" => {
                    let index = position("run_json")?;
                    let original: Json = serde_json::from_slice(cells[index].bytes()?)?;
                    let mut run: WorkRun = super::transform::strict(&original)?;
                    for hash in [&mut run.last_checkpoint, &mut run.completion_seal]
                        .into_iter()
                        .flatten()
                    {
                        *hash = self
                            .mapped(hash.as_str())?
                            .parse()
                            .map_err(|_| refused("invalid mapped run address"))?;
                    }
                    cells[index].replace_json(serde_json::to_vec(&run)?)?;
                }
                "work_operation_results"
                    if cells[position("operation")?].text()? == "complete_work" =>
                {
                    let key = cells[position("idempotency_key")?].text()?.to_owned();
                    self.reexpress_completion(&key, &mut cells[position("result_json")?])?;
                }
                _ => {}
            }
            let sql = format!(
                "INSERT INTO {} ({}) VALUES ({})",
                quoted(&table.name),
                names
                    .iter()
                    .map(|name| quoted(name))
                    .collect::<Vec<_>>()
                    .join(","),
                vec!["?"; count].join(",")
            );
            self.target.execute(&sql, params_from_iter(cells.iter()))?;
        }
        Ok(())
    }

    fn object_bytes(&self, hash: &str) -> Result<Vec<u8>, MigrationError> {
        Ok(self.target.query_row(
            "SELECT canonical_json FROM objects WHERE object_hash=?1",
            [hash],
            |row| row.get(0),
        )?)
    }

    fn validate_source_seal(&self, hash: &str, bytes: &[u8]) -> Result<(), MigrationError> {
        let (kind, original): (String, Vec<u8>) = self.target.query_row(
            "SELECT object_kind,canonical_json FROM migration_original_objects WHERE object_hash=?1",
            [hash],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let address = hash
            .parse()
            .map_err(|_| refused("invalid source seal address"))?;
        let canonical = CanonicalObject::verify(&address, original)?;
        let projection: Json = serde_json::from_slice(bytes)?;
        if kind != "completion_seal" || canonical.decode::<Json>()? != projection {
            return Err(refused(
                "source completion seal projection differs from its canonical object",
            ));
        }
        Ok(())
    }

    fn reexpress_completion(&self, key: &str, cell: &mut Cell<'_>) -> Result<(), MigrationError> {
        let original = cell.bytes()?.to_vec();
        let source: Json = serde_json::from_slice(&original)?;
        let source_seal = CanonicalObject::freeze(&source)?;
        let target_seal = self.mapped(source_seal.hash().as_str())?;
        let target_bytes = self.object_bytes(target_seal)?;
        let seal: CompletionSeal = serde_json::from_slice(&target_bytes)?;
        // This is the sole by-value replay exception. Full original bytes and
        // the per-key decision remain inspectable, not just their digests.
        self.target.execute("INSERT INTO migration_reexpressed_results VALUES (?1,'complete_work',?2,?3,?4,?5,?6,?7,?8)",params![seal.root_execution.project_id.0,key,original,ObjectHash::from_canonical_bytes(&original).as_str(),target_bytes,ObjectHash::from_canonical_bytes(&target_bytes).as_str(),source_seal.hash().as_str(),target_seal])?;
        cell.replace_json(target_bytes)
    }

    fn copy_root(
        &self,
        cells: &[Cell<'_>],
        position: &dyn Fn(&str) -> Result<usize, MigrationError>,
    ) -> Result<(), MigrationError> {
        let value: Json = serde_json::from_slice(cells[position("execution_json")?].bytes()?)?;
        let mut root: RootExecution = super::transform::strict(&value)?;
        let state: String = serde_json::from_value(serde_json::to_value(root.state)?)?;
        // The full source state is separately bound to canonical event history.
        // Every duplicated scalar must agree before deriving its new row.
        let generation = root.generation;
        let revision = root.revision;
        for (name, expected) in [
            (
                "root_execution_id",
                Value::Text(root.root_execution_id.0.to_string()),
            ),
            ("project_id", Value::Text(root.project_id.0.clone())),
            ("root_id", Value::Text(root.root_id.0.to_string())),
            ("generation", Value::Integer(generation)),
            ("state", Value::Text(state.clone())),
            ("revision", Value::Integer(revision)),
            (
                "created_at_ms",
                Value::Integer(root.created_at.timestamp_millis()),
            ),
            (
                "updated_at_ms",
                Value::Integer(root.updated_at.timestamp_millis()),
            ),
        ] {
            if cells[position(name)?].value() != ValueRef::from(&expected) {
                return Err(refused(format!(
                    "source root projection column differs from its full state: {name}"
                )));
            }
        }
        super::transform::map_root_references(&mut root, &mut |hash| {
            self.mapped(hash.as_str())?
                .parse()
                .map_err(|_| refused("invalid mapped root member"))
        })?;
        let header = roots::header(&root);
        roots::normalize(&mut root)?;
        let (head, delta) = self
            .heads
            .get(&root.root_execution_id.0.to_string())
            .ok_or_else(|| refused("current root has no converted head"))?;
        if delta.header != header {
            return Err(refused(
                "converted current root does not match its last observed header",
            ));
        }
        if CanonicalObject::freeze(&root)?.hash() != &delta.state_checksum {
            return Err(refused(
                "converted current root accounting differs from its head",
            ));
        }
        self.target.execute("INSERT INTO work_root_executions(root_execution_id,project_id,root_id,generation,state,revision,created_at_ms,updated_at_ms,header_json,head_hash) VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9,?10)",params![root.root_execution_id.0.to_string(),root.project_id.0,root.root_id.0.to_string(),generation,state,revision,root.created_at.timestamp_millis(),root.updated_at.timestamp_millis(),serde_json::to_vec(&header)?,head])?;
        for (hash, member) in roots::members(&root)? {
            self.target.execute(
                "INSERT INTO work_root_members VALUES (?1,?2,?3)",
                params![
                    root.root_execution_id.0.to_string(),
                    hash.as_str(),
                    serde_json::to_vec(&member)?
                ],
            )?;
        }
        Ok(())
    }
}
