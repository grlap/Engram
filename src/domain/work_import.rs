//! Explicit external intake. Source changes notify; they never revise local work.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use super::{ActorContext, ProjectId, WorkId, WorkObservationBasis, WorkSourceSnapshot};
use crate::ObjectHash;

/// Exact source identity within one local project. Neither field is a display alias.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkSourceKey {
    pub adapter_kind: String,
    pub canonical_ref: String,
}

/// Authored local contract for first intake, independent of external status/owner.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkImportDraft {
    pub title: String,
    pub outcome: String,
    /// Omission means no authored criteria, never a title-derived criterion.
    #[serde(default)]
    pub acceptance: Vec<String>,
}

/// File input shared by preview and apply. A refresh has no local draft.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkImportInput {
    pub snapshot: WorkSourceSnapshot,
    pub draft: Option<WorkImportDraft>,
}

/// Inert source-change provenance retained by planning recovery.
/// The work revision is historical metadata, never authority to apply a patch.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkSourceNotice {
    pub work_revision: i64,
    pub cited_snapshot: ObjectHash,
    pub proposed_snapshot: ObjectHash,
    pub actor: ActorContext,
    pub recorded_at: DateTime<Utc>,
}

/// Native immutable notification tied to the verified local cut at capture.
/// It enters project/root feeds, never a run feed or completion evidence.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkSourceProposal {
    pub schema_version: u16,
    pub project_id: ProjectId,
    pub work_id: WorkId,
    pub root_id: WorkId,
    pub basis: WorkObservationBasis,
    pub notice: WorkSourceNotice,
}

/// The only effects an import may propose or report.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkImportEffect {
    Create,
    Notify,
    AlreadyKnown,
}

/// Read-only preview. Apply carries its token and rechecks the same local cut.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkImportPreview {
    pub project_id: ProjectId,
    pub source_key: WorkSourceKey,
    pub snapshot: ObjectHash,
    pub effect: WorkImportEffect,
    pub work_id: Option<WorkId>,
    pub work_ref: Option<String>,
    pub work_revision: Option<i64>,
    pub cited_snapshot: Option<ObjectHash>,
    pub draft: Option<WorkImportDraft>,
    pub preview_token: ObjectHash,
}

/// Committed import facts, not a claim that later local state stayed unchanged.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkImportReceipt {
    pub effect: WorkImportEffect,
    pub source_key: WorkSourceKey,
    pub snapshot: ObjectHash,
    pub cited_snapshot: ObjectHash,
    pub work_id: WorkId,
    pub work_ref: String,
    pub work_revision: i64,
    pub proposal: Option<ObjectHash>,
}

/// Current source citation, with the latest notification kept distinct from it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkSourceLookup {
    pub source_key: WorkSourceKey,
    pub work_id: WorkId,
    pub work_ref: String,
    pub work_revision: i64,
    pub cited_snapshot: ObjectHash,
    pub notice_count: usize,
    pub latest_notice: Option<WorkSourceNotice>,
}

/// Operator detail exposes immutable snapshots, not local execution state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkSourceDetail {
    #[serde(flatten)]
    pub lookup: WorkSourceLookup,
    pub notices_omitted: usize,
    pub cited_source: WorkSourceSnapshot,
    pub latest_proposed_source: Option<WorkSourceSnapshot>,
}
