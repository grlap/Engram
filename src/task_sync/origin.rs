//! Explicit local enrollment identity, distinct from shared asserted aliases.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::{ActorContext, ObjectId, ProjectId};

pub(crate) mod continuity;
pub(crate) mod private;
pub mod recovery;

/// An explicit request; its operation UUID is retained before SQL mutation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EnrollRequest {
    pub operation: uuid::Uuid,
    /// Set only for an explicit fork; retained history is never reassigned.
    pub predecessor: Option<ObjectId>,
}

/// Immutable enrollment lineage; the containing object's minted id is the origin.
pub const PRODUCING_ORIGIN_KIND: &str = "task_sync_producing_origin";

/// Canonical payload. Enrollment is explicit; reading never creates a record.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Enrollment {
    pub project_id: ProjectId,
    pub predecessor_origin_id: Option<ObjectId>,
    pub enrollment_operation_id: uuid::Uuid,
    pub actor: ActorContext,
    pub enrolled_at: DateTime<Utc>,
}

/// The active identity read from a validated lineage, not continuity authority.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProducingOrigin {
    pub origin_id: ObjectId,
    pub enrollment: Enrollment,
}
