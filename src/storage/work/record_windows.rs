//! Transient note/history navigation over existing canonical objects. Nothing
//! here creates identities, persists cursors, or grants execution authority.

use chrono::{DateTime, Utc};
use rusqlite::params;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use super::super::{SqliteStore, StoreError};
use super::feeds::load_typed_work_object;
use super::notes::{NOTE_OBJECTS, WorkNoteRecord, load_note};
use super::query::{load_work_item, restored_records_for_item};
use crate::{
    ActorContext, FeedId, FeedPosition, ObjectHash, ProjectId, RestoredRecord, WorkEvent, WorkId,
};

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkRecordKind {
    Notes,
    NotesWithGates,
    History,
}

impl WorkRecordKind {
    pub(crate) fn word(self) -> &'static str {
        match self {
            Self::Notes | Self::NotesWithGates => "notes",
            Self::History => "history",
        }
    }

    pub(crate) fn is_notes(self) -> bool {
        matches!(self, Self::Notes | Self::NotesWithGates)
    }
}

/// Presentation families are disjoint; a structured gate is never inferred
/// from prose. Counts cover the complete index before window filtering.
#[derive(Clone, Copy, Debug, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum WorkRecordFamily {
    Notes,
    Observations,
    Gates,
    History,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RestoredMember {
    Note(usize),
    Event(usize),
    Completion,
}

/// Existing object identity, optionally selecting a member of an immutable
/// restored record. Member positions are one-based, never display ordinals.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkRecordAddress {
    pub hash: ObjectHash,
    pub member: Option<RestoredMember>,
}

impl WorkRecordAddress {
    pub(crate) fn locator(&self, hash_chars: usize) -> String {
        let hash = &self.hash.as_str()[..hash_chars];
        match self.member {
            None => hash.to_owned(),
            Some(RestoredMember::Note(index)) => format!("{hash}:{index}"),
            Some(RestoredMember::Event(index)) => format!("{hash}:event-{index}"),
            Some(RestoredMember::Completion) => format!("{hash}:completion"),
        }
    }
}

/// Inherited generations precede native project-feed positions. Within an
/// inherited layer the member order is derived from that immutable record.
#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct WorkRecordOrder {
    layer: u8,
    generation: usize,
    position: i64,
}

impl WorkRecordOrder {
    /// Only native rows have a position in this host's project feed. Inherited
    /// member order must never masquerade as a project-feed position.
    pub(crate) fn project_position(&self) -> Option<i64> {
        (self.layer == 1).then_some(self.position)
    }
}

pub(crate) struct WorkRecordIndex {
    pub address: WorkRecordAddress,
    pub order: WorkRecordOrder,
    pub locator: String,
    pub record_family: WorkRecordFamily,
    family: String,
    object_kind: String,
    // Shared verified immutable bytes decoded by this snapshot's index read.
    // Member reads borrow this record, never reload its whole history.
    restored: Option<Arc<RestoredRecord>>,
}

pub(crate) enum WorkRecordContent {
    Note(WorkNoteRecord),
    Event(Box<WorkEvent>, FeedPosition),
    InheritedHistory {
        kind: String,
        summary: String,
        actor: ActorContext,
        recorded_at: DateTime<Utc>,
    },
}

impl SqliteStore {
    /// The caller owns one read snapshot encompassing metadata and body reads.
    /// Native family classification probes stored JSON only for work evidence,
    /// the sole native kind that can carry a gate. Other kinds and restored
    /// evidence use projection metadata without loading their object bodies.
    /// These navigation probes are not canonical verification: selected content
    /// is verified by `work_record_content`, and doctor checks the full store.
    /// Both notes modes index all families; the service filters after counting
    /// so explicit detail and item-wide family totals never lose gate members.
    pub(crate) fn work_record_index(
        &self,
        project: &ProjectId,
        work_id: WorkId,
        kind: WorkRecordKind,
    ) -> Result<Vec<WorkRecordIndex>, StoreError> {
        if load_work_item(&self.connection, work_id)?.project_id != *project {
            return Err(invalid("record window cannot cross projects"));
        }
        let mut indexed = Vec::new();
        for record in restored_records_for_item(&self.connection, work_id)? {
            // Use the verified stored identity, never re-freeze historical data.
            let raw: String = self.connection.query_row(
                "SELECT record_hash FROM work_restored_records WHERE work_id = ?1 AND generation_index = ?2",
                params![work_id.0.to_string(), i64::try_from(record.generation_index).map_err(|_| invalid("record generation overflow"))?], |row| row.get(0),
            )?;
            let hash = parse_hash(raw)?;
            let record = Arc::new(record);
            let mut members = record
                .history
                .notes
                .iter()
                .enumerate()
                .map(|(index, note)| (note.recorded_at, RestoredMember::Note(index + 1)))
                .collect::<Vec<_>>();
            if kind == WorkRecordKind::History {
                members.extend(
                    record
                        .history
                        .events
                        .iter()
                        .enumerate()
                        .map(|(index, event)| {
                            (event.occurred_at, RestoredMember::Event(index + 1))
                        }),
                );
                if let Some(completion) = &record.history.completion {
                    members.push((completion.completed_at, RestoredMember::Completion));
                }
                // Preserve the existing restored history's stable timestamp order.
                members.sort_by_key(|(time, _)| *time);
            }
            for (position, (_, member)) in members.into_iter().enumerate() {
                let record_family = match member {
                    RestoredMember::Note(index) => {
                        let note = &record.history.notes[index - 1];
                        if note.gate.is_some() {
                            WorkRecordFamily::Gates
                        } else if note
                            .actor
                            .provenance_chain
                            .iter()
                            .any(crate::domain::is_non_holder_note_marker)
                        {
                            WorkRecordFamily::Observations
                        } else {
                            WorkRecordFamily::Notes
                        }
                    }
                    RestoredMember::Event(_) | RestoredMember::Completion => {
                        WorkRecordFamily::History
                    }
                };
                indexed.push(WorkRecordIndex {
                    record_family,
                    address: WorkRecordAddress {
                        hash: hash.clone(),
                        member: Some(member),
                    },
                    order: WorkRecordOrder {
                        layer: 0,
                        generation: record.generation_index,
                        position: i64::try_from(position)
                            .map_err(|_| invalid("record order overflow"))?,
                    },
                    locator: String::new(),
                    family: "inherited".into(),
                    object_kind: "work_restored_record".into(),
                    restored: Some(Arc::clone(&record)),
                });
            }
        }
        let sql = match kind {
            WorkRecordKind::Notes | WorkRecordKind::NotesWithGates => format!(
                "SELECT notes.hash, notes.family, entry.position, entry.object_kind,
                    CASE WHEN notes.family = 'observation' THEN 'observations'
                         WHEN notes.family = 'restored' THEN
                             CASE WHEN notes.restored_gate THEN 'gates' ELSE 'notes' END
                         WHEN entry.object_kind = 'work_evidence' THEN
                             CASE WHEN json_type(object.canonical_json, '$.gate') = 'object'
                                  THEN 'gates' ELSE 'notes' END
                         ELSE 'notes' END
                 FROM ({NOTE_OBJECTS}) notes
                 LEFT JOIN work_feed_entries entry
                   ON entry.object_hash = notes.hash AND entry.feed_kind = 'project' AND entry.feed_id = ?2
                 LEFT JOIN objects object ON notes.family = 'run'
                   AND entry.object_kind = 'work_evidence' AND object.object_hash = notes.hash
                 ORDER BY entry.position"),
            WorkRecordKind::History =>
                "SELECT entry.object_hash, 'event', entry.position, entry.object_kind, 'history'
                 FROM work_feed_entries entry WHERE entry.work_id = ?1 AND entry.feed_kind = 'project'
                   AND entry.feed_id = ?2 AND entry.object_kind = 'work_event' ORDER BY entry.position".into(),
        };
        let mut statement = self.connection.prepare(&sql)?;
        let mut rows = statement.query(params![work_id.0.to_string(), project.0])?;
        while let Some(row) = rows.next()? {
            let position: Option<i64> = row.get(2)?;
            indexed.push(WorkRecordIndex {
                record_family: match row.get::<_, String>(4)?.as_str() {
                    "gates" => WorkRecordFamily::Gates,
                    "observations" => WorkRecordFamily::Observations,
                    "notes" => WorkRecordFamily::Notes,
                    "history" => WorkRecordFamily::History,
                    _ => return Err(invalid("unknown record family")),
                },
                address: WorkRecordAddress {
                    hash: parse_hash(row.get(0)?)?,
                    member: None,
                },
                order: WorkRecordOrder {
                    layer: 1,
                    generation: 0,
                    position: position
                        .ok_or_else(|| invalid("record is missing its project-feed position"))?,
                },
                locator: String::new(),
                family: row.get(1)?,
                object_kind: row.get(3)?,
                restored: None,
            });
        }
        for entry in &mut indexed {
            // Full locators are unambiguous without a quadratic prefix scan.
            // The detail resolver also accepts unique prefixes of eight digits.
            entry.locator = entry.address.locator(64);
        }
        Ok(indexed)
    }

    pub(crate) fn work_record_content(
        &self,
        project: &ProjectId,
        work_id: WorkId,
        index: &WorkRecordIndex,
    ) -> Result<WorkRecordContent, StoreError> {
        if let Some(member) = &index.address.member {
            let record = index
                .restored
                .as_deref()
                .ok_or_else(|| invalid("inherited member has no verified record"))?;
            if record.work_id != work_id || record.project_id != *project {
                return Err(invalid("inherited member belongs to another work item"));
            }
            return inherited_content(record, member);
        }
        if index.family == "event" {
            let event: WorkEvent =
                load_typed_work_object(&self.connection, &index.address.hash, "work_event")?;
            if event.work_id != work_id || event.project_id != *project {
                return Err(invalid("history event belongs to another work item"));
            }
            return Ok(WorkRecordContent::Event(
                Box::new(event),
                FeedPosition {
                    feed: FeedId::Project(project.clone()),
                    position: index.order.position,
                },
            ));
        }
        Ok(WorkRecordContent::Note(load_note(
            &self.connection,
            work_id,
            &index.address.hash,
            &index.family,
            &index.object_kind,
        )?))
    }
}

fn inherited_content(
    record: &RestoredRecord,
    member: &RestoredMember,
) -> Result<WorkRecordContent, StoreError> {
    Ok(match *member {
        RestoredMember::Note(index) => {
            let note = record
                .history
                .notes
                .get(
                    index
                        .checked_sub(1)
                        .ok_or_else(|| invalid("invalid note member"))?,
                )
                .ok_or_else(|| invalid("missing note member"))?
                .clone();
            WorkRecordContent::Note(WorkNoteRecord {
                kind: note.evidence_kind,
                summary: note.summary,
                refs: note.refs,
                gate: note.gate.map(|gate| crate::GateEvidenceRecord {
                    schema_version: crate::domain::SCHEMA_VERSION,
                    name: gate.name,
                    passed: gate.passed,
                    failed: gate.failed,
                    previous: None,
                }),
                actor: note.actor,
                recorded_at: note.recorded_at,
            })
        }
        RestoredMember::Event(index) => {
            let event = record
                .history
                .events
                .get(
                    index
                        .checked_sub(1)
                        .ok_or_else(|| invalid("invalid event member"))?,
                )
                .ok_or_else(|| invalid("missing event member"))?
                .clone();
            WorkRecordContent::InheritedHistory {
                summary: event.reason.unwrap_or_else(|| event.kind.clone()),
                kind: event.kind,
                actor: event.actor,
                recorded_at: event.occurred_at,
            }
        }
        RestoredMember::Completion => {
            let completion = record
                .history
                .completion
                .as_ref()
                .ok_or_else(|| invalid("missing completion member"))?;
            WorkRecordContent::InheritedHistory {
                kind: "completed".into(),
                summary: completion.summary.clone(),
                actor: completion.actor.clone(),
                recorded_at: completion.completed_at,
            }
        }
    })
}

fn parse_hash(raw: String) -> Result<ObjectHash, StoreError> {
    ObjectHash::from_stored(raw.clone()).ok_or(StoreError::InvalidStoredHash(raw))
}
fn invalid(message: &str) -> StoreError {
    StoreError::InvalidWorkProjection(message.into())
}
