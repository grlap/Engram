//! Durable reconciliation facts and admission fences. Planning successors wait
//! for local claims and unfinished runs; host requests keep their own identity.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::{ImportOrigin, ORIGIN_KIND, SqliteStore, StoreError, invalid, query};
use crate::task_sync::ledger::{
    BINDING_KIND, Binding, Direction, EVENT_KIND, Event, Transition, require,
};
use crate::task_sync::reconciliation::{
    HOLD_KIND, ReconciliationHold, ReconciliationRunBasis, ReconciliationWorkBasis,
    SHARED_VERSION_KIND, SharedImportVersion,
};
use crate::{CanonicalObject, ObjectId, ProjectId, WorkId, WorkRunId, WorkTransition};

pub(in crate::storage::work) mod active;
mod heads;
pub(in crate::storage) mod holds;
mod lookup;
pub(in crate::storage::work) mod successor;
mod versions;
pub(super) use heads::advance_on;
pub(in crate::storage) use heads::head_for_imported_task_on;
pub(super) use heads::validate_current_on as validate_current_heads_on;

#[cfg(test)]
mod tests;

pub(in crate::storage) use holds::refuse_held_on;
pub(super) use versions::{validate_receipt_base, validate_receipt_intent, validate_receipt_on};

const MAX_RECORDS: usize = 100_000;
const MAX_BYTES: usize = 128 * 1024 * 1024;
const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;

pub(super) fn validate_imported_head_on(
    connection: &Connection,
    id: &ObjectId,
    project: &ProjectId,
    work: WorkId,
) -> Result<Option<crate::task_sync::TaskDocument>, StoreError> {
    versions::baseline_on(connection, id, project, work)
}

#[cfg(test)]
thread_local! {
    static AFFECTED_ITEM_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

fn family_on<T: serde::de::DeserializeOwned>(
    connection: &Connection,
    id: &ObjectId,
    kind: &str,
) -> Result<Option<T>, StoreError> {
    let actual: Option<String> = connection
        .query_row(
            "SELECT object_kind FROM objects WHERE object_id=?1",
            [id.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    if actual.as_deref() != Some(kind) {
        return Ok(None);
    }
    SqliteStore::get_typed_object_on(connection, id, kind)
}

pub(super) fn validate_work_basis_links(
    connection: &Connection,
    project: &ProjectId,
    basis: &ReconciliationWorkBasis,
) -> Result<(), StoreError> {
    let item = query::load_work_item(connection, basis.work_id)?;
    require(
        &item.project_id == project && basis.revision > 0 && basis.revision <= item.revision,
        "captured work basis scope or revision differs",
    )?;
    require(
        item.created_by.source_tool.as_deref() != Some(super::IMPORT_TOOL)
            || basis.imported_head.is_some(),
        "captured origin-created task is missing its imported baseline",
    )?;
    if let Some(head) = &basis.head {
        let event: crate::WorkEvent =
            SqliteStore::get_typed_object_on(connection, head, "work_event")?
                .ok_or_else(|| invalid("captured historical work head is missing"))?;
        require(
            &event.project_id == project
                && event.work_id == basis.work_id
                && event.revision == basis.revision
                && event.work.active_run_id == basis.active_run_id,
            "captured historical work head scope differs",
        )?;
    } else {
        require(
            item.restored
                && basis.revision == 1
                && basis.active_run_id.is_none()
                && basis.runs.is_empty(),
            "native execution basis is missing its historical head",
        )?;
    }
    if let Some(head) = &basis.imported_head {
        if let Some(origin) = family_on::<ImportOrigin>(connection, head, ORIGIN_KIND)? {
            require(
                &origin.project_id == project && origin.task.id == basis.work_id,
                "captured imported head scope differs",
            )?;
            super::origin::validate_record(connection, &origin)?;
        } else if family_on::<serde_json::Value>(
            connection,
            head,
            super::folder::IMPORT_PLANNING_KIND,
        )?
        .is_some()
        {
            heads::folder_baseline_on(connection, head, project, basis.work_id)?;
        } else {
            let version: SharedImportVersion =
                SqliteStore::get_typed_object_on(connection, head, SHARED_VERSION_KIND)?
                    .ok_or_else(|| invalid("captured imported head missing"))?;
            require(
                &version.project_id == project && version.task.id == basis.work_id,
                "captured shared head scope differs",
            )?;
        }
    }
    let mut runs = HashSet::new();
    for captured in &basis.runs {
        let run = query::load_work_run(connection, captured.run_id)?;
        let fence: i64 = connection.query_row(
            "SELECT claim_fence_head FROM work_runs WHERE run_id=?1",
            [captured.run_id.0.to_string()],
            |row| row.get(0),
        )?;
        let claim = query::load_work_claim_optional(connection, captured.run_id)?;
        require(
            runs.insert(captured.run_id)
                && run.work_id == basis.work_id
                && run.generation == captured.generation
                && captured.revision > 0
                && captured.revision <= run.revision
                && captured.claim_fence >= 0
                && captured.claim_fence <= fence
                && captured
                    .claim_id
                    .is_none_or(|id| claim.as_ref().is_some_and(|claim| claim.claim_id == id)),
            "captured run basis scope or fence differs",
        )?;
    }
    require(
        basis.active_run_id.is_none_or(|id| runs.contains(&id)),
        "captured active run is outside readset",
    )?;
    Ok(())
}

#[derive(Default)]
struct Records {
    holds: BTreeMap<ObjectId, ReconciliationHold>,
    versions: BTreeMap<ObjectId, SharedImportVersion>,
}

impl Records {
    fn load(connection: &Connection) -> Result<Self, StoreError> {
        let mut records = Self::default();
        let mut statement = connection.prepare("SELECT object_id, object_kind, canonical_json FROM objects WHERE object_kind IN (?1, ?2)")?;
        let mut rows = statement.query(params![HOLD_KIND, SHARED_VERSION_KIND])?;
        let mut count = 0;
        let mut total = 0usize;
        while let Some(row) = rows.next()? {
            let id = ObjectId::from_stored(row.get(0)?)
                .ok_or_else(|| invalid("invalid reconciliation record id"))?;
            let kind: String = row.get(1)?;
            let bytes: Vec<u8> = row.get(2)?;
            count += 1;
            total = total
                .checked_add(bytes.len())
                .ok_or_else(|| invalid("reconciliation byte bound exceeded"))?;
            require(
                count <= MAX_RECORDS && total <= MAX_BYTES && bytes.len() <= MAX_RECORD_BYTES,
                "reconciliation records exceed supported bounds; preserve them",
            )?;
            let object = CanonicalObject::stored(&id, bytes)?;
            if kind == HOLD_KIND {
                records.holds.insert(id, object.decode()?);
            } else {
                records.versions.insert(id, object.decode()?);
            }
        }
        Ok(records)
    }
}

fn prepared_on(
    connection: &Connection,
    binding_id: &ObjectId,
    prepared_id: &ObjectId,
    project: &ProjectId,
) -> Result<(Binding, Event), StoreError> {
    let binding: Binding = SqliteStore::get_typed_object_on(connection, binding_id, BINDING_KIND)?
        .ok_or_else(|| invalid("reconciliation binding is missing"))?;
    binding.validate()?;
    require(
        &binding.project == project,
        "reconciliation project differs from binding",
    )?;
    let prepared: Event = SqliteStore::get_typed_object_on(connection, prepared_id, EVENT_KIND)?
        .ok_or_else(|| invalid("reconciliation prepared intent is missing"))?;
    require(
        &prepared.binding == binding_id,
        "reconciliation prepared binding differs",
    )?;
    let (direction, actor) = match &prepared.transition {
        Transition::Prepared { intent } => {
            intent.validate()?;
            (intent.direction, &intent.actor)
        }
        Transition::DeltaPrepared { intent, payload } => {
            payload.validate_historical(binding_id, &binding, intent)?;
            (intent.direction, &intent.actor)
        }
        _ => return Err(invalid("reconciliation requires a planning preparation")),
    };
    require(
        direction == Direction::Incoming && &prepared.actor == actor,
        "reconciliation requires an incoming intent with matching attribution",
    )?;
    Ok((binding, prepared))
}

/// Capture connected roots and prerequisite edges, including reverse edges.
/// Every generation is retained; current focus and claim absence are irrelevant.
pub(in crate::storage) fn capture_affected_on(
    connection: &Connection,
    project: &ProjectId,
    seeds: &[WorkId],
) -> Result<Vec<ReconciliationWorkBasis>, StoreError> {
    require(
        !seeds.is_empty() && seeds.len() <= MAX_RECORDS,
        "reconciliation scope requires bounded seeds",
    )?;
    let mut pending = seeds.iter().copied().collect::<BTreeSet<_>>();
    require(
        pending.len() == seeds.len(),
        "reconciliation seed is repeated, missing or cross-project",
    )?;
    let seed_ids = pending.clone();
    let mut items = HashMap::new();
    let mut roots = HashSet::new();
    while let Some(id) = pending.pop_first() {
        if items.contains_key(&id) {
            continue;
        }
        require(
            items.len() < MAX_RECORDS,
            "reconciliation scope exceeds supported bound",
        )?;
        #[cfg(test)]
        AFFECTED_ITEM_READS.with(|count| count.set(count.get() + 1));
        let seed_error = "reconciliation seed is repeated, missing or cross-project";
        let item = query::load_work_item(connection, id).map_err(|error| {
            if seed_ids.contains(&id) && matches!(error, StoreError::WorkNotFound(_)) {
                invalid(seed_error)
            } else {
                error
            }
        })?;
        require(
            item.project_id == *project,
            if seed_ids.contains(&id) {
                seed_error
            } else {
                "affected prerequisite crosses project"
            },
        )?;
        let mut enqueue = |found: String| -> Result<(), StoreError> {
            let found = query::parse_work_id(&found)?;
            if found != id && !items.contains_key(&found) {
                pending.insert(found);
            }
            require(
                items.len() + pending.len() < MAX_RECORDS,
                "reconciliation scope exceeds supported bound",
            )
        };
        if roots.insert(item.root_id) {
            for found in connection.prepare("SELECT work_id FROM work_items INDEXED BY work_items_root WHERE project_id=?1 AND root_id=?2")?
                .query_map(params![&project.0, item.root_id.0.to_string()], |row| row.get::<_, String>(0))? {
                enqueue(found?)?;
            }
        }
        for sql in [
            "SELECT prerequisite_id FROM work_prerequisites WHERE work_id=?1",
            "SELECT work_id FROM work_prerequisites INDEXED BY work_prerequisites_reverse WHERE prerequisite_id=?1",
        ] {
            for found in connection
                .prepare(sql)?
                .query_map([id.0.to_string()], |row| row.get::<_, String>(0))?
            {
                enqueue(found?)?;
            }
        }
        items.insert(id, item);
        require(
            items.len() + pending.len() <= MAX_RECORDS,
            "reconciliation scope exceeds supported bound",
        )?;
    }
    let mut selected: Vec<_> = items.keys().copied().collect();
    selected.sort_by_key(|id| id.0);
    selected.into_iter().map(|work_id| {
        let item = &items[&work_id];
        let head: Option<String> = connection.query_row("SELECT latest_event_id FROM work_items WHERE work_id = ?1", [work_id.0.to_string()], |row| row.get(0))?;
        let head = head.map(|id| ObjectId::from_stored(id).ok_or_else(|| invalid("invalid affected work head"))).transpose()?;
        let imported_head = versions::imported_head_on(connection, project, work_id)?;
        let mut statement = connection.prepare("SELECT run_id, claim_fence_head FROM work_runs WHERE work_id = ?1 ORDER BY generation")?;
        let runs = statement.query_map([work_id.0.to_string()], |row| Ok((row.get::<_, String>(0)?,row.get::<_, i64>(1)?)))?.collect::<Result<Vec<_>,_>>()?;
        let runs = runs.into_iter().map(|(id,claim_fence)| {
            let run_id = WorkRunId(uuid::Uuid::parse_str(&id).map_err(|_| invalid("invalid affected run id"))?);
            let run = query::load_work_run(connection, run_id)?;
            let claim = query::load_work_claim_optional(connection, run_id)?;
            Ok(ReconciliationRunBasis { run_id, generation:run.generation, revision:run.revision, claim_id:claim.map(|c|c.claim_id), claim_fence })
        }).collect::<Result<Vec<_>,StoreError>>()?;
        Ok(ReconciliationWorkBasis { work_id, revision:item.revision, head, imported_head, active_run_id:item.active_run_id, runs })
    }).collect()
}

pub(in crate::storage) fn verify_reconciliation(
    connection: &Connection,
    checked: &mut usize,
    failures: &mut Vec<String>,
) -> Result<(), StoreError> {
    let records = Records::load(connection)?;
    for (id, hold) in &records.holds {
        *checked += 1;
        if let Err(error) = holds::validate_hold(connection, id, hold)
            .and_then(|()| holds::resolution_on(connection, id, hold).map(|_| ()))
        {
            failures.push(format!("reconciliation hold {}: {error}", id.as_str()));
        }
    }
    for (id, version) in &records.versions {
        *checked += 1;
        if let Err(error) = versions::validate_version(connection, id, version) {
            failures.push(format!("shared import version {}: {error}", id.as_str()));
        }
    }
    versions::verify_heads(connection, &records, checked, failures)?;
    *checked += 1;
    if let Err(error) = active::verify(connection, &records) {
        failures.push(error.to_string());
    }
    holds::verify_slots(connection, &records, checked, failures)
}

pub(in crate::storage) fn validate_retirement_on(
    connection: &Connection,
    id: &ObjectId,
    event: &crate::WorkEvent,
) -> Result<(), StoreError> {
    let WorkTransition::SupersededByImport {
        hold,
        prepared,
        receipt,
        reason,
    } = &event.transition
    else {
        return Ok(());
    };
    require(
        !reason.trim().is_empty(),
        "import retirement needs an attributed reason",
    )?;
    let held: ReconciliationHold = SqliteStore::get_typed_object_on(connection, hold, HOLD_KIND)?
        .ok_or_else(|| invalid("retirement hold missing"))?;
    let run = event
        .run
        .as_ref()
        .ok_or_else(|| invalid("retirement run missing"))?;
    require(
        event.project_id == held.project_id
            && &held.prepared == prepared
            && held.affected.iter().any(|basis| {
                basis.work_id == event.work_id
                    && basis.runs.iter().any(|basis| {
                        basis.run_id == run.run_id && basis.generation == run.generation
                    })
            }),
        "retirement is outside held candidate scope",
    )?;
    require(
        run.state == crate::WorkRunState::SupersededByImport
            && run.completion_seal.is_none()
            && event.work.active_run_id.is_none()
            && event
                .claim
                .as_ref()
                .is_none_or(|claim| claim.state != crate::WorkClaimState::Active),
        "import retirement carries live authority or successful completion",
    )?;
    let result: Event = SqliteStore::get_typed_object_on(connection, receipt, EVENT_KIND)?
        .ok_or_else(|| invalid("retirement reconciled receipt missing"))?;
    require(
        result.binding == held.binding
            && matches!(&result.transition, Transition::ReconciledImportApplied {prepared:selected,receipt:outcome} if selected == prepared && outcome.holds.contains(hold) && outcome.terminal_evidence.contains(id)),
        "retirement receipt does not account for exact event and hold",
    )?;
    Ok(())
}

impl SqliteStore {
    /// Capture the complete connected Work basis for a retained candidate.
    /// This is a readset, not a host responsibility or settlement decision.
    ///
    /// # Errors
    /// Refuses missing/cross-project seeds and damaged canonical projections.
    pub fn task_sync_reconciliation_basis(
        &self,
        project: &ProjectId,
        seeds: &[WorkId],
    ) -> Result<Vec<ReconciliationWorkBasis>, StoreError> {
        self.work_read_snapshot(|store| capture_affected_on(&store.connection, project, seeds))
    }
}
