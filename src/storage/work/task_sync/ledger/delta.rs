//! One incoming SQL commit owns preparation, native changes and its outcome.
//! Custody admission stays with the existing origin guard; managed roots read
//! selected artifacts and audit-only roots retain exhaustive observation.

use super::super::{baseline, closure, folder, origin, producing_origin, reconciliation};
use super::{
    Binding, Event, Ledger, MAX_RECORD_BYTES, ObjectId, SqliteStore, StoreError, Transition,
    require, view,
};
use crate::{
    CanonicalObject,
    task_sync::{
        self,
        delta::{
            DELTA_PLANNING_KIND, DeltaBasis, DeltaImportReceipt, DeltaIntent, DeltaPlanning,
            SourceMemberRef,
        },
        exchange::delta::InspectedDelta,
        ledger::{CompletedExchange, Direction, EVENT_KIND},
        progress::ExportCut,
    },
};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, Transaction};
use std::collections::{BTreeMap, BTreeSet};

pub(super) mod deferred;
mod export;
mod readset;
mod receipt;
mod retain;
mod scope;
pub(super) mod successor;

fn invalid(message: impl Into<String>) -> StoreError {
    super::super::invalid(message)
}

fn preflight(input: &InspectedDelta, now: DateTime<Utc>) -> Result<(), StoreError> {
    input
        .payload
        .validate(&input.binding_id, &input.binding, &input.intent)?;
    // This envelope is at least as wide as its actual sequence/predecessor.
    // It is a byte comparison only, never a minted preparation or link.
    let envelope = Event {
        schema_version: 1,
        binding: input.binding_id.clone(),
        sequence: u64::MAX,
        predecessor: Some(input.binding_id.clone()),
        actor: input.intent.actor.clone(),
        recorded_at: now,
        transition: Transition::DeltaPrepared {
            intent: Box::new(input.intent.clone()),
            payload: Box::new(input.payload.clone()),
        },
    };
    require(
        CanonicalObject::freeze(&envelope)?.bytes().len() <= MAX_RECORD_BYTES,
        "wrapped delta preparation exceeds byte bound",
    )
}

pub(super) fn local_store(connection: &Connection, binding: &Binding) -> Result<(), StoreError> {
    super::recovery::require_scope_on(connection, binding)?;
    let actual = producing_origin::load_on(connection, &binding.project)?
        .ok_or_else(|| invalid("strict delta requires explicit origin enrollment"))?;
    require(
        binding.store.origin.as_ref() == Some(&actual.origin_id),
        "strict delta binding does not name current producing origin",
    )?;
    super::audit::restore_basis(binding)?;
    let opened = connection
        .path()
        .filter(|path| !path.is_empty())
        .ok_or_else(|| invalid("strict delta requires a file-backed store"))?;
    require(
        std::fs::canonicalize(opened).map_err(|_| invalid("cannot resolve strict delta store"))?
            == std::fs::canonicalize(&binding.store.database)
                .map_err(|_| invalid("cannot resolve bound delta store"))?,
        "strict delta store differs",
    )
}

fn fresh(connection: &Connection, input: &InspectedDelta) -> Result<(), StoreError> {
    let binding = &input.binding;
    require(
        input.intent.direction == Direction::Incoming,
        "delta writer requires an incoming request",
    )?;
    local_store(connection, binding)?;
    readset::historical_on(connection, &binding.project, &input.intent.local_cut)?;
    let DeltaBasis::Incoming { local_before, .. } = &input.intent.basis else {
        return Err(invalid("incoming delta basis missing"));
    };
    let head = baseline::head(connection, &binding.project)?
        .ok_or_else(|| invalid("strict delta requires a local baseline"))?;
    require(
        head.record == local_before.baseline
            && head.inventory_root == local_before.membership.inventory_root.hex()
            && head.include_notes == local_before.membership.profile.include_notes,
        "strict delta local baseline changed",
    )?;
    require(
        baseline::next_root(
            connection,
            &binding.project,
            &input
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
                        .ok_or_else(|| invalid("selected local payload member missing"))
                })
                .collect::<Result<BTreeMap<_, _>, _>>()?,
        )? == input.intent.candidate.output.inventory_root.hex(),
        "strict delta local output root differs",
    )
}

/// The selected binding and its current tail, read by key. A delta writer
/// holds this rather than the whole ledger, so its cost does not grow with
/// other exchanges or with the history of this one. Full replay stays with
/// doctor and repair.
pub(super) struct Chain {
    binding: Binding,
    tail: (ObjectId, Event),
}

impl Chain {
    /// Resume an existing operation at the binding's actual current tail.
    /// The canonical reducer checks whether this operation can append there;
    /// historical exact replay returns before append.
    fn resume(
        connection: &Connection,
        input: &InspectedDelta,
        prepared: &ObjectId,
    ) -> Result<Self, StoreError> {
        let mut budget = view::read::ReadBudget::default();
        let binding = view::read::binding_on(connection, &input.binding_id, &mut budget)?;
        require(
            binding == input.binding
                && view::select::successor(connection, &input.binding_id)?.is_none(),
            "delta binding changed or was superseded",
        )?;
        let tail = view::read::tail_on(connection, &input.binding_id, &binding, &mut budget)?;
        require(
            tail.operation.as_ref().is_some_and(|operation| {
                &operation.prepared == prepared
                    && operation.progress.stage != super::reduce::Stage::Settled
            }),
            "delta append requires its exact current open operation",
        )?;
        Ok(Self {
            binding,
            tail: (tail.id, tail.event),
        })
    }

    /// The unsuperseded binding the delta names, at a settled tail.
    pub(super) fn open(
        connection: &Connection,
        input: &InspectedDelta,
    ) -> Result<Self, StoreError> {
        let mut budget = view::read::ReadBudget::default();
        let binding = view::read::binding_on(connection, &input.binding_id, &mut budget)?;
        require(
            binding == input.binding
                && view::select::successor(connection, &input.binding_id)?.is_none(),
            "delta binding changed or was superseded",
        )?;
        let tail = view::read::tail_on(connection, &input.binding_id, &binding, &mut budget)?;
        require(
            super::settled_tail(&tail.event),
            "delta cannot bypass an unresolved operation",
        )?;
        Ok(Self {
            binding,
            tail: (tail.id, tail.event),
        })
    }
}

/// Appends after a keyed [`Chain`] tail, without the whole ledger.
fn append_keyed(
    connection: &Connection,
    chain: &mut Chain,
    input: &InspectedDelta,
    transition: Transition,
    now: DateTime<Utc>,
) -> Result<CanonicalObject, StoreError> {
    let (previous, tail) = &chain.tail;
    require(
        tail.binding == input.binding_id,
        "delta binding activation missing",
    )?;
    let event = Event {
        schema_version: 1,
        binding: input.binding_id.clone(),
        sequence: tail
            .sequence
            .checked_add(1)
            .ok_or_else(|| invalid("delta sequence overflow"))?,
        predecessor: Some(previous.clone()),
        actor: input.intent.actor.clone(),
        recorded_at: now,
        transition,
    };
    let object = CanonicalObject::mint(&event)?;
    let stored = super::operations::store_event_keyed(connection, &chain.binding, &object)?;
    chain.tail = (object.key().clone(), stored);
    Ok(object)
}

impl SqliteStore {
    /// Finish the private acknowledgement of an already committed incoming
    /// delta. This explicit recovery audits retained custody and canonical
    /// provenance; historical receipt lookup itself remains read-only/offline.
    /// No SQL event or task change is created. Exact repeat preserves the
    /// current witness, and another pending request is never released.
    ///
    /// # Errors
    /// Refuses an uncommitted/changed request, changed origin or restore state,
    /// missing custody, visible rewind, or a different pending private request.
    pub fn recover_task_sync_delta_import(
        &mut self,
        binding_id: &ObjectId,
        intent: &DeltaIntent,
    ) -> Result<CompletedExchange, StoreError> {
        crate::storage::admit_live_actor_session(&intent.actor)?;
        task_sync::ledger::attribution(&intent.actor)?;
        let original = self
            .completed_task_sync_delta_import(binding_id, intent)?
            .ok_or_else(|| invalid("delta recovery requires its original committed outcome"))?;
        let binding = view::read::binding_on(
            &self.connection,
            binding_id,
            &mut view::read::ReadBudget::default(),
        )?;
        local_store(&self.connection, &binding)?;
        let request = task_sync::retention::delta::request_fingerprint(binding_id, intent)
            .map_err(|error| invalid(error.to_string()))?;
        producing_origin::with_exchange_recovery(
            self,
            &binding.project,
            &binding.repository.git_directory,
            intent.operation,
            &request,
            |store, guard| {
                let transaction = store.begin_work_mutation()?;
                require(
                    guard.is_some(),
                    "delta recovery requires private origin continuity",
                )?;
                producing_origin::check_exchange_on(
                    &transaction,
                    &binding.project,
                    guard,
                    &request,
                )?;
                local_store(&transaction, &binding)?;
                require(
                    view::read::binding_on(
                        &transaction,
                        binding_id,
                        &mut view::read::ReadBudget::default(),
                    )? == binding,
                    "delta recovery binding changed",
                )?;
                require(
                    view::delta::lookup(&transaction, binding_id, intent)?.as_ref()
                        == Some(&original),
                    "delta recovery original outcome changed",
                )?;
                Ok(original)
            },
        )
    }

    pub(crate) fn apply_task_sync_delta_exchange(
        &mut self,
        input: &InspectedDelta,
        now: DateTime<Utc>,
    ) -> Result<CompletedExchange, StoreError> {
        crate::storage::admit_live_actor_session(&input.intent.actor)?;
        task_sync::ledger::attribution(&input.intent.actor)?;
        if let Some(original) =
            self.completed_task_sync_delta_import(&input.binding_id, &input.intent)?
        {
            return Ok(original);
        }
        preflight(input, now)?;
        fresh(&self.connection, input)?;
        let request =
            task_sync::retention::delta::request_fingerprint(&input.binding_id, &input.intent)
                .map_err(|error| invalid(error.to_string()))?;
        producing_origin::with_exchange(
            self,
            &input.binding.project,
            &input.binding.repository.git_directory,
            Some(input.intent.operation),
            &request,
            |store, guard| {
                let transaction = store.begin_work_mutation()?;
                require(
                    guard.is_some(),
                    "strict delta requires checked private origin continuity",
                )?;
                producing_origin::check_exchange_on(
                    &transaction,
                    &input.binding.project,
                    guard,
                    &request,
                )?;
                fresh(&transaction, input)?;
                input
                    .recheck_files()
                    .map_err(|error| invalid(error.to_string()))?;
                let mut chain = Chain::open(&transaction, input)?;
                origin::with_origin_scope(&transaction, || {
                    apply_on(&transaction, &mut chain, input, now)
                })
                .and_then(|result| {
                    transaction.commit()?;
                    Ok(result)
                })
            },
        )
    }
}

fn apply_on(
    transaction: &Transaction<'_>,
    chain: &mut Chain,
    input: &InspectedDelta,
    now: DateTime<Utc>,
) -> Result<CompletedExchange, StoreError> {
    // A strict delta never exports the whole folder, so its baseline record
    // carries no full-folder basis; its membership root binds the result.
    let previous = baseline::entries(
        transaction,
        &input.binding.project,
        input.intent.candidate.changed.iter().copied(),
    )?;
    let prepared = append_keyed(
        transaction,
        chain,
        input,
        Transition::DeltaPrepared {
            intent: Box::new(input.intent.clone()),
            payload: Box::new(input.payload.clone()),
        },
        now,
    )?;
    if let Some(deferred) = deferred::try_defer_on(transaction, chain, input, &prepared, now)? {
        return Ok(deferred);
    }
    let (changes, before) = scope::apply(transaction, input, now)?;
    let DeltaBasis::Incoming {
        local_before,
        source_changed,
        ..
    } = &input.intent.basis
    else {
        return Err(invalid("incoming delta basis missing"));
    };
    let local = baseline::append(
        transaction,
        &input.binding.project,
        Some(local_before.baseline.clone()),
        changes,
        None,
        &input.binding.profile,
        now,
    )?;
    reconciliation::advance_on(
        transaction,
        &input.binding.project,
        input.intent.candidate.changed.iter().copied(),
        &previous,
    )?;
    let planning = DeltaPlanning {
        project_id: input.binding.project.clone(),
        prepared: prepared.key().clone(),
        local_planning: local,
        source_changes: source_changed
            .iter()
            .map(|id| {
                (
                    *id,
                    input.payload.documents[id]
                        .as_ref()
                        .map(|_| SourceMemberRef {
                            record_id: prepared.key().clone(),
                        }),
                )
            })
            .collect(),
    };
    planning.validate(&input.binding.project, prepared.key(), &input.intent)?;
    let planning = CanonicalObject::mint(&planning)?;
    SqliteStore::insert_object(transaction, DELTA_PLANNING_KIND, &planning)?;
    let cut = producing_origin::cut_on(transaction, &input.binding.project)?;
    let mut after = input.intent.local_cut.clone();
    after.position.position = cut.position;
    after.anchor = cut.anchor;
    let receipt = DeltaImportReceipt {
        planning: planning.key().clone(),
        tasks: receipt::tasks(
            transaction,
            &input.binding,
            &input.intent,
            &before,
            &after,
            now,
        )?,
        after_cut: after,
        before,
        holds: Vec::new(),
        terminal_evidence: Vec::new(),
    };
    let outcome = append_keyed(
        transaction,
        chain,
        input,
        Transition::DeltaImportApplied {
            prepared: prepared.key().clone(),
            receipt: Box::new(receipt),
        },
        now,
    )?;
    Ok(CompletedExchange {
        prepared: prepared.key().clone(),
        outcome: outcome.key().clone(),
        event: outcome.decode()?,
    })
}

pub(super) fn validate_prepared_on(
    connection: &Connection,
    binding: &Binding,
    event: &Event,
    intent: &DeltaIntent,
) -> Result<(), StoreError> {
    producing_origin::check_cut_on(
        connection,
        &binding.project,
        &ExportCut {
            position: intent.local_cut.position.position,
            anchor: intent.local_cut.anchor.clone(),
        },
    )?;
    if let DeltaBasis::Outgoing {
        export_before,
        confirmed_before,
    } = &intent.basis
    {
        let anchor = match &export_before.anchor {
            task_sync::delta::PreparedExportAnchor::Bootstrap { checkpoint } => checkpoint,
            task_sync::delta::PreparedExportAnchor::Prepared { event } => event,
        };
        let head = closure::membership::head::read_on(
            connection,
            anchor,
            &event.binding,
            binding,
            task_sync::TaskSyncMembershipView::Export,
        )?;
        require(
            head.basis == closure::membership::head::MembershipBasis::Export(export_before.clone()),
            "canonical outgoing export predecessor differs",
        )?;
        if let Some(confirmed) = confirmed_before {
            require(
                export::canonical_confirmed_on(connection, &event.binding, binding, confirmed)?
                    < event.sequence,
                "canonical publication basis does not precede preparation",
            )?;
        }
        return Ok(());
    }
    let DeltaBasis::Incoming {
        source_before,
        local_before,
        ..
    } = &intent.basis
    else {
        return Err(invalid("incoming delta basis missing"));
    };
    let source = match &source_before.anchor {
        task_sync::delta::IncomingSourceAnchor::Bootstrap { checkpoint } => checkpoint,
        task_sync::delta::IncomingSourceAnchor::Applied { outcome } => outcome,
    };
    let head = closure::membership::head::read_on(
        connection,
        source,
        &event.binding,
        binding,
        task_sync::TaskSyncMembershipView::Source,
    )?;
    require(
        head.basis == closure::membership::head::MembershipBasis::Source(source_before.clone()),
        "canonical delta source predecessor differs",
    )?;
    let local = folder::retained_planning_on(connection, &local_before.baseline, &binding.project)?;
    require(
        local.inventory_root == local_before.membership.inventory_root.hex()
            && local.profile == local_before.membership.profile
            && local.profile == binding.profile,
        "canonical delta destination predecessor differs",
    )
}

pub(super) fn validate_receipt_on(
    connection: &Connection,
    id: &ObjectId,
    event: &Event,
    binding: &Binding,
    intent: &DeltaIntent,
    result: &DeltaImportReceipt,
) -> Result<(), StoreError> {
    let Transition::DeltaImportApplied { prepared, .. } = &event.transition else {
        return Err(invalid("delta receipt event kind differs"));
    };
    let preparation: Event = view::read::object(
        connection,
        prepared,
        EVENT_KIND,
        &mut view::read::ReadBudget::default(),
    )?;
    let Transition::DeltaPrepared {
        intent: original,
        payload,
    } = &preparation.transition
    else {
        return Err(invalid("delta receipt preparation missing"));
    };
    require(
        original.as_ref() == intent && preparation.binding == event.binding,
        "delta receipt request differs",
    )?;
    closure::source::validate_delta_event_on(connection, id, event, binding)?;
    let planning: DeltaPlanning = view::read::object(
        connection,
        &result.planning,
        DELTA_PLANNING_KIND,
        &mut view::read::ReadBudget::default(),
    )?;
    let local =
        folder::retained_planning_on(connection, &planning.local_planning, &binding.project)?;
    require(
        local.created_at == event.recorded_at
            && result.after_cut.anchor.as_ref().is_some_and(|anchor| {
                anchor.object_id == planning.local_planning
                    && anchor.object_kind == folder::IMPORT_PLANNING_KIND
            }),
        "delta receipt final planning cut or time differs",
    )?;
    producing_origin::check_cut_on(
        connection,
        &binding.project,
        &ExportCut {
            position: result.after_cut.position.position,
            anchor: result.after_cut.anchor.clone(),
        },
    )?;
    receipt::validate(connection, binding, intent, payload, event, result)
}

pub(super) fn validate_planning_owners_on(
    connection: &Connection,
    ledger: &Ledger,
) -> Result<(), StoreError> {
    let mut used = BTreeSet::new();
    for event in ledger.events.values() {
        if let Transition::DeltaImportApplied { receipt, .. }
        | Transition::PlanningSuccessorApplied { receipt, .. } = &event.transition
        {
            require(
                used.insert(receipt.planning.clone()),
                "delta planning is shared by distinct outcomes",
            )?;
        }
    }
    let mut query = connection.prepare("SELECT object_id FROM objects WHERE object_kind=?1")?;
    for id in query.query_map([DELTA_PLANNING_KIND], |row| row.get::<_, String>(0))? {
        let id =
            ObjectId::from_stored(id?).ok_or_else(|| invalid("invalid delta planning identity"))?;
        require(
            used.remove(&id),
            "delta planning has no exact canonical outcome",
        )?;
    }
    require(used.is_empty(), "delta receipt planning is missing")
}
