//! Exact immutable receipt provenance, independent of later local movement.

use super::super::super::{
    IMPORT_TOOL, ImportOrigin, ORIGIN_KIND, origin_reference, query, reconciliation,
};
use crate::{
    ObjectId, WorkId,
    storage::{SqliteStore, StoreError},
    task_sync::{
        delta::{DeltaImportReceipt, DeltaImportedTask, DeltaIntent, OriginQualifiedCut},
        ledger::{Binding, Event, require},
        reconciliation::ReconciliationWorkBasis,
    },
};
use rusqlite::{Connection, params};
use std::collections::{BTreeMap, BTreeSet};

fn records(
    connection: &Connection,
    binding: &Binding,
    intent: &DeltaIntent,
    before: &[ReconciliationWorkBasis],
    after: &OriginQualifiedCut,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<BTreeMap<WorkId, Vec<ObjectId>>, StoreError> {
    let old = before
        .iter()
        .map(|basis| (basis.work_id, basis))
        .collect::<BTreeMap<_, _>>();
    let mut found: BTreeMap<WorkId, Vec<ObjectId>> = BTreeMap::new();
    let mut revisions: BTreeMap<WorkId, i64> = BTreeMap::new();
    // A historical cut may precede unrelated work. Select only this import's
    // task histories; their attribution, time and contiguous revisions remain
    // checked below. Disconnected events never become claimed import effects.
    let mut statement=connection.prepare("SELECT object_id FROM work_feed_entries INDEXED BY work_feed_entries_work_event_item WHERE feed_kind='project' AND feed_id=?1 AND position>?2 AND position<=?3 AND object_kind='work_event' AND work_id=?4 AND work_id IS NOT NULL ORDER BY position")?;
    for work in &intent.candidate.changed {
        for id in statement.query_map(
            params![
                binding.project.0,
                intent.local_cut.position.position,
                after.position.position,
                work.0.to_string()
            ],
            |row| row.get::<_, String>(0),
        )? {
            let id = ObjectId::from_stored(id?).ok_or_else(|| {
                super::super::super::invalid("invalid delta local record identity")
            })?;
            let event: crate::WorkEvent =
                SqliteStore::get_typed_object_on(connection, &id, "work_event")?.ok_or_else(
                    || super::super::super::invalid("delta local work event missing"),
                )?;
            require(
                event.project_id == binding.project
                    && event.actor == intent.actor
                    && event.created_at == now
                    && event.work_id == *work,
                "delta local work record scope, attribution or time differs",
            )?;
            let basis = old.get(&event.work_id).ok_or_else(|| {
                super::super::super::invalid("delta work event has no prior basis")
            })?;
            let previous = revisions
                .get(&event.work_id)
                .copied()
                .unwrap_or(basis.revision);
            require(
                previous.checked_add(1) == Some(event.revision),
                "delta local work revisions are not contiguous",
            )?;
            revisions.insert(event.work_id, event.revision);
            found.entry(event.work_id).or_default().push(id);
        }
    }
    for &work in &intent.candidate.changed {
        if !old.contains_key(&work) {
            let item = query::load_work_item(connection, work)?;
            require(
                item.project_id == binding.project
                    && item.created_by.source_tool.as_deref() == Some(IMPORT_TOOL),
                "new delta task has no original import provenance",
            )?;
            let id = origin_reference(&item.created_by)?;
            let origin: ImportOrigin =
                SqliteStore::get_typed_object_on(connection, &id, ORIGIN_KIND)?
                    .ok_or_else(|| super::super::super::invalid("new delta origin missing"))?;
            require(
                origin.project_id == binding.project
                    && origin.task.id == work
                    && origin.actor == super::super::super::import_actor(&intent.actor)
                    && origin.imported_at == now,
                "new delta origin attribution or subject differs",
            )?;
            super::super::super::origin::validate_record(connection, &origin)?;
            found.insert(work, vec![id, origin.record_id]);
        }
    }
    Ok(found)
}

pub(super) fn tasks(
    connection: &Connection,
    binding: &Binding,
    intent: &DeltaIntent,
    before: &[ReconciliationWorkBasis],
    after: &OriginQualifiedCut,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<DeltaImportedTask>, StoreError> {
    reconciliation::validate_current_heads_on(
        connection,
        &binding.project,
        intent.candidate.changed.iter().copied(),
    )?;
    task_observations(connection, binding, intent, before, after, now)
}

/// The new baseline is already appended, but the held imported heads advance
/// only after the canonical successor outcome resolves the hold. Derive their
/// prospective values through the same native owner without erasing the hold.
pub(super) fn successor_tasks(
    checked: &reconciliation::successor::CheckedPlanningSuccessor<'_, '_, '_>,
    binding: &Binding,
    after: &OriginQualifiedCut,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<DeltaImportedTask>, StoreError> {
    crate::task_sync::ledger::require(
        checked.project() == &binding.project,
        "successor receipt project differs from checked hold",
    )?;
    let recipe = checked.recipe();
    task_observations(
        checked.transaction(),
        binding,
        &recipe.request.intent,
        &recipe.before,
        after,
        now,
    )
}

fn task_observations(
    connection: &Connection,
    binding: &Binding,
    intent: &DeltaIntent,
    before: &[ReconciliationWorkBasis],
    after: &OriginQualifiedCut,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Vec<DeltaImportedTask>, StoreError> {
    let mut records = records(connection, binding, intent, before, after, now)?;
    let old = before
        .iter()
        .map(|basis| (basis.work_id, basis))
        .collect::<BTreeMap<_, _>>();
    intent
        .candidate
        .changed
        .iter()
        .map(|&work| {
            Ok(DeltaImportedTask {
                task: work,
                previous_head: old.get(&work).and_then(|basis| basis.imported_head.clone()),
                imported_head: reconciliation::head_for_imported_task_on(
                    connection,
                    &binding.project,
                    work,
                )?,
                local_record_ids: records.remove(&work).unwrap_or_default(),
            })
        })
        .collect()
}

pub(super) fn validate(
    connection: &Connection,
    binding: &Binding,
    intent: &DeltaIntent,
    payload: &crate::task_sync::delta::DeltaCandidatePayload,
    event: &Event,
    receipt: &DeltaImportReceipt,
) -> Result<(), StoreError> {
    require(
        receipt.holds.is_empty() && receipt.terminal_evidence.is_empty(),
        "strict delta writer cannot substitute for execution reconciliation",
    )?;
    validate_planning_effects(connection, binding, intent, payload, event, receipt)
}

/// Shared native planning provenance for ordinary deltas and explicit
/// successors; the typed owner separately validates its exact hold resolution.
pub(super) fn validate_planning_effects(
    connection: &Connection,
    binding: &Binding,
    intent: &DeltaIntent,
    payload: &crate::task_sync::delta::DeltaCandidatePayload,
    event: &Event,
    receipt: &DeltaImportReceipt,
) -> Result<(), StoreError> {
    require(
        receipt
            .tasks
            .iter()
            .map(|task| task.task)
            .collect::<BTreeSet<_>>()
            == intent.candidate.changed,
        "delta receipt local task selection differs",
    )?;
    for basis in &receipt.before {
        reconciliation::validate_work_basis_links(connection, &binding.project, basis)?;
    }
    let old = receipt
        .before
        .iter()
        .map(|basis| (basis.work_id, basis))
        .collect::<BTreeMap<_, _>>();
    let mut expected = records(
        connection,
        binding,
        intent,
        &receipt.before,
        &receipt.after_cut,
        event.recorded_at,
    )?;
    for task in &receipt.tasks {
        require(
            task.previous_head
                == old
                    .get(&task.task)
                    .and_then(|basis| basis.imported_head.clone())
                && task.local_record_ids == expected.remove(&task.task).unwrap_or_default(),
            "delta receipt task predecessor or local record set differs",
        )?;
        if let Some(head) = &task.imported_head {
            // Resolves immutable original, baseline/tombstone and existing
            // shared-version owners; never invents a version for a deletion.
            let document = reconciliation::validate_imported_head_on(
                connection,
                head,
                &binding.project,
                task.task,
            )?;
            require(
                payload.documents.get(&task.task) == Some(&document),
                "delta imported head differs from its exact prepared member",
            )?;
        } else {
            let item = query::load_work_item(connection, task.task)?;
            require(
                item.created_by.source_tool.as_deref() != Some(IMPORT_TOOL)
                    && task.previous_head.is_none(),
                "delta receipt drops an imported task head",
            )?;
        }
    }
    Ok(())
}
