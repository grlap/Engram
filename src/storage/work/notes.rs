//! Canonical native note loading shared by explicit record windows and detail.

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};

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

impl super::super::SqliteStore {
    /// Resolve a displayed note identity without promoting observations or
    /// inherited provenance into execution evidence. The final completion
    /// validator independently enforces its exact evidence subset.
    pub(crate) fn resolve_criterion_evidence(
        &self,
        project: &crate::ProjectId,
        work: WorkId,
        run: crate::WorkRunId,
        criterion: usize,
        locator: &str,
        index: &[super::record_windows::WorkRecordIndex],
    ) -> Result<ObjectHash, StoreError> {
        use super::record_windows::WorkRecordFamily;
        let refuse = |reason| StoreError::WorkCriterionLinkInvalid {
            criterion: (criterion > 0).then_some(criterion),
            reason,
        };
        let (prefix, member) = locator
            .split_once(':')
            .map_or((locator, None), |(p, m)| (p, Some(m)));
        if !(8..=64).contains(&prefix.len()) || !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
            return Err(refuse(
                "use a note/gate locator of at least eight hex digits; an artifact path or URL is not the recorded evidence identity",
            ));
        }
        let prefix = prefix.to_ascii_lowercase();
        let matches: Vec<_> = index
            .iter()
            .filter(|row| {
                row.address.hash.as_str().starts_with(&prefix)
                    && member.is_none_or(|m| {
                        row.locator
                            .split_once(':')
                            .is_some_and(|(_, suffix)| suffix == m)
                    })
            })
            .collect();
        if matches.len() > 1 {
            return Err(refuse(
                "note locator is ambiguous; use the complete locator from show --notes --gates",
            ));
        }
        let Some(row) = matches.first() else {
            let kind: Option<String> = self.connection.query_row(
                "SELECT entry.object_kind FROM work_feed_entries entry
                 JOIN work_runs run ON entry.feed_kind = 'run_execution' AND entry.feed_id = run.run_id
                 WHERE run.work_id = ?1 AND entry.object_hash LIKE ?2 LIMIT 1",
                params![work.0.to_string(), format!("{prefix}%")], |row| row.get(0),
            ).optional()?;
            return Err(refuse(match kind.as_deref() {
                Some("work_checkpoint") => {
                    "this is a checkpoint, not its note evidence; use the note locator from show --notes --gates"
                }
                Some("work_event") => {
                    "this is a history event, not note/gate evidence; use show --notes --gates"
                }
                _ => {
                    "locator is not a note/gate on this item; use this item's show --notes --gates locators"
                }
            }));
        };
        // Validate canonical content even for a refusal classification.
        self.work_record_content(project, work, row)?;
        if row.address.member.is_some() {
            return Err(refuse(
                "this is an inherited record member, not current-run evidence; choose an existing current-run holder note or gate",
            ));
        }
        if row.record_family == WorkRecordFamily::Observations {
            return Err(refuse(
                "this is a non-holder or pre-claim observation, not run evidence; choose an existing current-run holder note or gate",
            ));
        }
        let (current_run, any_run): (bool, bool) = self
            .connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM work_run_evidence WHERE work_id = ?1 AND evidence_hash = ?2 AND run_id = ?3),
                        EXISTS(SELECT 1 FROM work_run_evidence WHERE work_id = ?1 AND evidence_hash = ?2)",
                params![work.0.to_string(), row.address.hash.as_str(), run.0.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
        match (current_run, any_run) {
            (true, _) => Ok(row.address.hash.clone()),
            (false, true) => Err(refuse(
                "this evidence belongs to an earlier run, not the current completion; choose a current-run note or gate",
            )),
            (false, false) => Err(refuse(
                "this is restored evidence, not current-run evidence; choose an existing current-run holder note or gate",
            )),
        }
    }

    /// Read a bounded display basis from a canonical, item-bound note. Failure
    /// here is advisory to an already committed seal, never a new completion barrier.
    pub(crate) fn criterion_evidence_preview(
        &self,
        project: &crate::ProjectId,
        work: WorkId,
        hash: &ObjectHash,
    ) -> Result<Option<String>, StoreError> {
        if super::query::load_work_item(&self.connection, work)?.project_id != *project {
            return Err(invalid("criterion evidence preview cannot cross projects"));
        }
        let selected: Option<(String, String)> = self
            .connection
            .query_row(
                &format!(
                    "SELECT notes.family, object.object_kind FROM ({NOTE_OBJECTS}) notes
                JOIN objects object ON object.object_hash = notes.hash WHERE notes.hash = ?2"
                ),
                params![work.0.to_string(), hash.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((family, kind)) = selected else {
            return Ok(None);
        };
        let note = load_note(&self.connection, work, hash, &family, &kind)?;
        Ok(Some(note.gate.map_or(note.summary, |gate| {
            format!(
                "gate {}: {}",
                gate.name,
                if gate.passed { "passed" } else { "failed" }
            )
        })))
    }
}
