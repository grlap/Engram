use super::{
    BTreeSet, CanonicalObject, Connection, EVENT_KIND, Event, HOLD_KIND, HashSet, MAX_RECORD_BYTES,
    MAX_RECORDS, ObjectId, ProjectId, ReconciliationHold, Records, SqliteStore, StoreError,
    Transaction, Transition, WorkId, WorkRunId, WorkTransition, capture_affected_on, family_on,
    invalid, params, prepared_on, query, require, validate_work_basis_links,
};

/// Work's portion of the combined core check. A caller cannot deserialize or
/// construct a successful check, retain it across transactions, or clone it.
pub(in crate::storage) struct CheckedReconciliationHold<'tx, 'conn> {
    transaction: &'tx Transaction<'conn>,
    id: ObjectId,
    hold: Box<ReconciliationHold>,
}

impl CheckedReconciliationHold<'_, '_> {
    pub(in crate::storage) fn transaction(&self) -> &Transaction<'_> {
        self.transaction
    }
    pub(in crate::storage) fn scope_ref(&self) -> &ObjectId {
        &self.id
    }
    pub(in crate::storage) fn hold(&self) -> &ReconciliationHold {
        &self.hold
    }
}

/// The adapter supplies expected identities, never authority. The affected
/// scope reference names the canonical hold, which owns that immutable readset.
pub(in crate::storage) struct HoldExpectation<'a> {
    pub project: &'a ProjectId,
    pub hold: &'a ObjectId,
    pub prepared: &'a ObjectId,
    pub scope: &'a ObjectId,
    pub association: &'a ObjectId,
    pub candidate_revision: u64,
    pub hold_revision: u64,
    pub scope_revision: u64,
    pub owner_set_revision: u64,
}

#[derive(Debug, Eq, PartialEq)]
pub(in crate::storage) enum ReconciliationGap {
    Missing,
    Resolved,
    BasisChanged,
}

pub(in crate::storage) enum ReconciliationHoldCheck<'tx, 'conn> {
    Ready(CheckedReconciliationHold<'tx, 'conn>),
    Pending(ReconciliationGap),
}

pub(in crate::storage) fn load_reconciliation_hold_on<'tx, 'conn>(
    transaction: &'tx Transaction<'conn>,
    expected: &HoldExpectation<'_>,
) -> Result<ReconciliationHoldCheck<'tx, 'conn>, StoreError> {
    let Some(hold) = SqliteStore::get_typed_object_on::<ReconciliationHold>(
        transaction,
        expected.hold,
        HOLD_KIND,
    )?
    else {
        return Ok(ReconciliationHoldCheck::Pending(ReconciliationGap::Missing));
    };
    validate_hold(transaction, expected.hold, &hold)?;
    if &hold.project_id != expected.project
        || expected.scope != expected.hold
        || &hold.prepared != expected.prepared
        || &hold.association != expected.association
        || hold.candidate_revision != expected.candidate_revision
        || hold.hold_revision != expected.hold_revision
        || hold.scope_revision != expected.scope_revision
        || hold.owner_set_revision != expected.owner_set_revision
    {
        return Ok(ReconciliationHoldCheck::Pending(
            ReconciliationGap::BasisChanged,
        ));
    }
    if resolution_on(transaction, expected.hold, &hold)?.is_some() {
        return Ok(ReconciliationHoldCheck::Pending(
            ReconciliationGap::Resolved,
        ));
    }
    let current = capture_affected_on(transaction, &hold.project_id, &hold.seeds)?;
    for basis in &hold.affected {
        let item = query::load_work_item(transaction, basis.work_id)?;
        let historical = if let Some(head) = &basis.head {
            SqliteStore::get_typed_object_on::<crate::WorkEvent>(transaction, head, "work_event")?
                .ok_or_else(|| invalid("held historical work head missing"))?
                .work
        } else {
            if item.revision != basis.revision {
                return Ok(ReconciliationHoldCheck::Pending(
                    ReconciliationGap::BasisChanged,
                ));
            }
            item.clone()
        };
        let mut planning = item;
        planning.revision = historical.revision;
        planning.updated_at = historical.updated_at;
        planning.active_run_id = historical.active_run_id;
        if planning.lifecycle == crate::WorkLifecycle::Completed
            && historical.lifecycle == crate::WorkLifecycle::Open
        {
            planning.lifecycle = historical.lifecycle;
        }
        if planning != historical
            || current
                .iter()
                .find(|current| current.work_id == basis.work_id)
                .is_none_or(|current| current.imported_head != basis.imported_head)
        {
            return Ok(ReconciliationHoldCheck::Pending(
                ReconciliationGap::BasisChanged,
            ));
        }
        for (_, event) in query::canonical_work_event_records_for_item(transaction, basis.work_id)?
        {
            if event.revision > basis.revision
                && matches!(
                    event.transition,
                    WorkTransition::Created { .. }
                        | WorkTransition::Decomposed { .. }
                        | WorkTransition::Revised { .. }
                        | WorkTransition::PrerequisiteAdded { .. }
                        | WorkTransition::PrerequisiteRemoved { .. }
                        | WorkTransition::Blocked { .. }
                        | WorkTransition::Unblocked { .. }
                        | WorkTransition::Disposed { .. }
                        | WorkTransition::RequiredChildWaived { .. }
                        | WorkTransition::Reopened { .. }
                )
            {
                return Ok(ReconciliationHoldCheck::Pending(
                    ReconciliationGap::BasisChanged,
                ));
            }
        }
    }
    if current
        .iter()
        .map(|basis| {
            (
                basis.work_id,
                basis
                    .runs
                    .iter()
                    .map(|run| (run.run_id, run.generation))
                    .collect::<Vec<_>>(),
            )
        })
        .collect::<Vec<_>>()
        != hold
            .affected
            .iter()
            .map(|basis| {
                (
                    basis.work_id,
                    basis
                        .runs
                        .iter()
                        .map(|run| (run.run_id, run.generation))
                        .collect::<Vec<_>>(),
                )
            })
            .collect::<Vec<_>>()
    {
        return Ok(ReconciliationHoldCheck::Pending(
            ReconciliationGap::BasisChanged,
        ));
    }
    Ok(ReconciliationHoldCheck::Ready(CheckedReconciliationHold {
        transaction,
        id: expected.hold.clone(),
        hold: Box::new(hold),
    }))
}

pub(in crate::storage::work::task_sync) fn validate_hold(
    connection: &Connection,
    id: &ObjectId,
    hold: &ReconciliationHold,
) -> Result<(), StoreError> {
    require(
        hold.schema_version == 1
            && hold.candidate_revision > 0
            && hold.hold_revision > 0
            && hold.scope_revision > 0
            && hold.owner_set_revision > 0,
        "invalid reconciliation hold schema or revision",
    )?;
    crate::task_sync::ledger::attribution(&hold.actor)?;
    let (_, prepared) = prepared_on(connection, &hold.binding, &hold.prepared, &hold.project_id)?;
    let (actor, fingerprint) = match &prepared.transition {
        Transition::Prepared { intent } => (&intent.actor, &intent.candidate.documents_fingerprint),
        Transition::DeltaPrepared { intent, .. } => {
            (&intent.actor, &intent.candidate.content_fingerprint)
        }
        _ => return Err(invalid("reconciliation requires a planning preparation")),
    };
    require(
        &hold.actor == actor
            && &hold.candidate_fingerprint == fingerprint
            && hold.created_at >= prepared.recorded_at,
        "hold differs from its prepared candidate",
    )?;
    require(
        !hold.affected.is_empty()
            && hold.affected.len() <= MAX_RECORDS
            && hold.non_run_owner_refs.len() <= MAX_RECORDS,
        "invalid hold scope size",
    )?;
    let mut works = HashSet::new();
    let mut runs = HashSet::new();
    for basis in &hold.affected {
        validate_work_basis_links(connection, &hold.project_id, basis)?;
        require(
            works.insert(basis.work_id) && basis.revision > 0,
            "invalid or duplicate held task",
        )?;
        let item = query::load_work_item(connection, basis.work_id)?;
        require(
            item.project_id == hold.project_id,
            "hold task belongs to another project",
        )?;
        for run_basis in &basis.runs {
            let run = query::load_work_run(connection, run_basis.run_id)?;
            require(
                runs.insert(run_basis.run_id)
                    && run.work_id == basis.work_id
                    && run.generation == run_basis.generation
                    && run_basis.revision > 0
                    && run_basis.revision <= run.revision
                    && run_basis.claim_fence >= 0,
                "held generation differs or repeats",
            )?;
            let fence: i64 = connection.query_row(
                "SELECT claim_fence_head FROM work_runs WHERE run_id=?1",
                [run.run_id.0.to_string()],
                |row| row.get(0),
            )?;
            let claim = query::load_work_claim_optional(connection, run.run_id)?;
            require(
                run_basis.claim_fence <= fence
                    && run_basis
                        .claim_id
                        .is_none_or(|id| claim.as_ref().is_some_and(|claim| claim.claim_id == id)),
                "held claim identity or fence differs",
            )?;
        }
        require(
            basis
                .active_run_id
                .is_none_or(|active| basis.runs.iter().any(|run| run.run_id == active)),
            "held active run is outside scope",
        )?;
    }
    require(
        !hold.seeds.is_empty() && hold.seeds.iter().all(|seed| works.contains(seed)),
        "hold seed is outside scope",
    )?;
    require(
        hold.previous.as_ref() != Some(id),
        "hold is its own predecessor",
    )?;
    if let Some(previous) = &hold.previous {
        let old: ReconciliationHold =
            SqliteStore::get_typed_object_on(connection, previous, HOLD_KIND)?
                .ok_or_else(|| invalid("hold predecessor is missing"))?;
        require(
            old.project_id == hold.project_id
                && old.hold_revision < hold.hold_revision
                && old.prepared == hold.prepared
                && old.binding == hold.binding
                && old.association == hold.association
                && old.candidate_fingerprint == hold.candidate_fingerprint,
            "hold predecessor scope differs",
        )?;
    }
    let owners: BTreeSet<_> = hold.non_run_owner_refs.iter().collect();
    require(
        owners.len() == hold.non_run_owner_refs.len(),
        "duplicate non-run owner reference",
    )?;
    Ok(())
}

fn slots(hold: &ReconciliationHold) -> HashSet<WorkRunId> {
    hold.affected
        .iter()
        .flat_map(|work| work.runs.iter().map(|run| run.run_id))
        .collect()
}

pub(super) fn resolution_on(
    connection: &Connection,
    id: &ObjectId,
    hold: &ReconciliationHold,
) -> Result<Option<ObjectId>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT run_id, resolution_id FROM work_task_sync_reconciliation_holds WHERE hold_id = ?1",
    )?;
    let actual = statement
        .query_map([id.as_str()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut unresolved = 0;
    let mut resolutions = BTreeSet::new();
    let mut actual_runs = HashSet::new();
    for (run, resolution) in actual {
        actual_runs.insert(WorkRunId(
            uuid::Uuid::parse_str(&run).map_err(|_| invalid("invalid hold slot run"))?,
        ));
        match resolution {
            Some(id) => {
                resolutions.insert(
                    ObjectId::from_stored(id)
                        .ok_or_else(|| invalid("invalid hold resolution id"))?,
                );
            }
            None => unresolved += 1,
        }
    }
    // An old slot may have been replaced only after its canonical resolution.
    // Resolve from immutable history too, including scopes with no run slots.
    let work_ids = serde_json::to_string(
        &hold
            .affected
            .iter()
            .map(|basis| basis.work_id.0.to_string())
            .collect::<Vec<_>>(),
    )?;
    let mut statement = connection.prepare("SELECT object_id, canonical_json FROM objects WHERE object_kind = 'work_event' AND json_extract(canonical_json, '$.work_id') IN (SELECT value FROM json_each(?1)) AND json_extract(canonical_json, '$.transition.kind') = 'reconciliation_hold_abandoned' AND json_extract(canonical_json, '$.transition.hold') = ?2 LIMIT ?3")?;
    for (event_id, event) in
        super::lookup::decode_rows::<crate::WorkEvent>(statement.query(params![
            work_ids,
            id.as_str(),
            super::lookup::record_limit()?
        ])?)?
    {
        require(
            event.project_id == hold.project_id
                && event.actor == hold.actor
                && hold
                    .affected
                    .iter()
                    .any(|basis| basis.work_id == event.work_id),
            "hold abandonment is not by its original placer in scope",
        )?;
        resolutions.insert(event_id);
    }
    let mut statement = connection.prepare("SELECT object_id, canonical_json FROM objects
        WHERE object_kind IN ('task_sync_reconciliation_hold','task_sync_exchange_event') AND object_kind=?1 AND json_valid(canonical_json)
          AND CASE WHEN object_kind='task_sync_reconciliation_hold' THEN json_extract(canonical_json,'$.project_id') ELSE json_extract(canonical_json,'$.binding') END=?2
          AND json_extract(canonical_json,'$.transition.prepared')=?3 LIMIT ?4")?;
    for (event_id, event) in super::lookup::decode_rows::<Event>(statement.query(params![
        EVENT_KIND,
        hold.binding.as_str(),
        hold.prepared.as_str(),
        super::lookup::record_limit()?
    ])?)? {
        if let Transition::ReconciledImportApplied { prepared, receipt } = &event.transition
            && receipt.holds.contains(id)
        {
            require(
                prepared == &hold.prepared
                    && event.binding == hold.binding
                    && receipt.candidate_fingerprint == hold.candidate_fingerprint,
                "hold receipt candidate differs",
            )?;
            resolutions.insert(event_id);
        }
    }
    let mut statement = connection.prepare(
        "SELECT outcome.object_id,outcome.canonical_json FROM objects preparation
         INDEXED BY objects_task_sync_exchange_successor_hold JOIN objects outcome
         INDEXED BY objects_task_sync_exchange_prepared
         ON json_extract(outcome.canonical_json,'$.transition.prepared')=preparation.object_id
           AND json_extract(outcome.canonical_json,'$.binding')=json_extract(preparation.canonical_json,'$.binding')
         WHERE preparation.object_kind='task_sync_exchange_event'
           AND json_valid(preparation.canonical_json)
           AND json_extract(preparation.canonical_json,'$.transition.transition')='planning_successor_prepared'
           AND json_extract(preparation.canonical_json,'$.binding')=?1
           AND json_extract(preparation.canonical_json,'$.transition.recipe.request.hold')=?2
           AND outcome.object_kind='task_sync_exchange_event' AND json_valid(outcome.canonical_json)
           AND json_extract(outcome.canonical_json,'$.transition.transition')='planning_successor_applied'
         LIMIT ?3",
    )?;
    for (event_id, event) in super::lookup::decode_rows::<Event>(statement.query(params![
        hold.binding.as_str(),
        id.as_str(),
        super::lookup::record_limit()?,
    ])?)? {
        require(
            successor_resolution_on(connection, &event_id, &event, id, hold)?,
            "hold successor receipt candidate differs",
        )?;
        resolutions.insert(event_id);
    }
    require(
        resolutions.len() <= 1 && (resolutions.is_empty() || unresolved == 0),
        "hold slots have mixed resolutions",
    )?;
    if let Some(resolution) = resolutions.into_iter().next() {
        if let Some(event) = family_on::<crate::WorkEvent>(connection, &resolution, "work_event")? {
            require(
                matches!(&event.transition, WorkTransition::ReconciliationHoldAbandoned {hold:selected,reason} if selected == id && !reason.trim().is_empty())
                    && event.actor == hold.actor
                    && event.project_id == hold.project_id,
                "hold resolution differs",
            )?;
        } else {
            let event: Event =
                SqliteStore::get_typed_object_on(connection, &resolution, EVENT_KIND)?
                    .ok_or_else(|| invalid("hold resolution is missing"))?;
            require(
                (matches!(&event.transition, Transition::ReconciledImportApplied {prepared,receipt} if prepared == &hold.prepared && receipt.holds.contains(id) && receipt.candidate_fingerprint == hold.candidate_fingerprint)
                    || successor_resolution_on(connection, &resolution, &event, id, hold)?)
                    && event.binding == hold.binding,
                "hold resolution is not its reconciled receipt",
            )?;
        }
        return Ok(Some(resolution));
    }
    require(
        actual_runs == slots(hold),
        "active hold slot membership is incomplete",
    )?;
    Ok(None)
}

fn successor_resolution_on(
    connection: &Connection,
    outcome: &ObjectId,
    event: &Event,
    id: &ObjectId,
    hold: &ReconciliationHold,
) -> Result<bool, StoreError> {
    let Transition::PlanningSuccessorApplied { prepared, receipt } = &event.transition else {
        return Ok(false);
    };
    let Some(preparation) =
        SqliteStore::get_typed_object_on::<Event>(connection, prepared, EVENT_KIND)?
    else {
        return Ok(false);
    };
    let Transition::PlanningSuccessorPrepared { recipe, .. } = &preparation.transition else {
        return Ok(false);
    };
    Ok(event.binding == hold.binding
        && preparation.binding == hold.binding
        && &recipe.prepared == prepared
        && &recipe.outcome == outcome
        && &recipe.request.hold == id
        && recipe.request.original_prepared == hold.prepared
        && recipe.request.original_candidate == hold.candidate_fingerprint
        && receipt.holds == [id.clone()]
        && receipt.before == recipe.before
        && receipt.planning == recipe.planning)
}

pub(in crate::storage) fn refuse_held_on(
    connection: &Connection,
    work: WorkId,
) -> Result<(), StoreError> {
    if let Some(candidate) = held_by_on(connection, work)? {
        return Err(crate::task_sync::TaskSyncRefusal::ReconciliationHeld.invalid(format!("work is held by reconciliation candidate {candidate}; settle the original responsibilities or resolve the hold")));
    }
    Ok(())
}

pub(in crate::storage) fn held_by_on(
    connection: &Connection,
    work: WorkId,
) -> Result<Option<ObjectId>, StoreError> {
    let Some(id) = super::active::lookup(connection, work)? else {
        return Ok(None);
    };
    let hold: ReconciliationHold = SqliteStore::get_typed_object_on(connection, &id, HOLD_KIND)?
        .ok_or_else(|| invalid("active hold record missing"))?;
    let item = query::load_work_item(connection, work)?;
    validate_hold(connection, &id, &hold)?;
    require(
        hold.project_id == item.project_id
            && hold.affected.iter().any(|basis| basis.work_id == work),
        "active hold scope differs",
    )?;
    for basis in &hold.affected {
        require(
            super::active::lookup(connection, basis.work_id)?.as_ref() == Some(&id),
            "active reconciliation projection is incomplete",
        )?;
    }
    require(
        resolution_on(connection, &id, &hold)?.is_none(),
        "active reconciliation projection retains a resolved hold",
    )?;
    Ok(Some(hold.prepared))
}

pub(super) fn verify_slots(
    connection: &Connection,
    records: &Records,
    checked: &mut usize,
    failures: &mut Vec<String>,
) -> Result<(), StoreError> {
    let mut statement =
        connection.prepare("SELECT run_id, hold_id FROM work_task_sync_reconciliation_holds")?;
    for row in statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })? {
        let (run, hold) = row?;
        *checked += 1;
        let id = ObjectId::from_stored(hold.clone());
        if id
            .as_ref()
            .and_then(|id| records.holds.get(id))
            .is_none_or(|hold| {
                !hold.affected.iter().any(|work| {
                    work.runs
                        .iter()
                        .any(|basis| basis.run_id.0.to_string() == run)
                })
            })
        {
            failures.push(format!(
                "reconciliation slot {run} does not belong to hold {hold}"
            ));
        }
    }
    Ok(())
}

/// Place the existing canonical hold in the caller's admitted writer. The
/// caller supplies its already selected immutable identity and body.
pub(in crate::storage::work::task_sync) fn place_on(
    transaction: &Transaction<'_>,
    object: &CanonicalObject,
) -> Result<(), StoreError> {
    let hold: ReconciliationHold = object.decode()?;
    let hold = &hold;
    for (id, old) in
        super::lookup::scoped_on::<ReconciliationHold>(transaction, HOLD_KIND, &hold.project_id.0)?
    {
        require(
            old.prepared != hold.prepared || resolution_on(transaction, &id, &old)?.is_some(),
            "the prepared candidate already has an active hold",
        )?;
    }
    require(
        object.bytes().len() <= MAX_RECORD_BYTES,
        "reconciliation hold exceeds supported record bound",
    )?;
    validate_hold(transaction, object.key(), hold)?;
    super::super::ledger::require_pending_incoming_on(transaction, &hold.binding, &hold.prepared)?;
    require(
        capture_affected_on(transaction, &hold.project_id, &hold.seeds)? == hold.affected,
        "reconciliation scope moved before hold placement",
    )
    .map_err(|error| crate::task_sync::TaskSyncRefusal::DivergentPlanning.classify(error))?;
    for work in &hold.affected {
        refuse_held_on(transaction, work.work_id)?;
    }
    SqliteStore::insert_object(transaction, HOLD_KIND, object)?;
    for work in &hold.affected {
        transaction.execute(
            "INSERT INTO work_task_sync_reconciliation_active(work_id,hold_id) VALUES (?1,?2)",
            params![work.work_id.0.to_string(), object.key().as_str()],
        )?;
    }
    for run in slots(hold) {
        transaction.execute("INSERT INTO work_task_sync_reconciliation_holds(run_id,hold_id,resolution_id) VALUES (?1,?2,NULL) ON CONFLICT(run_id) DO UPDATE SET hold_id=excluded.hold_id,resolution_id=NULL", params![run.0.to_string(),object.key().as_str()])?;
    }
    crate::storage::work::feeds::append_to_project_feed(
        transaction,
        &hold.project_id,
        HOLD_KIND,
        object,
    )?;
    // Exercise the same Work validator that the core's combined authority
    // consumes. Success here proves only the hold, never host completeness.
    let expected = HoldExpectation {
        project: &hold.project_id,
        hold: object.key(),
        prepared: &hold.prepared,
        scope: object.key(),
        association: &hold.association,
        candidate_revision: hold.candidate_revision,
        hold_revision: hold.hold_revision,
        scope_revision: hold.scope_revision,
        owner_set_revision: hold.owner_set_revision,
    };
    let checked = match load_reconciliation_hold_on(transaction, &expected)? {
        ReconciliationHoldCheck::Ready(checked) => checked,
        ReconciliationHoldCheck::Pending(gap) => {
            return Err(invalid(format!("new hold did not validate: {gap:?}")));
        }
    };
    require(
        checked.scope_ref() == object.key()
            && checked.hold() == hold
            && !checked.transaction().is_autocommit(),
        "new hold scope differs",
    )?;
    drop(checked);
    Ok(())
}

impl SqliteStore {
    /// Retain one incoming candidate's complete local scope and fence new
    /// admissions. This grants no execution, claim transfer or host settlement.
    ///
    /// # Errors
    /// Refuses moved scope, cross-candidate records and competing active holds.
    #[cfg_attr(
        not(test),
        expect(
            dead_code,
            reason = "The resolved-candidate admission owner installs holds in the next implementation slice"
        )
    )]
    pub(crate) fn place_task_sync_reconciliation_hold(
        &mut self,
        hold: &ReconciliationHold,
    ) -> Result<ObjectId, StoreError> {
        super::super::validate_task_sync_actor(&hold.actor)?;
        let transaction = self.begin_work_mutation()?;
        let holds = super::lookup::scoped_on::<ReconciliationHold>(
            &transaction,
            HOLD_KIND,
            &hold.project_id.0,
        )?;
        for (id, old) in &holds {
            if old == hold {
                validate_hold(&transaction, id, old)?;
                resolution_on(&transaction, id, old)?;
                return Ok(id.clone());
            }
            if old.project_id == hold.project_id
                && old.prepared == hold.prepared
                && resolution_on(&transaction, id, old)?.is_none()
            {
                return Err(crate::task_sync::TaskSyncRefusal::ReconciliationHeld
                    .invalid("the prepared candidate already has a different active hold"));
            }
        }
        let object = CanonicalObject::mint(hold)?;
        place_on(&transaction, &object)?;
        transaction.commit()?;
        Ok(object.key().clone())
    }

    /// Read a hold after restart without turning it into host authority.
    ///
    /// # Errors
    /// Refuses malformed scope or inconsistent durable run slots.
    pub fn task_sync_reconciliation_hold(
        &self,
        id: &ObjectId,
    ) -> Result<Option<ReconciliationHold>, StoreError> {
        self.work_read_snapshot(|store| {
            let hold: Option<ReconciliationHold> =
                Self::get_typed_object_on(&store.connection, id, HOLD_KIND)?;
            if let Some(hold) = &hold {
                validate_hold(&store.connection, id, hold)?;
                resolution_on(&store.connection, id, hold)?;
            }
            Ok(hold)
        })
    }
}
