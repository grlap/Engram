//! Read-time navigation for children left below completed work.

use super::{
    Connection, SessionId, SqliteStore, StoreError, WorkEvent, WorkItem, WorkLifecycle,
    load_typed_work_object, load_work_item, params, parse_work_id,
    restored_records_with_id_for_item,
};
use crate::{ObjectId, ProjectId};

pub(crate) struct StrandedChildren {
    pub items: Vec<(WorkItem, WorkItem)>,
    pub omitted: usize,
}

const CANDIDATES: &str = "
    WITH parents AS MATERIALIZED (
        SELECT parent.work_id FROM work_items parent
        WHERE parent.project_id = ?1 AND parent.lifecycle = 'completed'
          AND EXISTS(SELECT 1 FROM work_items child WHERE child.parent_id = parent.work_id
                     AND child.project_id = ?1 AND child.lifecycle = 'open')
    ), note_bindings AS MATERIALIZED (
        SELECT parent.work_id, evidence.evidence_id AS object_id, 'run' AS family
        FROM parents parent CROSS JOIN work_run_evidence evidence ON evidence.work_id = parent.work_id
        UNION ALL
        SELECT parent.work_id, evidence.evidence_id, 'restored'
        FROM parents parent CROSS JOIN work_restored_evidence evidence ON evidence.work_id = parent.work_id
        UNION ALL
        SELECT parent.work_id, observation.observation_id, 'observation'
        FROM parents parent CROSS JOIN work_observations observation ON observation.work_id = parent.work_id
    ), participation AS (
        SELECT parent.work_id, object.object_id, 'event' AS family, object.object_kind
        FROM parents parent CROSS JOIN work_feed_entries entry ON entry.work_id = parent.work_id
        CROSS JOIN objects object USING(object_id)
        WHERE entry.feed_kind = 'project' AND entry.feed_id = ?1
          AND entry.object_kind = 'work_event' AND object.object_kind = 'work_event'
          AND json_extract(object.canonical_json, '$.actor.session_id') = ?2
        UNION ALL
        SELECT parent.work_id, object.object_id, note.family, object.object_kind
        FROM parents parent CROSS JOIN note_bindings note ON note.work_id = parent.work_id
        CROSS JOIN objects object USING(object_id)
        WHERE json_extract(object.canonical_json, '$.actor.session_id') = ?2
        UNION ALL
        SELECT parent.work_id, object.object_id, 'history', object.object_kind
        FROM parents parent CROSS JOIN work_restored_records record ON record.work_id = parent.work_id
        CROSS JOIN objects object ON object.object_id = record.record_id
        WHERE object.object_kind = 'work_restored_record'
          AND json_extract(object.canonical_json, '$.project_id') = ?1
          AND (json_extract(object.canonical_json, '$.history.completion.actor.session_id') = ?2
            OR EXISTS(SELECT 1 FROM json_each(object.canonical_json, '$.history.events') event
                      WHERE json_extract(event.value, '$.actor.session_id') = ?2)
            OR EXISTS(SELECT 1 FROM json_each(object.canonical_json, '$.history.notes') note
                      WHERE json_extract(note.value, '$.actor.session_id') = ?2))
    ), attributed AS (
        SELECT *, ROW_NUMBER() OVER(PARTITION BY work_id ORDER BY family, object_id) AS ordinal
        FROM participation
    ) SELECT child.work_id, parent.work_id, parent.object_id, parent.family, parent.object_kind,
             COUNT(*) OVER()
      FROM attributed parent JOIN work_items child ON child.parent_id = parent.work_id
      WHERE parent.ordinal = 1 AND child.project_id = ?1 AND child.lifecycle = 'open'
      ORDER BY parent.work_id, child.work_id LIMIT 5
";

impl SqliteStore {
    /// Candidates, canonical attribution, and current planning state share the
    /// caller's advisory snapshot. Participation is by session, never actor.
    pub(crate) fn stranded_work_children(
        &self,
        project: &ProjectId,
        session: &SessionId,
    ) -> Result<StrandedChildren, StoreError> {
        self.work_read_snapshot(|store| {
            let rows = store
                .connection
                .prepare(CANDIDATES)?
                .query_map(params![project.0, session.0], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, String>(4)?,
                        row.get::<_, i64>(5)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let total = usize::try_from(rows.first().map_or(0, |row| row.5))
                .map_err(|_| invalid("invalid stranded child count"))?;
            let mut items = Vec::with_capacity(rows.len());
            for (child_id, parent_id, anchor, family, kind, _) in rows {
                let child = load_work_item(&store.connection, parse_work_id(&child_id)?)?;
                let parent = load_work_item(&store.connection, parse_work_id(&parent_id)?)?;
                let anchor = ObjectId::from_stored(anchor.clone())
                    .ok_or(StoreError::InvalidStoredKey(anchor))?;
                if child.project_id != *project
                    || parent.project_id != *project
                    || child.parent_id != Some(parent.work_id)
                    || child.root_id != parent.root_id
                    || child.lifecycle != WorkLifecycle::Open
                    || parent.lifecycle != WorkLifecycle::Completed
                    || !participated(&store.connection, &parent, &anchor, &family, &kind, session)?
                {
                    return Err(invalid(
                        "stranded child candidate differs from its canonical basis",
                    ));
                }
                items.push((child, parent));
            }
            Ok(StrandedChildren {
                omitted: total.saturating_sub(items.len()),
                items,
            })
        })
    }
}

fn participated(
    connection: &Connection,
    parent: &WorkItem,
    anchor: &ObjectId,
    family: &str,
    kind: &str,
    session: &SessionId,
) -> Result<bool, StoreError> {
    match family {
        "event" => {
            let event: WorkEvent = load_typed_work_object(connection, anchor, "work_event")?;
            Ok(event.project_id == parent.project_id
                && event.work_id == parent.work_id
                && event.actor.session_id.as_ref() == Some(session))
        }
        "history" => {
            let records = restored_records_with_id_for_item(connection, parent.work_id)?;
            let record = records
                .iter()
                .find(|(id, _)| id == anchor)
                .map(|(_, record)| record)
                .ok_or_else(|| invalid("missing stranded parent history anchor"))?;
            Ok(record.project_id == parent.project_id
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
                        })))
        }
        _ => Ok(super::super::notes::load_note(
            connection,
            parent.work_id,
            anchor,
            family,
            kind,
            false,
        )?
        .actor
        .session_id
        .as_ref()
            == Some(session)),
    }
}

fn invalid(message: &str) -> StoreError {
    StoreError::InvalidWorkProjection(message.into())
}
