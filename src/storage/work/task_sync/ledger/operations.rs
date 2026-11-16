//! One outer writer covers task import, immutable provenance and its receipt.

use super::super::{
    ImportOrigin, ORIGIN_KIND, import_task_tree_on, invalid, origin_reference, query,
};
use super::{EVENT_KIND, Ledger};
use crate::{
    CanonicalObject, ObjectId,
    storage::{SqliteStore, StoreError},
    task_sync::{
        exchange::InspectedExchange,
        ledger::{
            Binding, Direction, Event, ImportDisposition, ImportReceipt, ImportedTask, Transition,
            require,
        },
        retention,
    },
};
use chrono::{DateTime, Utc};

mod outgoing;
mod plan;

pub(super) fn verify_historical_plan(
    connection: &rusqlite::Connection,
    ledger: &Ledger,
    input: &InspectedExchange,
    retained: &crate::task_sync::ledger::ImportPlan,
) -> Result<(), StoreError> {
    plan::historical(connection, ledger, input, retained)
}

impl SqliteStore {
    /// Record a previously inspected retained request under one serialized writer.
    /// Same-key retry compares the complete attributed request, never only its key.
    ///
    /// # Errors
    /// Refuses changed bindings/bases, unknown retained operations and unresolved work.
    pub(crate) fn prepare_task_sync_exchange(
        &mut self,
        input: &InspectedExchange,
        now: DateTime<Utc>,
    ) -> Result<ObjectId, StoreError> {
        admit(input, &self.connection)?;
        super::recovery::require_scope_on(&self.connection, &input.binding)?;
        let request = super::origin_request(
            "prepare",
            &input.binding,
            Some(&input.binding_id),
            Some(&input.intent),
            None,
        )?;
        super::super::producing_origin::with_exchange(
            self,
            &input.binding.project,
            &input.binding.repository.git_directory,
            Some(input.intent.operation),
            &request,
            |store, guard| store.prepare_task_sync_exchange_checked(input, now, guard, &request),
        )
    }

    fn prepare_task_sync_exchange_checked(
        &mut self,
        input: &InspectedExchange,
        now: DateTime<Utc>,
        guard: Option<&crate::task_sync::origin::continuity::PreparedContinuity>,
        request: &ObjectId,
    ) -> Result<ObjectId, StoreError> {
        admit(input, &self.connection)?;
        // This explicit full-folder operation retains its exhaustive contract.
        let observed =
            super::custody::Observed::capture_cold(&input.binding.repository.git_directory)?;
        let transaction = self.begin_work_mutation()?;
        super::recovery::require_scope_on(&transaction, &input.binding)?;
        super::super::producing_origin::check_exchange_on(
            &transaction,
            &input.binding.project,
            guard,
            request,
        )?;
        if guard.is_some() {
            super::super::producing_origin::check_cut_on(
                &transaction,
                &input.binding.project,
                &input.intent.candidate.source_cut,
            )?;
        }
        let mut ledger = Ledger::load(&transaction)?;
        active(&ledger, input)?;
        external(&ledger, input, &observed)?;
        input
            .recheck_files()
            .map_err(|error| invalid(error.to_string()))?;
        if let Some((id, old)) = ledger.events.iter().find_map(|(id, event)| {
            if event.binding == input.binding_id
                && let Transition::Prepared { intent } = &event.transition
                && intent.key == input.intent.key
            {
                return Some((id, intent));
            }
            None
        }) {
            require(
                old.as_ref() == &input.intent,
                "exchange request key was reused with different content or attribution",
            )?;
            let applied = applied(&ledger, id);
            let expected = applied.map_or(&input.intent.local_planning_basis, |(_, receipt)| {
                &receipt.after_basis
            });
            require(
                &basis(&transaction, &input.binding, input.intent.direction)? == expected,
                "local planning basis changed; reconcile retained exchange",
            )?;
            // Exact-repeat validation still checks origin/history/native state.
            if applied.is_some() && input.intent.direction == Direction::Incoming {
                let result = import_task_tree_on(
                    &transaction,
                    &input.binding.project,
                    &input.intent.actor,
                    &input.tree,
                    true,
                    now,
                )?;
                require(!result.changed, "recorded receipt has no exact local graph")?;
            } else if input.intent.direction == Direction::Incoming {
                let retained = retention::read_import_plan(
                    &input.binding.repository.git_directory,
                    input.intent.operation,
                )
                .map_err(|error| invalid(error.to_string()))?
                .ok_or_else(|| {
                    invalid("prepared import plan is missing; reconcile without reminting")
                })?;
                plan::resumed(&ledger, input, &retained)?;
            }
            return Ok(id.clone());
        }
        require(
            basis(&transaction, &input.binding, input.intent.direction)?
                == input.intent.local_planning_basis,
            "local planning basis changed before exchange admission",
        )?;
        if input.intent.direction == Direction::Outgoing {
            require(
                input.intent.candidate.planning_fingerprint == input.intent.local_planning_basis,
                "outgoing candidate differs from the admitted local planning basis",
            )?;
        }
        let object = if input.intent.direction == Direction::Incoming {
            let tail = ledger
                .events
                .values()
                .filter(|event| event.binding == input.binding_id)
                .max_by_key(|event| event.sequence)
                .ok_or_else(|| invalid("exchange activation missing"))?;
            // The common settlement contract, shared with evidence, delta and
            // origin admission: any settled family outcome admits a new import.
            require(
                super::settled_tail(tail),
                "new exchange bypasses unresolved operation",
            )?;
            let proposed = plan::create(&transaction, &ledger, input, now)?;
            let retained =
                retention::publish_import_plan(&input.binding.repository.git_directory, &proposed)
                    .map_err(|error| invalid(error.to_string()))?;
            plan::new_admission(&ledger, input, &retained)?;
            let object = plan::prepared_object(input, &retained)?;
            store_event(&transaction, &mut ledger, &object)?;
            object
        } else {
            append(
                &transaction,
                &mut ledger,
                input,
                Transition::Prepared {
                    intent: Box::new(input.intent.clone()),
                },
                now,
            )?
        };
        transaction.commit()?;
        Ok(object.key().clone())
    }

    /// Apply the inspected initial graph and record its exact source receipt in
    /// the same transaction. A dry run rolls back every staged graph/receipt row.
    ///
    /// # Errors
    /// Refuses missing/changed intent, local drift and native/divergent imports.
    pub(crate) fn apply_task_sync_exchange(
        &mut self,
        input: &InspectedExchange,
        dry_run: bool,
        now: DateTime<Utc>,
    ) -> Result<ImportReceipt, StoreError> {
        admit(input, &self.connection)?;
        require(
            input.intent.direction == Direction::Incoming,
            "exchange is not an import",
        )?;
        if dry_run {
            let enrolled =
                super::super::producing_origin::load_on(&self.connection, &input.binding.project)?
                    .is_some();
            let guard = if enrolled {
                Some(self.prepare_task_sync_origin(&input.binding.project)?)
            } else {
                None
            };
            if let Some(guard) = &guard {
                guard.recheck()?;
            }
            return self.apply_task_sync_exchange_checked(input, true, now, guard.as_ref(), None);
        }
        let request = super::origin_request(
            "apply",
            &input.binding,
            Some(&input.binding_id),
            Some(&input.intent),
            None,
        )?;
        super::super::producing_origin::with_exchange(
            self,
            &input.binding.project,
            &input.binding.repository.git_directory,
            Some(input.intent.operation),
            &request,
            |store, guard| {
                store.apply_task_sync_exchange_checked(input, false, now, guard, Some(&request))
            },
        )
    }

    fn apply_task_sync_exchange_checked(
        &mut self,
        input: &InspectedExchange,
        dry_run: bool,
        now: DateTime<Utc>,
        guard: Option<&crate::task_sync::origin::continuity::PreparedContinuity>,
        request: Option<&ObjectId>,
    ) -> Result<ImportReceipt, StoreError> {
        admit(input, &self.connection)?;
        require(
            input.intent.direction == Direction::Incoming,
            "exchange is not an import",
        )?;
        let observed =
            super::custody::Observed::capture_cold(&input.binding.repository.git_directory)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(request) = request {
            super::super::producing_origin::check_exchange_on(
                &transaction,
                &input.binding.project,
                guard,
                request,
            )?;
        } else if let Some(guard) = guard {
            let checked = super::super::producing_origin::check_on(
                &transaction,
                &input.binding.project,
                guard,
            )?;
            checked.require_on(&transaction)?;
        } else if super::super::producing_origin::load_on(&transaction, &input.binding.project)?
            .is_some()
        {
            return Err(invalid("origin enrollment changed before dry-run writer"));
        }
        let mut ledger = Ledger::load(&transaction)?;
        let (_, receipt) = apply_on(&transaction, &mut ledger, input, dry_run, now, &observed)?;
        if !dry_run {
            transaction.commit()?;
        }
        Ok(receipt)
    }
}

fn admit(input: &InspectedExchange, connection: &rusqlite::Connection) -> Result<(), StoreError> {
    require(
        super::super::producing_origin::load_on(connection, &input.binding.project)?
            .as_ref()
            .map(|origin| &origin.origin_id)
            == input.binding.store.origin.as_ref(),
        "exchange binding does not name the current producing origin",
    )?;
    input.binding.validate()?;
    input.intent.validate()?;
    super::super::validate_task_import(
        &input.binding.project,
        &input.intent.actor,
        &input.tree,
        false,
    )?;
    let opened = connection
        .path()
        .filter(|path| !path.is_empty())
        .ok_or_else(|| invalid("exchange requires a file-backed store"))?;
    require(
        std::fs::canonicalize(opened).map_err(|_| invalid("cannot resolve exchange store"))?
            == std::fs::canonicalize(&input.binding.store.database)
                .map_err(|_| invalid("cannot resolve bound store"))?,
        "exchange store path differs",
    )
}

fn active(ledger: &Ledger, input: &InspectedExchange) -> Result<(), StoreError> {
    require(
        ledger.bindings.get(&input.binding_id) == Some(&input.binding),
        "exchange binding or restore basis changed",
    )?;
    require(
        !ledger
            .bindings
            .values()
            .any(|binding| binding.supersedes.as_ref() == Some(&input.binding_id)),
        "exchange binding was superseded",
    )
}

fn external(
    ledger: &Ledger,
    input: &InspectedExchange,
    observed: &super::custody::Observed,
) -> Result<(), StoreError> {
    let root = &input.binding.repository.git_directory;
    super::custody::check(
        ledger,
        root,
        observed,
        Some(input.intent.operation),
        super::same_directory,
    )
}

/// The local planning basis an exchange compares. An outgoing operation
/// publishes what preparation exported, so it uses that same profile export,
/// native notes and activity included. An incoming import compares in
/// import-comparison mode, which reads imported provenance as the importer
/// will reproduce it.
fn basis(
    connection: &rusqlite::Connection,
    binding: &Binding,
    direction: Direction,
) -> Result<ObjectId, StoreError> {
    let tree = match direction {
        Direction::Outgoing => {
            super::super::export_on_with_profile(connection, &binding.project, &binding.profile)?
                .tree
        }
        Direction::Incoming => super::super::export_on_snapshot(
            connection,
            &binding.project,
            binding.profile.include_notes,
            binding.profile.origin_machine.as_deref(),
            true,
            &mut crate::task_sync::notes::NoteOmissions::default(),
            &mut crate::task_sync::notes::NoteOmissions::default(),
        )?,
    };
    let bytes = crate::task_sync::progress::documents_fingerprint(&tree.documents()?);
    ObjectId::from_stored(bytes).ok_or_else(|| invalid("invalid local planning comparison"))
}

fn applied<'a>(
    ledger: &'a Ledger,
    prepared: &ObjectId,
) -> Option<(&'a ObjectId, &'a ImportReceipt)> {
    ledger
        .events
        .iter()
        .find_map(|(id, event)| match &event.transition {
            Transition::ImportApplied {
                prepared: selected,
                receipt,
            } if selected == prepared => Some((id, receipt)),
            _ => None,
        })
}

fn append(
    connection: &rusqlite::Connection,
    ledger: &mut Ledger,
    input: &InspectedExchange,
    transition: Transition,
    now: DateTime<Utc>,
) -> Result<CanonicalObject, StoreError> {
    let (previous, tail) = ledger
        .events
        .iter()
        .filter(|(_, event)| event.binding == input.binding_id)
        .max_by_key(|(_, event)| event.sequence)
        .ok_or_else(|| invalid("exchange activation missing"))?;
    let event = Event {
        schema_version: 1,
        binding: input.binding_id.clone(),
        sequence: tail
            .sequence
            .checked_add(1)
            .ok_or_else(|| invalid("exchange sequence overflow"))?,
        predecessor: Some(previous.clone()),
        actor: input.intent.actor.clone(),
        recorded_at: now,
        transition,
    };
    let object = CanonicalObject::mint(&event)?;
    store_event(connection, ledger, &object)?;
    Ok(object)
}

pub(super) fn store_event(
    connection: &rusqlite::Connection,
    ledger: &mut Ledger,
    object: &CanonicalObject,
) -> Result<(), StoreError> {
    let event: Event = object.decode()?;
    let binding = ledger
        .bindings
        .get(&event.binding)
        .ok_or_else(|| invalid("exchange binding is missing"))?
        .clone();
    ledger.capacity(&[object])?;
    write_event(connection, &binding, object, &event)?;
    ledger.bytes += object.bytes().len();
    ledger.events.insert(object.key().clone(), event);
    Ok(())
}

/// The same write as [`store_event`] for a caller that read its binding and
/// tail by key instead of loading the whole ledger. Capacity is checked from
/// stored lengths, without decoding or replaying any other exchange.
pub(super) fn store_event_keyed(
    connection: &rusqlite::Connection,
    binding: &Binding,
    object: &CanonicalObject,
) -> Result<Event, StoreError> {
    let event: Event = object.decode()?;
    super::capacity::require_on(connection, &[object])?;
    write_event(connection, binding, object, &event)?;
    Ok(event)
}

fn write_event(
    connection: &rusqlite::Connection,
    binding: &Binding,
    object: &CanonicalObject,
    event: &Event,
) -> Result<(), StoreError> {
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM objects WHERE object_id = ?1)",
        [object.key().as_str()],
        |row| row.get(0),
    )?;
    require(
        !exists,
        "planned event identity already exists without an intact committed receipt",
    )?;
    require(
        !super::recovery::quarantined(connection, &event.binding, binding)?,
        "origin recovery quarantined this exchange; preserve predecessor history",
    )?;
    super::recovery::require_scope_on(connection, binding)?;
    let proposed = super::view::append::propose(connection, binding, object)?;
    let created_at = event.recorded_at.format("%Y-%m-%d %H:%M:%S").to_string();
    connection.execute("INSERT INTO objects (object_id, object_kind, canonical_json, created_at) VALUES (?1, ?2, ?3, ?4)",
        rusqlite::params![object.key().as_str(), EVENT_KIND, object.bytes(), created_at])?;
    super::view::append::record(connection, &event.binding, &proposed)?;
    if matches!(
        event.transition,
        Transition::CustodyCheckpoint { .. }
            | Transition::DeltaPrepared { .. }
            | Transition::DeltaImportApplied { .. }
            | Transition::PlanningSuccessorPrepared { .. }
            | Transition::PlanningSuccessorApplied { .. }
    ) {
        super::super::closure::documents::record_on(connection, EVENT_KIND, object)?;
    }
    Ok(())
}

fn apply_on(
    transaction: &rusqlite::Transaction<'_>,
    ledger: &mut Ledger,
    input: &InspectedExchange,
    dry_run: bool,
    now: DateTime<Utc>,
    observed: &super::custody::Observed,
) -> Result<(ObjectId, ImportReceipt), StoreError> {
    active(ledger, input)?;
    external(ledger, input, observed)?;
    input
        .recheck_files()
        .map_err(|error| invalid(error.to_string()))?;
    let prepared = ledger
        .events
        .iter()
        .find_map(|(id, event)| match &event.transition {
            Transition::Prepared { intent }
                if event.binding == input.binding_id
                    && intent.operation == input.intent.operation =>
            {
                Some((id.clone(), intent))
            }
            _ => None,
        })
        .ok_or_else(|| invalid("exchange intent is missing; reconcile retained artifacts"))?;
    require(
        prepared.1.as_ref() == &input.intent,
        "exchange intent differs from recorded request",
    )?;
    if let Some((id, receipt)) = applied(ledger, &prepared.0) {
        require(
            basis(transaction, &input.binding, Direction::Incoming)? == receipt.after_basis,
            "local planning changed after import; reconcile retained receipt",
        )?;
        let result = import_task_tree_on(
            transaction,
            &input.binding.project,
            &input.intent.actor,
            &input.tree,
            true,
            now,
        )?;
        require(!result.changed, "recorded receipt has no exact local graph")?;
        return Ok((id.clone(), receipt.clone()));
    }
    let before = basis(transaction, &input.binding, Direction::Incoming)?;
    require(
        before == input.intent.local_planning_basis,
        "local planning changed before import",
    )?;
    let retained = retention::read_import_plan(
        &input.binding.repository.git_directory,
        input.intent.operation,
    )
    .map_err(|error| invalid(error.to_string()))?
    .ok_or_else(|| invalid("import plan missing; reconcile without reminting"))?;
    plan::resumed(ledger, input, &retained)?;
    require(
        retained.prepared == prepared.0,
        "import plan names another prepared event",
    )?;
    let imported = super::super::import_task_tree_with_plan_on(
        transaction,
        &input.binding.project,
        &input.intent.actor,
        &input.tree,
        &retained,
    )?;
    let _ = dry_run; // The transaction owner rolls back dry runs, including receipts.
    require(
        imported.changed == (retained.disposition == ImportDisposition::Created),
        "planned import disposition changed",
    )?;
    plan::verify_effect(transaction, &retained)?;
    let mut tasks = Vec::new();
    for task in &input.tree.tasks {
        let item = query::load_work_item(transaction, task.id)?;
        let origin_id = origin_reference(&item.created_by)?;
        let origin: ImportOrigin =
            SqliteStore::get_typed_object_on(transaction, &origin_id, ORIGIN_KIND)?
                .ok_or_else(|| invalid("import origin missing before receipt"))?;
        let after = CanonicalObject::freeze(task)?.key().clone();
        tasks.push(ImportedTask {
            task: task.id,
            source_task_path: format!("tasks/{}.json", task.id.0),
            before_basis: if imported.changed {
                None
            } else {
                Some(after.clone())
            },
            after_basis: after,
            local_record_ids: vec![origin_id, origin.record_id],
            disposition: if imported.changed {
                ImportDisposition::Created
            } else {
                ImportDisposition::ExactRepeat
            },
        });
    }
    let receipt = ImportReceipt {
        source_commit: input.intent.commit.oid.clone(),
        source_tree: input.intent.commit.tree.clone(),
        before_basis: before,
        after_basis: basis(transaction, &input.binding, Direction::Incoming)?,
        notes_included: input.tree.project.notes_included,
        tasks,
    };
    require(
        receipt.after_basis == input.intent.candidate.planning_fingerprint,
        "applied import differs from retained planning basis",
    )?;
    require(
        receipt == plan::receipt(input, &retained)?,
        "applied receipt differs from original import plan",
    )?;
    super::verify_receipt_content(transaction, &input.binding, &receipt)?;
    let object = plan::receipt_object(input, &retained)?;
    store_event(transaction, ledger, &object)?;
    Ok((object.key().clone(), receipt))
}

#[cfg(test)]
mod checkpoint_tests;
#[cfg(test)]
mod compatibility;
#[cfg(test)]
mod origin_tests;
#[cfg(test)]
mod tests;
