//! Atomic admission of an explicit, inert task planning candidate.

use chrono::{DateTime, Utc};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use super::query;
use crate::storage::{SqliteStore, StoreError, graph_snapshot};
use crate::{
    ActorContext, CanonicalObject, ObjectId, ProjectId, RestoredRecord, RestoredRelationBasis,
    WorkBlocker, WorkGraphSnapshotCompletion, WorkGraphSnapshotEvent, WorkGraphSnapshotHistory,
    WorkId, WorkItem,
    task_sync::{
        FORMAT, ProjectDocument, SharedActor, SharedBlocker, SharedCompletion, TaskDocument,
        TaskSyncRefusal, TaskTree, invalid,
    },
};

mod activity;
pub(crate) mod folder;
mod folder_dry_run;
mod incremental;
mod notes;
mod origin;
pub(crate) use origin::OriginScopeGuard;
pub(in crate::storage) use origin::enter_origin_scope;
pub(super) use origin::validate_item_origin;
pub(in crate::storage) use origin::verify_origins;
pub(in crate::storage) use origin::with_origin_scope;

#[cfg(test)]
mod tests;

const ORIGIN_KIND: &str = "task_sync_import_origin";
const IMPORT_TOOL: &str = "task-sync:import";

fn linked_actor(actor: &ActorContext, origin_id: &ObjectId) -> ActorContext {
    let mut actor = actor.clone();
    actor.provenance_chain.push(crate::domain::ProvenanceLink {
        relation: crate::domain::ProvenanceRelation::DerivedFrom,
        source: ORIGIN_KIND.into(),
        reference: Some(origin_id.as_str().into()),
    });
    actor
}

fn origin_reference(actor: &ActorContext) -> Result<ObjectId, StoreError> {
    let mut links = actor
        .provenance_chain
        .iter()
        .filter(|link| link.source == ORIGIN_KIND);
    let link = links
        .next()
        .ok_or_else(|| invalid(origin::REFERENCE_MISSING))?;
    if links.next().is_some() || link.relation != crate::domain::ProvenanceRelation::DerivedFrom {
        return Err(invalid(origin::REFERENCE_INVALID));
    }
    link.reference
        .as_ref()
        .and_then(|id| ObjectId::from_stored(id.clone()))
        .ok_or_else(|| invalid(origin::REFERENCE_INVALID))
}

/// Import attribution and complete policy supplement a plain inert restored record.
/// Existing graph-load origins and records retain their original interpretation.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct ImportOrigin {
    project_id: ProjectId,
    record_id: ObjectId,
    task: TaskDocument,
    actor: ActorContext,
    imported_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    notes_included: bool,
}

#[derive(Clone, Debug, Serialize)]
pub struct TaskSyncNoteExport {
    pub tree: TaskTree,
    pub omissions: crate::task_sync::notes::NoteOmissions,
    pub activity_omissions: crate::task_sync::notes::NoteOmissions,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TaskSyncImportResult {
    pub task_count: usize,
    pub changed: bool,
    pub dry_run: bool,
}

impl SqliteStore {
    /// Capture shared planning state in one read snapshot, excluding live authority.
    ///
    /// This plain candidate is not the exchange route: `task-sync export`
    /// publishes the folder through [`Self::export_task_sync_folder`]. It
    /// remains library API for tests and inspection.
    ///
    /// # Errors
    /// Refuses invalid canonical/projection state or an incomplete project graph.
    pub fn export_task_tree(&self, project: &ProjectId) -> Result<TaskTree, StoreError> {
        self.work_read_snapshot(|store| export_on(&store.connection, project))
    }

    /// Opt-in shared prose with asserted origin-machine and visible exclusions.
    ///
    /// Not the exchange route; see [`Self::export_task_tree`].
    ///
    /// # Errors
    /// Refuses invalid aliases, note bindings or shared candidates.
    pub fn export_task_tree_with_notes(
        &self,
        project: &ProjectId,
        machine: &str,
    ) -> Result<TaskSyncNoteExport, StoreError> {
        self.export_task_tree_with_options(
            project,
            &crate::task_sync::TaskSyncExportOptions {
                include_notes: true,
                origin_machine: Some(machine.into()),
            },
        )
    }

    /// Shared activity with an explicit native alias, independently of prose notes.
    ///
    /// Not the exchange route; see [`Self::export_task_tree`].
    ///
    /// # Errors
    /// Refuses invalid aliases, retained-entry conflicts or invalid shared state.
    pub fn export_task_tree_with_options(
        &self,
        project: &ProjectId,
        options: &crate::task_sync::TaskSyncExportOptions,
    ) -> Result<TaskSyncNoteExport, StoreError> {
        options.validate()?;
        self.work_read_snapshot(|store| {
            with_origin_scope(&store.connection, || {
                let mut omissions = crate::task_sync::notes::NoteOmissions::default();
                let mut activity_omissions = crate::task_sync::notes::NoteOmissions::default();
                let tree = export_on_snapshot(
                    &store.connection,
                    project,
                    options.include_notes,
                    options.origin_machine.as_deref(),
                    false,
                    &mut omissions,
                    &mut activity_omissions,
                )?;
                Ok(TaskSyncNoteExport {
                    tree,
                    omissions,
                    activity_omissions,
                })
            })
        })
    }

    /// Admit an empty graph once; exact repeats have no durable side effects.
    ///
    /// This plain candidate import is not the exchange route: `task-sync
    /// import` applies a resolved folder through
    /// [`Self::apply_task_sync_folder`] against a selected destination basis.
    /// It remains library API for tests.
    ///
    /// # Errors
    /// Refuses project mismatch, malformed graphs, drift, or existing native work.
    pub fn import_task_tree(
        &mut self,
        project: &ProjectId,
        actor: &ActorContext,
        tree: &TaskTree,
        dry_run: bool,
        now: DateTime<Utc>,
    ) -> Result<TaskSyncImportResult, StoreError> {
        validate_task_import(project, actor, tree, false)?;
        let transaction = self.begin_work_mutation()?;
        let result = import_task_tree_on(&transaction, project, actor, tree, dry_run, now)?;
        if !dry_run && result.changed {
            transaction.commit()?;
        }
        Ok(result)
    }
}

/// Stages an existing empty-graph import in the caller's write transaction.
/// The caller either commits its own receipt with these writes or rolls all of
/// them back, so no planning basis can outlive a failed admission.
pub(super) fn import_task_tree_on(
    transaction: &rusqlite::Transaction<'_>,
    project: &ProjectId,
    actor: &ActorContext,
    tree: &TaskTree,
    dry_run: bool,
    now: DateTime<Utc>,
) -> Result<TaskSyncImportResult, StoreError> {
    import_task_tree_on_with_scope(
        transaction,
        project,
        actor,
        tree,
        ImportScope::legacy(),
        dry_run,
        now,
    )
}

/// Folder import supplies the full disclosure profile used to form its
/// destination-local basis.  The machine alias is comparison input only; it
/// is not an authority claim or persistent host identity.
pub(super) fn import_task_tree_on_with_profile(
    transaction: &rusqlite::Transaction<'_>,
    project: &ProjectId,
    actor: &ActorContext,
    tree: &TaskTree,
    profile: &crate::task_sync::progress::ExportProfile,
    dry_run: bool,
    now: DateTime<Utc>,
) -> Result<TaskSyncImportResult, StoreError> {
    profile.options().validate()?;
    if tree.project.notes_included != profile.include_notes {
        return Err(
            TaskSyncRefusal::InvalidGraph.invalid("task folder profile and tree disclosure differ")
        );
    }
    import_task_tree_on_with_scope(
        transaction,
        project,
        actor,
        tree,
        ImportScope::folder(profile.origin_machine.as_deref()),
        dry_run,
        now,
    )
}

#[derive(Clone, Copy)]
struct ImportScope<'a> {
    machine: Option<&'a str>,
    allow_empty_notes_profile: bool,
}

impl<'a> ImportScope<'a> {
    const fn legacy() -> Self {
        Self {
            machine: None,
            allow_empty_notes_profile: false,
        }
    }

    const fn folder(machine: Option<&'a str>) -> Self {
        Self {
            machine,
            allow_empty_notes_profile: true,
        }
    }

    fn documents(
        self,
        tree: &TaskTree,
    ) -> Result<std::collections::BTreeMap<String, Vec<u8>>, StoreError> {
        if self.allow_empty_notes_profile {
            tree.folder_documents()
        } else {
            tree.documents()
        }
    }
}

fn import_task_tree_on_with_scope(
    transaction: &rusqlite::Transaction<'_>,
    project: &ProjectId,
    actor: &ActorContext,
    tree: &TaskTree,
    scope: ImportScope<'_>,
    dry_run: bool,
    now: DateTime<Utc>,
) -> Result<TaskSyncImportResult, StoreError> {
    validate_task_import(project, actor, tree, scope.allow_empty_notes_profile)?;
    let native: bool = transaction.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_items WHERE project_id = ?1
             AND (json_extract(item_json, '$.restored') = 0 OR latest_event_id IS NOT NULL
                  OR active_run_id IS NOT NULL))",
        [project.0.as_str()],
        |row| row.get(0),
    )?;
    if native {
        return Err(TaskSyncRefusal::NativeState
            .invalid("task import refuses existing native execution state"));
    }
    admit_origins(transaction)?;
    notes::refuse_known_changes(transaction, tree)?;
    activity::refuse_known_changes(transaction, tree)?;
    let current = with_origin_scope(transaction, || {
        export_on_snapshot(
            transaction,
            project,
            tree.project.notes_included,
            scope.machine,
            true,
            &mut crate::task_sync::notes::NoteOmissions::default(),
            &mut crate::task_sync::notes::NoteOmissions::default(),
        )
    })?;
    if scope.documents(&current)? == scope.documents(tree)? {
        return Ok(TaskSyncImportResult {
            task_count: tree.tasks.len(),
            changed: false,
            dry_run,
        });
    }
    if !current.tasks.is_empty() {
        let candidate = tree
            .tasks
            .iter()
            .map(|task| task.id)
            .collect::<std::collections::HashSet<_>>();
        let removed = current
            .tasks
            .iter()
            .any(|held| !candidate.contains(&held.id));
        let refusal = if removed {
            TaskSyncRefusal::RemovedPlanning
        } else {
            TaskSyncRefusal::DivergentPlanning
        };
        return Err(refusal.invalid("task import refuses divergent or removed planning state"));
    }
    // UUIDs are store-wide identities, even when another project owns them.
    for task in &tree.tasks {
        let exists: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM work_items WHERE work_id = ?1)",
            [task.id.0.to_string()],
            |row| row.get(0),
        )?;
        if exists {
            return Err(
                TaskSyncRefusal::IdCollision.invalid("task id already belongs to this store")
            );
        }
        for blocker in &task.blockers {
            let exists: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM work_blockers WHERE blocker_id = ?1)",
                [&blocker.blocker_id],
                |row| row.get(0),
            )?;
            if exists {
                return Err(TaskSyncRefusal::IdCollision
                    .invalid("blocker id already belongs to this store"));
            }
        }
    }
    transaction.execute_batch("PRAGMA defer_foreign_keys = ON")?;
    for (task, snapshot) in tree.tasks.iter().zip(tree.snapshots()?) {
        super::planning::refuse_record_id_pins_on(transaction, &task.acceptance_bindings)
            .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
        insert_task(transaction, tree, task, snapshot, actor, now)?;
    }
    if transaction
        .prepare("PRAGMA foreign_key_check")?
        .query([])?
        .next()?
        .is_some()
    {
        return Err(
            TaskSyncRefusal::InvalidGraph.invalid("task import foreign key admission failed")
        );
    }
    admit_origins(transaction)?;
    Ok(TaskSyncImportResult {
        task_count: tree.tasks.len(),
        changed: true,
        dry_run,
    })
}

fn validate_task_import(
    project: &ProjectId,
    actor: &ActorContext,
    tree: &TaskTree,
    allow_empty_notes_profile: bool,
) -> Result<(), StoreError> {
    if allow_empty_notes_profile {
        tree.validate_folder()
    } else {
        tree.validate()
    }
    .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
    validate_task_sync_actor(actor)?;
    if &tree.project.project_id != project {
        return Err(TaskSyncRefusal::OriginMismatch.invalid("task candidate project mismatch"));
    }
    Ok(())
}

pub(super) fn validate_task_sync_actor(actor: &ActorContext) -> Result<(), StoreError> {
    crate::storage::admit_live_actor_session(actor)?;
    actor.validate_attribution_context().map_err(invalid)?;
    if actor.actor_id.trim().is_empty()
        || actor
            .provenance_chain
            .iter()
            .any(|link| link.source == ORIGIN_KIND)
        || actor.actor_id.trim() != actor.actor_id
        || actor.actor_kind.trim().is_empty()
        || actor.actor_kind.trim() != actor.actor_kind
        || actor
            .session_id
            .as_ref()
            .is_none_or(|id| id.0.trim().is_empty())
    {
        return Err(invalid("import attribution is required"));
    }
    Ok(())
}

fn admit_origins(connection: &Connection) -> Result<(), StoreError> {
    let mut checked = 0;
    let mut failures = Vec::new();
    verify_origins(connection, &mut checked, &mut failures)?;
    if failures.is_empty() {
        Ok(())
    } else {
        Err(TaskSyncRefusal::OriginMismatch.invalid(format!(
            "task import origin audit failed: {}",
            failures.join(", ")
        )))
    }
}

fn export_on(connection: &Connection, project: &ProjectId) -> Result<TaskTree, StoreError> {
    with_origin_scope(connection, || {
        export_on_snapshot(
            connection,
            project,
            false,
            None,
            false,
            &mut crate::task_sync::notes::NoteOmissions::default(),
            &mut crate::task_sync::notes::NoteOmissions::default(),
        )
    })
}

pub(super) fn export_on_with_profile(
    connection: &Connection,
    project: &ProjectId,
    profile: &crate::task_sync::progress::ExportProfile,
) -> Result<TaskSyncNoteExport, StoreError> {
    profile.options().validate()?;
    with_origin_scope(connection, || {
        let mut omissions = crate::task_sync::notes::NoteOmissions::default();
        let mut activity_omissions = crate::task_sync::notes::NoteOmissions::default();
        let tree = export_on_snapshot(
            connection,
            project,
            profile.include_notes,
            profile.origin_machine.as_deref(),
            false,
            &mut omissions,
            &mut activity_omissions,
        )?;
        Ok(TaskSyncNoteExport {
            tree,
            omissions,
            activity_omissions,
        })
    })
}

fn export_on_snapshot(
    connection: &Connection,
    project: &ProjectId,
    include_notes: bool,
    machine: Option<&str>,
    comparison: bool,
    omissions: &mut crate::task_sync::notes::NoteOmissions,
    activity_omissions: &mut crate::task_sync::notes::NoteOmissions,
) -> Result<TaskTree, StoreError> {
    let ids = connection
        .prepare("SELECT work_id FROM work_items WHERE project_id = ?1 ORDER BY work_id")?
        .query_map([&project.0], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut tasks = Vec::new();
    // A comparison reports only the retained disclosure profile.  In
    // particular, an empty legacy destination has no note-bearing task from
    // which to recover that profile; do not synthesize a zero-task
    // notes-enabled legacy tree here.  Folder envelopes use their distinct
    // folder-only validation path for that representation.
    let mut notes_included = include_notes && !comparison;
    for id in ids {
        let id = WorkId(uuid::Uuid::parse_str(&id).map_err(|error| invalid(error.to_string()))?);
        // Loading, notes and activity read the task's import origin through the
        // shared validators; their binding refusals are origin mismatches here.
        let item = query::load_work_item(connection, id).map_err(origin::classify_binding)?;
        if item.origin == crate::WorkOrigin::Imported || item.source_snapshot_id.is_some() {
            return Err(TaskSyncRefusal::OriginMismatch.invalid(
                "task export refuses external imported origin and source snapshot; use graph export to retain that provenance",
            ));
        }
        let event_records = query::canonical_work_event_records_for_item(connection, id)?;
        let shared_activity = activity::capture(
            connection,
            &item,
            &event_records,
            machine,
            comparison,
            activity_omissions,
        )
        .map_err(origin::classify_binding)?;
        let events = event_records
            .into_iter()
            .map(|(_, event)| event)
            .collect::<Vec<_>>();
        let records = graph_snapshot::restored_records_on(connection, id)?;
        let prerequisites = query::load_prerequisite_projection_ids(connection, id)?;
        let snapshot =
            graph_snapshot::snapshot_item(&item, prerequisites.clone(), &events, &records)?;
        let completion = graph_snapshot::completion_from_events(
            connection, &item, &events, &records,
        )?
        .map(|completion| SharedCompletion {
            summary: completion.summary,
            completed_at: completion.completed_at,
            actor: SharedActor::from_actor(&completion.actor),
        });
        let mut blockers = query::load_active_blocker_projections(connection, id)?
            .into_iter()
            .map(|blocker| SharedBlocker {
                blocker_id: blocker.blocker_id,
                kind: blocker.kind,
                detail: blocker.detail,
                created_by: SharedActor::from_actor(&blocker.created_by),
                created_at: blocker.created_at,
            })
            .collect::<Vec<_>>();
        blockers.sort_by(|a, b| a.blocker_id.cmp(&b.blocker_id));
        let shared_notes = if include_notes || comparison {
            let (notes, stored_inclusion) = notes::capture(
                connection,
                &item,
                machine,
                include_notes,
                comparison,
                omissions,
            )
            .map_err(origin::classify_binding)?;
            notes_included |= stored_inclusion;
            notes
        } else {
            Vec::new()
        };
        tasks.push(TaskDocument {
            id,
            external_ref: item.external_ref,
            title: item.title,
            outcome: item.outcome,
            acceptance: item.acceptance,
            acceptance_bindings: item.acceptance_bindings,
            evaluation_mode: item.evaluation_mode,
            kind: item.kind,
            priority: item.priority,
            labels: item.labels,
            assigned_to: item.assigned_to,
            deferred_until: item.deferred_until,
            lifecycle: item.lifecycle,
            parent_id: item.parent_id,
            child_requirement: item.child_requirement,
            prerequisites,
            blockers,
            superseded_by: item.superseded_by,
            disposal_reason: snapshot.disposal_reason,
            completion,
            notes: shared_notes,
            activity: shared_activity,
        });
    }
    let tree = TaskTree {
        project: ProjectDocument {
            format: FORMAT.into(),
            project_id: project.clone(),
            notes_included,
        },
        tasks,
    };
    // A retained note can exceed today's write limit; the candidate it would
    // make is refused as an invalid graph like any other.
    tree.validate_stored()
        .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
    Ok(tree)
}

fn insert_task(
    connection: &Connection,
    tree: &TaskTree,
    task: &TaskDocument,
    snapshot: crate::WorkGraphSnapshotItem,
    actor: &ActorContext,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    let mut actor = actor.clone();
    actor.source_tool = Some(IMPORT_TOOL.into());
    let actor = &actor;
    let record = RestoredRecord {
        schema_version: crate::WORK_GRAPH_SNAPSHOT_SCHEMA_VERSION,
        project_id: tree.project.project_id.clone(),
        work_id: task.id,
        generation_index: 0,
        item: snapshot.clone(),
        relations: RestoredRelationBasis {
            prerequisites: task.prerequisites.clone(),
            blockers: task.snapshot_blockers(),
        },
        history: history(task, actor, now),
    };
    let object = CanonicalObject::mint(&record)?;
    let origin = ImportOrigin {
        project_id: tree.project.project_id.clone(),
        record_id: object.key().clone(),
        task: task.clone(),
        actor: actor.clone(),
        imported_at: now,
        notes_included: tree.project.notes_included,
    };
    let origin_object = CanonicalObject::mint(&origin)?;
    let item = WorkItem {
        schema_version: crate::schema::SCHEMA_VERSION,
        project_id: tree.project.project_id.clone(),
        work_id: task.id,
        external_ref: snapshot.external_ref,
        short_ref: snapshot.short_ref,
        root_id: snapshot.root_id,
        parent_id: task.parent_id,
        child_requirement: task.child_requirement,
        title: task.title.clone(),
        outcome: task.outcome.clone(),
        acceptance: task.acceptance.clone(),
        kind: task.kind,
        priority: task.priority,
        labels: task.labels.clone(),
        assigned_to: task.assigned_to.clone(),
        deferred_until: task.deferred_until,
        origin: crate::WorkOrigin::Local,
        source_snapshot_id: None,
        lifecycle: task.lifecycle,
        revision: 1,
        active_run_id: None,
        restored: true,
        superseded_by: task.superseded_by,
        evaluation_mode: task.evaluation_mode,
        acceptance_bindings: task.acceptance_bindings.clone(),
        created_by: linked_actor(actor, origin_object.key()),
        created_at: now,
        updated_at: now,
    };
    insert_item(connection, &item)?;
    SqliteStore::insert_object(connection, "work_restored_record", &object)?;
    SqliteStore::insert_object(connection, ORIGIN_KIND, &origin_object)?;
    connection.execute("INSERT INTO work_restored_records (work_id, generation_index, record_id) VALUES (?1, 0, ?2)",
        params![task.id.0.to_string(), object.key().as_str()])?;
    for prerequisite in &task.prerequisites {
        connection.execute("INSERT INTO work_prerequisites (work_id, prerequisite_id, event_id) VALUES (?1, ?2, ?3)",
            params![task.id.0.to_string(), prerequisite.0.to_string(), object.key().as_str()])?;
    }
    for blocker in task.snapshot_blockers() {
        let blocker = WorkBlocker {
            work_id: blocker.work_id,
            blocker_id: blocker.blocker_id,
            kind: blocker.kind,
            detail: blocker.detail,
            created_by: blocker.created_by,
            created_at: blocker.created_at,
        };
        connection.execute(
            "INSERT INTO work_blockers (blocker_id, work_id, state, blocker_json,
            created_event_id, cleared_event_id) VALUES (?1, ?2, 'active', ?3, ?4, NULL)",
            params![
                blocker.blocker_id,
                task.id.0.to_string(),
                serde_json::to_vec(&blocker)?,
                object.key().as_str()
            ],
        )?;
    }
    // Search includes active blockers, so derive it after the relations exist,
    // just as graph loading does.
    super::refresh_work_catalog_for_restore(connection, &item)
}

fn insert_item(connection: &Connection, item: &WorkItem) -> Result<(), StoreError> {
    connection.execute("INSERT INTO work_items (work_id, project_id, short_ref, root_id, parent_id,
        child_requirement, lifecycle, priority, assigned_to, deferred_until_ms, revision, active_run_id,
        superseded_by, source_snapshot_id, latest_event_id, created_at_ms, updated_at_ms, item_json)
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1, NULL, ?11, NULL, NULL, ?12, ?12, ?13)",
        params![item.work_id.0.to_string(), item.project_id.0, item.short_ref, item.root_id.0.to_string(),
            item.parent_id.map(|id| id.0.to_string()), super::encode_state_for_restore(item.child_requirement)?,
            super::encode_state_for_restore(item.lifecycle)?, item.priority, item.assigned_to,
            item.deferred_until.map(|time| time.timestamp_millis()), item.superseded_by.map(|id| id.0.to_string()),
            item.created_at.timestamp_millis(), serde_json::to_vec(item)?])?;
    Ok(())
}

fn history(
    task: &TaskDocument,
    actor: &ActorContext,
    now: DateTime<Utc>,
) -> WorkGraphSnapshotHistory {
    let events = task
        .disposal_reason
        .as_ref()
        .map(|reason| WorkGraphSnapshotEvent {
            kind: "disposed".into(),
            work_revision: 1,
            occurred_at: now,
            reason: Some(reason.clone()),
            lifecycle: Some(task.lifecycle),
            related_work_id: task.superseded_by,
            related_work_revision: None,
            actor: SharedActor::from_actor(actor).reported(),
        })
        .into_iter()
        .collect();
    WorkGraphSnapshotHistory {
        source_notices: Vec::new(),
        notes: task
            .notes
            .iter()
            .map(|note| crate::WorkGraphSnapshotNote {
                evidence_kind: crate::WorkEvidenceKind::Generic,
                summary: note.body.clone(),
                refs: Vec::new(),
                gate: None,
                actor: note.author.reported(),
                recorded_at: note.recorded_at,
            })
            .collect(),
        events,
        completion: task
            .completion
            .as_ref()
            .map(|completion| WorkGraphSnapshotCompletion {
                summary: completion.summary.clone(),
                completed_at: completion.completed_at,
                actor: completion.actor.reported(),
            }),
    }
}
