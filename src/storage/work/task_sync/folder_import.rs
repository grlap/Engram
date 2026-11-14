//! The ordinary incremental folder import.
//!
//! The changed task documents come from a verified Git delta between the
//! folder head's source commit and the incoming one. They are admitted per
//! task, like a complete folder import: a task that cannot take its change is
//! held or refused by name while the others apply. The folder head advances
//! in the same transaction: a complete import moves its source to the
//! incoming commit, a partial one keeps the earlier source, so its held tasks
//! return on the next import, and records them as pending.

use std::collections::{BTreeMap, BTreeSet};

use chrono::{DateTime, Utc};

use super::{
    baseline,
    closure::graph,
    folder_head::{self, FolderHead, FolderHeadReason, FolderSource},
    origin, producing_origin, reconciliation, selected,
};
use crate::{
    ActorContext, ObjectId, ProjectId, WorkId,
    storage::{SqliteStore, StoreError, TaskSyncImportResult, TaskSyncTaskStatus},
    task_sync::{TaskDocument, TaskSyncRefusal, progress::ExportProfile},
};

/// One incremental folder import, read from a verified Git delta.
pub struct FolderDeltaApply<'a> {
    pub project: &'a ProjectId,
    pub actor: &'a ActorContext,
    pub profile: &'a ExportProfile,
    /// The folder head the delta was read against, compared and set.
    pub head: &'a ObjectId,
    /// The incoming commit with the root and count the delta derived for it.
    pub incoming: FolderSource,
    /// Every changed task path, with its incoming document or `None`.
    pub changes: &'a BTreeMap<WorkId, Option<TaskDocument>>,
    pub dry_run: bool,
    pub now: DateTime<Utc>,
}

/// What an incremental folder import applied, and the head it recorded.
#[derive(Debug)]
pub struct FolderDeltaOutcome {
    pub result: TaskSyncImportResult,
    pub head: FolderHead,
}

impl SqliteStore {
    /// Apply one incremental folder import in one transaction.
    ///
    /// # Errors
    /// Refuses a moved or missing folder head, a head with no source commit,
    /// local planning changed since the head for a task the change touches
    /// (remedy: export and commit it first), and every admission refusal.
    pub fn apply_task_sync_folder_delta(
        &mut self,
        request: &FolderDeltaApply<'_>,
    ) -> Result<FolderDeltaOutcome, StoreError> {
        let transaction = self.begin_work_mutation()?;
        let outcome = origin::with_origin_scope(&transaction, || apply_on(&transaction, request))?;
        if request.dry_run {
            // Dropping the transaction rolls every effect back.
            return Ok(outcome);
        }
        transaction.commit()?;
        Ok(outcome)
    }
}

fn apply_on(
    transaction: &rusqlite::Transaction<'_>,
    request: &FolderDeltaApply<'_>,
) -> Result<FolderDeltaOutcome, StoreError> {
    let project = request.project;
    let (id, head) = folder_head::head_on(transaction, project, request.profile)?.ok_or_else(|| {
        TaskSyncRefusal::IncrementalBasisUnavailable.invalid(
            "no recorded folder head; enrol with `engram task-sync import --basis` or `--empty-basis` first",
        )
    })?;
    if id != *request.head {
        return Err(TaskSyncRefusal::BaselineHeadMoved
            .invalid("another export or import recorded the shared folder first; import again"));
    }
    let source = head.source.clone().ok_or_else(|| {
        TaskSyncRefusal::IncrementalBasisUnavailable
            .invalid("the folder head records no imported commit yet; import once with `--rebuild`")
    })?;
    let cut = producing_origin::cut_on(transaction, project)?;
    let local = super::super::feed_tasks::changed_between(
        transaction,
        project,
        head.cut.position,
        cut.position,
    )?;
    let mut seeds = request.changes.keys().copied().collect::<BTreeSet<_>>();
    for document in request.changes.values().flatten() {
        seeds.extend(
            graph::document_edges(document)
                .into_iter()
                .map(|(_, target)| target),
        );
    }
    let previous = baseline::entries(transaction, project, request.changes.keys().copied())?;
    let (_, application) = selected::admit_on(
        transaction,
        project,
        request.profile,
        request.actor,
        request.changes,
        seeds,
        |read| {
            let touched = read
                .intersection(&local)
                .map(|id| id.0.to_string())
                .collect::<Vec<_>>();
            if touched.is_empty() {
                Ok(())
            } else {
                Err(TaskSyncRefusal::DivergentPlanning.invalid(format!(
                    "local planning of {} changed since the last export or import; export and commit it, then import again",
                    touched.join(", ")
                )))
            }
        },
        request.now,
        true,
    )?;
    let unapplied = application
        .tasks
        .iter()
        .filter(|task| task.status != TaskSyncTaskStatus::Applied)
        .map(|task| task.task)
        .collect::<BTreeSet<_>>();
    let applied = request
        .changes
        .iter()
        .filter(|(id, _)| !unapplied.contains(id))
        .map(|(id, document)| (*id, document.clone()))
        .collect::<BTreeMap<_, _>>();
    if !applied.is_empty() {
        let recorded = baseline::head(transaction, project)?.map(|head| head.record);
        baseline::append(
            transaction,
            project,
            recorded,
            applied.clone(),
            None,
            request.profile,
            request.now,
        )?;
        reconciliation::advance_on(transaction, project, applied.keys().copied(), &previous)?;
    }
    let after = producing_origin::cut_on(transaction, project)?;
    let complete = unapplied.is_empty();
    let next = FolderHead {
        project_id: project.clone(),
        profile: request.profile.clone(),
        previous: Some(id),
        reason: FolderHeadReason::Import,
        cut: after,
        // The folder on disk is the incoming commit either way.
        inventory_root: request.incoming.inventory_root,
        task_count: request.incoming.task_count,
        source: Some(if complete {
            request.incoming.clone()
        } else {
            source
        }),
        folder_commit: Some(request.incoming.commit.clone()),
        pending: unapplied,
    };
    folder_head::append_on(transaction, &next)?;
    Ok(FolderDeltaOutcome {
        result: TaskSyncImportResult {
            task_count: usize::try_from(request.incoming.task_count).unwrap_or(usize::MAX),
            changed: !applied.is_empty(),
            dry_run: request.dry_run,
            tasks: application.tasks,
        },
        head: next,
    })
}
