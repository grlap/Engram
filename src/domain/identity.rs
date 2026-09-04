//! Stable project, session, cursor, and record identities shared by every
//! domain family.

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Stable host-local project identity shared by every session and worktree.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ProjectId(pub String);

/// Runtime session identity asserted by the host integration.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SessionId(pub String);

/// Monotonic position in a task's durable change feed.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ChangeCursor(pub i64);

/// Stable identifier for a memory across immutable versions.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct MemoryId(pub Uuid);

impl MemoryId {
    /// Creates a time-sortable identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for MemoryId {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable identifier for a local operational task.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct TaskId(pub Uuid);

impl TaskId {
    /// Creates a time-sortable identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for TaskId {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable planning identity for first-class local work.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WorkId(pub Uuid);

impl WorkId {
    /// Creates a time-sortable identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for WorkId {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable identity of one root execution generation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct RootExecutionId(pub Uuid);

impl RootExecutionId {
    /// Creates a time-sortable identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for RootExecutionId {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable identity of one execution generation for one work item.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WorkRunId(pub Uuid);

impl WorkRunId {
    /// Creates a time-sortable identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for WorkRunId {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable identity of one immutable execution obligation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WorkObligationId(pub Uuid);

impl WorkObligationId {
    /// Creates a time-sortable identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for WorkObligationId {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable identity of a fenced work claim across renewal and handoff.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WorkClaimId(pub Uuid);

impl WorkClaimId {
    /// Creates a time-sortable identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for WorkClaimId {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable identity of one pending handoff offer.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WorkHandoffOfferId(pub Uuid);

impl WorkHandoffOfferId {
    /// Creates a time-sortable identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for WorkHandoffOfferId {
    fn default() -> Self {
        Self::new()
    }
}
