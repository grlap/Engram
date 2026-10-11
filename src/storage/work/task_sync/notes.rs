//! Shared-note capture uses existing task-bound projections and primary-key origins.

use super::{
    Connection, ImportOrigin, ORIGIN_KIND, ObjectId, RestoredRecord, SharedActor, SqliteStore,
    StoreError, TaskTree, WorkItem, invalid, origin,
};
use crate::task_sync::notes::{
    NoteOmissions, SharedNote, SharedNoteKind, author_exclusion, prose_exclusion, restricted,
};

pub(super) fn capture(
    connection: &Connection,
    item: &WorkItem,
    machine: Option<&str>,
    include_native: bool,
    comparison: bool,
    omissions: &mut NoteOmissions,
) -> Result<(Vec<SharedNote>, bool), StoreError> {
    let origin = origin::for_item(connection, item)?;
    let included = origin.as_ref().is_some_and(|origin| origin.notes_included);
    let mut notes = Vec::new();
    if let Some(origin) = &origin {
        for note in &origin.task.notes {
            if !comparison && restricted(&item.labels) {
                omissions.omit("restricted task");
            } else {
                notes.push(note.clone());
            }
        }
    }
    // Planning-only comparison still notices retained shared notes, but local
    // prose cannot change a repeat of a candidate that never included notes.
    if !include_native {
        return Ok((notes, included));
    }
    // Inherited snapshot members lost their original note id. Import-owned
    // history is only a view of the shared members already read above.
    let records = connection.prepare(
        "SELECT record_id FROM work_restored_records WHERE work_id = ?1 ORDER BY generation_index",
    )?.query_map([item.work_id.0.to_string()], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for id in records {
        if origin
            .as_ref()
            .is_some_and(|origin| origin.record_id.as_str() == id)
        {
            continue;
        }
        let id =
            ObjectId::from_stored(id).ok_or_else(|| invalid("invalid restored note record id"))?;
        let record: RestoredRecord =
            SqliteStore::get_canonical_object_on(connection, &id, "work_restored_record")?
                .ok_or_else(|| invalid("restored note record missing"))?
                .decode()?;
        for _ in record.history.notes {
            omissions.omit("legacy note without original id");
        }
    }
    // The ordinary note allow-list cannot enumerate any host-private kind.
    let rows = connection
        .prepare(&format!(
            "SELECT notes.hash, notes.family, object.object_kind FROM ({}) notes
         JOIN objects object ON object.object_id = notes.hash ORDER BY notes.hash",
            super::super::notes::NOTE_OBJECTS,
        ))?
        .query_map([item.work_id.0.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (id, family, kind) in rows {
        let id = ObjectId::from_stored(id).ok_or_else(|| invalid("invalid native note id"))?;
        let note =
            super::super::notes::load_note(connection, item.work_id, &id, &family, &kind, false)?;
        let excluded = match kind.as_str() {
            "verification_evidence" => Some("host verification"),
            "environment_evidence" => Some("environment"),
            "work_evidence" | "work_restored_evidence" if note.gate.is_some() => Some("gate"),
            "work_evidence" | "work_restored_evidence" | "work_observation" => None,
            _ => return Err(invalid("unsupported native note kind")),
        }
        .or_else(|| restricted(&item.labels).then_some("restricted task"))
        .or_else(|| (!note.refs.is_empty()).then_some("references"))
        .or_else(|| prose_exclusion(&note.summary))
        .or_else(|| author_exclusion(&note.actor.actor_id))
        .or_else(|| author_exclusion(&note.actor.actor_kind));
        if let Some(reason) = excluded {
            omissions.omit(reason);
            continue;
        }
        let machine = machine.ok_or_else(|| {
            invalid("task import refuses divergent shared notes from local native notes")
        })?;
        notes.push(SharedNote {
            id,
            kind: if crate::domain::status_note_role(&note.actor).is_some() {
                SharedNoteKind::Status
            } else {
                SharedNoteKind::Generic
            },
            body: note.summary,
            author: SharedActor::from_actor(&note.actor),
            recorded_at: note.recorded_at,
            origin_machine: machine.into(),
        });
    }
    // These stored families belong to execution history, never note candidates.
    // These entries deliberately have no feed-row work_id; their run binds
    // them to the task. Read the indexed run feeds, counting each record once.
    let rows = connection
        .prepare(
            "SELECT entry.object_kind, count(DISTINCT entry.object_id)
         FROM work_runs run JOIN work_feed_entries entry
           ON entry.feed_kind = 'run_execution' AND entry.feed_id = run.run_id
         WHERE run.work_id = ?1 AND entry.object_kind IN ('acceptance_evaluation',
             'execution_observation', 'unadmitted_execution_observation', 'named_root_binding')
         GROUP BY entry.object_kind",
        )?
        .query_map([item.work_id.0.to_string()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (kind, count) in rows {
        let reason = match kind.as_str() {
            "acceptance_evaluation" => "evaluation",
            "named_root_binding" => "path binding",
            _ => "execution observation",
        };
        let count = usize::try_from(count)
            .map_err(|_| invalid("excluded-note count exceeds local receipt capacity"))?;
        *omissions.counts.entry(reason.into()).or_default() += count;
    }
    notes.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
    Ok((notes, included))
}

/// Audit-time comparison is store-wide; ordinary reads never enumerate origins.
pub(super) fn refuse_known_changes(
    connection: &Connection,
    tree: &TaskTree,
) -> Result<(), StoreError> {
    let mut candidate = std::collections::HashMap::new();
    for task in &tree.tasks {
        for note in &task.notes {
            let local: bool = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM objects WHERE object_id = ?1)",
                [note.id.as_str()],
                |row| row.get(0),
            )?;
            if local {
                return Err(invalid(
                    "shared note id collides with a local canonical record",
                ));
            }
            candidate.insert(&note.id, (task.id, note));
        }
    }
    if candidate.is_empty() {
        return Ok(());
    }
    let ids = connection
        .prepare("SELECT object_id FROM objects WHERE object_kind = ?1")?
        .query_map([ORIGIN_KIND], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for id in ids {
        let id = ObjectId::from_stored(id).ok_or_else(|| invalid("invalid import origin id"))?;
        let origin: ImportOrigin =
            SqliteStore::get_canonical_object_on(connection, &id, ORIGIN_KIND)?
                .ok_or_else(|| invalid("task import origin missing"))?
                .decode()?;
        for note in &origin.task.notes {
            if let Some((task, incoming)) = candidate.get(&note.id)
                && (origin.project_id != tree.project.project_id
                    || *task != origin.task.id
                    || *incoming != note)
            {
                return Err(invalid(
                    "known shared note id differs from its task or shared fields",
                ));
            }
        }
    }
    Ok(())
}
