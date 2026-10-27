//! Rebuildable public note text, derived once at append and queried without
//! canonical decoding. Explicit detail and doctor retain canonical validation.

use rusqlite::{Connection, Statement, params};

use super::super::StoreError;
use super::notes::search_fields;
use super::record_windows::{
    RestoredMember, WorkRecordAddress, WorkRecordContent, WorkRecordFamily, WorkRecordKind,
    WorkRecordOrder, WorkRecordReader, inherited_note_family,
};
use crate::{FeedId, FeedPosition, ObjectId, ProjectId, RestoredRecord, WorkEvidenceKind, WorkId};

pub(super) fn create_schema(connection: &Connection) -> Result<(), StoreError> {
    connection.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS work_note_fts USING fts5(
        locator UNINDEXED, work_id UNINDEXED, family UNINDEXED,
        layer UNINDEXED, generation UNINDEXED, position UNINDEXED,
        fields_json UNINDEXED, search_text, tokenize='trigram remove_diacritics 0'
    );",
    )?;
    Ok(())
}

struct NoteRow {
    work_id: WorkId,
    address: WorkRecordAddress,
    family: WorkRecordFamily,
    order: WorkRecordOrder,
    fields: Vec<String>,
}

impl NoteRow {
    fn values(&self) -> Result<[String; 8], StoreError> {
        Ok([
            self.address.locator(64),
            self.work_id.0.to_string(),
            serde_json::to_value(self.family)?
                .as_str()
                .ok_or_else(|| invalid("note family is not text"))?
                .to_owned(),
            self.order.layer.to_string(),
            self.order.generation.to_string(),
            self.order.position.to_string(),
            serde_json::to_string(&self.fields)?,
            self.fields.join("\n"),
        ])
    }

    fn insert_with(&self, statement: &mut Statement<'_>) -> Result<(), StoreError> {
        let values = self.values()?;
        statement.execute(rusqlite::params_from_iter(values.iter()))?;
        Ok(())
    }

    fn insert(&self, connection: &Connection) -> Result<(), StoreError> {
        self.insert_with(&mut connection.prepare(INSERT_NOTE)?)
    }
}

const INSERT_NOTE: &str = "INSERT INTO work_note_fts
    (locator, work_id, family, layer, generation, position, fields_json, search_text)
    VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)";

pub(super) enum NativeNote<'a> {
    Evidence(&'a crate::WorkEvidence),
    Verification(&'a crate::VerificationEvidence),
    Observation(&'a crate::domain::WorkObservation),
    Restored(&'a crate::RestoredWorkEvidence),
}

/// The writer supplies its already validated value and appended positions;
/// projecting the public fields never decodes the newly minted object again.
pub(super) fn append_native(
    connection: &Connection,
    positions: &[FeedPosition],
    object_id: &ObjectId,
    note: &NativeNote<'_>,
) -> Result<(), StoreError> {
    let position = positions
        .iter()
        .find(|position| matches!(position.feed, FeedId::Project(_)))
        .ok_or_else(|| invalid("note search projection has no project feed"))?;
    let FeedId::Project(project) = &position.feed else {
        return Ok(());
    };
    let (work_id, fields, family) = match note {
        NativeNote::Evidence(note) => (
            note.work_id,
            search_fields(
                &note.summary,
                &note.refs,
                note.gate
                    .as_ref()
                    .map(|gate| (gate.name.as_str(), gate.failed.as_slice())),
            ),
            if note.gate.is_some() {
                WorkRecordFamily::Gates
            } else {
                WorkRecordFamily::Notes
            },
        ),
        NativeNote::Verification(note) => (
            note.binding.work_id,
            search_fields(&note.summary, &note.refs, None),
            WorkRecordFamily::Notes,
        ),
        NativeNote::Observation(note) => (
            note.work_id,
            search_fields(&note.summary, &note.refs, None),
            WorkRecordFamily::Observations,
        ),
        NativeNote::Restored(note) => (
            note.work_id,
            search_fields(
                &note.summary,
                &note.refs,
                note.gate
                    .as_ref()
                    .map(|gate| (gate.name.as_str(), gate.failed.as_slice())),
            ),
            if note.gate.is_some() {
                WorkRecordFamily::Gates
            } else {
                WorkRecordFamily::Notes
            },
        ),
    };
    let bound: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_items
        WHERE work_id = ?1 AND project_id = ?2)",
        params![work_id.0.to_string(), project.0],
        |row| row.get(0),
    )?;
    if !bound {
        return Err(invalid("note search projection crosses its project feed"));
    }
    NoteRow {
        work_id,
        fields,
        family,
        address: WorkRecordAddress {
            hash: object_id.clone(),
            member: None,
        },
        order: WorkRecordOrder {
            layer: 1,
            generation: 0,
            position: position.position,
        },
    }
    .insert(connection)
}

pub(in crate::storage) fn append_inherited(
    connection: &Connection,
    record_id: &ObjectId,
    record: &RestoredRecord,
) -> Result<(), StoreError> {
    inherited_rows(record_id, record, |row| row.insert(connection))
}

fn inherited_rows(
    record_id: &ObjectId,
    record: &RestoredRecord,
    mut visit: impl FnMut(NoteRow) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    for (index, note) in record.history.notes.iter().enumerate() {
        visit(NoteRow {
            work_id: record.work_id,
            address: WorkRecordAddress {
                hash: record_id.clone(),
                member: Some(RestoredMember::Note(index + 1)),
            },
            family: inherited_note_family(note),
            order: WorkRecordOrder {
                layer: 0,
                generation: record.generation_index,
                position: i64::try_from(index)
                    .map_err(|_| invalid("note member order overflow"))?,
            },
            fields: search_fields(
                &note.summary,
                &note.refs,
                note.gate
                    .as_ref()
                    .map(|gate| (gate.name.as_str(), gate.failed.as_slice())),
            ),
        })?;
    }
    Ok(())
}

/// Canonical derivation for doctor and explicit repair only. Ordinary writes
/// project just the appended record, never an item or the complete history.
fn expected_rows(
    connection: &Connection,
    findings: &mut Vec<String>,
    mut visit: impl FnMut(NoteRow) -> Result<(), StoreError>,
) -> Result<(), StoreError> {
    let reader = WorkRecordReader::new(connection);
    let mut items =
        connection.prepare("SELECT work_id, project_id FROM work_items ORDER BY work_id")?;
    for item in items.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (work_id, project) = item?;
        let label = format!("work_note:{work_id}:canonical_binding");
        let derived = (|| -> Result<(), StoreError> {
            let work_id = super::query::parse_work_id(&work_id)?;
            let project = ProjectId(project);
            for entry in
                reader.work_record_index(&project, work_id, WorkRecordKind::NotesWithGates)?
            {
                let WorkRecordContent::Note(note) =
                    reader.public_note_content(&project, work_id, &entry)?
                else {
                    return Err(invalid("note record index returned history"));
                };
                if entry.address.member.is_none() && note.kind == WorkEvidenceKind::Environment {
                    continue;
                }
                visit(NoteRow {
                    work_id,
                    address: entry.address,
                    family: entry.record_family,
                    order: entry.order,
                    fields: note.search_fields(),
                })?;
            }
            Ok(())
        })();
        match derived {
            Ok(()) => {}
            Err(error @ StoreError::Sqlite(_)) => return Err(error),
            // Repair must reach the full doctor so every bad record is named.
            // Decode diagnostics name shape/member, never stored field values.
            Err(error) => findings.push(crate::storage::decode_failure_label(label, &error)),
        }
    }
    Ok(())
}

pub(super) fn rebuild(connection: &Connection) -> Result<(), StoreError> {
    let findings = rebuild_for_repair(connection)?;
    if !findings.is_empty() {
        return Err(invalid(&format!(
            "note search derivation refused: {}",
            findings.join(", ")
        )));
    }
    Ok(())
}

/// Full repair collects derivation defects and still runs its exhaustive
/// doctor before refusing and rolling back every rebuilt projection.
pub(super) fn rebuild_for_repair(connection: &Connection) -> Result<Vec<String>, StoreError> {
    let mut insert = connection.prepare(INSERT_NOTE)?;
    let mut findings = Vec::new();
    expected_rows(connection, &mut findings, |row| {
        row.insert_with(&mut insert)
    })?;
    Ok(findings)
}

pub(in crate::storage) fn require_missing_on(connection: &Connection) -> Result<(), StoreError> {
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema
         WHERE name = 'work_note_fts' OR name GLOB 'work_note_fts_*')",
        [],
        |row| row.get(0),
    )?;
    if exists {
        return Err(invalid(
            "note search index already exists or has damaged target objects; use doctor --repair-projections",
        ));
    }
    Ok(())
}

/// Caller owns the transaction and the durable/policy preflights.
pub(in crate::storage) fn build_missing_on(connection: &Connection) -> Result<usize, StoreError> {
    require_missing_on(connection)?;
    create_schema(connection)?;
    // Creating only the absent target lets the normal, complete schema guard
    // check every other work projection before population. Errors roll back.
    for owner in [
        super::super::SchemaOwner::Core,
        super::super::SchemaOwner::Work,
    ] {
        if let Some(issue) = super::super::current_schema_definition_issue(
            connection,
            owner,
            super::super::SchemaDurability::Rebuildable,
        )? {
            return Err(invalid(&issue));
        }
    }
    rebuild(connection)?;
    let mut checked = 0;
    let mut findings = Vec::new();
    verify(connection, &mut checked, &mut findings)?;
    if !findings.is_empty() {
        return Err(invalid(&format!(
            "note search index build refused: {}",
            findings.join(", ")
        )));
    }
    Ok(checked)
}

pub(super) fn verify(
    connection: &Connection,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    use crate::storage::fts_verification::{bounded_finding, fts_content, fts_index_finding, text};
    let content = fts_content(
        connection,
        "SELECT locator, work_id, family, layer, generation, position, fields_json, search_text FROM work_note_fts",
        |row| {
            (1..8)
                .map(|index| text(row, index))
                .collect::<rusqlite::Result<Vec<_>>>()
        },
    )?;
    match content {
        Ok(mut content) => {
            let mut derivation_findings = Vec::new();
            expected_rows(connection, &mut derivation_findings, |row| {
                *checked += 1;
                let values = row.values()?;
                let expected = values[1..].iter().cloned().map(Some).collect::<Vec<_>>();
                if content.remove(&Some(values[0].clone())).as_deref()
                    != Some(std::slice::from_ref(&expected))
                {
                    invalid.push(format!("work_note:{}:projection_binding", values[0]));
                }
                Ok(())
            })?;
            invalid.extend(derivation_findings);
            if !content.is_empty() {
                invalid.push("work_note:orphaned_fts_rows".into());
            }
        }
        Err(detail) => invalid.push(bounded_finding("work_note:fts_content", &detail)),
    }
    if let Some(finding) = fts_index_finding(connection, "work_note_fts", "work_note:fts_index")? {
        invalid.push(finding);
    }
    Ok(())
}

fn invalid(message: &str) -> StoreError {
    StoreError::InvalidWorkProjection(message.into())
}
