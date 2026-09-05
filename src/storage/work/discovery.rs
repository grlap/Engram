//! Read-only session discovery from existing assignment and canonical participation.

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params};

use super::feeds::load_typed_work_object;
use super::notes::load_note;
use super::query::{load_work_claim_optional, load_work_item, parse_work_id};
use crate::ObjectHash;
use crate::domain::{ProjectId, SessionId, WorkClaim, WorkEvent, WorkId, WorkItem, WorkTransition};
use crate::storage::{SqliteStore, StoreError};

const DISCOVERY_LIMIT: i64 = 5;

#[cfg(test)]
mod tests;

pub(crate) struct WorkDiscoveryRow {
    pub work: WorkItem,
    pub claim: Option<WorkClaim>,
    pub note: Option<String>,
}

#[derive(Default)]
pub(crate) struct WorkDiscoveryPage {
    pub items: Vec<WorkDiscoveryRow>,
    pub omitted: usize,
    #[cfg(test)]
    vm_steps: i32,
}

// Candidate-first relational probes keep unrelated closed history out of this
// hot read. Only candidate note/event payloads are read; selected content then
// passes the ordinary typed canonical readers before leaving storage.
// CROSS JOIN intentionally keeps those small bindings on the outer side;
// otherwise SQLite can scan an entire project feed before matching hashes.
const DISCOVERY_SQL: &str = "
    WITH candidates AS MATERIALIZED (
        SELECT item.work_id, item.active_run_id
        FROM work_items item
        WHERE item.project_id = ?1 AND item.lifecycle = 'open' {candidate_filter}
    ), note_bindings AS MATERIALIZED (
        SELECT c.work_id, evidence.evidence_hash AS hash, 'run' AS family
        FROM candidates c CROSS JOIN work_run_evidence evidence ON evidence.work_id = c.work_id
        UNION ALL
        SELECT c.work_id, evidence.evidence_hash, 'restored'
        FROM candidates c CROSS JOIN work_restored_evidence evidence ON evidence.work_id = c.work_id
        UNION ALL
        SELECT c.work_id, observation.observation_hash, 'observation'
        FROM candidates c CROSS JOIN work_observations observation ON observation.work_id = c.work_id
    ), sources AS MATERIALIZED (
        SELECT note.work_id, entry.position, note.hash AS object_hash,
               entry.object_kind, note.family
        FROM note_bindings note
        CROSS JOIN work_feed_entries entry ON entry.feed_kind = 'project' AND entry.feed_id = ?1
            AND entry.object_hash = note.hash
        CROSS JOIN objects object ON object.object_hash = note.hash
        WHERE entry.object_kind IN ('work_evidence', 'work_observation', 'work_restored_evidence')
          AND json_extract(object.canonical_json, '$.actor.session_id') = ?2
        UNION ALL
        SELECT c.work_id, entry.position, entry.object_hash, entry.object_kind, 'handoff'
        FROM candidates c CROSS JOIN work_feed_entries entry
            ON entry.feed_kind = 'project' AND entry.feed_id = ?1
              AND entry.work_id = c.work_id AND entry.work_id IS NOT NULL
              AND entry.object_kind = 'work_event'
        CROSS JOIN objects object ON object.object_hash = entry.object_hash
        WHERE json_extract(object.canonical_json, '$.transition.kind') = 'handoff_offered'
          AND json_extract(object.canonical_json, '$.handoff_offer.to') = ?2
    ), positions AS (
        SELECT c.work_id, (
            SELECT entry.position FROM work_feed_entries entry
            WHERE entry.feed_kind = 'project' AND entry.feed_id = ?1
              AND entry.work_id = c.work_id AND entry.work_id IS NOT NULL
              AND entry.object_kind = 'work_event'
            ORDER BY entry.position DESC LIMIT 1
        ) AS position FROM candidates c
        UNION ALL
        SELECT note.work_id, entry.position FROM note_bindings note
        CROSS JOIN work_feed_entries entry ON entry.feed_kind = 'project' AND entry.feed_id = ?1
            AND entry.object_hash = note.hash
        UNION ALL
        SELECT c.work_id, project_entry.position
        FROM candidates c CROSS JOIN work_runs run ON run.work_id = c.work_id
        CROSS JOIN work_feed_heads head ON head.feed_kind = 'run_execution' AND head.feed_id = run.run_id
        CROSS JOIN work_feed_entries tail ON tail.feed_kind = head.feed_kind
            AND tail.feed_id = head.feed_id AND tail.position = head.position
        CROSS JOIN work_feed_entries project_entry ON project_entry.feed_kind = 'project'
            AND project_entry.feed_id = ?1 AND project_entry.object_hash = tail.object_hash
    ), latest AS (
        SELECT work_id, MAX(position) AS latest_position FROM positions GROUP BY work_id
    ), selected AS MATERIALIZED (
        SELECT c.work_id, COUNT(*) OVER() AS total, latest.latest_position
        FROM candidates c JOIN latest ON latest.work_id = c.work_id
        {participation_filter}
        ORDER BY latest.latest_position DESC, c.work_id LIMIT ?4
    )
    SELECT work_id, total,
           (SELECT object_hash FROM sources WHERE sources.work_id = selected.work_id
            ORDER BY (family = 'handoff'), position DESC LIMIT 1),
           (SELECT family FROM sources WHERE sources.work_id = selected.work_id
            ORDER BY (family = 'handoff'), position DESC LIMIT 1),
           (SELECT object_kind FROM sources WHERE sources.work_id = selected.work_id
            ORDER BY (family = 'handoff'), position DESC LIMIT 1)
    FROM selected ORDER BY latest_position DESC, work_id
";

fn discovery_sql(assigned: bool) -> String {
    // Separate statements expose assignment equality to work_items_assigned.
    let (candidate_filter, participation_filter) = if assigned {
        ("AND item.assigned_to_key = ?3", "")
    } else {
        (
            "AND NOT EXISTS(SELECT 1 FROM work_claims claim
              WHERE claim.run_id = item.active_run_id AND claim.state = 'active'
                AND claim.expires_at_ms > ?3 AND claim.holder_session_id = ?2)",
            "WHERE EXISTS(SELECT 1 FROM sources WHERE sources.work_id = c.work_id)",
        )
    };
    DISCOVERY_SQL
        .replace("{candidate_filter}", candidate_filter)
        .replace("{participation_filter}", participation_filter)
}

impl SqliteStore {
    /// Runs advisory reads on one cut, reusing an enclosing read snapshot.
    pub(crate) fn work_read_snapshot<T>(
        &self,
        read: impl FnOnce(&Self) -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        if !self.connection.is_autocommit() {
            return read(self);
        }
        let transaction = self.connection.unchecked_transaction()?;
        let result = read(self)?;
        transaction.commit()?;
        Ok(result)
    }

    pub(crate) fn work_discovery(
        &self,
        project: &ProjectId,
        session: &SessionId,
        actor: &str,
        assigned: bool,
        now: DateTime<Utc>,
    ) -> Result<WorkDiscoveryPage, StoreError> {
        self.work_read_snapshot(|store| {
            let mut statement = store.connection.prepare(&discovery_sql(assigned))?;
            let mode_parameter = if assigned {
                rusqlite::types::Value::Text(super::planning::normalize_work_catalog_key(actor))
            } else {
                rusqlite::types::Value::Integer(now.timestamp_millis())
            };
            let mut rows = statement.query(params![
                project.0,
                session.0,
                mode_parameter,
                DISCOVERY_LIMIT
            ])?;
            let mut page = WorkDiscoveryPage::default();
            let mut total = 0;
            while let Some(row) = rows.next()? {
                let work_id = parse_work_id(&row.get::<_, String>(0)?)?;
                total = usize::try_from(row.get::<_, i64>(1)?)
                    .map_err(|_| invalid("discovery count overflow"))?;
                let work = load_work_item(&store.connection, work_id)?;
                if work.project_id != *project {
                    return Err(invalid("discovery cannot cross projects"));
                }
                let claim = work
                    .active_run_id
                    .map(|run| load_work_claim_optional(&store.connection, run))
                    .transpose()?
                    .flatten();
                let hash: Option<String> = row.get(2)?;
                let note = hash
                    .map(|hash| {
                        let family: String = row.get(3)?;
                        let kind: String = row.get(4)?;
                        own_summary(
                            &store.connection,
                            project,
                            session,
                            work_id,
                            &hash,
                            &family,
                            &kind,
                        )
                    })
                    .transpose()?
                    .flatten();
                page.items.push(WorkDiscoveryRow { work, claim, note });
            }
            page.omitted = total.saturating_sub(page.items.len());
            #[cfg(test)]
            {
                drop(rows);
                page.vm_steps = statement.get_status(rusqlite::StatementStatus::VmStep);
            }
            Ok(page)
        })
    }

    /// The marker alone is asserted provenance, not proof of a detach. Require
    /// the reciprocal source relation and its canonical disposal event as well.
    pub(crate) fn detached_work_origin(
        &self,
        item: &WorkItem,
    ) -> Result<Option<(String, String)>, StoreError> {
        self.work_read_snapshot(|store| {
            for link in item.created_by.provenance_chain.iter().rev() {
                if link.source != "work_detach"
                    || link.relation != crate::domain::ProvenanceRelation::DerivedFrom
                {
                    continue;
                }
                let Some(reference) = &link.reference else {
                    continue;
                };
                let source_id = parse_work_id(reference)?;
                let exists: bool = store.connection.query_row(
                    "SELECT EXISTS(SELECT 1 FROM work_items WHERE work_id = ?1)",
                    [source_id.0.to_string()],
                    |row| row.get(0),
                )?;
                // Portable provenance can name a source outside this native store.
                if !exists {
                    continue;
                }
                let source = load_work_item(&store.connection, source_id)?;
                if source.project_id != item.project_id
                    || source.superseded_by != Some(item.work_id)
                {
                    continue;
                }
                let raw: Option<String> = store
                    .connection
                    .query_row(
                        "SELECT entry.object_hash FROM work_feed_entries entry
                     JOIN objects object ON object.object_hash = entry.object_hash
                     WHERE entry.feed_kind = 'project' AND entry.feed_id = ?1
                       AND entry.work_id = ?2 AND entry.object_kind = 'work_event'
                       AND json_extract(object.canonical_json, '$.transition.kind') = 'disposed'
                       AND json_extract(object.canonical_json, '$.transition.replacement_id') = ?3
                     ORDER BY entry.position DESC LIMIT 1",
                        params![
                            item.project_id.0,
                            source_id.0.to_string(),
                            item.work_id.0.to_string()
                        ],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(raw) = raw {
                    let event: WorkEvent = load_typed_work_object(
                        &store.connection,
                        &stored_hash(raw)?,
                        "work_event",
                    )?;
                    if event.project_id != item.project_id || event.work_id != source_id {
                        return Err(invalid("detach origin event crosses its source binding"));
                    }
                    if let WorkTransition::Disposed {
                        lifecycle: crate::domain::WorkLifecycle::Superseded,
                        replacement_id: Some(replacement),
                        reason,
                    } = event.transition
                        && replacement == item.work_id
                    {
                        return Ok(Some((source.short_ref, reason)));
                    }
                    return Err(invalid("detach origin event differs from its successor"));
                }
            }
            Ok(None)
        })
    }
}

fn own_summary(
    connection: &Connection,
    project: &ProjectId,
    session: &SessionId,
    work_id: WorkId,
    raw: &str,
    family: &str,
    kind: &str,
) -> Result<Option<String>, StoreError> {
    let hash = stored_hash(raw.into())?;
    if family == "handoff" {
        let event: WorkEvent = load_typed_work_object(connection, &hash, kind)?;
        if event.project_id != *project
            || event.work_id != work_id
            || !matches!(event.transition, WorkTransition::HandoffOffered { .. })
            || event
                .handoff_offer
                .as_ref()
                .is_none_or(|offer| offer.to != *session || offer.work_id != work_id)
        {
            return Err(invalid(
                "participation handoff differs from its session or work binding",
            ));
        }
        return Ok(None);
    }
    let note = load_note(connection, work_id, &hash, family, kind)?;
    if note.actor.session_id.as_ref() != Some(session) {
        return Err(invalid("participation note differs from its session"));
    }
    let summary = note.gate.map_or(note.summary, |gate| {
        format!(
            "gate {}: {}",
            gate.name,
            if gate.passed { "passed" } else { "failed" }
        )
    });
    Ok(Some(summary.lines().next().unwrap_or_default().to_owned()))
}

fn stored_hash(raw: String) -> Result<ObjectHash, StoreError> {
    ObjectHash::from_stored(raw.clone()).ok_or(StoreError::InvalidStoredHash(raw))
}

fn invalid(message: &str) -> StoreError {
    StoreError::InvalidWorkProjection(message.into())
}
