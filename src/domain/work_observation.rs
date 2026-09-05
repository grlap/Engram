//! Attributed work observations that carry no live execution authority.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{ActorContext, ProjectId, ProvenanceLink, ProvenanceRelation, SessionId, WorkId};
use crate::ObjectHash;

pub(crate) const NON_HOLDER_NOTE_SOURCE: &str = "work_observation:non_holder";
pub(crate) const NON_HOLDER_NOTE_REFERENCE: &str = "non_holder";

pub(crate) fn is_non_holder_note_marker(link: &ProvenanceLink) -> bool {
    link.relation == ProvenanceRelation::DerivedFrom
        && link.source == NON_HOLDER_NOTE_SOURCE
        && link.reference.as_deref() == Some(NON_HOLDER_NOTE_REFERENCE)
}

/// The immutable planning state observed by a non-holder, not an execution grant.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkObservationBasis {
    NativeEvent { event: ObjectHash },
    RestoredRecord { record: ObjectHash },
}

/// A non-holder's note on open work. It enters project/root feeds only and
/// never supplies a run checkpoint, contribution or completion-seal credit.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkObservation {
    pub schema_version: u16,
    pub project_id: ProjectId,
    pub root_id: WorkId,
    pub work_id: WorkId,
    pub work_revision: i64,
    pub basis: WorkObservationBasis,
    pub sequence: i64,
    pub summary: String,
    pub refs: Vec<String>,
    pub actor: ActorContext,
    pub created_at: DateTime<Utc>,
}

#[derive(Serialize)]
pub(crate) struct RecordWorkObservationRequest {
    pub project_id: ProjectId,
    pub work_id: WorkId,
    pub expected_work_revision: i64,
    pub session_id: SessionId,
    pub summary: String,
    pub refs: Vec<String>,
    pub actor: ActorContext,
    pub idempotency_key: String,
    pub recorded_at: DateTime<Utc>,
}
