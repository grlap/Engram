//! Evidence-family metadata shares the exchange chain, never task membership.

use std::collections::{BTreeMap, BTreeSet};

use super::{ActorContext, Binding, Direction, ObjectId, RetainedCommit, require};
use crate::storage::StoreError;
use crate::task_sync::{delta::OriginQualifiedCut, evidence::limits};
use serde::{Deserialize, Serialize};

mod recipe;
pub use recipe::EvidenceImportRecipe;

/// A known structural rejection; uncertain storage and I/O failures stay pending.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvidenceRefusal {
    Dependency,
    Witness,
    RecordConflict,
    InvalidPayload,
    /// The selected branch was observed at another head than the request
    /// expects. Its expected head is fixed, so the request ends here instead
    /// of waiting on a manual rewind; a new request selects the current head.
    SelectionChanged,
}

/// Terminal metadata accounts for a rejected batch without granting coverage.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum EvidenceStop {
    Refused { reason: EvidenceRefusal },
    Oversize { detail: limits::Oversize },
}

impl EvidenceStop {
    pub(crate) fn from_error(error: &crate::task_sync::evidence::EvidenceError) -> Option<Self> {
        use crate::task_sync::evidence::EvidenceError;
        let reason = match error {
            EvidenceError::Dependency { .. } => EvidenceRefusal::Dependency,
            EvidenceError::Witness(_) => EvidenceRefusal::Witness,
            EvidenceError::RecordConflict(_) => EvidenceRefusal::RecordConflict,
            EvidenceError::Json { .. }
            | EvidenceError::UnknownKind(_)
            | EvidenceError::NonCanonical(_) => EvidenceRefusal::InvalidPayload,
            EvidenceError::Oversize(detail) => {
                return Some(Self::Oversize {
                    detail: detail.clone(),
                });
            }
            EvidenceError::InvalidPack(_) | EvidenceError::Io(_) => return None,
        };
        Some(Self::Refused { reason })
    }

    pub(crate) fn validate(&self) -> Result<(), StoreError> {
        if let Self::Oversize { detail } = self {
            use limits::{Dimension, LimitKind};
            let expected = match (detail.kind, detail.dimension) {
                (LimitKind::RecordTooLarge, Dimension::Bytes) => limits::MAX_RECORD_BYTES,
                (LimitKind::PackTooLarge, Dimension::Bytes) => limits::MAX_PACK_BYTES,
                (LimitKind::PackTooLarge, Dimension::Objects) => limits::MAX_PACK_RECORDS,
                (LimitKind::ComponentTooLarge, Dimension::Bytes) => limits::MAX_COMPONENT_BYTES,
                (LimitKind::ComponentTooLarge, Dimension::Objects) => limits::MAX_COMPONENT_OBJECTS,
                (LimitKind::BatchTooLarge, Dimension::Bytes) => limits::MAX_BATCH_BYTES,
                (LimitKind::BatchTooLarge, Dimension::Objects) => limits::MAX_BATCH_OBJECTS,
                (LimitKind::PlanTooLarge, Dimension::Bytes) => limits::MAX_PLAN_BYTES,
                (LimitKind::AttachmentTooLarge, Dimension::Bytes) => limits::MAX_ATTACHMENT_BYTES,
                (LimitKind::StagingBudgetExceeded, Dimension::Bytes) => limits::MAX_STAGING_BYTES,
                _ => return Err(super::super::invalid("invalid evidence oversize dimension")),
            };
            require(
                detail.limit == expected && detail.actual > expected,
                "evidence oversize does not exceed the declared profile bound",
            )?;
        }
        Ok(())
    }
}

/// A frozen pack locator and the local admission selected before mutation.
/// Bodies remain in retained pack files, outside the bounded metadata recipe.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EvidencePackSelection {
    pub publishing_origin: ObjectId,
    pub admission: ObjectId,
    pub content_fingerprint: ObjectId,
    pub encoded_bytes: usize,
    pub records: usize,
}

/// A separate incoming evidence request. Its explicit event variant prevents
/// reusing a planning request key with a different family under the same binding.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceIntent {
    pub operation: uuid::Uuid,
    pub key: String,
    pub direction: Direction,
    pub actor: ActorContext,
    pub producer: ObjectId,
    pub expected_head: Option<String>,
    pub source: RetainedCommit,
    pub local_cut: OriginQualifiedCut,
    pub packs: BTreeMap<ObjectId, EvidencePackSelection>,
}

impl EvidenceIntent {
    pub(crate) fn validate(
        &self,
        binding_id: &ObjectId,
        binding: &Binding,
    ) -> Result<(), StoreError> {
        require(
            !self.operation.is_nil(),
            "missing evidence exchange operation",
        )?;
        require(
            self.direction == Direction::Incoming,
            "evidence request is not incoming",
        )?;
        super::text(&self.key, 4096)?;
        super::attribution(&self.actor)?;
        super::fingerprint(&self.producer)?;
        if let Some(head) = &self.expected_head {
            super::oid(head)?;
        }
        self.source.validate()?;
        self.local_cut.validate(binding_id, binding)?;
        require(
            !self.packs.is_empty(),
            "evidence request has no selected packs",
        )?;
        let mut identities = BTreeSet::new();
        let (mut bytes, mut records) = (0_usize, 0_usize);
        for (pack, selected) in &self.packs {
            require(
                selected.publishing_origin.as_str().len() == 32,
                "selected evidence origin is not a minted identity",
            )?;
            require(
                pack.as_str().len() == 32 && identities.insert(pack),
                "invalid evidence pack identity",
            )?;
            require(
                selected.admission.as_str().len() == 32 && identities.insert(&selected.admission),
                "evidence admission identity is missing or reused",
            )?;
            super::fingerprint(&selected.content_fingerprint)?;
            require(
                selected.encoded_bytes > 0 && selected.encoded_bytes <= limits::MAX_PACK_BYTES,
                "selected evidence pack exceeds byte bound",
            )?;
            require(
                selected.records > 0 && selected.records <= limits::MAX_PACK_RECORDS,
                "selected evidence pack exceeds record bound",
            )?;
            bytes = bytes
                .checked_add(selected.encoded_bytes)
                .ok_or_else(|| super::super::invalid("evidence byte count overflow"))?;
            records = records
                .checked_add(selected.records)
                .ok_or_else(|| super::super::invalid("evidence record count overflow"))?;
        }
        require(
            bytes <= limits::MAX_BATCH_BYTES && records <= limits::MAX_BATCH_OBJECTS,
            "selected evidence batch exceeds profile bounds",
        )?;
        require(
            crate::CanonicalObject::freeze(self)?.bytes().len() <= limits::MAX_PLAN_BYTES,
            "evidence construction metadata exceeds plan bound",
        )
    }
}

/// Original local admission links, not imported verification or planning credit.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceReceipt {
    pub admissions: BTreeMap<ObjectId, ObjectId>,
}

impl EvidenceReceipt {
    pub(crate) fn validate(&self, intent: &EvidenceIntent) -> Result<(), StoreError> {
        require(
            self.admissions.len() == intent.packs.len()
                && intent
                    .packs
                    .iter()
                    .all(|(pack, selected)| self.admissions.get(pack) == Some(&selected.admission)),
            "evidence receipt differs from frozen pack admissions",
        )
    }
}
