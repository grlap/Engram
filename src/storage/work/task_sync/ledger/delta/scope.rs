//! Fresh connected source, local membership, native and immutable-origin views.

use super::super::super::{
    baseline,
    closure::{component, graph, membership, native, overlay},
    folder_admission::{self, OriginGraph, Prospective},
    reconciliation,
};
use crate::task_sync::exchange::delta::InspectedDelta;
use crate::{
    WorkGraphSnapshotItem, WorkId,
    storage::StoreError,
    task_sync::{
        self, ProjectDocument, TaskDocument, TaskGraphComponent, TaskSyncGraphView,
        TaskSyncMembershipView, TaskTree, delta::DeltaBasis, folder_plan, ledger::require,
    },
};
use rusqlite::Transaction;
use std::collections::{BTreeMap, BTreeSet, HashMap};

type AppliedScope = (
    BTreeMap<WorkId, Option<TaskDocument>>,
    Vec<crate::task_sync::reconciliation::ReconciliationWorkBasis>,
);

pub(super) fn snapshots(
    project: &crate::ProjectId,
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

pub(super) fn apply(
    transaction: &Transaction<'_>,
    input: &InspectedDelta,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<AppliedScope, StoreError> {
    let DeltaBasis::Incoming {
        source_before,
        source_after,
        source_changed,
        ..
    } = &input.intent.basis
    else {
        return Err(super::super::super::invalid("incoming delta basis missing"));
    };
    let changes = input
        .intent
        .candidate
        .changed
        .iter()
        .map(|id| {
            input
                .payload
                .documents
                .get(id)
                .cloned()
                .map(|task| (*id, task))
                .ok_or_else(|| super::super::super::invalid("selected local delta member missing"))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    let mut seeds = input
        .payload
        .documents
        .keys()
        .copied()
        .collect::<BTreeSet<_>>();
    for task in input.payload.documents.values().flatten() {
        seeds.extend(
            graph::document_edges(task)
                .into_iter()
                .map(|(_, target)| target),
        );
    }
    let source = membership::capture_on(
        transaction,
        &input.binding_id,
        TaskSyncMembershipView::Source,
        seeds.clone(),
    )?;
    let expected = match &source_before.anchor {
        task_sync::delta::IncomingSourceAnchor::Applied { outcome } => outcome,
        task_sync::delta::IncomingSourceAnchor::Bootstrap { checkpoint } => checkpoint,
    };
    require(
        &source.head == expected && source.membership == source_before.snapshot.membership,
        "incoming source head or membership changed before delta application",
    )?;
    let source_changes = source_changed
        .iter()
        .map(|id| {
            input
                .payload
                .documents
                .get(id)
                .cloned()
                .map(|task| (*id, task))
                .ok_or_else(|| super::super::super::invalid("selected source delta member missing"))
        })
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    for (id, task) in &input.payload.documents {
        if !source_changed.contains(id) {
            require(
                task.as_ref() == source.component.documents.get(id),
                "local-only delta member differs from unchanged incoming source",
            )?;
        }
    }
    let mut root = source.membership.inventory_root;
    for (id, next) in &source_changes {
        if let Some(old) = source.component.documents.get(id) {
            root = root.excluding(old)?;
        } else {
            require(next.is_some(), "source delta removes an absent member")?;
        }
        if let Some(next) = next {
            root = root.including(next)?;
        }
    }
    require(
        root == source_after.membership.inventory_root,
        "incoming source delta membership differs",
    )?;
    overlay::overlay(&input.binding.project, source.component, &source_changes)?;
    let local = overlay::capture_on(transaction, &input.binding.project, &changes)?;
    let durable = native::capture_on(
        transaction,
        &input.binding.project,
        &input.binding.profile,
        seeds.clone(),
    )?;
    let mut native_snapshots = snapshots(&input.binding.project, &durable)?;
    super::readset::unchanged_on(
        transaction,
        &input.binding.project,
        &input.intent.local_cut,
        durable
            .documents
            .keys()
            .chain(&durable.missing)
            .chain(local.before.documents.keys())
            .chain(&local.before.missing)
            .copied(),
    )?;
    let origins = component::capture(
        transaction,
        &input.binding.project,
        TaskSyncGraphView::Origin,
        seeds.clone(),
    )?;
    snapshots(&input.binding.project, &origins)?;
    let previous = baseline::entries(transaction, &input.binding.project, changes.keys().copied())?;
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
    let project = ProjectDocument {
        format: task_sync::FORMAT.into(),
        project_id: input.binding.project.clone(),
        notes_included: input.binding.profile.include_notes,
    };
    let members = previous
        .iter()
        .filter(|(_, entry)| entry.document.is_some())
        .map(|(id, _)| *id)
        .collect();
    let plan = folder_plan::plan_selected(&project, &current, &changes, &members)?;
    let local_tree = TaskTree {
        project: project.clone(),
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
                .ok_or_else(|| super::super::super::invalid("added task snapshot missing"))?,
        );
    }
    let mut resolved = durable.documents.clone();
    for (id, next) in &changes {
        if let Some(task) = next {
            resolved.insert(*id, task.clone());
        } else {
            resolved.remove(id);
        }
    }
    reconciliation::validate_current_heads_on(
        transaction,
        &input.binding.project,
        durable.documents.keys().copied(),
    )?;
    let before = if durable.documents.is_empty() {
        Vec::new()
    } else {
        reconciliation::capture_affected_on(
            transaction,
            &input.binding.project,
            &durable.documents.keys().copied().collect::<Vec<_>>(),
        )?
    };
    let prospective = Prospective::from_documents(
        &project,
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
    folder_admission::apply_on(
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
            let after = native::capture_on(
                transaction,
                &input.binding.project,
                &input.binding.profile,
                seeds,
            )?;
            require(
                folder_admission::dependency_cycles(
                    &after.documents.into_values().collect::<Vec<_>>(),
                )
                .is_empty(),
                "applied delta creates a durable dependency cycle",
            )
        },
        &input.intent.actor,
        now,
        false,
    )?;
    Ok((changes, before))
}
