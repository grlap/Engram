//! Read-time navigation for children left below completed work.

use super::{
    Connection, OptionalExtension, SessionId, SqliteStore, StoreError, WorkEvent, WorkItem,
    WorkLifecycle, load_typed_work_object, load_work_item, params, parse_work_id,
};
use crate::{ObjectId, ProjectId, RestoredRecord};

#[cfg(test)]
mod tests;

pub(crate) struct StrandedChildren {
    pub items: Vec<(WorkItem, WorkItem)>,
    pub omitted: usize,
}

// Start from indexed Open children, never accumulated Completed history.
// Siblings are grouped so one canonical attribution probe serves each parent.
const CANDIDATES: &str = "
    SELECT child.work_id, parent.work_id
    FROM work_items child INDEXED BY work_items_ready
    CROSS JOIN work_items parent ON parent.work_id = child.parent_id
    WHERE child.project_id = ?1 AND child.lifecycle = 'open'
      AND parent.project_id = ?1 AND parent.lifecycle = 'completed'
    ORDER BY parent.work_id, child.work_id
";

const EVENT_ANCHOR: &str = "
    SELECT object.object_id, object.object_kind, NULL
    FROM work_feed_entries entry CROSS JOIN objects object USING(object_id)
    WHERE entry.work_id = ?1 AND entry.work_id IS NOT NULL
      AND entry.feed_kind = 'project' AND entry.feed_id = ?2
      AND entry.object_kind = 'work_event'
      AND json_extract(object.canonical_json, '$.actor.session_id') = ?3
    LIMIT 1
";

const HISTORY_ANCHOR: &str = "
    SELECT object.object_id, object.object_kind, record.generation_index
    FROM work_restored_records record
    CROSS JOIN objects object ON object.object_id = record.record_id
    WHERE record.work_id = ?1
      AND (json_extract(object.canonical_json, '$.history.completion.actor.session_id') = ?3
        OR EXISTS(SELECT 1 FROM json_each(object.canonical_json, '$.history.events') event
                  WHERE json_extract(event.value, '$.actor.session_id') = ?3)
        OR EXISTS(SELECT 1 FROM json_each(object.canonical_json, '$.history.notes') note
                  WHERE json_extract(note.value, '$.actor.session_id') = ?3))
    LIMIT 1
";

impl SqliteStore {
    /// Open candidates and item-bound probes share the caller's advisory
    /// snapshot. Invalid attribution is an error, never a reason to try a
    /// weaker anchor. Families stop at their first canonically verified match.
    pub(crate) fn stranded_work_children(
        &self,
        project: &ProjectId,
        session: &SessionId,
    ) -> Result<StrandedChildren, StoreError> {
        self.work_read_snapshot(|store| {
            let rows = store
                .connection
                .prepare(CANDIDATES)?
                .query_map([&project.0], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let mut previous_parent = None;
            let mut eligible_parent = None;
            let mut total = 0;
            let mut items = Vec::new();
            for (child_id, parent_id) in rows {
                if previous_parent.as_ref() != Some(&parent_id) {
                    let parent = load_work_item(&store.connection, parse_work_id(&parent_id)?)?;
                    if parent.project_id != *project || parent.lifecycle != WorkLifecycle::Completed
                    {
                        return Err(invalid("stranded parent differs from its canonical basis"));
                    }
                    eligible_parent =
                        participated(&store.connection, &parent, session)?.then_some(parent);
                    previous_parent = Some(parent_id);
                }
                let Some(parent) = &eligible_parent else {
                    continue;
                };
                total += 1;
                if items.len() == 5 {
                    continue;
                }
                let child = load_work_item(&store.connection, parse_work_id(&child_id)?)?;
                if child.project_id != *project
                    || child.parent_id != Some(parent.work_id)
                    || child.root_id != parent.root_id
                    || child.lifecycle != WorkLifecycle::Open
                {
                    return Err(invalid("stranded child differs from its canonical basis"));
                }
                items.push((child, parent.clone()));
            }
            Ok(StrandedChildren {
                omitted: total - items.len(),
                items,
            })
        })
    }
}

fn participated(
    connection: &Connection,
    parent: &WorkItem,
    session: &SessionId,
) -> Result<bool, StoreError> {
    // Fixed family order, no sort: the anchor is existential, not presentation.
    for (family, sql) in [
        ("event", EVENT_ANCHOR.to_owned()),
        ("run", note_anchor("work_run_evidence", "evidence_id")),
        (
            "restored",
            note_anchor("work_restored_evidence", "evidence_id"),
        ),
        (
            "observation",
            note_anchor("work_observations", "observation_id"),
        ),
        ("history", HISTORY_ANCHOR.to_owned()),
    ] {
        let row = connection
            .prepare(&sql)?
            .query_row(
                params![parent.work_id.0.to_string(), parent.project_id.0, session.0],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((id, kind, generation)) = row else {
            continue;
        };
        let anchor = ObjectId::from_stored(id.clone()).ok_or(StoreError::InvalidStoredKey(id))?;
        let matches = match family {
            "event" => {
                let event: WorkEvent = load_typed_work_object(connection, &anchor, "work_event")?;
                kind == "work_event"
                    && event.project_id == parent.project_id
                    && event.work_id == parent.work_id
                    && event.actor.session_id.as_ref() == Some(session)
            }
            "history" => {
                let record: RestoredRecord =
                    load_typed_work_object(connection, &anchor, "work_restored_record")?;
                kind == "work_restored_record"
                    && record.work_id == parent.work_id
                    && record.project_id == parent.project_id
                    && i64::try_from(record.generation_index).ok() == generation
                    && (record
                        .history
                        .events
                        .iter()
                        .any(|event| event.actor.session_id.as_ref() == Some(session))
                        || record
                            .history
                            .notes
                            .iter()
                            .any(|note| note.actor.session_id.as_ref() == Some(session))
                        || record
                            .history
                            .completion
                            .as_ref()
                            .is_some_and(|completion| {
                                completion.actor.session_id.as_ref() == Some(session)
                            }))
            }
            _ => {
                super::super::notes::load_note(
                    connection,
                    parent.work_id,
                    &anchor,
                    family,
                    &kind,
                    false,
                )?
                .actor
                .session_id
                .as_ref()
                    == Some(session)
            }
        };
        if !matches {
            return Err(invalid(
                "stranded parent participation differs from its canonical basis",
            ));
        }
        return Ok(true);
    }
    Ok(false)
}

fn note_anchor(table: &str, id: &str) -> String {
    format!(
        "SELECT object.object_id, object.object_kind, NULL
        FROM {table} note CROSS JOIN objects object ON object.object_id = note.{id}
        WHERE note.work_id = ?1 AND json_extract(object.canonical_json, '$.actor.session_id') = ?3
        LIMIT 1"
    )
}

fn invalid(message: &str) -> StoreError {
    StoreError::InvalidWorkProjection(message.into())
}
