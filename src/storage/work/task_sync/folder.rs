//! Durable, project-scoped baselines for a human-resolved task folder.

use chrono::{DateTime, Utc};
use std::collections::HashSet;

use rusqlite::params;
use serde::{Deserialize, Serialize};

use super::{
    TaskSyncImportResult, import_task_tree_on_with_profile, invalid, validate_task_sync_actor,
};
use crate::{
    ActorContext, CanonicalObject, ObjectId, ProjectId,
    storage::{SqliteStore, StoreError},
    task_sync::{
        TaskSyncRefusal, TaskTree,
        folder_plan::{self, DeltaPlan},
        progress::{ExportProfile, documents_fingerprint},
    },
};

pub(crate) const IMPORT_PLANNING_KIND: &str = "task_sync_import_planning";
const APPLY_OPERATION: &str = "apply_task_sync_folder";

/// The complete shared inventory selected by a successful folder application.
/// The record deliberately contains no transport, claim, or local runtime state.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct TaskSyncImportPlanning {
    project_id: ProjectId,
    tree: TaskTree,
    inventory_fingerprint: String,
    export_basis: String,
    profile: ExportProfile,
    created_at: DateTime<Utc>,
}

/// Caller-selected folder input. The export basis is the destination-local
/// comparison captured before the human resolves a Git merge.
#[derive(Clone, Copy, Serialize)]
pub struct TaskSyncFolderApply<'a> {
    pub project: &'a ProjectId,
    pub actor: &'a ActorContext,
    pub tree: &'a TaskTree,
    pub export_basis: &'a str,
    pub profile: &'a ExportProfile,
    pub complete: bool,
    /// The admission timestamp is audit data, not an idempotency input.
    #[serde(skip)]
    pub now: DateTime<Utc>,
}

/// The exact shared planning tree selected for a folder export and its
/// destination-local content comparison. A caller retains its own Git-side
/// value before resolving a merge; this is not an identity or authentication
/// assertion.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskSyncFolderExport {
    pub tree: TaskTree,
    pub export_basis: String,
    pub profile: ExportProfile,
    pub omissions: crate::task_sync::notes::NoteOmissions,
    pub activity_omissions: crate::task_sync::notes::NoteOmissions,
}

impl SqliteStore {
    /// Applies an empty-graph folder baseline and records the exact exported
    /// inventory atomically. A replayed idempotency key returns its receipt
    /// without adding a second baseline event.
    ///
    /// # Errors
    ///
    /// Returns an error when the request is invalid, conflicts with local
    /// planning, or cannot be durably recorded.
    pub fn apply_task_sync_folder(
        &mut self,
        request: TaskSyncFolderApply<'_>,
        idempotency_key: &str,
    ) -> Result<TaskSyncImportResult, StoreError> {
        let transaction = self.begin_work_mutation()?;
        let result = apply_task_sync_folder_on(&transaction, request, idempotency_key)?;
        transaction.commit()?;
        Ok(result)
    }

    /// Exports current shared planning while retaining the latest successful
    /// complete baseline as the later import basis.
    ///
    /// # Errors
    ///
    /// Returns an error when the stored baseline is inconsistent, or when the
    /// current planning projection cannot form a valid task tree. A source
    /// with no prior folder baseline may export its current planning inventory;
    /// its emitted inventory fingerprint is the bootstrap basis.
    pub fn export_task_sync_folder(
        &self,
        project: &ProjectId,
        profile: &ExportProfile,
    ) -> Result<TaskSyncFolderExport, StoreError> {
        self.work_read_snapshot(|store| folder_export_on(&store.connection, project, profile))
    }
}

/// Applies one complete folder inside the caller's transaction; the public
/// method owns commit or rollback of that transaction.
pub(super) fn apply_task_sync_folder_on(
    transaction: &rusqlite::Transaction<'_>,
    request: TaskSyncFolderApply<'_>,
    idempotency_key: &str,
) -> Result<TaskSyncImportResult, StoreError> {
    if idempotency_key.trim().is_empty() {
        return Err(invalid("task folder idempotency key is required"));
    }
    if !request.complete {
        return Err(
            TaskSyncRefusal::InvalidGraph.invalid("task folder import requires a complete folder")
        );
    }
    if request.export_basis.trim().is_empty() {
        return Err(TaskSyncRefusal::DivergentPlanning
            .invalid("task folder import requires a destination export basis"));
    }
    if request.tree.project.project_id != *request.project {
        return Err(TaskSyncRefusal::OriginMismatch
            .invalid("task folder import tree belongs to a different project"));
    }
    request.profile.options().validate()?;
    if request.tree.project.notes_included != request.profile.include_notes {
        return Err(
            TaskSyncRefusal::InvalidGraph.invalid("task folder profile and tree disclosure differ")
        );
    }
    request
        .tree
        .validate_folder()
        .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
    let request_object = CanonicalObject::freeze(&request)?;
    if let Some(result) = super::super::feeds::replay_operation::<TaskSyncImportResult>(
        transaction,
        APPLY_OPERATION,
        idempotency_key,
        request_object.key(),
    )? {
        return Ok(result);
    }
    validate_task_sync_actor(request.actor)?;
    let baseline = latest_planning(transaction, request.project)?;
    // Only the note selection is shared; each host keeps its own alias.
    if baseline
        .as_ref()
        .is_some_and(|baseline| baseline.profile.include_notes != request.profile.include_notes)
    {
        return Err(TaskSyncRefusal::DivergentPlanning
            .invalid("task folder import note selection differs from its complete baseline"));
    }
    let current = folder_export_on(transaction, request.project, request.profile)?;
    if let Some(baseline) = &baseline
        && baseline.export_basis == request.export_basis
        && baseline.tree.folder_documents()? == request.tree.folder_documents()?
        && current.tree.folder_documents()? == baseline.tree.folder_documents()?
    {
        let result = TaskSyncImportResult {
            task_count: request.tree.tasks.len(),
            changed: false,
            dry_run: false,
        };
        super::super::planning::persist_operation_result(
            transaction,
            APPLY_OPERATION,
            idempotency_key,
            request_object.key(),
            &result,
        )?;
        return Ok(result);
    }
    if current.export_basis != request.export_basis {
        return Err(TaskSyncRefusal::DivergentPlanning
            .invalid("task folder import refuses a locally changed planning inventory"));
    }
    // An enrolled destination always reconciles the folder against its own
    // planning, even when that planning is empty, so its exchange history
    // keeps judging every later folder. The empty-graph import below serves
    // only a destination that holds no exchange baseline.
    if let Some(baseline) = &baseline {
        let membership = baseline
            .tree
            .tasks
            .iter()
            .map(|task| task.id)
            .collect::<HashSet<_>>();
        let plan = folder_plan::plan(&current.tree, request.tree, &membership)?;
        return apply_planned_changes(
            transaction,
            request,
            &plan,
            &current.tree,
            baseline,
            idempotency_key,
            &request_object,
        );
    }
    let mut result = import_task_tree_on_with_profile(
        transaction,
        request.project,
        request.actor,
        request.tree,
        request.profile,
        false,
        request.now,
    )?;
    // Only a destination with no baseline reaches this import, and it always
    // records the folder as its first one.
    record_planning(transaction, request)?;
    result.changed = true;
    super::super::planning::persist_operation_result(
        transaction,
        APPLY_OPERATION,
        idempotency_key,
        request_object.key(),
        &result,
    )?;
    Ok(result)
}

/// Applies a classified folder to an enrolled destination: every requested
/// delta is an admitted addition or scalar edit, or the whole folder refuses,
/// naming its refused tasks, and nothing is written. The baseline records the
/// folder whenever its inventory differs from the latest baseline, even when
/// no task changes, so a host-local task the folder now carries joins the
/// exchange. It is recorded before the final export check, which therefore
/// reads the new exchange membership.
fn apply_planned_changes(
    transaction: &rusqlite::Transaction<'_>,
    request: TaskSyncFolderApply<'_>,
    plan: &DeltaPlan,
    current: &TaskTree,
    baseline: &TaskSyncImportPlanning,
    idempotency_key: &str,
    request_object: &CanonicalObject,
) -> Result<TaskSyncImportResult, StoreError> {
    super::folder_admission::apply_plan(
        transaction,
        plan,
        current,
        request.tree,
        request.actor,
        request.now,
    )?;
    let changed = plan
        .changed_ids()
        .chain(plan.additions())
        .collect::<HashSet<_>>();
    let removed = plan
        .tasks
        .iter()
        .filter_map(|task| matches!(task.delta, folder_plan::TaskDelta::Deleted).then_some(task.id))
        .collect::<HashSet<_>>();
    if plan.additions().next().is_some() || !removed.is_empty() {
        // Admission named every predictable conflict; these remain as the
        // guards that roll the whole folder back.
        if transaction
            .prepare("PRAGMA foreign_key_check")?
            .query([])?
            .next()?
            .is_some()
        {
            return Err(StoreError::InvalidWorkProjection(
                "a folder addition left a dangling reference".into(),
            ));
        }
        audit_origins(transaction)?;
    }
    let result = TaskSyncImportResult {
        task_count: request.tree.tasks.len(),
        changed: !changed.is_empty()
            || baseline.tree.folder_documents()? != request.tree.folder_documents()?,
        dry_run: false,
    };
    if result.changed {
        record_planning(transaction, request)?;
    }
    if !changed.is_empty() || !removed.is_empty() {
        // Each added or edited task now exports exactly as the folder
        // carries it, whole document by whole document.
        let applied = folder_export_on(transaction, request.project, request.profile)?;
        let applied = applied
            .tree
            .tasks
            .iter()
            .map(|task| (task.id, task))
            .collect::<std::collections::HashMap<_, _>>();
        let resolved = request
            .tree
            .tasks
            .iter()
            .filter(|task| changed.contains(&task.id));
        for task in resolved {
            if applied.get(&task.id).copied() != Some(task) {
                return Err(StoreError::InvalidWorkProjection(
                    "a folder change does not export as the folder carries it".into(),
                ));
            }
        }
        if removed.iter().any(|id| applied.contains_key(id)) {
            return Err(StoreError::InvalidWorkProjection(
                "a folder removal remained in the new exchange membership".into(),
            ));
        }
        // Omitted local nonmembers survive byte-for-byte alongside selected changes.
        let current = current
            .tasks
            .iter()
            .map(|task| (task.id, task))
            .collect::<std::collections::HashMap<_, _>>();
        for retained in plan
            .tasks
            .iter()
            .filter(|task| matches!(task.delta, folder_plan::TaskDelta::RetainedLocal))
        {
            if applied.get(&retained.id).copied() != current.get(&retained.id).copied() {
                return Err(StoreError::InvalidWorkProjection(
                    "a folder change rewrote retained local work".into(),
                ));
            }
        }
    }
    super::super::planning::persist_operation_result(
        transaction,
        APPLY_OPERATION,
        idempotency_key,
        request_object.key(),
        &result,
    )?;
    Ok(result)
}

/// The final import-origin audit after a folder's additions. Admission
/// already judged every addition against the whole origin history, so a
/// failure here is a projection bug that rolls the folder back, never an
/// input refusal.
fn audit_origins(connection: &rusqlite::Connection) -> Result<(), StoreError> {
    let mut checked = 0;
    let mut failures = Vec::new();
    super::origin::verify_origins(connection, &mut checked, &mut failures)?;
    if failures.is_empty() {
        Ok(())
    } else {
        Err(StoreError::InvalidWorkProjection(format!(
            "a folder addition failed the import origin audit: {}",
            failures.join(", ")
        )))
    }
}

/// Records the applied folder as the latest exchange baseline.
fn record_planning(
    transaction: &rusqlite::Transaction<'_>,
    request: TaskSyncFolderApply<'_>,
) -> Result<(), StoreError> {
    let planning = TaskSyncImportPlanning {
        project_id: request.project.clone(),
        tree: request.tree.clone(),
        inventory_fingerprint: documents_fingerprint(&request.tree.folder_documents()?),
        export_basis: request.export_basis.into(),
        profile: request.profile.clone(),
        created_at: request.now,
    };
    let planning_object = CanonicalObject::mint(&planning)?;
    SqliteStore::insert_object(transaction, IMPORT_PLANNING_KIND, &planning_object)?;
    super::super::feeds::append_to_project_feed(
        transaction,
        request.project,
        IMPORT_PLANNING_KIND,
        &planning_object,
    )?;
    Ok(())
}

fn folder_export_on(
    connection: &rusqlite::Connection,
    project: &ProjectId,
    profile: &ExportProfile,
) -> Result<TaskSyncFolderExport, StoreError> {
    profile.options().validate()?;
    let history = planning_history(connection, project)?;
    let retired = if let Some(planning) = history.last() {
        // Only the note selection is shared; each host exports under its
        // own origin-machine alias.
        if planning.profile.include_notes != profile.include_notes {
            return Err(TaskSyncRefusal::DivergentPlanning
                .invalid("task folder export note selection differs from its complete baseline"));
        }
        history
            .iter()
            .flat_map(|entry| entry.tree.tasks.iter().map(|task| task.id))
            .collect::<HashSet<_>>()
            .difference(&planning.tree.tasks.iter().map(|task| task.id).collect())
            .copied()
            .collect::<HashSet<_>>()
    } else {
        HashSet::new()
    };
    let mut captured = super::export_on_with_profile(connection, project, profile)?;
    captured
        .tree
        .tasks
        .retain(|task| !retired.contains(&task.id));
    captured.tree.validate_stored().map_err(|error| {
        TaskSyncRefusal::DivergentPlanning.invalid(format!(
            "task folder export would retain a reference to an exchange-retired task: {error}"
        ))
    })?;
    let export_basis = documents_fingerprint(&captured.tree.folder_documents()?);
    Ok(TaskSyncFolderExport {
        tree: captured.tree,
        export_basis,
        profile: profile.clone(),
        omissions: captured.omissions,
        activity_omissions: captured.activity_omissions,
    })
}

fn latest_planning(
    connection: &rusqlite::Connection,
    project: &ProjectId,
) -> Result<Option<TaskSyncImportPlanning>, StoreError> {
    Ok(planning_history(connection, project)?.pop())
}

/// Recovers every complete exchange baseline in durable project-feed order.
/// History is deliberately fail-closed: it defines which task IDs were shared.
/// No rows means a pre-enrollment source; a malformed row must never be treated
/// as an empty history.
fn planning_history(
    connection: &rusqlite::Connection,
    project: &ProjectId,
) -> Result<Vec<TaskSyncImportPlanning>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT object_id FROM work_feed_entries
         WHERE feed_kind = 'project' AND feed_id = ?1 AND object_kind = ?2
         ORDER BY position",
    )?;
    statement
        .query_map(params![project.0, IMPORT_PLANNING_KIND], |row| {
            row.get::<_, String>(0)
        })?
        .map(|row| {
            let id = ObjectId::from_stored(row?)
                .ok_or_else(|| invalid("task folder baseline has an invalid object id"))?;
            let planning = SqliteStore::get_typed_object_on(connection, &id, IMPORT_PLANNING_KIND)?
                .ok_or_else(|| invalid("task folder baseline object is missing"))?;
            validate_planning(&planning, project)?;
            Ok(planning)
        })
        .collect()
}

fn validate_planning(
    planning: &TaskSyncImportPlanning,
    project: &ProjectId,
) -> Result<(), StoreError> {
    let fingerprint = documents_fingerprint(&planning.tree.folder_documents()?);
    if planning.project_id != *project
        || planning.tree.project.project_id != *project
        || planning.inventory_fingerprint != fingerprint
        || planning.export_basis.trim().is_empty()
        || planning.profile.include_notes != planning.tree.project.notes_included
        || planning.profile.options().validate().is_err()
    {
        return Err(invalid("task folder baseline inventory is inconsistent"));
    }
    Ok(())
}

#[cfg(test)]
pub(super) fn record_complete_baseline_for_test(
    store: &mut SqliteStore,
    tree: &TaskTree,
) -> Result<(), StoreError> {
    let transaction = store.begin_work_mutation()?;
    let planning = TaskSyncImportPlanning {
        project_id: tree.project.project_id.clone(),
        tree: tree.clone(),
        inventory_fingerprint: documents_fingerprint(&tree.folder_documents()?),
        export_basis: documents_fingerprint(&tree.folder_documents()?),
        profile: ExportProfile {
            include_notes: tree.project.notes_included,
            origin_machine: None,
        },
        created_at: Utc::now(),
    };
    let object = CanonicalObject::mint(&planning)?;
    SqliteStore::insert_object(&transaction, IMPORT_PLANNING_KIND, &object)?;
    super::super::feeds::append_to_project_feed(
        &transaction,
        &planning.project_id,
        IMPORT_PLANNING_KIND,
        &object,
    )?;
    transaction.commit()?;
    Ok(())
}

pub(in crate::storage::work) fn planning_project(object: &CanonicalObject) -> Option<ProjectId> {
    let planning = object.decode::<TaskSyncImportPlanning>().ok()?;
    (planning.tree.project.project_id == planning.project_id
        && documents_fingerprint(&planning.tree.folder_documents().ok()?)
            == planning.inventory_fingerprint
        && !planning.export_basis.trim().is_empty()
        && planning.profile.include_notes == planning.tree.project.notes_included
        && planning.profile.options().validate().is_ok())
    .then_some(planning.project_id)
}

pub(crate) fn folder_apply_projection(
    object: serde_json::Value,
    project: &ProjectId,
) -> Result<(String, DateTime<Utc>), StoreError> {
    let planning = serde_json::from_value::<TaskSyncImportPlanning>(object)?;
    validate_planning(&planning, project)?;
    Ok((
        folder_apply_summary(planning.tree.tasks.len()),
        planning.created_at,
    ))
}

pub(crate) fn folder_apply_summary(task_count: usize) -> String {
    format!("task-sync folder apply: {task_count} tasks in the applied baseline")
}
