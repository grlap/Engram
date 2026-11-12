use crate::task_sync::evidence::{RecordEnvelope, limits};

use super::{
    ADMISSION_KIND, Admission, CanonicalObject, Connection, Deserialize, EvidenceError,
    EvidencePack, MANIFEST_KIND, Manifest, ObjectId, OptionalExtension, ProjectId, Result,
    Serialize, SqliteStore, Transaction, invalid, object_on, params, records, schema, stored_id,
    witnesses,
};

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct Retention {
    pub admission: ObjectId,
    pub pack_id: ObjectId,
    pub imported_objects: usize,
    pub replayed: bool,
}

/// A captured local pack interval. UUIDs locate actual immutable receipts;
/// this is neither a global SQLite row id nor an origin feed position.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct PackCursor {
    pub project_id: ProjectId,
    pub capture: ObjectId,
    pub after: Option<ObjectId>,
}

pub struct PackBatch {
    pub packs: Vec<EvidencePack>,
    pub next: Option<PackCursor>,
}

impl SqliteStore {
    /// Retain immutable foreign evidence without activating a local execution.
    /// This validates supplied feed, cut and governing witnesses, but does not
    /// require the complete execution graph of other originals. The import
    /// writer checks that closure with `inventory::plan_batch` before retention.
    ///
    /// # Errors
    /// Refuses conflicting identities, malformed provenance or incomplete witnesses.
    pub fn retain_evidence_pack(
        &mut self,
        project: &ProjectId,
        pack: &EvidencePack,
        admission: &ObjectId,
    ) -> Result<super::Retention> {
        let mut transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let result = super::retain_pack_on(&mut transaction, project, pack, admission)?;
        transaction.commit()?;
        Ok(result)
    }

    /// Retain one bounded selected batch with its preallocated local receipts.
    /// Like single-pack retention, this validates supplied witnesses without
    /// requiring every original's execution graph. The import writer checks
    /// that closure with `inventory::plan_batch` before retention.
    ///
    /// # Errors
    /// Refuses the entire batch on any collision, bound, provenance or witness failure.
    pub fn retain_evidence_batch(
        &mut self,
        project: &ProjectId,
        selected: &[(&EvidencePack, &ObjectId)],
    ) -> Result<Vec<Retention>> {
        let mut transaction = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        let result = super::retain_batch_on(&mut transaction, project, selected)?;
        transaction.commit()?;
        Ok(result)
    }

    /// Capture the current retained-pack endpoint, without advancing any feed.
    ///
    /// # Errors
    /// Returns a storage error if the keyed catalog cannot be read.
    pub fn capture_evidence_packs(&self, project: &ProjectId) -> Result<Option<super::PackCursor>> {
        capture_on(&self.connection, project)
    }

    /// Read a bounded foreign-only whole-pack batch in one SQLite snapshot.
    /// Native origins and originals require the profile-bound native exporter.
    ///
    /// # Errors
    /// Refuses native data, invalid cursors, missing originals and damaged projections.
    pub fn export_evidence_packs(&self, cursor: &super::PackCursor) -> Result<super::PackBatch> {
        let snapshot = self.connection.unchecked_transaction()?;
        let batch = export_after_on(&snapshot, cursor)?;
        snapshot.commit()?;
        Ok(batch)
    }
}

fn retention(admission: ObjectId, record: &Admission, replayed: bool) -> Retention {
    Retention {
        admission,
        pack_id: record.pack_id.clone(),
        imported_objects: if replayed {
            0
        } else {
            record.imported_objects.len()
        },
        replayed,
    }
}

pub(crate) fn load_on(
    connection: &Connection,
    project: &ProjectId,
    pack: &ObjectId,
) -> Result<EvidencePack> {
    #[cfg(test)]
    load_probe::count();
    let manifest = manifest_on(connection, project, pack)?;
    let records = manifest
        .members
        .iter()
        .map(|member| records::load_on(connection, member))
        .collect::<Result<Vec<_>>>()?;
    Ok(EvidencePack::prepare(
        pack.clone(),
        manifest.project_id,
        manifest.publishing_origin,
        records,
    )?)
}

/// Read immutable membership metadata without loading original record bodies.
pub(crate) fn header_on(
    connection: &Connection,
    project: &ProjectId,
    pack: &ObjectId,
) -> Result<(ObjectId, usize)> {
    let manifest = manifest_on(connection, project, pack)?;
    Ok((manifest.publishing_origin, manifest.members.len()))
}

fn manifest_on(connection: &Connection, project: &ProjectId, pack: &ObjectId) -> Result<Manifest> {
    let (kind, bytes) =
        object_on(connection, pack)?.ok_or_else(|| invalid("retained evidence pack is missing"))?;
    if kind != MANIFEST_KIND {
        return Err(EvidenceError::RecordConflict(pack.clone()).into());
    }
    let manifest: Manifest = serde_json::from_slice(&bytes)?;
    if &manifest.project_id != project {
        return Err(invalid("retained evidence pack belongs to another project"));
    }
    Ok(manifest)
}

/// Retain one pack under an exchange caller's existing transaction. The local
/// admission UUID must already belong to its frozen construction recipe.
/// A savepoint also makes a refusal atomic if the caller keeps its writer alive.
/// Witness validation is retained; complete execution-graph closure belongs to
/// the caller's `inventory::plan_batch` check before retention.
pub fn retain_pack_on(
    transaction: &mut Transaction<'_>,
    project: &ProjectId,
    pack: &EvidencePack,
    admission_id: &ObjectId,
) -> Result<Retention> {
    retain_batch_on(transaction, project, &[(pack, admission_id)])?
        .pop()
        .ok_or_else(|| invalid("missing evidence retention result"))
}

fn replay_on(
    connection: &Connection,
    project: &ProjectId,
    pack: &EvidencePack,
) -> Result<Option<Retention>> {
    let manifest = Manifest::from_pack(pack);
    let object = CanonicalObject::identified(pack.pack_id(), &manifest)?;
    if let Some((kind, bytes)) = object_on(connection, pack.pack_id())? {
        if kind != MANIFEST_KIND || bytes != object.bytes() {
            return Err(EvidenceError::RecordConflict(pack.pack_id().clone()).into());
        }
        for record in pack.records() {
            if !records::compare_on(connection, record)? {
                return Err(invalid("retained pack has a missing original"));
            }
        }
        let (id, bytes): (String, Vec<u8>) = connection.query_row(
            "SELECT admission.object_id,admission.canonical_json FROM work_task_sync_evidence_packs pack
             JOIN objects admission ON admission.object_id=pack.admission_id
             WHERE pack.pack_id=?1 AND pack.project_id=?2 AND admission.object_kind=?3",
            params![pack.pack_id().as_str(), project.0, ADMISSION_KIND], |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        let admission: Admission = serde_json::from_slice(&bytes)?;
        if admission.pack_id != *pack.pack_id() || admission.project_id != *project {
            return Err(invalid("retained evidence admission differs"));
        }
        return Ok(Some(retention(stored_id(id)?, &admission, true)));
    }
    Ok(None)
}

/// Retain the complete bounded selection under one savepoint. All original
/// objects and qualified memberships are present before cross-pack validation.
/// Supplied witnesses must be complete. Other originals may lack execution
/// context; the import writer checks `inventory::plan_batch` before this call.
pub fn retain_batch_on(
    transaction: &mut Transaction<'_>,
    project: &ProjectId,
    selected: &[(&EvidencePack, &ObjectId)],
) -> Result<Vec<Retention>> {
    preflight(selected, project)?;
    let savepoint = transaction.savepoint()?;
    let mut results = Vec::with_capacity(selected.len());
    for (pack, _) in selected {
        results.push(replay_on(&savepoint, project, pack)?);
    }
    // Replay is a keyed read, with no provenance re-admission or fresh receipt.
    if results.iter().all(Option::is_some) {
        return Ok(results.into_iter().flatten().collect());
    }
    for ((pack, admission), result) in selected.iter().zip(&results) {
        if result.is_none() {
            if object_on(&savepoint, admission)?.is_some() {
                return Err(EvidenceError::RecordConflict((*admission).clone()).into());
            }
            for record in pack.records() {
                records::compare_on(&savepoint, record)?;
            }
        }
    }
    let mut imports = vec![Vec::new(); selected.len()];
    for (((pack, _), result), imported) in selected.iter().zip(&results).zip(&mut imports) {
        if result.is_none() {
            for record in pack.records() {
                if !records::compare_on(&savepoint, record)? {
                    records::insert_on(&savepoint, record)?;
                    imported.push(record.object_id().clone());
                }
            }
        }
    }
    for (((pack, admission), result), imported) in selected.iter().zip(&mut results).zip(imports) {
        if result.is_none() {
            *result = Some(stage_on(&savepoint, project, pack, admission, imported)?);
        }
    }
    for (pack, _) in selected {
        records::validate_origins(&savepoint, pack)?;
        records::validate_profile(&savepoint, pack)?;
    }
    for (pack, _) in selected {
        records::index_origins(&savepoint, pack)?;
    }
    witnesses::retain_records_on(
        &savepoint,
        project,
        selected.iter().flat_map(|(pack, _)| pack.records()),
    )?;
    for (pack, _) in selected {
        super::verification::retain_on(&savepoint, pack)?;
    }
    savepoint.commit()?;
    Ok(results.into_iter().flatten().collect())
}

fn preflight(selected: &[(&EvidencePack, &ObjectId)], project: &ProjectId) -> Result<()> {
    let mut headers = std::collections::BTreeSet::new();
    let mut originals = std::collections::BTreeMap::new();
    let (mut bytes, mut objects) = (0_usize, 0_usize);
    limits::enforce(
        limits::LimitKind::BatchTooLarge,
        None,
        limits::Dimension::Objects,
        selected.len(),
        limits::MAX_BATCH_OBJECTS,
    )
    .map_err(EvidenceError::from)?;
    for (pack, admission) in selected {
        if pack.project_id() != project || admission.as_str().len() != 32 {
            return Err(invalid(
                "evidence admission scope or prepared identity differs",
            ));
        }
        if !headers.insert(pack.pack_id()) || !headers.insert(*admission) {
            return Err(invalid("selected pack or admission identity is repeated"));
        }
        bytes = bytes.saturating_add(pack.to_bytes()?.len());
        objects = objects.saturating_add(pack.records().len());
        for (dimension, actual, limit) in [
            (limits::Dimension::Bytes, bytes, limits::MAX_BATCH_BYTES),
            (
                limits::Dimension::Objects,
                objects,
                limits::MAX_BATCH_OBJECTS,
            ),
        ] {
            limits::enforce(
                limits::LimitKind::BatchTooLarge,
                None,
                dimension,
                actual,
                limit,
            )
            .map_err(EvidenceError::from)?;
        }
        for record in pack.records() {
            if let Some(previous) = originals.insert(record.object_id(), record)
                && !record.same_record(previous)
            {
                return Err(EvidenceError::RecordConflict(record.object_id().clone()).into());
            }
        }
    }
    if let Some(id) = headers.iter().find(|id| originals.contains_key(**id)) {
        return Err(EvidenceError::RecordConflict((**id).clone()).into());
    }
    Ok(())
}

fn stage_on(
    connection: &Connection,
    project: &ProjectId,
    pack: &EvidencePack,
    admission_id: &ObjectId,
    imported_objects: Vec<ObjectId>,
) -> Result<Retention> {
    let manifest = Manifest::from_pack(pack);
    let object = CanonicalObject::identified(pack.pack_id(), &manifest)?;
    let previous: i64 = connection.query_row(
        "SELECT COALESCE((SELECT position FROM work_task_sync_evidence_packs
             WHERE project_id=?1 ORDER BY position DESC LIMIT 1),0)",
        [&project.0],
        |row| row.get(0),
    )?;
    let position = previous
        .checked_add(1)
        .ok_or_else(|| invalid("evidence capture position exhausted"))?;
    let admission = Admission {
        project_id: project.clone(),
        pack_id: pack.pack_id().clone(),
        position,
        imported_objects,
    };
    let admission_object = CanonicalObject::identified(admission_id, &admission)?;
    SqliteStore::insert_object(connection, MANIFEST_KIND, &object)?;
    SqliteStore::insert_object(connection, ADMISSION_KIND, &admission_object)?;
    schema::append_on(connection, admission_id, &admission, &manifest)?;
    Ok(retention(admission_id.clone(), &admission, false))
}

/// Start an immutable, bounded interval over the keyed retained-pack catalog.
pub fn capture_on(connection: &Connection, project: &ProjectId) -> Result<Option<PackCursor>> {
    let capture = connection
        .query_row(
            "SELECT pack_id FROM work_task_sync_evidence_packs WHERE project_id=?1
         ORDER BY position DESC LIMIT 1",
            [&project.0],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    capture
        .map(|id| {
            Ok(PackCursor {
                project_id: project.clone(),
                capture: stored_id(id)?,
                after: None,
            })
        })
        .transpose()
}

fn position_on(connection: &Connection, project: &ProjectId, pack: &ObjectId) -> Result<i64> {
    connection
        .query_row(
            "SELECT position FROM work_task_sync_evidence_packs WHERE project_id=?1 AND pack_id=?2",
            params![project.0, pack.as_str()],
            |row| row.get(0),
        )
        .optional()?
        .ok_or_else(|| invalid("evidence cursor does not name a retained pack"))
}

fn require_foreign_pack(connection: &Connection, pack: &EvidencePack) -> Result<()> {
    let ids = std::iter::once(pack.publishing_origin())
        .chain(pack.records().iter().map(RecordEnvelope::origin))
        .chain(pack.records().iter().map(RecordEnvelope::object_id))
        .collect::<std::collections::BTreeSet<_>>();
    for id in ids {
        if !super::is_imported_only(connection, id)? {
            return Err(invalid("native evidence requires profile-bound export"));
        }
    }
    Ok(())
}

/// Read whole immutable packs within the captured interval, using keyed seeks.
/// No object is truncated, and later imports cannot move this cursor's endpoint.
pub fn export_after_on(connection: &Connection, cursor: &PackCursor) -> Result<PackBatch> {
    let through = position_on(connection, &cursor.project_id, &cursor.capture)?;
    let mut after = cursor
        .after
        .as_ref()
        .map(|id| position_on(connection, &cursor.project_id, id))
        .transpose()?
        .unwrap_or(0);
    if after > through {
        return Err(invalid("evidence cursor is beyond its capture"));
    }
    let mut packs = Vec::new();
    let (mut bytes, mut objects) = (0_usize, 0_usize);
    let mut last = cursor.after.clone();
    while after < through {
        let (position, pack_id): (i64, String) = connection
            .query_row(
                "SELECT position,pack_id FROM work_task_sync_evidence_packs
             WHERE project_id=?1 AND position>?2 AND position<=?3 ORDER BY position LIMIT 1",
                params![cursor.project_id.0, after, through],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| invalid("retained evidence capture contains a gap"))?;
        if position != after + 1 {
            return Err(invalid("retained evidence capture contains a gap"));
        }
        let id = stored_id(pack_id)?;
        let pack = load_on(connection, &cursor.project_id, &id)?;
        let encoded = pack.to_bytes()?.len();
        if bytes + encoded > limits::MAX_BATCH_BYTES
            || objects + pack.records().len() > limits::MAX_BATCH_OBJECTS
        {
            break;
        }
        // Raw loading also serves doctor and ledger history. Apply disclosure
        // only here, before exposing a selected pack, including resumed pages.
        require_foreign_pack(connection, &pack)?;
        bytes += encoded;
        objects += pack.records().len();
        packs.push(pack);
        last = Some(id);
        after = position;
    }
    let next = (after < through).then(|| PackCursor {
        after: last,
        ..cursor.clone()
    });
    Ok(PackBatch { packs, next })
}

#[cfg(test)]
pub(crate) mod load_probe {
    use std::cell::Cell;

    thread_local! {
        static LOADS: Cell<usize> = const { Cell::new(0) };
    }

    pub(super) fn count() {
        LOADS.with(|loads| loads.set(loads.get() + 1));
    }

    pub(crate) fn loads() -> usize {
        LOADS.with(Cell::get)
    }
}
