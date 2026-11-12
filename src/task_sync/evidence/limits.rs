//! One owning definition of the full-evidence profile's capacity bounds.

use serde::{Deserialize, Serialize};

use crate::ObjectId;

pub const MAX_RECORD_BYTES: usize = 8 * 1024 * 1024;
pub const MAX_PACK_BYTES: usize = 16 * 1024 * 1024;
pub const MAX_PACK_RECORDS: usize = 4096;
pub const MAX_COMPONENT_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_COMPONENT_OBJECTS: usize = 8192;
pub const MAX_BATCH_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_BATCH_OBJECTS: usize = 8192;
pub const MAX_PLAN_BYTES: usize = 2 * 1024 * 1024;
pub const MAX_ATTACHMENT_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_ATTACHMENT_OPERATION_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_STAGING_BYTES: usize = 256 * 1024 * 1024;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LimitKind {
    RecordTooLarge,
    PackTooLarge,
    ComponentTooLarge,
    BatchTooLarge,
    PlanTooLarge,
    AttachmentTooLarge,
    StagingBudgetExceeded,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Dimension {
    Bytes,
    Objects,
}

/// A retained candidate's locator and actual size; no body is truncated.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize, thiserror::Error)]
#[error("{kind:?}: {actual} {dimension:?} exceeds {limit}")]
#[serde(deny_unknown_fields)]
pub struct Oversize {
    pub kind: LimitKind,
    pub subject: Option<ObjectId>,
    pub dimension: Dimension,
    pub actual: usize,
    pub limit: usize,
}

pub(crate) fn enforce(
    kind: LimitKind,
    subject: Option<&ObjectId>,
    dimension: Dimension,
    actual: usize,
    limit: usize,
) -> Result<(), Oversize> {
    if actual > limit {
        Err(Oversize {
            kind,
            subject: subject.cloned(),
            dimension,
            actual,
            limit,
        })
    } else {
        Ok(())
    }
}

/// Operation-owned accounting; a refused reservation changes nothing.
#[derive(Debug, Default)]
pub struct StagingBudget {
    retained_bytes: usize,
}

impl StagingBudget {
    pub fn retained_bytes(&self) -> usize {
        self.retained_bytes
    }

    /// Reserve retained operation bytes before extending private staging.
    ///
    /// # Errors
    /// Refuses an allocation beyond the profile budget without changing the total.
    pub fn reserve(&mut self, bytes: usize) -> Result<(), Oversize> {
        let next = self.retained_bytes.saturating_add(bytes);
        enforce(
            LimitKind::StagingBudgetExceeded,
            None,
            Dimension::Bytes,
            next,
            MAX_STAGING_BYTES,
        )?;
        self.retained_bytes = next;
        Ok(())
    }
}
