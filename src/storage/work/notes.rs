//! Canonical native note loading shared by explicit record windows and detail.

use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};

use super::super::StoreError;
use super::execution::work_evidence_kind_on;
use super::feeds::load_typed_work_object;
use crate::domain::{
    ActorContext, EnvironmentEvidence, GateEvidenceRecord, VerificationEvidence, WorkEvidence,
    WorkEvidenceKind, WorkId, WorkObservation,
};
use crate::{ObjectHash, RestoredWorkEvidence};

pub(crate) struct WorkNoteRecord {
    pub kind: WorkEvidenceKind,
    pub summary: String,
    pub gate: Option<GateEvidenceRecord>,
    pub refs: Vec<String>,
    pub actor: ActorContext,
    pub recorded_at: DateTime<Utc>,
}

pub(super) const NOTE_OBJECTS: &str = "
    SELECT evidence_hash AS hash, 'run' AS family, NULL AS restored_gate
    FROM work_run_evidence WHERE work_id = ?1
    UNION ALL
    SELECT evidence_hash, 'restored', gate_name IS NOT NULL
    FROM work_restored_evidence WHERE work_id = ?1
    UNION ALL
    SELECT observation_hash, 'observation', NULL FROM work_observations WHERE work_id = ?1
";

fn validate_gate(gate: Option<&GateEvidenceRecord>, refs: &[String]) -> Result<(), StoreError> {
    gate.map_or(Ok(()), |gate| {
        gate.validate(refs)
            .map_err(StoreError::InvalidWorkProjection)
    })
}

pub(super) fn load_note(
    connection: &Connection,
    work_id: WorkId,
    hash: &ObjectHash,
    family: &str,
    kind: &str,
) -> Result<WorkNoteRecord, StoreError> {
    let (subject, note) = match (family, kind) {
        ("run", "work_evidence") => {
            let evidence: WorkEvidence = load_typed_work_object(connection, hash, kind)?;
            let evidence_kind = work_evidence_kind_on(connection, evidence.run_id, hash)?;
            crate::domain::validate_gate_evidence_payload(&evidence)
                .map_err(StoreError::InvalidWorkProjection)?;
            (
                evidence.work_id,
                WorkNoteRecord {
                    kind: evidence_kind,
                    summary: evidence.summary,
                    gate: evidence.gate,
                    refs: evidence.refs,
                    actor: evidence.actor,
                    recorded_at: evidence.created_at,
                },
            )
        }
        ("run", "verification_evidence") => {
            let evidence: VerificationEvidence = load_typed_work_object(connection, hash, kind)?;
            work_evidence_kind_on(connection, evidence.binding.run_id, hash)?;
            (
                evidence.binding.work_id,
                WorkNoteRecord {
                    kind: WorkEvidenceKind::Verification,
                    summary: evidence.summary,
                    gate: None,
                    refs: evidence.refs,
                    actor: evidence.actor,
                    recorded_at: evidence.recorded_at,
                },
            )
        }
        ("run", "environment_evidence") => {
            let evidence: EnvironmentEvidence = load_typed_work_object(connection, hash, kind)?;
            work_evidence_kind_on(connection, evidence.binding.run_id, hash)?;
            (
                evidence.binding.work_id,
                WorkNoteRecord {
                    kind: WorkEvidenceKind::Environment,
                    summary: String::new(),
                    gate: None,
                    refs: Vec::new(),
                    actor: evidence.actor,
                    recorded_at: evidence.recorded_at,
                },
            )
        }
        ("restored", "work_restored_evidence") => {
            let evidence: RestoredWorkEvidence = load_typed_work_object(connection, hash, kind)?;
            let matches: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM work_restored_evidence evidence
                 JOIN work_restored_records record ON record.record_hash = evidence.record_hash
                    AND record.work_id = evidence.work_id
                 WHERE evidence.evidence_hash = ?1 AND evidence.record_hash = ?2
                    AND evidence.sequence = ?3 AND evidence.created_at_ms = ?4)",
                params![
                    hash.as_str(),
                    evidence.restored_record.as_str(),
                    evidence.sequence,
                    evidence.created_at.timestamp_millis()
                ],
                |row| row.get(0),
            )?;
            if !matches {
                return Err(invalid("restored note differs from its projection"));
            }
            validate_gate(evidence.gate.as_ref(), &evidence.refs)?;
            (
                evidence.work_id,
                WorkNoteRecord {
                    kind: WorkEvidenceKind::Generic,
                    summary: evidence.summary,
                    gate: evidence.gate,
                    refs: evidence.refs,
                    actor: evidence.actor,
                    recorded_at: evidence.created_at,
                },
            )
        }
        ("observation", "work_observation") => {
            let observation: WorkObservation = load_typed_work_object(connection, hash, kind)?;
            super::observation::validate(connection, &observation)?;
            (
                observation.work_id,
                WorkNoteRecord {
                    kind: WorkEvidenceKind::Generic,
                    summary: observation.summary,
                    gate: None,
                    refs: observation.refs,
                    actor: observation.actor,
                    recorded_at: observation.created_at,
                },
            )
        }
        _ => return Err(invalid("note family differs from its canonical kind")),
    };
    if subject != work_id {
        return Err(invalid("note differs from its work binding"));
    }
    Ok(note)
}

fn invalid(message: &str) -> StoreError {
    StoreError::InvalidWorkProjection(message.into())
}
