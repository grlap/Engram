//! One SQL writer owns the new preparation, native effects and hold resolution.

use super::{
    Binding, Connection, EVENT_KIND, Event, ObjectId, PlanningSuccessorRecipe,
    PlanningSuccessorRequest, SqliteStore, StoreError, Transition, admit_on, invalid, local_store,
    original_facts_on, original_on, producing_origin, reconciliation, require, view,
};
use crate::storage::work::planning;
use crate::storage::work::task_sync::{baseline, closure, origin};
use crate::task_sync::{
    TaskDocument, TaskSyncMembershipView,
    delta::{
        DELTA_PLANNING_KIND, DeltaBasis, DeltaCandidatePayload, DeltaImportReceipt, DeltaPlanning,
        SourceMemberRef,
    },
    ledger::CompletedExchange,
    retention::successor::RetainedSuccessor,
};
use crate::{CanonicalObject, WorkId};
use std::collections::{BTreeMap, BTreeSet};

type Changes = BTreeMap<WorkId, Option<TaskDocument>>;

fn changes(retained: &RetainedSuccessor) -> Result<Changes, StoreError> {
    retained
        .recipe
        .request
        .intent
        .candidate
        .changed
        .iter()
        .map(|id| {
            retained
                .payload
                .documents
                .get(id)
                .cloned()
                .map(|value| (*id, value))
                .ok_or_else(|| invalid("successor selected local member is missing"))
        })
        .collect()
}

fn fresh_output_on(
    connection: &Connection,
    binding: &Binding,
    retained: &RetainedSuccessor,
) -> Result<Changes, StoreError> {
    retained
        .prepared_event(binding)
        .map_err(|error| invalid(error.to_string()))?;
    let intent = &retained.recipe.request.intent;
    let DeltaBasis::Incoming {
        local_before,
        source_before,
        source_after,
        source_changed,
    } = &intent.basis
    else {
        return Err(invalid("successor incoming basis is missing"));
    };
    let head = baseline::head(connection, &binding.project)?
        .ok_or_else(|| invalid("successor local baseline is missing"))?;
    require(
        head.record == local_before.baseline
            && head.inventory_root == local_before.membership.inventory_root.hex()
            && head.include_notes == local_before.membership.profile.include_notes,
        "successor local baseline changed",
    )?;
    let selected = changes(retained)?;
    require(
        baseline::next_root(connection, &binding.project, &selected)?
            == intent.candidate.output.inventory_root.hex(),
        "successor resolved local membership differs",
    )?;
    closure::overlay::capture_on(connection, &binding.project, &selected)?;
    let mut seeds = retained
        .payload
        .documents
        .keys()
        .copied()
        .collect::<BTreeSet<_>>();
    for task in retained.payload.documents.values().flatten() {
        seeds.extend(
            closure::graph::document_edges(task)
                .into_iter()
                .map(|(_, target)| target),
        );
    }
    let source = closure::membership::capture_on(
        connection,
        &retained.binding,
        TaskSyncMembershipView::Source,
        seeds.clone(),
    )?;
    let expected = match &source_before.anchor {
        crate::task_sync::delta::IncomingSourceAnchor::Applied { outcome } => outcome,
        crate::task_sync::delta::IncomingSourceAnchor::Bootstrap { checkpoint } => checkpoint,
    };
    require(
        &source.head == expected && source.membership == source_before.snapshot.membership,
        "successor original source frontier changed",
    )?;
    let original: Event = SqliteStore::get_typed_object_on(
        connection,
        &retained.recipe.request.original_prepared,
        EVENT_KIND,
    )?
    .ok_or_else(|| invalid("successor original source is missing"))?;
    let Transition::DeltaPrepared { payload, .. } = original.transition else {
        return Err(invalid("successor original source family differs"));
    };
    let source_changes: Changes = source_changed
        .iter()
        .map(|id| {
            payload
                .documents
                .get(id)
                .cloned()
                .map(|value| (*id, value))
                .ok_or_else(|| invalid("successor original source member is missing"))
        })
        .collect::<Result<_, _>>()?;
    let mut root = source.membership.inventory_root;
    for (id, next) in &source_changes {
        if let Some(old) = source.component.documents.get(id) {
            root = root.excluding(old)?;
        } else {
            require(next.is_some(), "successor source removes an absent member")?;
        }
        if let Some(next) = next {
            root = root.including(next)?;
        }
    }
    require(
        root == source_after.membership.inventory_root,
        "successor original source membership differs",
    )?;
    closure::overlay::overlay(&binding.project, source.component, &source_changes)?;
    let native =
        closure::native::capture_on(connection, &binding.project, &binding.profile, seeds)?;
    let resolved = closure::overlay::overlay(&binding.project, native, &selected)?;
    let mut resolved_component = resolved.before;
    resolved_component.documents = resolved.after;
    super::super::scope::snapshots(&binding.project, &resolved_component)?;
    Ok(selected)
}

/// Choose a fresh construction at the binding's current settled tail,
/// allocating its identities without retaining files or changing SQL state.
fn select_on(
    connection: &Connection,
    binding_id: &ObjectId,
    request: &PlanningSuccessorRequest,
    payload: &DeltaCandidatePayload,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(Binding, RetainedSuccessor), StoreError> {
    let (binding, _) = original_facts_on(connection, binding_id, request)?;
    let (_, before) = original_on(connection, binding_id, request)?;
    reconciliation::successor::require_ended_on(connection, &before)?;
    let mut recipe = PlanningSuccessorRecipe::new(request.clone(), before, &[])?;
    let tail = view::read::tail_on(
        connection,
        binding_id,
        &binding,
        &mut view::read::ReadBudget::default(),
    )?;
    require(
        super::super::super::settled_tail(&tail.event),
        "successor cannot bypass an unresolved request",
    )?;
    recipe.bind_chain(tail.id, tail.event.sequence, now)?;
    let mut retained = RetainedSuccessor {
        binding: binding_id.clone(),
        recipe,
        payload: payload.clone(),
    };
    fresh_output_on(connection, &binding, &retained)?;
    planning::successor::select_successor_effects_on(
        connection,
        &binding.project,
        payload,
        &mut retained.recipe,
    )?;
    retained
        .prepared_event(&binding)
        .map_err(|error| invalid(error.to_string()))?;
    Ok((binding, retained))
}

impl SqliteStore {
    /// Select local construction once and retain it before the applying writer.
    /// Selection and retention share the origin reservation and one SQL writer,
    /// so no other exchange can advance the binding between them; a concurrent
    /// writer makes this refuse with nothing retained. The applying writer
    /// repeats admission inside its own transaction. A caller holding an
    /// existing retained construction reuses it instead of calling this.
    ///
    /// # Errors
    /// Refuses changed source/readsets, unresolved requests, native changes
    /// that the successor construction owner cannot represent, busy or
    /// uncertain custody, and any refusal to retain the construction file.
    pub fn retain_task_sync_planning_successor<R: crate::Redactor>(
        &mut self,
        binding_id: &ObjectId,
        request: &PlanningSuccessorRequest,
        payload: &DeltaCandidatePayload,
        now: chrono::DateTime<chrono::Utc>,
        redactor: &R,
    ) -> Result<RetainedSuccessor, StoreError> {
        crate::storage::work::feeds::inspect_work_request(
            redactor,
            &(request, payload),
            &request.intent.actor,
        )?;
        crate::storage::admit_live_actor_session(&request.intent.actor)?;
        let (binding, _) = self.work_read_snapshot(|store| {
            original_facts_on(&store.connection, binding_id, request)
        })?;
        let construction = crate::task_sync::retention::successor::construction_request(
            binding_id, request, payload,
        )
        .map_err(|error| invalid(error.to_string()))?;
        producing_origin::with_construction(
            self,
            &binding.project,
            &binding.repository.git_directory,
            request.intent.operation,
            &construction,
            |transaction| {
                let (current, retained) =
                    select_on(transaction, binding_id, request, payload, now)?;
                require(current == binding, "successor binding changed")?;
                crate::task_sync::retention::successor::retain(&current, &retained)
                    .map_err(|error| invalid(error.to_string()))?;
                Ok(retained)
            },
        )
    }

    /// Apply exactly retained local construction after normal local execution
    /// ended. No Git fetch, ref update, push or host-runtime action is performed.
    /// Completed requests replay before opening any private artifact.
    ///
    /// # Errors
    /// Refuses changed construction, live execution, stale source/local cuts,
    /// unsupported native changes or any failure of the atomic SQL application.
    pub fn apply_task_sync_planning_successor<R: crate::Redactor>(
        &mut self,
        binding: &Binding,
        retained: &RetainedSuccessor,
        redactor: &R,
    ) -> Result<CompletedExchange, StoreError> {
        if let Some(original) = self
            .completed_task_sync_planning_successor(&retained.binding, &retained.recipe.request)?
        {
            return Ok(original);
        }
        crate::storage::work::feeds::inspect_work_request(
            redactor,
            retained,
            &retained.recipe.request.intent.actor,
        )?;
        crate::storage::admit_live_actor_session(&retained.recipe.request.intent.actor)?;
        local_store(&self.connection, binding)?;
        let fingerprint = retained
            .request_fingerprint()
            .map_err(|error| invalid(error.to_string()))?;
        producing_origin::with_planning_successor(
            self,
            binding,
            retained,
            &fingerprint,
            |store, continuity| {
                let transaction = store.begin_work_mutation()?;
                require(
                    continuity.is_some(),
                    "successor requires checked private origin continuity",
                )?;
                producing_origin::check_exchange_on(
                    &transaction,
                    &binding.project,
                    continuity,
                    &fingerprint,
                )?;
                require(
                    crate::task_sync::retention::successor::read(
                        &retained.binding,
                        binding,
                        &retained.recipe.request,
                    )
                    .map_err(|error| invalid(error.to_string()))?
                    .as_ref()
                        == Some(retained),
                    "successor construction changed after retention",
                )?;
                let selected = fresh_output_on(&transaction, binding, retained)?;
                require(
                    super::super::super::view::read::binding_on(
                        &transaction,
                        &retained.binding,
                        &mut super::super::super::view::read::ReadBudget::default(),
                    )? == *binding,
                    "successor binding changed",
                )?;
                let checked = admit_on(&transaction, &retained.binding, &retained.recipe)?;
                let result = origin::with_origin_scope(&transaction, || {
                    apply_on(&checked, binding, retained, selected)
                })?;
                drop(checked);
                transaction.commit()?;
                Ok(result)
            },
        )
    }
}

fn apply_on(
    checked: &reconciliation::successor::CheckedPlanningSuccessor<'_, '_, '_>,
    binding: &Binding,
    retained: &RetainedSuccessor,
    selected: Changes,
) -> Result<CompletedExchange, StoreError> {
    let tx = checked.transaction();
    let recipe = checked.recipe();
    let intent = &recipe.request.intent;
    let now = recipe
        .chain
        .as_ref()
        .ok_or_else(|| invalid("successor envelope missing"))?
        .recorded_at;
    let previous = baseline::entries(tx, &binding.project, selected.keys().copied())?;
    let prepared = CanonicalObject::identified(
        &recipe.prepared,
        &retained
            .prepared_event(binding)
            .map_err(|error| invalid(error.to_string()))?,
    )?;
    super::super::super::operations::store_event_keyed(tx, binding, &prepared)?;
    planning::successor::apply_successor_effects_on(checked, &retained.payload)?;
    let actual = closure::native::capture_on(
        tx,
        &binding.project,
        &binding.profile,
        selected.keys().copied().collect(),
    )?;
    require(
        selected
            .iter()
            .all(|(work, document)| actual.documents.get(work) == document.as_ref()),
        "successor native effects differ from resolved planning",
    )?;
    let DeltaBasis::Incoming {
        local_before,
        source_changed,
        ..
    } = &intent.basis
    else {
        return Err(invalid("successor incoming basis missing"));
    };
    let local = baseline::TaskSyncImportPlanning {
        project_id: binding.project.clone(),
        previous: Some(local_before.baseline.clone()),
        inventory_root: baseline::next_root(tx, &binding.project, &selected)?,
        changes: selected,
        // A strict successor never exports the whole folder either.
        export_basis: None,
        profile: binding.profile.clone(),
        created_at: now,
    };
    baseline::append_object(tx, &CanonicalObject::identified(&recipe.baseline, &local)?)?;
    let original: Event =
        SqliteStore::get_typed_object_on(tx, &recipe.request.original_prepared, EVENT_KIND)?
            .ok_or_else(|| invalid("successor original source is missing"))?;
    let Transition::DeltaPrepared {
        payload: source, ..
    } = original.transition
    else {
        return Err(invalid("successor source family differs"));
    };
    let planning = DeltaPlanning {
        project_id: binding.project.clone(),
        prepared: recipe.prepared.clone(),
        local_planning: recipe.baseline.clone(),
        source_changes: source_changed
            .iter()
            .map(|work| {
                (
                    *work,
                    source.documents[work].as_ref().map(|_| SourceMemberRef {
                        record_id: recipe.request.original_prepared.clone(),
                    }),
                )
            })
            .collect(),
    };
    planning.validate(&binding.project, &recipe.prepared, intent)?;
    SqliteStore::insert_object(
        tx,
        DELTA_PLANNING_KIND,
        &CanonicalObject::identified(&recipe.planning, &planning)?,
    )?;
    let cut = producing_origin::cut_on(tx, &binding.project)?;
    let mut after = intent.local_cut.clone();
    after.position.position = cut.position;
    after.anchor = cut.anchor;
    let receipt = DeltaImportReceipt {
        planning: recipe.planning.clone(),
        tasks: super::super::receipt::successor_tasks(checked, binding, &after, now)?,
        after_cut: after,
        before: recipe.before.clone(),
        holds: vec![recipe.request.hold.clone()],
        terminal_evidence: Vec::new(),
    };
    let event = recipe.applied_event(&retained.binding, binding, receipt)?;
    let outcome = CanonicalObject::identified(&recipe.outcome, &event)?;
    super::super::super::operations::store_event_keyed(tx, binding, &outcome)?;
    tx.execute("UPDATE work_task_sync_reconciliation_holds SET resolution_id=?1 WHERE hold_id=?2 AND resolution_id IS NULL",
        rusqlite::params![recipe.outcome.as_str(), recipe.request.hold.as_str()])?;
    reconciliation::active::resolve_on(tx, &recipe.request.hold)?;
    reconciliation::advance_on(
        tx,
        &binding.project,
        intent.candidate.changed.iter().copied(),
        &previous,
    )?;
    reconciliation::validate_current_heads_on(
        tx,
        &binding.project,
        intent.candidate.changed.iter().copied(),
    )?;
    Ok(CompletedExchange {
        prepared: recipe.prepared.clone(),
        outcome: recipe.outcome.clone(),
        event,
    })
}
