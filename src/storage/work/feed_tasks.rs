//! The tasks each project-feed position may have changed.
//!
//! An incremental task-sync export reads the changed tasks of a feed interval
//! here instead of capturing every task. The rows are derived: every
//! project-feed append records its own rows in the same transaction, repair
//! rebuilds them from the feed and the canonical objects it orders, and the
//! doctor compares the two. A task listed for an interval may still export
//! the same bytes; a task not listed cannot have changed its shared document
//! in that interval.

use std::collections::BTreeSet;

use rusqlite::{Connection, params};

use super::super::StoreError;
use super::task_sync::baseline::TaskSyncImportPlanning;
use super::task_sync::folder::IMPORT_PLANNING_KIND;
use crate::{CanonicalObject, ObjectId, ProjectId, WorkId};

pub(super) fn create_schema(connection: &Connection) -> Result<(), StoreError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS work_project_feed_tasks (
             project_id TEXT NOT NULL,
             position INTEGER NOT NULL CHECK(position > 0),
             work_id TEXT NOT NULL,
             PRIMARY KEY(project_id, position, work_id)
         ) STRICT;
         CREATE INDEX IF NOT EXISTS work_project_feed_tasks_task
             ON work_project_feed_tasks(project_id, work_id);
         CREATE TABLE IF NOT EXISTS work_project_feed_coverage (
             project_id TEXT NOT NULL,
             position INTEGER NOT NULL CHECK(position > 0),
             PRIMARY KEY(project_id, position)
         ) STRICT, WITHOUT ROWID;
         CREATE INDEX IF NOT EXISTS work_feed_entries_planning
             ON work_feed_entries(feed_id, position)
             WHERE feed_kind = 'project' AND object_kind = 'task_sync_import_planning';",
    )?;
    Ok(())
}

/// Records the tasks one project-feed append may change, in the caller's
/// transaction.
pub(super) fn record_on(
    connection: &Connection,
    project: &str,
    position: i64,
    object_kind: &str,
    object: &CanonicalObject,
    work_id: Option<&str>,
) -> Result<(), StoreError> {
    let affected = affected(object_kind, Some(object), work_id)?;
    insert_rows(connection, project, position, &affected)?;
    // Every classified position is covered, also one that changes no task,
    // so a missing row reads as a gap and never as "nothing changed".
    connection.execute(
        "INSERT INTO work_project_feed_coverage (project_id, position) VALUES (?1, ?2)",
        params![project, position],
    )?;
    Ok(())
}

/// Rebuilds every row from the project feeds and their canonical objects,
/// naming each object it cannot classify so that the caller refuses the
/// whole repair with every such object listed.
pub(super) fn rebuild(connection: &Connection) -> Result<Vec<String>, StoreError> {
    connection.execute("DELETE FROM work_project_feed_tasks", [])?;
    connection.execute("DELETE FROM work_project_feed_coverage", [])?;
    let (rows, covered, findings) = derive(connection)?;
    for (project, position, work_id) in rows {
        connection.execute(
            "INSERT INTO work_project_feed_tasks (project_id, position, work_id)
             VALUES (?1, ?2, ?3)",
            params![project, position, work_id],
        )?;
    }
    for (project, position) in covered {
        connection.execute(
            "INSERT INTO work_project_feed_coverage (project_id, position) VALUES (?1, ?2)",
            params![project, position],
        )?;
    }
    Ok(findings)
}

/// Compares the stored rows with the rows the feeds derive.
pub(super) fn verify(
    connection: &Connection,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let (expected, expected_covered, findings) = derive(connection)?;
    invalid.extend(findings);
    let actual = connection
        .prepare("SELECT project_id, position, work_id FROM work_project_feed_tasks")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
        .collect::<Result<BTreeSet<(String, i64, String)>, _>>()?;
    // One check per row either side holds, as the other projections count.
    *checked += expected.union(&actual).count();
    for (project, position, work_id) in expected.symmetric_difference(&actual) {
        invalid.push(format!(
            "work_project_feed_tasks:{project}:{position}:{work_id}"
        ));
    }
    let covered = connection
        .prepare("SELECT project_id, position FROM work_project_feed_coverage")?
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
        .collect::<Result<BTreeSet<(String, i64)>, _>>()?;
    *checked += expected_covered.union(&covered).count();
    for (project, position) in expected_covered.symmetric_difference(&covered) {
        invalid.push(format!("work_project_feed_coverage:{project}:{position}"));
    }
    Ok(())
}

/// The distinct tasks the project-feed interval `(after, through]` may have
/// changed, read from the index alone. Every position of the interval must be
/// covered, so an index that lost or never recorded a position refuses by name
/// instead of reporting fewer changes.
pub(super) fn changed_between(
    connection: &Connection,
    project: &ProjectId,
    after: i64,
    through: i64,
) -> Result<BTreeSet<WorkId>, StoreError> {
    let covered: i64 = connection.query_row(
        "SELECT count(*) FROM work_project_feed_coverage
         WHERE project_id = ?1 AND position > ?2 AND position <= ?3",
        params![project.0, after, through],
        |row| row.get(0),
    )?;
    if covered != through.saturating_sub(after).max(0) {
        return Err(
            crate::task_sync::TaskSyncRefusal::FeedProjectionGap.invalid(format!(
                "the changed-task index covers {covered} of the {} project-feed positions after {after}; repair the projections",
                through.saturating_sub(after).max(0)
            )),
        );
    }
    connection
        .prepare(
            "SELECT DISTINCT work_id FROM work_project_feed_tasks
             WHERE project_id = ?1 AND position > ?2 AND position <= ?3",
        )?
        .query_map(params![project.0, after, through], |row| {
            row.get::<_, String>(0)
        })?
        .map(|row| {
            let stored = row?;
            uuid::Uuid::parse_str(&stored)
                .map(WorkId)
                .map_err(|_| invalid(format!("feed task row names an invalid work id {stored}")))
        })
        .collect()
}

/// Whether `project` holds a task that no project-feed entry names, as a
/// loaded graph's tasks are. Only a first capture asks: from feed position
/// zero it must name every task, and such a task has no row to be named by.
pub(super) fn has_unindexed_task(
    connection: &Connection,
    project: &ProjectId,
) -> Result<bool, StoreError> {
    Ok(connection.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM work_items item
             WHERE item.project_id = ?1
               AND NOT EXISTS (
                   SELECT 1 FROM work_project_feed_tasks row
                   WHERE row.project_id = item.project_id AND row.work_id = item.work_id
               )
         )",
        [&project.0],
        |row| row.get::<_, bool>(0),
    )?)
}

fn insert_rows(
    connection: &Connection,
    project: &str,
    position: i64,
    affected: &BTreeSet<String>,
) -> Result<(), StoreError> {
    for work_id in affected {
        connection.execute(
            "INSERT INTO work_project_feed_tasks (project_id, position, work_id)
             VALUES (?1, ?2, ?3)",
            params![project, position, work_id],
        )?;
    }
    Ok(())
}

/// Every row the project feeds derive, and a finding for each feed object
/// that cannot be classified. Only a database failure is an error. Rows are
/// streamed, and a record's bytes are read only for the kinds whose
/// classification needs them: a work event names its task in the feed entry.
fn derive(connection: &Connection) -> Result<(Rows, Covered, Vec<String>), StoreError> {
    let mut statement = connection.prepare(
        "SELECT entry.feed_id, entry.position, entry.object_kind, entry.object_id,
                entry.work_id,
                CASE WHEN entry.object_kind IN (
                         'work_evidence', 'work_restored_evidence', 'work_observation',
                         'task_sync_import_planning'
                     ) THEN object.canonical_json END
         FROM work_feed_entries entry
         JOIN objects object ON object.object_id = entry.object_id
         WHERE entry.feed_kind = 'project'
         ORDER BY entry.feed_id, entry.position",
    )?;
    let mut entries = statement.query([])?;
    let mut rows = BTreeSet::new();
    let mut covered = BTreeSet::new();
    let mut findings = Vec::new();
    while let Some(entry) = entries.next()? {
        let project = entry.get::<_, String>(0)?;
        let position = entry.get::<_, i64>(1)?;
        let kind = entry.get::<_, String>(2)?;
        let object_id = entry.get::<_, String>(3)?;
        let work_id = entry.get::<_, Option<String>>(4)?;
        let bytes = entry.get::<_, Option<Vec<u8>>>(5)?;
        let classified = ObjectId::from_stored(object_id.clone())
            .ok_or_else(|| invalid("project feed names an invalid object id"))
            .and_then(|id| {
                bytes
                    .map(|bytes| CanonicalObject::stored(&id, bytes))
                    .transpose()
            })
            .and_then(|object| affected(&kind, object.as_ref(), work_id.as_deref()));
        match classified {
            Ok(affected) => {
                covered.insert((project.clone(), position));
                rows.extend(
                    affected
                        .into_iter()
                        .map(|work_id| (project.clone(), position, work_id)),
                );
            }
            Err(StoreError::Sqlite(error)) => return Err(error.into()),
            Err(error) => findings.push(crate::storage::decode_failure_label(
                format!("work_project_feed_tasks:{object_id}"),
                &error,
            )),
        }
    }
    Ok((rows, covered, findings))
}

type Rows = BTreeSet<(String, i64, String)>;
type Covered = BTreeSet<(String, i64)>;

/// The one field a subject-bound record contributes here. Reading only the
/// subject keeps a feed append from decoding a whole record it does not use;
/// the doctor still validates every record in full.
#[derive(serde::Deserialize)]
struct Subject {
    work_id: WorkId,
}

fn subject(object: &CanonicalObject) -> Result<WorkId, StoreError> {
    Ok(serde_json::from_slice::<Subject>(object.bytes())?.work_id)
}

/// The tasks one ordinary project-feed object may change. Execution,
/// verification, obligation, evaluation, source and memory records are no
/// part of a shared task document, and a checkpoint reaches one only through
/// the completion event that selects it. A kind with no classification here
/// is refused, so a new feed family cannot be silently left out of change
/// discovery.
fn affected(
    object_kind: &str,
    object: Option<&CanonicalObject>,
    work_id: Option<&str>,
) -> Result<BTreeSet<String>, StoreError> {
    let one = |id: WorkId| Ok(BTreeSet::from([id.0.to_string()]));
    let content = || {
        object.ok_or_else(|| {
            invalid(format!(
                "a {object_kind} feed entry was classified without its record"
            ))
        })
    };
    match object_kind {
        "work_event" => work_id
            .map(|id| BTreeSet::from([id.to_owned()]))
            .ok_or_else(|| invalid("a work-event feed entry has no work id")),
        "work_evidence" | "work_restored_evidence" | "work_observation" => {
            one(subject(content()?)?)
        }
        // A baseline lists exactly the tasks that joined, changed or left.
        IMPORT_PLANNING_KIND => Ok(content()?
            .decode::<TaskSyncImportPlanning>()?
            .changes
            .into_keys()
            .map(|id| id.0.to_string())
            .collect()),
        "work_checkpoint"
        | "acceptance_evaluation"
        | "work_source_proposal"
        | "execution_observation"
        | "named_root_binding"
        | super::UNADMITTED_OBSERVATION_KIND
        | "verification_evidence"
        | crate::task_sync::evidence::consumption::CONSUMPTION_KIND
        | "environment_evidence"
        | "registered_host_run"
        | crate::domain::REMOTE_TERMINAL_ORIGIN_KIND
        | crate::domain::REMOTE_VERIFICATION_ASSOCIATION_KIND
        | "work_obligation"
        | "work_obligation_resolution"
        | "memory_version"
        | "memory_assertion_event"
        // Private identity and reconciliation facts do not change exported task text.
        // Ordinary planning/work records name any actual task mutations.
        | crate::task_sync::origin::PRODUCING_ORIGIN_KIND
        | crate::task_sync::origin::recovery::RECOVERY_KIND
        | "task_sync_reconciliation_hold"
        | "task_sync_shared_import_version" => Ok(BTreeSet::new()),
        other => Err(invalid(format!(
            "project feed object kind {other} has no task-sync change classification"
        ))),
    }
}

fn invalid(reason: impl Into<String>) -> StoreError {
    StoreError::InvalidWorkProjection(reason.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn task_sync_private_metadata_facts_do_not_mint_exported_task_changes() {
        for kind in [
            crate::task_sync::origin::PRODUCING_ORIGIN_KIND,
            crate::task_sync::origin::recovery::RECOVERY_KIND,
            "task_sync_reconciliation_hold",
            "task_sync_shared_import_version",
        ] {
            assert!(affected(kind, None, None).unwrap().is_empty(), "{kind}");
        }
        assert!(affected("unclassified_feed_family", None, None).is_err());
    }
}
