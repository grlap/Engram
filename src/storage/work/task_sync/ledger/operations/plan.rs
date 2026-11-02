//! Frozen construction recipe; neither missing plans nor changed encoders remint.

use super::{
    CanonicalObject, DateTime, Event, ImportDisposition, ImportOrigin, ImportReceipt, ImportedTask,
    InspectedExchange, Ledger, ORIGIN_KIND, ObjectId, SqliteStore, StoreError, Transition, Utc,
    basis, invalid, origin_reference, query, require,
};
use crate::task_sync::ledger::{ImportPlan, PlannedImportTask};

fn encoder() -> Result<ObjectId, StoreError> {
    crate::build_identity::current()
        .build_fingerprint
        .clone()
        .ok_or_else(|| invalid("import plan encoder identity is unavailable"))
}

pub(super) fn create(
    connection: &rusqlite::Connection,
    ledger: &Ledger,
    input: &InspectedExchange,
    now: DateTime<Utc>,
) -> Result<ImportPlan, StoreError> {
    let before = basis(
        connection,
        &input.binding,
        crate::task_sync::ledger::Direction::Incoming,
    )?;
    require(
        before == input.intent.local_planning_basis,
        "local basis changed before import planning",
    )?;
    let native: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM work_items WHERE project_id = ?1 AND (json_extract(item_json, '$.restored') = 0 OR latest_event_id IS NOT NULL OR active_run_id IS NOT NULL))", [&input.binding.project.0], |row| row.get(0))?;
    require(!native, "import plan refuses native execution state")?;
    super::super::super::admit_origins(connection)?;
    let repeat = before == input.intent.candidate.planning_fingerprint;
    if !repeat {
        let count: i64 = connection.query_row(
            "SELECT count(*) FROM work_items WHERE project_id = ?1",
            [&input.binding.project.0],
            |row| row.get(0),
        )?;
        require(
            count == 0,
            "import plan refuses divergent existing planning",
        )?;
    }
    let (tail_id, tail) = ledger
        .events
        .iter()
        .filter(|(_, event)| event.binding == input.binding_id)
        .max_by_key(|(_, event)| event.sequence)
        .ok_or_else(|| invalid("exchange activation missing before plan"))?;
    let mut tasks = Vec::new();
    for (task, snapshot) in input.tree.tasks.iter().zip(input.tree.snapshots()?) {
        let (item, restored, origin) = if repeat {
            let item = query::load_work_item(connection, task.id)?;
            let origin_id = origin_reference(&item.created_by)?;
            let imported: ImportOrigin =
                SqliteStore::get_typed_object_on(connection, &origin_id, ORIGIN_KIND)?
                    .ok_or_else(|| invalid("original import origin missing"))?;
            let restored = SqliteStore::get_canonical_object_on(
                connection,
                &imported.record_id,
                "work_restored_record",
            )?
            .ok_or_else(|| invalid("original restored record missing"))?;
            let origin = SqliteStore::get_canonical_object_on(connection, &origin_id, ORIGIN_KIND)?
                .ok_or_else(|| invalid("original import origin missing"))?;
            (item, restored, origin)
        } else {
            let constructed = super::super::super::construct_task(
                &input.tree,
                task,
                snapshot,
                &input.intent.actor,
                now,
                &ObjectId::mint(),
                &ObjectId::mint(),
            )?;
            (
                constructed.item,
                constructed.object,
                constructed.origin_object,
            )
        };
        tasks.push(PlannedImportTask {
            task: task.id,
            restored_id: restored.key().clone(),
            origin_id: origin.key().clone(),
            restored_fingerprint: ObjectId::from_canonical_bytes(restored.bytes()),
            origin_fingerprint: ObjectId::from_canonical_bytes(origin.bytes()),
            item_fingerprint: CanonicalObject::freeze(&item)?.key().clone(),
        });
    }
    let encoder = encoder()?;
    let prepared = ObjectId::mint();
    let mut plan = ImportPlan {
        schema_version: 1,
        encoder: encoder.clone(),
        binding: input.binding_id.clone(),
        operation: input.intent.operation,
        request_fingerprint: CanonicalObject::freeze(&input.intent)?.key().clone(),
        prepared: prepared.clone(),
        prepared_predecessor: tail_id.clone(),
        predecessor: prepared,
        sequence: tail
            .sequence
            .checked_add(2)
            .ok_or_else(|| invalid("import plan sequence overflow"))?,
        actor: input.intent.actor.clone(),
        recorded_at: now,
        before_basis: before,
        after_basis: input.intent.candidate.planning_fingerprint.clone(),
        receipt_id: ObjectId::mint(),
        receipt_fingerprint: encoder,
        disposition: if repeat {
            ImportDisposition::ExactRepeat
        } else {
            ImportDisposition::Created
        },
        tasks,
    };
    plan.receipt_fingerprint =
        ObjectId::from_canonical_bytes(receipt_object(input, &plan)?.bytes());
    Ok(plan)
}

fn common(input: &InspectedExchange, plan: &ImportPlan) -> Result<(), StoreError> {
    require(
        plan.schema_version == 1
            && plan.encoder == encoder()?
            && plan.binding == input.binding_id
            && plan.operation == input.intent.operation
            && plan.request_fingerprint == *CanonicalObject::freeze(&input.intent)?.key()
            && plan.actor == input.intent.actor
            && plan.before_basis == input.intent.local_planning_basis
            && plan.after_basis == input.intent.candidate.planning_fingerprint
            && plan.predecessor == plan.prepared
            && plan.receipt_id != plan.prepared
            && plan.sequence >= 3
            && plan.tasks.len() == input.tree.tasks.len(),
        "import plan format, encoder, request or basis differs; reconcile without reminting",
    )?;
    let mut ids =
        std::collections::BTreeSet::from([plan.receipt_id.clone(), plan.prepared.clone()]);
    for (planned, task) in plan.tasks.iter().zip(&input.tree.tasks) {
        require(
            planned.task == task.id
                && ids.insert(planned.restored_id.clone())
                && ids.insert(planned.origin_id.clone()),
            "import plan task identities differ or collide",
        )?;
    }
    require(
        ObjectId::from_canonical_bytes(receipt_object(input, plan)?.bytes())
            == plan.receipt_fingerprint,
        "import plan cannot reproduce original receipt bytes",
    )
}

pub(super) fn new_admission(
    ledger: &Ledger,
    input: &InspectedExchange,
    plan: &ImportPlan,
) -> Result<(), StoreError> {
    common(input, plan)?;
    let (id, tail) = ledger
        .events
        .iter()
        .filter(|(_, event)| event.binding == input.binding_id)
        .max_by_key(|(_, event)| event.sequence)
        .ok_or_else(|| invalid("exchange activation missing"))?;
    require(
        id == &plan.prepared_predecessor && tail.sequence.checked_add(2) == Some(plan.sequence),
        "import plan predecessor changed before admission",
    )
}

pub(super) fn resumed(
    ledger: &Ledger,
    input: &InspectedExchange,
    plan: &ImportPlan,
) -> Result<(), StoreError> {
    common(input, plan)?;
    let prepared = ledger
        .events
        .get(&plan.prepared)
        .ok_or_else(|| invalid("import plan survives a missing prepared record; reconcile"))?;
    require(
        prepared == &prepared_object(input, plan)?.decode::<Event>()?,
        "import plan prepared bytes or predecessor changed",
    )
}

pub(super) fn prepared_object(
    input: &InspectedExchange,
    plan: &ImportPlan,
) -> Result<CanonicalObject, StoreError> {
    CanonicalObject::identified(
        &plan.prepared,
        &Event {
            schema_version: 1,
            binding: input.binding_id.clone(),
            sequence: plan.sequence - 1,
            predecessor: Some(plan.prepared_predecessor.clone()),
            actor: plan.actor.clone(),
            recorded_at: plan.recorded_at,
            transition: Transition::Prepared {
                intent: Box::new(input.intent.clone()),
            },
        },
    )
}

pub(super) fn receipt(
    input: &InspectedExchange,
    plan: &ImportPlan,
) -> Result<ImportReceipt, StoreError> {
    let tasks = plan
        .tasks
        .iter()
        .zip(&input.tree.tasks)
        .map(|(planned, task)| {
            let after = CanonicalObject::freeze(task)?.key().clone();
            Ok(ImportedTask {
                task: task.id,
                source_task_path: format!("tasks/{}.json", task.id.0),
                before_basis: if plan.disposition == ImportDisposition::ExactRepeat {
                    Some(after.clone())
                } else {
                    None
                },
                after_basis: after,
                local_record_ids: vec![planned.origin_id.clone(), planned.restored_id.clone()],
                disposition: plan.disposition,
            })
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    Ok(ImportReceipt {
        source_commit: input.intent.commit.oid.clone(),
        source_tree: input.intent.commit.tree.clone(),
        before_basis: plan.before_basis.clone(),
        after_basis: plan.after_basis.clone(),
        notes_included: input.tree.project.notes_included,
        tasks,
    })
}

pub(super) fn receipt_object(
    input: &InspectedExchange,
    plan: &ImportPlan,
) -> Result<CanonicalObject, StoreError> {
    CanonicalObject::identified(
        &plan.receipt_id,
        &Event {
            schema_version: 1,
            binding: input.binding_id.clone(),
            sequence: plan.sequence,
            predecessor: Some(plan.predecessor.clone()),
            actor: plan.actor.clone(),
            recorded_at: plan.recorded_at,
            transition: Transition::ImportApplied {
                prepared: plan.prepared.clone(),
                receipt: receipt(input, plan)?,
            },
        },
    )
}

pub(super) fn verify_effect(
    connection: &rusqlite::Connection,
    plan: &ImportPlan,
) -> Result<(), StoreError> {
    for planned in &plan.tasks {
        let item = query::load_work_item(connection, planned.task)?;
        require(
            origin_reference(&item.created_by)? == planned.origin_id,
            "planned origin identity differs",
        )?;
        let restored = SqliteStore::get_canonical_object_on(
            connection,
            &planned.restored_id,
            "work_restored_record",
        )?
        .ok_or_else(|| invalid("planned restored record missing"))?;
        let origin =
            SqliteStore::get_canonical_object_on(connection, &planned.origin_id, ORIGIN_KIND)?
                .ok_or_else(|| invalid("planned origin record missing"))?;
        require(
            ObjectId::from_canonical_bytes(restored.bytes()) == planned.restored_fingerprint
                && ObjectId::from_canonical_bytes(origin.bytes()) == planned.origin_fingerprint
                && CanonicalObject::freeze(&item)?.key() == &planned.item_fingerprint,
            "same planned identity has different canonical bytes; reconcile",
        )?;
    }
    Ok(())
}
