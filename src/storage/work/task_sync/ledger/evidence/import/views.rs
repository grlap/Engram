//! Compare selected pack projections against the same admitted original bodies.

use super::{invalid, require};
use crate::{ObjectId, storage::StoreError, task_sync::evidence::EvidencePack};
use rusqlite::{Connection, params};

pub(super) fn check(
    connection: &Connection,
    pack: &EvidencePack,
    admission: &ObjectId,
) -> Result<(), StoreError> {
    let valid: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_task_sync_evidence_packs pack
         JOIN objects receipt ON receipt.object_id=pack.admission_id
         JOIN objects manifest ON manifest.object_id=pack.pack_id
         WHERE pack.pack_id=?1 AND pack.project_id=?2 AND pack.admission_id=?3 AND pack.publishing_origin=?4
         AND receipt.object_kind='task_sync_evidence_admission' AND manifest.object_kind='task_sync_evidence_pack_manifest'
         AND json_extract(receipt.canonical_json,'$.pack_id')=pack.pack_id
         AND json_extract(receipt.canonical_json,'$.project_id')=pack.project_id
         AND json_extract(receipt.canonical_json,'$.position')=pack.position
         AND json_extract(manifest.canonical_json,'$.project_id')=pack.project_id
         AND json_extract(manifest.canonical_json,'$.publishing_origin')=pack.publishing_origin)",
        params![pack.pack_id().as_str(),pack.project_id().0,admission.as_str(),pack.publishing_origin().as_str()],|row|row.get(0))?;
    require(
        valid,
        "selected evidence pack projection differs; repair projections explicitly",
    )?;
    let count: i64 = connection.query_row(
        "SELECT count(*) FROM work_task_sync_evidence_members WHERE pack_id=?1",
        [pack.pack_id().as_str()],
        |row| row.get(0),
    )?;
    require(
        usize::try_from(count).ok() == Some(pack.records().len()),
        "selected evidence membership count differs; repair projections explicitly",
    )?;
    for (ordinal, record) in pack.records().iter().enumerate() {
        let actual: (String,String,String) = connection.query_row(
            "SELECT object_id,object_kind,origin_id FROM work_task_sync_evidence_members WHERE pack_id=?1 AND ordinal=?2",
            params![pack.pack_id().as_str(),i64::try_from(ordinal).map_err(|_| invalid("evidence ordinal exceeds SQLite range"))?],
            |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?;
        require(
            actual
                == (
                    record.object_id().to_string(),
                    record.kind().as_str().to_string(),
                    record.origin().to_string(),
                ),
            "selected evidence member projection differs; repair projections explicitly",
        )?;
    }
    Ok(())
}
