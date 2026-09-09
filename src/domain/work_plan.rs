//! Authored, payload-local work plans. These are requests, not another ledger.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{ActorContext, ChildRequirement, ProjectId, WorkId, WorkItemKind};

/// Operator admission bounds, independent of compact agent response budgets.
/// Root open-descendant and hierarchy-depth limits still apply to each tree.
pub const MAX_WORK_PLAN_TASKS: usize = 256;
pub const MAX_WORK_PLAN_EDGES: usize = 1024;
pub const MAX_WORK_PLAN_BYTES: usize = 1024 * 1024;
pub const MAX_WORK_PLAN_KEY_BYTES: usize = 64;

/// One complete plan. Parent keys name only newly supplied tasks.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkPlanInput {
    pub tasks: Vec<WorkPlanTask>,
    #[serde(default)]
    pub prerequisites: Vec<WorkPlanPrerequisite>,
    /// Reuse for retries; a different intent with this key is refused.
    pub idempotency_key: String,
}

/// A new root or child. No external lifecycle or existing parent is accepted.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkPlanTask {
    pub key: String,
    pub parent_key: Option<String>,
    pub title: String,
    pub outcome: String,
    pub acceptance: Vec<String>,
    pub requirement: Option<ChildRequirement>,
    pub kind: Option<WorkItemKind>,
    pub priority: Option<i32>,
    #[serde(default)]
    pub labels: Vec<String>,
    pub assigned_to: Option<String>,
    pub deferred_until: Option<DateTime<Utc>>,
    pub external_ref: Option<String>,
    #[serde(default)]
    pub notes: Vec<String>,
}

/// Direction: `work_key` requires the named prerequisite to complete first.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkPlanPrerequisite {
    pub work_key: String,
    pub prerequisite: WorkPlanDependency,
}

/// Explicit namespaces prevent a payload key from shadowing an existing ref.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(
    tag = "kind",
    content = "value",
    rename_all = "snake_case",
    deny_unknown_fields
)]
pub enum WorkPlanDependency {
    Local(String),
    Existing(String),
}

/// Storage admission includes complete asserted attribution for redaction.
#[derive(Clone, Debug, Serialize)]
pub struct ProposeWorkPlanRequest {
    pub project_id: ProjectId,
    pub plan: WorkPlanInput,
    pub actor: ActorContext,
    pub created_at: DateTime<Utc>,
}

/// Complete, bounded mapping in original payload order; never row-shed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkPlanReceipt {
    pub tasks: Vec<WorkPlanMapping>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkPlanMapping {
    pub key: String,
    pub work_id: WorkId,
    pub short_ref: String,
    pub revision: i64,
}
