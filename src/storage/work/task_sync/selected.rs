//! Admit a selected set of changed task documents into local planning.
//!
//! The strict delta writer and the ordinary incremental folder import share
//! this core: it captures the touched local, native and immutable-origin
//! components, classifies each change with the complete-folder field rules,
//! and applies the admitted changes in the caller's transaction. Nothing here
//! reads every task.

use std::collections::{BTreeMap, BTreeSet, HashMap};

use rusqlite::Transaction;

use super::{
    baseline,
    closure::{component, native, overlay},
    folder_admission::{self, OriginGraph, Prospective},
    reconciliation,
};
use crate::{
    ActorContext, ProjectId, WorkGraphSnapshotItem, WorkId,
    storage::StoreError,
    task_sync::{
        self, ProjectDocument, TaskDocument, TaskGraphComponent, TaskSyncGraphView, TaskTree,
        folder_plan, ledger::require, progress::ExportProfile,
        reconciliation::ReconciliationWorkBasis,
    },
};

pub(in crate::storage::work::task_sync) fn snapshots(
    project: &ProjectId,
    scope: &TaskGraphComponent,
) -> Result<HashMap<WorkId, WorkGraphSnapshotItem>, StoreError> {
    let tree = TaskTree {
        project: ProjectDocument {
            format: task_sync::FORMAT.into(),
            project_id: project.clone(),
            notes_included: scope.notes_included,
        },
        tasks: scope.documents.values().cloned().collect(),
    };
    let snapshots = tree
        .snapshots()?
        .into_iter()
        .map(|item| (item.work_id, item))
        .collect::<HashMap<_, _>>();
    require(
        scope
            .expected_roots
            .iter()
            .all(|(id, root)| snapshots.get(id).is_some_and(|item| item.root_id == *root)),
        "connected graph root differs from its native or original binding",
    )?;
    Ok(snapshots)
}

/// Admit `changes` against the local planning they touch. `unchanged` is the
/// caller's own freshness check over every task the capture read, called
/// before any classification. With `allow_partial`, a task that cannot take
/// its change is held or refused by name while the others apply, as a folder
/// import does; without it, any such task refuses the whole change. Returns
/// the reconciliation bases captured before the change and the application.
#[allow(
    clippy::too_many_arguments,
    reason = "the strict delta writer and the ordinary folder import supply their own freshness checks beside the captured change"
)]
pub(in crate::storage::work::task_sync) fn admit_on(
    transaction: &Transaction<'_>,
    project: &ProjectId,
    profile: &ExportProfile,
    actor: &ActorContext,
    changes: &BTreeMap<WorkId, Option<TaskDocument>>,
    seeds: BTreeSet<WorkId>,
    unchanged: impl FnOnce(BTreeSet<WorkId>) -> Result<(), StoreError>,
    now: chrono::DateTime<chrono::Utc>,
    allow_partial: bool,
) -> Result<(Vec<ReconciliationWorkBasis>, folder_admission::Application), StoreError> {
    let local = overlay::capture_on(transaction, project, changes)?;
    let durable = native::capture_on(transaction, project, profile, seeds.clone())?;
    let mut native_snapshots = snapshots(project, &durable)?;
    unchanged(
        durable
            .documents
            .keys()
            .chain(&durable.missing)
            .chain(local.before.documents.keys())
            .chain(&local.before.missing)
            .copied()
            .collect(),
    )?;
    let origins = component::capture(
        transaction,
        project,
        TaskSyncGraphView::Origin,
        seeds.clone(),
    )?;
    snapshots(project, &origins)?;
    let previous = baseline::entries(transaction, project, changes.keys().copied())?;
    let current = durable
        .documents
        .iter()
        .filter(|(id, _)| {
            previous
                .get(id)
                .is_none_or(|entry| entry.document.is_some())
        })
        .map(|(id, task)| (*id, task.clone()))
        .collect();
    let project_document = ProjectDocument {
        format: task_sync::FORMAT.into(),
        project_id: project.clone(),
        notes_included: profile.include_notes,
    };
    let members = previous
        .iter()
        .filter(|(_, entry)| entry.document.is_some())
        .map(|(id, _)| *id)
        .collect();
    let plan = folder_plan::plan_selected(&project_document, &current, changes, &members)?;
    let local_tree = TaskTree {
        project: project_document.clone(),
        tasks: local.after.values().cloned().collect(),
    };
    let local_snapshots = local_tree
        .snapshots()?
        .into_iter()
        .map(|item| (item.work_id, item))
        .collect::<HashMap<_, _>>();
    for id in plan.additions().chain(plan.changed_ids()) {
        native_snapshots.insert(
            id,
            local_snapshots
                .get(&id)
                .cloned()
                .ok_or_else(|| super::invalid("added task snapshot missing"))?,
        );
    }
    let mut resolved = durable.documents.clone();
    for (id, next) in changes {
        if let Some(task) = next {
            resolved.insert(*id, task.clone());
        } else {
            resolved.remove(id);
        }
    }
    reconciliation::validate_current_heads_on(
        transaction,
        project,
        durable.documents.keys().copied(),
    )?;
    let before = if durable.documents.is_empty() {
        Vec::new()
    } else {
        reconciliation::capture_affected_on(
            transaction,
            project,
            &durable.documents.keys().copied().collect::<Vec<_>>(),
        )?
    };
    let prospective = Prospective::from_documents(
        &project_document,
        durable
            .documents
            .iter()
            .map(|(id, task)| (*id, task))
            .collect(),
        resolved.iter().map(|(id, task)| (*id, task)).collect(),
        durable.expected_roots.clone().into_iter().collect(),
        native_snapshots,
    );
    let original = OriginGraph {
        documents: origins.documents.into_values().collect(),
        notes_included: origins.notes_included,
    };
    let application = folder_admission::apply_on(
        transaction,
        &plan,
        &prospective,
        Some(&original),
        || {
            Ok(durable
                .documents
                .iter()
                .map(|(id, task)| (*id, task.clone()))
                .collect())
        },
        || {
            let after = native::capture_on(transaction, project, profile, seeds)?;
            require(
                folder_admission::dependency_cycles(
                    &after.documents.into_values().collect::<Vec<_>>(),
                )
                .is_empty(),
                "applied delta creates a durable dependency cycle",
            )
        },
        actor,
        now,
        allow_partial,
    )?;
    Ok((before, application))
}
