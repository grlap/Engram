//! Shared members live in existing immutable origin/baseline documents. Reads
//! compose them with native facts; imports never create executable authority.

use super::{Connection, ObjectId, StoreError, TaskDocument, WorkItem, baseline, origin};
use crate::task_sync::{TaskSyncRefusal, activity::ActivityMap, notes::SharedNote};
use rusqlite::OptionalExtension;
use std::collections::BTreeMap;

#[cfg(test)]
thread_local! {
    pub(super) static NOTE_ENUMERATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(super) fn retained(
    connection: &Connection,
    item: &WorkItem,
) -> Result<Option<TaskDocument>, StoreError> {
    let initial = origin::for_item(connection, item)?.map(|origin| origin.task);
    let entry =
        baseline::entries(connection, &item.project_id, [item.work_id])?.remove(&item.work_id);
    let next = if entry.as_ref().is_some_and(|entry| entry.document.is_none()) {
        // Removing exchange membership retires shared planning, not historical
        // prose or the immutable detail locators already issued for this item.
        last_admitted_document(connection, item)?
    } else {
        entry.and_then(|entry| entry.document)
    };
    let retained = match (initial, next) {
        (Some(before), Some(mut after)) => {
            merge_notes(&mut after.notes, before.notes)?;
            merge_activity(&mut after.activity, before.activity)?;
            Some(after)
        }
        (initial, None) => initial,
        (None, next) => next,
    };
    Ok(retained)
}

fn last_admitted_document(
    connection: &Connection,
    item: &WorkItem,
) -> Result<Option<TaskDocument>, StoreError> {
    let selected: Option<(String, Vec<u8>)> = connection
        .query_row(
            "SELECT member.record_id, member.document_json
         FROM work_task_sync_document_members member
         JOIN work_feed_entries entry ON entry.object_id=member.record_id
         WHERE member.project_id=?1 AND member.work_id=?2 AND member.document_json IS NOT NULL
           AND entry.feed_kind='project' AND entry.feed_id=?1 AND entry.object_kind=?3
         ORDER BY entry.position DESC LIMIT 1",
            rusqlite::params![
                item.project_id.0,
                item.work_id.0.to_string(),
                super::folder::IMPORT_PLANNING_KIND
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((record, bytes)) = selected else {
        return Ok(None);
    };
    let record = ObjectId::from_stored(record)
        .ok_or_else(|| super::invalid("retired shared history record identity is invalid"))?;
    let planning = origin::planning_record(connection, &record, &item.project_id)?;
    let document = planning
        .record
        .changes
        .get(&item.work_id)
        .and_then(Option::as_ref)
        .ok_or_else(|| super::invalid("retired shared history has no task document"))?;
    if serde_json::from_slice::<TaskDocument>(&bytes)? != *document {
        return Err(super::invalid(
            "retired shared history projection differs from canonical document",
        ));
    }
    Ok(Some(document.clone()))
}

/// Explicit exchange capture may consult confirmed Git publication witnesses.
/// Ordinary note/detail/history/graph reads use `retained` and stay in SQLite.
pub(super) fn retained_for_capture(
    connection: &Connection,
    item: &WorkItem,
) -> Result<Option<TaskDocument>, StoreError> {
    let mut retained = retained(connection, item)?;
    let events = super::query::canonical_work_event_records_for_item(connection, item.work_id)?;
    let native_notes: bool = connection.query_row(
        &format!(
            "SELECT EXISTS(SELECT 1 FROM ({}) notes)",
            super::super::notes::NOTE_OBJECTS
        ),
        [item.work_id.0.to_string()],
        |row| row.get(0),
    )?;
    if !native_notes
        && !events.iter().any(|(_, event)| {
            matches!(
                event.transition,
                crate::WorkTransition::Claimed { .. } | crate::WorkTransition::HandedOff { .. }
            )
        })
    {
        return Ok(retained);
    }
    for mut published in origin::published_members(connection, item)? {
        let mut native_notes = Vec::new();
        for note in published.notes {
            let native: bool = connection.query_row(
                &format!(
                    "SELECT EXISTS(SELECT 1 FROM ({}) notes WHERE notes.hash=?2)",
                    super::super::notes::NOTE_OBJECTS
                ),
                rusqlite::params![item.work_id.0.to_string(), note.id.as_str()],
                |row| row.get(0),
            )?;
            if native {
                super::notes::validate_native_echo(connection, item, &note)?;
                native_notes.push(note);
            }
        }
        published.notes = native_notes;
        let mut activity = ActivityMap::new();
        for (alias, entries) in published.activity {
            let native = super::activity::native(item, &events, &alias)?;
            for (id, notice) in entries {
                if let Some(current) = native.get(&id) {
                    crate::task_sync::shared::advance(&notice, current).map_err(refusal)?;
                    activity
                        .entry(alias.clone())
                        .or_default()
                        .insert(id, notice);
                }
            }
        }
        published.activity = activity;
        if let Some(retained) = &mut retained {
            merge_notes(&mut retained.notes, published.notes)?;
            merge_activity(&mut retained.activity, published.activity)?;
        } else {
            retained = Some(published);
        }
    }
    Ok(retained)
}

pub(super) fn refusal(conflict: crate::task_sync::shared::Conflict) -> StoreError {
    let mut detail = conflict.detail;
    detail.insert_str(0, &format!("{}: ", conflict.reason));
    TaskSyncRefusal::DivergentPlanning.invalid(detail)
}

pub(super) fn merge_notes(
    target: &mut Vec<SharedNote>,
    incoming: impl IntoIterator<Item = SharedNote>,
) -> Result<(), StoreError> {
    let mut notes = target
        .drain(..)
        .map(|note| (note.id.clone(), note))
        .collect::<BTreeMap<_, _>>();
    for note in incoming {
        if let Some(old) = notes.get(&note.id) {
            if old != &note {
                return Err(TaskSyncRefusal::DivergentPlanning
                    .invalid("shared_identity: shared note content differs"));
            }
        } else {
            notes.insert(note.id.clone(), note);
        }
    }
    *target = notes.into_values().collect();
    Ok(())
}

pub(super) fn merge_activity(
    target: &mut ActivityMap,
    incoming: ActivityMap,
) -> Result<(), StoreError> {
    for (machine, entries) in incoming {
        for (id, notice) in entries {
            let existing = target.entry(machine.clone()).or_default();
            if let Some(old) = existing.get(&id) {
                if old.state_revision > notice.state_revision {
                    crate::task_sync::shared::advance(&notice, old).map_err(refusal)?;
                    continue;
                }
                crate::task_sync::shared::advance(old, &notice).map_err(refusal)?;
            }
            existing.insert(id, notice);
        }
    }
    Ok(())
}

/// Every retained identity is immutable in ownership even when its latest
/// activity assertion advances. Indexed member witnesses include updates on
/// native tasks, which never have an import origin.
pub(super) fn validate_identities(
    connection: &Connection,
    task: &TaskDocument,
    project: &crate::ProjectId,
) -> Result<(), StoreError> {
    for identity in super::closure::documents::document_identities(task) {
        let mut statement = connection.prepare(
            "SELECT member.project_id, member.work_id, member.document_json
             FROM work_task_sync_shared_identity_members identity
             JOIN work_task_sync_document_members member
               ON member.record_id=identity.record_id AND member.work_id=identity.work_id
             WHERE identity.family=?1 AND identity.machine=?2 AND identity.value=?3",
        )?;
        let witnesses = statement
            .query_map(
                rusqlite::params![identity.family, identity.machine, identity.value],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )?
            .collect::<Result<Vec<_>, _>>()?;
        for (owner_project, owner, bytes) in witnesses {
            if owner_project != project.0 || owner != task.id.0.to_string() {
                return Err(TaskSyncRefusal::IdCollision
                    .invalid(format!(
                        "shared {} identity {}/{} is retained for project {} task {}, requested project {} task {}; restore its original owner and re-export any distinct member from its source",
                        identity.family, identity.machine, identity.value,
                        owner_project, owner, project.0, task.id.0,
                    )));
            }
            let held: TaskDocument = serde_json::from_slice(&bytes)?;
            if identity.family == "note" {
                let old = held
                    .notes
                    .iter()
                    .find(|note| note.id.as_str() == identity.value);
                let next = task
                    .notes
                    .iter()
                    .find(|note| note.id.as_str() == identity.value);
                if old != next {
                    return Err(TaskSyncRefusal::DivergentPlanning
                        .invalid("shared_identity: known note content differs"));
                }
            } else {
                let id = ObjectId::from_stored(identity.value.clone())
                    .ok_or_else(|| super::invalid("invalid activity identity"))?;
                let old = held
                    .activity
                    .get(&identity.machine)
                    .and_then(|entries| entries.get(&id));
                let next = task
                    .activity
                    .get(&identity.machine)
                    .and_then(|entries| entries.get(&id));
                if let (Some(old), Some(next)) = (old, next) {
                    // Historical prepared/source documents are not current
                    // application authority. They still bind identity, and an
                    // equal revision must have one payload.
                    if old.state_revision <= next.state_revision {
                        crate::task_sync::shared::advance(old, next).map_err(refusal)?;
                    } else {
                        crate::task_sync::shared::advance(next, old).map_err(refusal)?;
                    }
                }
            }
        }
    }
    Ok(())
}

pub(super) fn validate_native_echoes(
    connection: &Connection,
    item: &WorkItem,
    incoming: &TaskDocument,
) -> Result<(), StoreError> {
    let events = super::query::canonical_work_event_records_for_item(connection, item.work_id)?;
    let known = retained_for_capture(connection, item)?;
    for (machine, entries) in &incoming.activity {
        let native = super::activity::native(item, &events, machine)?;
        for (id, notice) in entries {
            if let Some(actual) = native.get(id) {
                if known.as_ref().is_some_and(|known| {
                    known
                        .activity
                        .iter()
                        .any(|(alias, entries)| alias != machine && entries.contains_key(id))
                }) {
                    return Err(TaskSyncRefusal::DivergentPlanning.invalid(
                        "shared_identity: incoming native episode changes its origin-machine alias",
                    ));
                }
                if actual != notice {
                    let prefix = events
                        .iter()
                        .position(|(event_id, _)| event_id == &notice.event_id)
                        .map(|position| {
                            super::activity::native(item, &events[..=position], machine)
                        })
                        .transpose()?;
                    let historical = prefix.as_ref().and_then(|entries| entries.get(id));
                    if historical != Some(notice)
                        || crate::task_sync::shared::advance(notice, actual).is_err()
                    {
                        return Err(TaskSyncRefusal::DivergentPlanning.invalid(format!(
                            "activity_conflict: incoming episode {id} revision {} does not match a canonical prefix of local revision {}; export with the original alias {machine}, merge the local notice, and retry the resolved folder",
                            notice.state_revision, actual.state_revision,
                        )));
                    }
                }
            } else {
                if foreign_original(connection, item, id)? {
                    let kind: String = connection.query_row(
                        "SELECT object_kind FROM objects WHERE object_id=?1",
                        [id.as_str()],
                        |row| row.get(0),
                    )?;
                    if kind != "work_event" {
                        return Err(TaskSyncRefusal::IdCollision
                            .invalid("activity start names a different foreign object"));
                    }
                    let event: crate::WorkEvent = super::evidence::shared_original_on(
                        connection,
                        &item.project_id,
                        id,
                        "work_event",
                    )?
                    .decode()?;
                    if event.project_id != item.project_id || event.work_id != item.work_id {
                        return Err(TaskSyncRefusal::IdCollision
                            .invalid("activity start names a foreign event for another task"));
                    }
                    let original = super::activity::native(item, &[(id.clone(), event)], machine)?;
                    let start = original.get(id).ok_or_else(|| {
                        TaskSyncRefusal::IdCollision.invalid(
                            "activity id names a foreign event that did not start its episode",
                        )
                    })?;
                    crate::task_sync::shared::advance(start, notice).map_err(refusal)?;
                    continue;
                }
                let held: bool = connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM objects WHERE object_id=?1)",
                    [id.as_str()],
                    |row| row.get(0),
                )?;
                if held {
                    return Err(TaskSyncRefusal::IdCollision
                        .invalid("activity start names a different native object"));
                }
            }
        }
    }
    for note in &incoming.notes {
        super::notes::validate_native_echo(connection, item, note)?;
    }
    Ok(())
}

/// Foreign canonical originals corroborate asserted provenance, never local
/// runtime state. Their admission still binds this exact project.
pub(super) fn foreign_original(
    connection: &Connection,
    item: &WorkItem,
    id: &ObjectId,
) -> Result<bool, StoreError> {
    let project: Option<String> = connection.query_row(
        "SELECT pack.project_id FROM work_task_sync_evidence_imported original
         JOIN work_task_sync_evidence_packs pack ON pack.admission_id=original.admission_id WHERE original.object_id=?1",
        [id.as_str()], |row| row.get(0)).optional()?;
    if project
        .as_ref()
        .is_some_and(|project| project != &item.project_id.0)
    {
        return Err(TaskSyncRefusal::IdCollision
            .invalid("shared identity names a foreign original from another project"));
    }
    Ok(project.is_some())
}

pub(in crate::storage::work) fn validate_update(
    connection: &Connection,
    item: &WorkItem,
    incoming: &TaskDocument,
) -> Result<(), StoreError> {
    if let Some(before) = retained_for_capture(connection, item)? {
        crate::task_sync::shared::retains(&before, incoming).map_err(refusal)?;
    }
    validate_identities(connection, incoming, &item.project_id)?;
    validate_native_echoes(connection, item, incoming)
}

pub(in crate::storage) struct NoteMember {
    pub record: ObjectId,
    pub index: usize,
    pub position: i64,
    pub note: SharedNote,
}

/// Only late admitted notes need virtual rows. Initial origin members already
/// have inherited locators, and native echoes already have canonical rows.
/// Anchor each row at its first admitted planning record, never a prepared or
/// source-only copy. This reader does not participate in note-index derivation.
pub(in crate::storage) fn additional_notes(
    connection: &Connection,
    item: &WorkItem,
) -> Result<Vec<NoteMember>, StoreError> {
    super::query::on_one_snapshot(connection, |connection| {
        super::with_origin_scope(connection, || {
            #[cfg(test)]
            NOTE_ENUMERATIONS.with(|count| count.set(count.get() + 1));
            let Some(retained) = retained(connection, item)? else {
                return Ok(Vec::new());
            };
            let origin = origin::for_item(connection, item)?;
            let mut output = Vec::new();
            for note in retained.notes {
                if origin.as_ref().is_some_and(|origin| {
                    origin
                        .task
                        .notes
                        .iter()
                        .any(|initial| initial.id == note.id)
                }) {
                    continue;
                }
                let native: bool = connection.query_row(
                    &format!(
                        "SELECT EXISTS(SELECT 1 FROM ({}) notes WHERE notes.hash=?2)",
                        super::super::notes::NOTE_OBJECTS
                    ),
                    rusqlite::params![item.work_id.0.to_string(), note.id.as_str()],
                    |row| row.get(0),
                )?;
                if native {
                    continue;
                }
                let (record, position): (String, i64) = connection.query_row(
                "SELECT identity.record_id, entry.position FROM work_task_sync_shared_identity_members identity
                 JOIN work_feed_entries entry ON entry.object_id=identity.record_id
                 WHERE identity.family='note' AND identity.machine='' AND identity.value=?1 AND identity.work_id=?2
                   AND entry.feed_kind='project' AND entry.feed_id=?3 AND entry.object_kind=?4
                 ORDER BY entry.position LIMIT 1",
                rusqlite::params![note.id.as_str(), item.work_id.0.to_string(), item.project_id.0, super::folder::IMPORT_PLANNING_KIND],
                |row| Ok((row.get(0)?, row.get(1)?)))?;
                let record = ObjectId::from_stored(record)
                    .ok_or_else(|| super::invalid("shared note anchor identity is invalid"))?;
                let planning = origin::planning_record(connection, &record, &item.project_id)?;
                let document = planning
                    .record
                    .changes
                    .get(&item.work_id)
                    .and_then(Option::as_ref)
                    .ok_or_else(|| super::invalid("shared note anchor has no task document"))?;
                let index = *planning
                    .notes
                    .get(&(item.work_id, note.id.clone()))
                    .ok_or_else(|| super::invalid("shared note anchor member is missing"))?;
                if document.notes[index] != note {
                    return Err(super::invalid("shared note anchor content differs"));
                }
                output.push(NoteMember {
                    record,
                    index: index + 1,
                    position,
                    note,
                });
            }
            Ok(output)
        })
    })
}
