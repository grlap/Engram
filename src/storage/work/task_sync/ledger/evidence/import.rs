//! One admitted writer owns the recipe events, inert records and keyed progress.

use super::super::{delta, require, view};
use crate::storage::work::task_sync::producing_origin;
use crate::{
    CanonicalObject, ObjectId,
    storage::{SqliteStore, StoreError},
    task_sync::{
        self,
        evidence::{EvidenceError, RecordEnvelope, inventory},
        exchange::evidence::InspectedEvidence,
        ledger::{
            Binding, CompletedExchange, EVENT_KIND, Event, EvidenceImportRecipe, EvidenceIntent,
            EvidenceStop,
        },
        progress::ExportCut,
    },
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

#[cfg(test)]
mod tests;
mod views;

fn invalid(message: impl Into<String>) -> StoreError {
    crate::storage::work::task_sync::invalid(message)
}

fn fresh(
    connection: &Connection,
    binding_id: &ObjectId,
    binding: &Binding,
    intent: &EvidenceIntent,
) -> Result<(), StoreError> {
    delta::local_store(connection, binding)?;
    let mut budget = view::read::ReadBudget::default();
    require(
        view::read::binding_on(connection, binding_id, &mut budget)? == *binding,
        "evidence binding changed",
    )?;
    // Evidence retention is inert. Its historical anchor, binding, exact chain
    // and private continuity matter; unrelated native work is no readset here.
    producing_origin::check_cut_on(
        connection,
        &binding.project,
        &ExportCut {
            position: intent.local_cut.position.position,
            anchor: intent.local_cut.anchor.clone(),
        },
    )
}

/// Choose a fresh construction at the binding's current settled tail.
fn select_on(
    connection: &Connection,
    binding_id: &ObjectId,
    binding: &Binding,
    intent: &EvidenceIntent,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<EvidenceImportRecipe, StoreError> {
    fresh(connection, binding_id, binding, intent)?;
    let mut budget = view::read::ReadBudget::default();
    let tail = view::read::tail_on(connection, binding_id, binding, &mut budget)?;
    require(
        view::select::successor(connection, binding_id)?.is_none(),
        "evidence binding was superseded",
    )?;
    require(
        super::super::settled_tail(&tail.event),
        "evidence cannot bypass unresolved progress",
    )?;
    require(
        view::read::request_by(connection, binding_id, intent.operation, &intent.key)?.is_none(),
        "unfinished evidence request requires its original retained construction",
    )?;
    EvidenceImportRecipe::prepare(
        binding_id,
        binding,
        intent.clone(),
        &tail.id,
        tail.event.sequence,
        now,
    )
}

impl SqliteStore {
    /// Select a fresh evidence construction and retain it before any selected
    /// pack bytes are read. Selection and retention share the origin
    /// reservation and one SQL writer, so no other exchange can advance the
    /// binding between them; a concurrent writer makes this refuse with
    /// nothing retained. The caller observes Git first, outside the writer, and
    /// reuses an existing retained construction instead of calling this.
    ///
    /// # Errors
    /// Refuses a changed, superseded or unsettled binding, a recorded request,
    /// a missing or different carrier selection, busy or uncertain custody,
    /// and any refusal to retain the recipe file.
    pub(crate) fn retain_task_sync_evidence_recipe(
        &mut self,
        binding_id: &ObjectId,
        binding: &Binding,
        intent: &EvidenceIntent,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<EvidenceImportRecipe, StoreError> {
        crate::storage::admit_live_actor_session(&intent.actor)?;
        let request = task_sync::retention::evidence::construction_request(binding_id, intent)
            .map_err(|error| invalid(error.to_string()))?;
        producing_origin::with_construction(
            self,
            &binding.project,
            &binding.repository.git_directory,
            intent.operation,
            &request,
            |transaction| {
                let recipe = select_on(transaction, binding_id, binding, intent, now)?;
                require(
                    task_sync::retention::commit_selection(
                        &binding.repository.git_directory,
                        intent.operation,
                    )
                    .map_err(|error| invalid(error.to_string()))?
                    .as_ref()
                        == Some(&intent.source),
                    "selected evidence carrier retention is missing or differs",
                )?;
                task_sync::retention::evidence::retain_recipe(binding_id, binding, &recipe)
                    .map_err(|error| invalid(error.to_string()))?;
                Ok(recipe)
            },
        )
    }

    pub(crate) fn apply_task_sync_evidence_import(
        &mut self,
        input: &InspectedEvidence,
    ) -> Result<CompletedExchange, StoreError> {
        self.finish_task_sync_evidence_import(input, None)
    }

    pub(crate) fn stop_task_sync_evidence_import(
        &mut self,
        input: &InspectedEvidence,
        stopped: &EvidenceStop,
    ) -> Result<CompletedExchange, StoreError> {
        stopped.validate()?;
        self.finish_task_sync_evidence_import(input, Some(stopped))
    }

    fn finish_task_sync_evidence_import(
        &mut self,
        input: &InspectedEvidence,
        stopped: Option<&EvidenceStop>,
    ) -> Result<CompletedExchange, StoreError> {
        let intent = input.recipe.intent()?;
        crate::storage::admit_live_actor_session(&intent.actor)?;
        if let Some(original) = self.completed_task_sync_evidence(&input.binding_id, intent)? {
            self.acknowledge_committed_task_sync_evidence(&input.binding_id, intent, &original)?;
            return Ok(original);
        }
        input.recipe.validate(&input.binding_id, &input.binding)?;
        input
            .recheck_recipe()
            .map_err(|error| invalid(error.to_string()))?;
        let request = task_sync::retention::evidence::request_fingerprint(&input.recipe)
            .map_err(|error| invalid(error.to_string()))?;
        producing_origin::with_evidence_import(
            self,
            &input.binding,
            &input.recipe,
            &request,
            |store, guard| {
                let mut transaction = store.begin_work_mutation()?;
                require(
                    guard.is_some(),
                    "evidence import requires checked private origin continuity",
                )?;
                producing_origin::check_exchange_on(
                    &transaction,
                    &input.binding.project,
                    guard,
                    &request,
                )?;
                if let Some(original) =
                    view::evidence::lookup(&transaction, &input.binding_id, intent)?
                {
                    return Ok(original);
                }
                fresh(&transaction, &input.binding_id, &input.binding, intent)?;
                input
                    .recheck_recipe()
                    .map_err(|error| invalid(error.to_string()))?;
                let result = if let Some(stopped) = stopped {
                    append(
                        &transaction,
                        &input.binding,
                        &input.recipe.prepared_id,
                        &input.recipe.prepared,
                    )?;
                    finish_on(
                        &transaction,
                        input,
                        input.recipe.stopped_event(stopped.clone())?,
                    )?
                } else {
                    apply_on(transaction.transaction_mut(), input)?
                };
                transaction.commit()?;
                #[cfg(test)]
                after_commit::run();
                Ok(result)
            },
        )
    }
}

/// The caller owns writer admission, custody and the outer commit. A known
/// payload rejection rolls back only payload effects, then stores the fixed
/// terminal recipe outcome. Storage uncertainty rolls back the entire writer.
fn apply_on(
    transaction: &mut Transaction<'_>,
    input: &InspectedEvidence,
) -> Result<CompletedExchange, StoreError> {
    input.recipe.validate(&input.binding_id, &input.binding)?;
    append(
        transaction,
        &input.binding,
        &input.recipe.prepared_id,
        &input.recipe.prepared,
    )?;
    transaction.execute_batch("SAVEPOINT engram_evidence_batch_payload")?;
    let payload = payload_on(transaction, input);
    let outcome = match payload {
        Ok(()) => {
            transaction.execute_batch("RELEASE engram_evidence_batch_payload")?;
            input.recipe.outcome.clone()
        }
        Err(error) => {
            transaction.execute_batch(
                "ROLLBACK TO engram_evidence_batch_payload; RELEASE engram_evidence_batch_payload",
            )?;
            let crate::storage::work::task_sync::evidence::PersistenceError::Evidence(
                ref structural,
            ) = error
            else {
                return Err(invalid(error.to_string()));
            };
            let Some(stopped) = EvidenceStop::from_error(structural) else {
                return Err(invalid(error.to_string()));
            };
            input.recipe.stopped_event(stopped)?
        }
    };
    finish_on(transaction, input, outcome)
}

fn finish_on(
    transaction: &Connection,
    input: &InspectedEvidence,
    outcome: Event,
) -> Result<CompletedExchange, StoreError> {
    require(
        input.recipe.matches_outcome(&outcome)?,
        "evidence outcome differs from retained construction",
    )?;
    append(
        transaction,
        &input.binding,
        &input.recipe.outcome_id,
        &outcome,
    )?;
    Ok(CompletedExchange {
        prepared: input.recipe.prepared_id.clone(),
        outcome: input.recipe.outcome_id.clone(),
        event: outcome,
    })
}

fn payload_on(
    transaction: &mut Transaction<'_>,
    input: &InspectedEvidence,
) -> Result<(), crate::storage::work::task_sync::evidence::PersistenceError> {
    use crate::storage::work::task_sync::evidence::{PersistenceError, retain_batch_on};
    use sha2::{Digest, Sha256};
    let intent = input.recipe.intent()?;
    require(
        input.packs.len() == intent.packs.len(),
        "selected evidence batch membership differs",
    )?;
    let mut selected_packs = std::collections::BTreeSet::new();
    let reserved = std::iter::once(&input.recipe.prepared_id)
        .chain(std::iter::once(&input.recipe.outcome_id))
        .chain(
            intent
                .packs
                .iter()
                .flat_map(|(pack, selected)| [pack, &selected.admission]),
        )
        .collect::<std::collections::BTreeSet<_>>();
    for pack in &input.packs {
        let selected = intent
            .packs
            .get(pack.pack_id())
            .ok_or_else(|| invalid("evidence pack is not selected"))?;
        let bytes = pack.to_bytes()?;
        require(
            selected_packs.insert(pack.pack_id())
                && pack.project_id() == &input.binding.project
                && pack.publishing_origin() == &selected.publishing_origin
                && bytes.len() == selected.encoded_bytes
                && pack.records().len() == selected.records
                && format!("{:x}", Sha256::digest(&bytes)) == selected.content_fingerprint.as_str(),
            "selected evidence pack content differs",
        )?;
        if let Some(record) = pack
            .records()
            .iter()
            .find(|record| reserved.contains(record.object_id()))
        {
            return Err(EvidenceError::RecordConflict(record.object_id().clone()).into());
        }
    }
    inventory::plan_batch(&input.packs, |id| {
        known(transaction, &input.binding.project, id)
    })?;
    let selected = input
        .packs
        .iter()
        .map(|pack| (pack, &intent.packs[pack.pack_id()].admission))
        .collect::<Vec<_>>();
    let retained = retain_batch_on(transaction, &input.binding.project, &selected)?;
    let mut actual = std::collections::BTreeMap::new();
    for receipt in retained {
        require(
            actual.insert(receipt.pack_id, receipt.admission).is_none(),
            "evidence batch returned duplicate pack admissions",
        )?;
    }
    let expected = intent
        .packs
        .iter()
        .map(|(pack, selection)| (pack.clone(), selection.admission.clone()))
        .collect::<std::collections::BTreeMap<_, _>>();
    require(
        actual == expected,
        "evidence batch admissions differ from the frozen recipe",
    )?;
    for pack in &input.packs {
        let selected = &intent.packs[pack.pack_id()];
        views::check(transaction, pack, &selected.admission).map_err(PersistenceError::from)?;
    }
    Ok(())
}

fn append(
    connection: &Connection,
    binding: &Binding,
    id: &ObjectId,
    event: &Event,
) -> Result<(), StoreError> {
    let object = CanonicalObject::identified(id, event)?;
    super::super::capacity::require_on(connection, &[&object])?;
    let proposed = view::append::propose(connection, binding, &object)?;
    SqliteStore::insert_object(connection, EVENT_KIND, &object)?;
    view::append::record(connection, &event.binding, &proposed)
}

fn known(
    connection: &Connection,
    project: &crate::ProjectId,
    id: &ObjectId,
) -> Result<Option<RecordEnvelope>, EvidenceError> {
    let row: Option<(String,String,Vec<u8>)> = connection.query_row(
        "SELECT member.origin_id,original.object_kind,original.canonical_json
         FROM work_task_sync_evidence_members member JOIN work_task_sync_evidence_packs pack ON pack.pack_id=member.pack_id
         JOIN objects original ON original.object_id=member.object_id
         JOIN objects manifest ON manifest.object_id=pack.pack_id
         JOIN objects receipt ON receipt.object_id=pack.admission_id,
         json_each(manifest.canonical_json,'$.members') selected
         WHERE pack.project_id=?1 AND member.object_id=?2
         AND manifest.object_kind='task_sync_evidence_pack_manifest'
         AND receipt.object_kind='task_sync_evidence_admission'
         AND json_extract(manifest.canonical_json,'$.project_id')=pack.project_id
         AND json_extract(receipt.canonical_json,'$.project_id')=pack.project_id
         AND json_extract(receipt.canonical_json,'$.pack_id')=pack.pack_id
         AND CAST(selected.key AS INTEGER)=member.ordinal
         AND json_extract(selected.value,'$.object_id')=member.object_id
         AND json_extract(selected.value,'$.object_kind')=member.object_kind
         AND member.object_kind=original.object_kind
         AND json_extract(selected.value,'$.origin')=member.origin_id
         ORDER BY member.origin_id LIMIT 1",
        params![project.0,id.as_str()], |row|Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional()
        .map_err(|_| EvidenceError::InvalidPack("known evidence lookup failed"))?;
    if row.is_none()
        && crate::storage::work::task_sync::evidence::is_imported_only(connection, id)
            .map_err(|_| EvidenceError::InvalidPack("known evidence partition differs"))?
    {
        return Err(EvidenceError::InvalidPack(
            "known evidence membership is missing; repair projections explicitly",
        ));
    }
    row.map(|(origin, kind, bytes)| {
        let origin = ObjectId::from_stored(origin).ok_or(EvidenceError::InvalidPack(
            "known evidence origin is invalid",
        ))?;
        let record = RecordEnvelope::from_canonical(origin, id.clone(), &kind, bytes)?;
        inventory::check_project(&record, project)?;
        Ok(record)
    })
    .transpose()
}

/// A deterministic point right after an import's SQL commit and before its
/// exchange owner observes custody again, where a concurrent fetch can land.
#[cfg(test)]
pub(in crate::storage::work::task_sync::ledger) mod after_commit {
    use std::cell::RefCell;

    type Hook = Box<dyn FnOnce()>;

    thread_local! {
        static HOOK: RefCell<Option<Hook>> = const { RefCell::new(None) };
    }

    /// Run `hook` once, after the next evidence import on this thread commits.
    pub(in crate::storage::work::task_sync::ledger) fn set(hook: impl FnOnce() + 'static) {
        HOOK.with(|slot| *slot.borrow_mut() = Some(Box::new(hook)));
    }

    pub(super) fn run() {
        if let Some(hook) = HOOK.with(|slot| slot.borrow_mut().take()) {
            hook();
        }
    }
}
