//! Immutable foreign facts. Native execution and exchange dispatch stay with their owners.

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};

use crate::storage::{SqliteStore, StoreError};
use crate::task_sync::evidence::{EvidenceError, EvidencePack, RecordEnvelope, RecordKind};
use crate::{CanonicalObject, ObjectId, ProjectId};

pub(crate) mod acceptance;
pub(crate) mod consumption;
mod native_capture;
mod packs;
mod records;
mod schema;
pub(crate) mod verification;
mod witnesses;

pub use native_capture::{
    NativeEvidenceCapture, NativeEvidenceCaptureRequest, NativeEvidenceCursor, NativeEvidenceFeed,
    NativeEvidencePage,
};
pub(crate) use packs::header_on as retained_pack_header_on;
pub(crate) use packs::load_on as load_retained_pack_on;
#[cfg(test)]
pub(crate) use packs::load_probe as retained_pack_load_probe;
pub use packs::{PackBatch, PackCursor, Retention};
pub(crate) use packs::{retain_batch_on, retain_pack_on};
pub(in crate::storage) use schema::{create_schema, rebuild, verify};

#[cfg(test)]
pub(crate) mod test_support;
#[cfg(test)]
mod tests;

pub const MANIFEST_KIND: &str = "task_sync_evidence_pack_manifest";
pub const ADMISSION_KIND: &str = "task_sync_evidence_admission";

#[derive(Debug, thiserror::Error)]
pub enum PersistenceError {
    #[error(transparent)]
    Evidence(#[from] EvidenceError),
    #[error(transparent)]
    Storage(#[from] StoreError),
}

impl From<rusqlite::Error> for PersistenceError {
    fn from(error: rusqlite::Error) -> Self {
        Self::Storage(error.into())
    }
}

impl From<serde_json::Error> for PersistenceError {
    fn from(error: serde_json::Error) -> Self {
        Self::Evidence(error.into())
    }
}

type Result<T> = std::result::Result<T, PersistenceError>;

pub(crate) fn store_error(error: PersistenceError) -> StoreError {
    match error {
        PersistenceError::Storage(error) => error,
        PersistenceError::Evidence(error) => StoreError::InvalidWorkProjection(error.to_string()),
    }
}

impl From<PersistenceError> for StoreError {
    fn from(error: PersistenceError) -> Self {
        store_error(error)
    }
}

fn invalid(reason: &'static str) -> PersistenceError {
    EvidenceError::InvalidPack(reason).into()
}

fn stored_id(value: String) -> Result<ObjectId> {
    ObjectId::from_stored(value).ok_or_else(|| invalid("invalid retained evidence id"))
}

/// The pack's original UUID identifies its immutable header and membership.
/// Bodies live once in `objects`; encoding reconstructs the identical transport.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    project_id: ProjectId,
    publishing_origin: ObjectId,
    members: Vec<Member>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Member {
    object_id: ObjectId,
    object_kind: RecordKind,
    origin: ObjectId,
}

impl Manifest {
    fn from_pack(pack: &EvidencePack) -> Self {
        Self {
            project_id: pack.project_id().clone(),
            publishing_origin: pack.publishing_origin().clone(),
            members: pack
                .records()
                .iter()
                .map(|record| Member {
                    object_id: record.object_id().clone(),
                    object_kind: record.kind(),
                    origin: record.origin().clone(),
                })
                .collect(),
        }
    }
}

/// Local receipt metadata, minted/prepared by the calling exchange operation.
/// It is never an incoming original, a native feed entry or an execution proof.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Admission {
    project_id: ProjectId,
    pack_id: ObjectId,
    position: i64,
    imported_objects: Vec<ObjectId>,
}

fn object_on(connection: &Connection, id: &ObjectId) -> Result<Option<(String, Vec<u8>)>> {
    Ok(connection
        .query_row(
            "SELECT object_kind,canonical_json FROM objects WHERE object_id=?1",
            [id.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?)
}

/// Keyed native-reader boundary. Raw imported records are read only through
/// the evidence owner, until a separate destination association is admitted.
pub(in crate::storage) fn is_imported_only(
    connection: &Connection,
    id: &ObjectId,
) -> std::result::Result<bool, StoreError> {
    let indexed: Option<String> = connection
        .query_row(
            "SELECT admission_id FROM work_task_sync_evidence_imported WHERE object_id=?1",
            [id.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    if let Some(admission) = indexed {
        let valid: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM objects receipt,json_each(receipt.canonical_json,'$.imported_objects') original
             WHERE receipt.object_id=?1 AND receipt.object_kind=?2 AND original.value=?3)",
            params![admission,ADMISSION_KIND,id.as_str()], |row| row.get(0))?;
        if !valid {
            return Err(StoreError::InvalidWorkProjection(
                "imported evidence partition differs from its immutable admission".into(),
            ));
        }
        return Ok(true);
    }
    let missing: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_task_sync_evidence_members member
         JOIN work_task_sync_evidence_packs pack ON pack.pack_id=member.pack_id
         JOIN objects receipt ON receipt.object_id=pack.admission_id,
         json_each(receipt.canonical_json,'$.imported_objects') original
         WHERE member.object_id=?1 AND receipt.object_kind=?2 AND original.value=?1)",
        params![id.as_str(), ADMISSION_KIND],
        |row| row.get(0),
    )?;
    if missing {
        return Err(StoreError::InvalidWorkProjection(
            "imported evidence partition is missing; repair projections explicitly".into(),
        ));
    }
    Ok(false)
}

/// Every original that an immutable admission receipt imported; `?2` binds
/// [`ADMISSION_KIND`]. Native projection owners read the receipts rather than
/// the imported partition, so their rebuild does not depend on rebuild order.
const IMPORTED_ORIGINALS: &str = "SELECT imported.value FROM objects receipt,json_each(receipt.canonical_json,'$.imported_objects') imported WHERE receipt.object_kind=?2";

pub(in crate::storage) fn require_native(
    connection: &Connection,
    id: &ObjectId,
) -> std::result::Result<(), StoreError> {
    if is_imported_only(connection, id)? {
        return Err(StoreError::InvalidWorkProjection(
            "retained foreign evidence is not a native execution record".into(),
        ));
    }
    Ok(())
}
