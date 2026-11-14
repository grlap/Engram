//! Fresh connected source, local membership, native and immutable-origin views.

use super::super::super::closure::{graph, membership, overlay};
use crate::task_sync::exchange::delta::InspectedDelta;
use crate::{
    WorkId,
    storage::StoreError,
    task_sync::{self, TaskDocument, TaskSyncMembershipView, delta::DeltaBasis, ledger::require},
};
use rusqlite::Transaction;
use std::collections::{BTreeMap, BTreeSet};

type AppliedScope = (
    BTreeMap<WorkId, Option<TaskDocument>>,
    Vec<crate::task_sync::reconciliation::ReconciliationWorkBasis>,
);

pub(super) use super::super::super::selected::snapshots;

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
    let (before, _) = super::super::super::selected::admit_on(
        transaction,
        &input.binding.project,
        &input.binding.profile,
        &input.intent.actor,
        &changes,
        seeds,
        |read| {
            super::readset::unchanged_on(
                transaction,
                &input.binding.project,
                &input.intent.local_cut,
                read,
            )
        },
        now,
        false,
    )?;
    Ok((changes, before))
}
