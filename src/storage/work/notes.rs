//! Full note reads: inherited generation order followed by dense project-feed order.

use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};

use super::super::{SqliteStore, StoreError};
use super::execution::work_evidence_kind_on;
use super::feeds::load_typed_work_object;
use super::query::{load_work_item, restored_records_for_item};
use crate::domain::{
    ActorContext, EnvironmentEvidence, GateEvidenceRecord, ProjectId, SCHEMA_VERSION,
    VerificationEvidence, WorkEvidence, WorkEvidenceKind, WorkId, WorkObservation,
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

pub(crate) struct WorkNotePage {
    pub items: Vec<WorkNoteRecord>,
    pub total: usize,
}

const NOTE_OBJECTS: &str = "
    SELECT evidence_hash AS hash, 'run' AS family FROM work_run_evidence WHERE work_id = ?1
    UNION ALL
    SELECT evidence_hash, 'restored' FROM work_restored_evidence WHERE work_id = ?1
    UNION ALL
    SELECT observation_hash, 'observation' FROM work_observations WHERE work_id = ?1
";

// A serialized note necessarily carries kind, summary, attribution, timestamp
// and member punctuation beyond its content. This conservative lower bound
// caps retrieval without excluding a prefix that could fit the final envelope.
const MIN_NOTE_ENVELOPE_BYTES: usize = 64;

impl SqliteStore {
    /// Counts all generations and reads only a complete-note prefix that can
    /// fit within the caller's eventual response. Count and prefix share a cut.
    pub(crate) fn work_notes(
        &self,
        project: &ProjectId,
        work_id: WorkId,
        body_budget: usize,
    ) -> Result<WorkNotePage, StoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        let item = load_work_item(&transaction, work_id)?;
        if &item.project_id != project {
            return Err(invalid("notes cannot cross projects"));
        }
        let inherited = restored_records_for_item(&transaction, work_id)?;
        let native_total: i64 = transaction.query_row(
            &format!("SELECT COUNT(*) FROM ({NOTE_OBJECTS})"),
            [work_id.0.to_string()],
            |row| row.get(0),
        )?;
        let total = usize::try_from(native_total).map_err(|_| invalid("note count overflow"))?
            + inherited
                .iter()
                .map(|record| record.history.notes.len())
                .sum::<usize>();
        let mut page = WorkNotePage {
            items: Vec::new(),
            total,
        };
        let mut remaining = body_budget;
        for record in inherited {
            for note in record.history.notes {
                let gate = note.gate.map(|gate| GateEvidenceRecord {
                    schema_version: SCHEMA_VERSION,
                    name: gate.name,
                    passed: gate.passed,
                    failed: gate.failed,
                    previous: None,
                });
                validate_gate(gate.as_ref(), &note.refs)?;
                if !push_note(
                    &mut page,
                    &mut remaining,
                    WorkNoteRecord {
                        kind: note.evidence_kind,
                        summary: note.summary,
                        gate,
                        refs: note.refs,
                        actor: note.actor,
                        recorded_at: note.recorded_at,
                    },
                ) {
                    return Ok(page);
                }
            }
        }
        let mut statement = transaction.prepare(&format!(
            "SELECT notes.hash, notes.family, entry.position, entry.object_kind
             FROM ({NOTE_OBJECTS}) notes
             LEFT JOIN work_feed_entries entry ON entry.object_hash = notes.hash
                 AND entry.feed_kind = 'project' AND entry.feed_id = ?2
             ORDER BY entry.position"
        ))?;
        let mut rows = statement.query(params![work_id.0.to_string(), project.0])?;
        while let Some(row) = rows.next()? {
            let position: Option<i64> = row.get(2)?;
            if position.is_none() {
                return Err(invalid("note is missing its project-feed position"));
            }
            let raw: String = row.get(0)?;
            let hash =
                ObjectHash::from_stored(raw.clone()).ok_or(StoreError::InvalidStoredHash(raw))?;
            let family: String = row.get(1)?;
            let kind: String = row.get(3)?;
            let note = load_note(&transaction, work_id, &hash, &family, &kind)?;
            if !push_note(&mut page, &mut remaining, note) {
                break;
            }
        }
        // Dropping this read-only transaction after its statements releases the cut.
        Ok(page)
    }
}

fn push_note(page: &mut WorkNotePage, remaining: &mut usize, note: WorkNoteRecord) -> bool {
    // Every returned note needs at least these bytes in either front door;
    // receipt shaping accounts for JSON escaping, attribution and the envelope.
    let content = note.gate.as_ref().map_or(note.summary.len(), |gate| {
        gate.failed.iter().fold(gate.name.len(), |bytes, label| {
            bytes.saturating_add(label.len())
        })
    });
    let bytes = note
        .refs
        .iter()
        .fold(content, |bytes, value| bytes.saturating_add(value.len()))
        .saturating_add(MIN_NOTE_ENVELOPE_BYTES);
    if bytes > *remaining {
        return false;
    }
    *remaining -= bytes;
    page.items.push(note);
    true
}

fn validate_gate(gate: Option<&GateEvidenceRecord>, refs: &[String]) -> Result<(), StoreError> {
    gate.map_or(Ok(()), |gate| {
        gate.validate(refs)
            .map_err(StoreError::InvalidWorkProjection)
    })
}

fn load_note(
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
