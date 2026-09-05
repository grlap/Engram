//! Attributed work observations that carry no live execution authority.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{ActorContext, ProjectId, ProvenanceLink, ProvenanceRelation, SessionId, WorkId};
use crate::ObjectHash;

/// Bounds all initial observations in one root creation or decomposition.
pub(crate) const MAX_INITIAL_WORK_NOTES: usize = 16;

pub(crate) fn validate_initial_work_note_count(count: usize) -> Result<(), String> {
    if count > MAX_INITIAL_WORK_NOTES {
        return Err(format!(
            "initial notes count {count} exceeds limit {MAX_INITIAL_WORK_NOTES}"
        ));
    }
    Ok(())
}

pub(crate) fn normalize_initial_work_notes(notes: &[String]) -> Result<Vec<String>, String> {
    validate_initial_work_note_count(notes.len())?;
    notes
        .iter()
        .enumerate()
        .map(|(index, note)| {
            let note = note.trim();
            if note.is_empty() {
                Err(format!("initial note {} must not be blank", index + 1))
            } else {
                Ok(note.to_owned())
            }
        })
        .collect()
}

pub(crate) const NON_HOLDER_NOTE_SOURCE: &str = "work_observation:non_holder";
pub(crate) const NON_HOLDER_NOTE_REFERENCE: &str = "non_holder";

pub(crate) fn is_non_holder_note_marker(link: &ProvenanceLink) -> bool {
    link.relation == ProvenanceRelation::DerivedFrom
        && link.source == NON_HOLDER_NOTE_SOURCE
        && link.reference.as_deref() == Some(NON_HOLDER_NOTE_REFERENCE)
}

/// Mark authority-free observation provenance without rewriting asserted context.
/// The caller supplies creation or standalone-note tool/reason defaults.
pub(crate) fn non_holder_note_actor(mut actor: ActorContext) -> ActorContext {
    actor.provenance_chain.push(ProvenanceLink {
        relation: ProvenanceRelation::DerivedFrom,
        source: NON_HOLDER_NOTE_SOURCE.into(),
        reference: Some(NON_HOLDER_NOTE_REFERENCE.into()),
    });
    actor
}

const PEER_CHILD_PROPOSAL_SOURCE: &str = "work_planning:peer_child_proposal";
const PEER_CHILD_PROPOSAL_REFERENCE: &str = "optional_child";

pub(crate) fn peer_child_proposal_actor(mut actor: ActorContext) -> ActorContext {
    actor.provenance_chain.push(ProvenanceLink {
        relation: ProvenanceRelation::DerivedFrom,
        source: PEER_CHILD_PROPOSAL_SOURCE.into(),
        reference: Some(PEER_CHILD_PROPOSAL_REFERENCE.into()),
    });
    actor
}

pub(crate) fn is_peer_child_proposal_marker(link: &ProvenanceLink) -> bool {
    link.relation == ProvenanceRelation::DerivedFrom
        && link.source == PEER_CHILD_PROPOSAL_SOURCE
        && link.reference.as_deref() == Some(PEER_CHILD_PROPOSAL_REFERENCE)
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
