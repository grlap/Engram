//! Ordered conversion of the supported canonical history, with complete originals.

use std::{collections::HashMap, path::Path};

use rusqlite::{Connection, params};
use serde::Serialize;
use serde_json::Value;

use crate::domain::RootExecutionRef;
use crate::storage::work::ROOT_DELTA_KIND;
use crate::{CanonicalObject, ObjectHash, RootExecution};

use super::{MigrationError, RootHistoryEncoder, predecessors, read_only, refused, transform};

#[cfg(test)]
pub(super) mod tests;

#[derive(Debug, Serialize)]
pub struct ConversionCounts {
    pub original_objects: usize,
    pub changed_objects: usize,
    pub generated_root_deltas: usize,
}

/// Converts canonical objects into an empty current-format scratch store.
/// This is one importer phase, NOT a complete migrated or usable store.
/// Operational rows, projections and resume validation remain separate phases.
///
/// # Errors
/// Refuses populated targets, unknown or non-causal references and missing exact
/// predecessors. The target transaction rolls back on every conversion failure.
pub fn convert_aggregate_store_objects(
    source: &Path,
    target: &mut Connection,
) -> Result<ConversionCounts, MigrationError> {
    let source = read_only(source)?;
    let snapshot = source.unchecked_transaction()?;
    let transaction = target.transaction()?;
    let count: i64 = transaction.query_row("SELECT COUNT(*) FROM objects", [], |row| row.get(0))?;
    if count != 0 {
        return Err(refused("canonical conversion requires an empty target"));
    }
    let result = convert_on(&snapshot, &transaction)?;
    snapshot.commit()?;
    transaction.commit()?;
    Ok(result)
}

struct Converter<'a> {
    source: &'a Connection,
    target: &'a Connection,
    kinds: HashMap<ObjectHash, String>,
    mapped: HashMap<ObjectHash, ObjectHash>,
    generated: usize,
}

pub(super) fn convert_on(
    source: &Connection,
    target: &Connection,
) -> Result<ConversionCounts, MigrationError> {
    let plan = predecessors::inspect_on(source)?;
    let bindings: HashMap<_, _> = plan
        .bindings
        .into_iter()
        .map(|binding| (binding.completed_event.clone(), binding))
        .collect();
    let mut converter = Converter {
        source,
        target,
        kinds: HashMap::new(),
        mapped: HashMap::new(),
        generated: 0,
    };
    converter.retain_originals()?;
    let mut encoders: HashMap<String, RootHistoryEncoder> = HashMap::new();
    let mut observed: HashMap<ObjectHash, RootExecutionRef> = HashMap::new();
    let mut statement = source.prepare("SELECT f.object_hash FROM work_feed_entries f WHERE f.feed_kind='project' AND f.object_kind='work_event' ORDER BY f.feed_id,f.position")?;
    let mut rows = statement.query([])?;
    while let Some(row) = rows.next()? {
        let hash = address(&row.get::<_, String>(0)?)?;
        let (event, created) = converter.load(&hash, "work_event")?;
        if let Some(binding) = bindings.get(&hash) {
            let pre = observed
                .get(&binding.pre_event)
                .ok_or_else(|| refused("pre-seal event was not converted before completion"))?;
            let (seal, seal_created) = converter.load(&binding.seal, "completion_seal")?;
            let result =
                transform::convert_seal(seal, pre.clone(), &mut |hash| converter.resolve(hash))?;
            converter.record(&binding.seal, "completion_seal", &result, &seal_created)?;
        }
        let root = match event.get("root_execution") {
            Some(Value::Null) => None,
            Some(value) => {
                let mut root: RootExecution = serde_json::from_value(value.clone())?;
                transform::map_root_references(&mut root, &mut |hash| converter.resolve(hash))?;
                let encoded = encoders
                    .entry(root.root_execution_id.0.to_string())
                    .or_default()
                    .push(root)?;
                for object in encoded.objects {
                    insert_object(target, ROOT_DELTA_KIND, &object, &created)?;
                    converter.generated += 1;
                }
                observed.insert(hash.clone(), encoded.reference.clone());
                Some(encoded.reference)
            }
            None => return Err(refused("event has no root field")),
        };
        let result = transform::convert_event(event, root, &mut |hash| converter.resolve(hash))?;
        converter.record(&hash, "work_event", &result, &created)?;
    }
    let pending: Vec<_> = converter
        .kinds
        .keys()
        .filter(|hash| !converter.mapped.contains_key(*hash))
        .cloned()
        .collect();
    for hash in pending {
        converter.resolve(&hash)?;
    }
    Ok(ConversionCounts {
        original_objects: converter.kinds.len(),
        changed_objects: converter
            .mapped
            .iter()
            .filter(|(source, target)| source != target)
            .count(),
        generated_root_deltas: converter.generated,
    })
}

impl Converter<'_> {
    fn retain_originals(&mut self) -> Result<(), MigrationError> {
        let mut statement = self.source.prepare("SELECT rowid,object_hash,object_kind,canonical_json,created_at FROM objects ORDER BY rowid")?;
        let mut rows = statement.query([])?;
        while let Some(row) = rows.next()? {
            let hash = address(&row.get::<_, String>(1)?)?;
            let kind: String = row.get(2)?;
            let object = CanonicalObject::verify(&hash, row.get(3)?)?;
            let created: String = row.get(4)?;
            self.target.execute("INSERT INTO migration_original_objects(object_hash,object_kind,canonical_json,created_at,source_rowid) VALUES (?1,?2,?3,?4,?5)", params![hash.as_str(),kind,object.bytes(),created,row.get::<_,i64>(0)?])?;
            if !matches!(
                kind.as_str(),
                "work_event" | "completion_seal" | "work_observation"
            ) {
                self.record(&hash, &kind, &object, &created)?;
            }
            self.kinds.insert(hash, kind);
        }
        Ok(())
    }

    fn load(&self, hash: &ObjectHash, kind: &str) -> Result<(Value, String), MigrationError> {
        let (bytes, created) = self.source.query_row(
            "SELECT canonical_json,created_at FROM objects WHERE object_hash=?1 AND object_kind=?2",
            params![hash.as_str(), kind],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, String>(1)?)),
        )?;
        Ok((CanonicalObject::verify(hash, bytes)?.decode()?, created))
    }

    fn resolve(&mut self, hash: &ObjectHash) -> Result<ObjectHash, MigrationError> {
        if let Some(mapped) = self.mapped.get(hash) {
            return Ok(mapped.clone());
        }
        // Observation bases precede capture. No arbitrary recursive traversal,
        // cycle substitution, or identity fallback for a missing canonical link.
        if self.kinds.get(hash).map(String::as_str) != Some("work_observation") {
            return Err(refused(format!(
                "missing or non-causal canonical mapping for {hash}"
            )));
        }
        let (observation, created) = self.load(hash, "work_observation")?;
        let converted = transform::convert_observation(&observation, &mut |basis| {
            self.mapped
                .get(basis)
                .cloned()
                .ok_or_else(|| refused("observation basis has no prior canonical mapping"))
        })?;
        self.record(hash, "work_observation", &converted, &created)?;
        Ok(converted.hash().clone())
    }

    fn record(
        &mut self,
        source: &ObjectHash,
        kind: &str,
        target: &CanonicalObject,
        created: &str,
    ) -> Result<(), MigrationError> {
        insert_object(self.target, kind, target, created)?;
        let binding = CanonicalObject::freeze(&super::resolution::ObjectBinding {
            profile: "aggregate-root-v1".into(),
            source: source.clone(),
            target: target.hash().clone(),
            kind: kind.into(),
        })?;
        insert_object(self.target, "migration_object_binding", &binding, created)?;
        self.target.execute(
            "INSERT INTO migration_object_map(source_hash,target_hash,binding_hash) VALUES (?1,?2,?3)",
            params![source.as_str(), target.hash().as_str(),binding.hash().as_str()],
        )?;
        if self
            .mapped
            .insert(source.clone(), target.hash().clone())
            .is_some()
        {
            return Err(refused("source object was mapped twice"));
        }
        Ok(())
    }
}

fn address(value: &str) -> Result<ObjectHash, MigrationError> {
    value
        .parse()
        .map_err(|_| refused("invalid source canonical address"))
}

fn insert_object(
    target: &Connection,
    kind: &str,
    object: &CanonicalObject,
    created: &str,
) -> Result<(), MigrationError> {
    target.execute("INSERT INTO objects(object_hash,object_kind,canonical_json,created_at) VALUES (?1,?2,?3,?4) ON CONFLICT(object_hash) DO NOTHING", params![object.hash().as_str(),kind,object.bytes(),created])?;
    let (stored_kind, stored_bytes): (String, Vec<u8>) = target.query_row(
        "SELECT object_kind,canonical_json FROM objects WHERE object_hash=?1",
        [object.hash().as_str()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if stored_kind != kind || stored_bytes != object.bytes() {
        return Err(refused(
            "canonical output identity collides with a different kind or bytes",
        ));
    }
    Ok(())
}
