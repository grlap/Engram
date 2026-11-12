//! One bounded discovery of private exchange records per transaction/batch.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Connection, params};

use crate::storage::SqliteStore;
use crate::task_sync::ledger::{
    BINDING_KIND, Binding, Direction, EVENT_KIND, Event, ImportDisposition, ImportReceipt,
    PublicationObservation, Transition, attribution, oid, require,
};
use crate::{CanonicalObject, ObjectId, storage::StoreError};

pub(super) const MAX_RECORDS: usize = 100_000;
const MAX_BYTES: usize = 128 * 1024 * 1024;
pub(super) const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;

#[cfg(test)]
pub(super) fn validate_exchange_on(connection: &Connection) -> Result<(), StoreError> {
    Ledger::load(connection).map(|_| ())
}

pub(super) fn require_pending_incoming_on(
    connection: &Connection,
    binding: &ObjectId,
    prepared: &ObjectId,
) -> Result<(), StoreError> {
    let ledger = Ledger::load(connection)?;
    let (id, event) = ledger
        .events
        .iter()
        .filter(|(_, event)| &event.binding == binding)
        .max_by_key(|(_, event)| event.sequence)
        .ok_or_else(|| super::invalid("incoming exchange is missing"))?;
    require(
        id == prepared
            && (matches!(&event.transition, Transition::Prepared{intent} if intent.direction == Direction::Incoming)
                || matches!(&event.transition, Transition::DeltaPrepared{intent, ..} if intent.direction == Direction::Incoming)),
        "hold requires the current prepared incoming exchange",
    )
}

mod audit;
mod capacity;
mod coverage;
pub(crate) mod custody;
#[cfg(test)]
mod custody_tests;
mod delta;

pub(super) fn check_local_store_on(
    connection: &Connection,
    binding: &Binding,
) -> Result<(), StoreError> {
    delta::local_store(connection, binding)
}
mod evidence;
mod operations;
pub(in crate::storage::work) mod recovery;
mod reduce;
pub(in crate::storage::work) use recovery::{
    check_origin_recovery_on, origin_recovery_on, recovery_bindings_on,
};
pub(in crate::storage::work) mod view;

pub(super) fn completed_evidence_on(
    connection: &Connection,
    binding: &ObjectId,
    intent: &crate::task_sync::ledger::EvidenceIntent,
) -> Result<Option<crate::task_sync::ledger::CompletedExchange>, StoreError> {
    view::evidence::lookup(connection, binding, intent)
}

pub(crate) fn origin_repositories_on(
    connection: &Connection,
    project: &crate::ProjectId,
) -> Result<BTreeSet<std::path::PathBuf>, StoreError> {
    let mut budget = view::read::ReadBudget::default();
    view::select::bindings(connection, view::select::Bindings::Project(project))?
        .into_iter()
        .filter_map(|id| {
            let selected = (|| {
                let binding = view::read::binding_on(connection, &id, &mut budget)?;
                if recovery::quarantined(connection, &id, &binding)? {
                    return Ok(None);
                }
                require(
                    &binding.project == project,
                    "origin repository binding project differs",
                )?;
                Ok(Some(binding.repository.git_directory))
            })();
            selected.transpose()
        })
        .collect()
}

pub(super) fn check_origin_custody_on(
    connection: &Connection,
    project: &crate::ProjectId,
    observed: &BTreeMap<std::path::PathBuf, custody::Observed>,
    admitted_operation: Option<uuid::Uuid>,
    admitted_request: Option<&ObjectId>,
    audit: Option<&crate::task_sync::origin::private::PendingAudit>,
) -> Result<(), StoreError> {
    let mut budget = view::read::ReadBudget::default();
    for id in view::select::bindings(connection, view::select::Bindings::Project(project))? {
        let binding = view::read::binding_on(connection, &id, &mut budget)?;
        if recovery::quarantined(connection, &id, &binding)? {
            continue;
        }
        require(
            &binding.project == project && observed.contains_key(&binding.repository.git_directory),
            "origin Git custody basis changed before writer admission",
        )?;
    }
    for (root, observation) in observed {
        custody::check_on(
            connection,
            root,
            observation,
            admitted_operation,
            admitted_request,
            audit,
            &mut budget,
        )?;
    }
    Ok(())
}

impl SqliteStore {
    /// Activate an immutable private binding without changing work/control feeds.
    /// An explicit successor must name the old binding and its exact settled tail.
    ///
    /// # Errors
    /// Refuses malformed attribution, competing activation and unresolved progress.
    pub fn activate_task_sync_binding(
        &mut self,
        binding: &Binding,
    ) -> Result<ObjectId, StoreError> {
        binding.validate()?;
        crate::storage::admit_live_actor_session(&binding.actor)?;
        let request = origin_request("activate", binding, None, None, None)?;
        super::producing_origin::with_binding_activation(
            self,
            &binding.project,
            &binding.repository.git_directory,
            &request,
            |store, guard| store.activate_task_sync_binding_checked(binding, guard, &request),
        )
    }

    fn activate_task_sync_binding_checked(
        &mut self,
        binding: &Binding,
        guard: Option<&crate::task_sync::origin::continuity::PreparedContinuity>,
        request: &ObjectId,
    ) -> Result<ObjectId, StoreError> {
        binding.validate()?;
        crate::storage::admit_live_actor_session(&binding.actor)?;
        let database = self
            .connection
            .path()
            .filter(|path| !path.is_empty())
            .ok_or_else(|| super::invalid("durable exchange requires a file-backed store"))?;
        let opened = std::fs::canonicalize(database)
            .map_err(|_| super::invalid("cannot resolve exchange store path"))?;
        let declared = std::fs::canonicalize(&binding.store.database)
            .map_err(|_| super::invalid("cannot resolve declared exchange store path"))?;
        require(
            opened == declared,
            "exchange binding names a different store",
        )?;
        let transaction = self.begin_work_mutation()?;
        super::producing_origin::check_exchange_on(&transaction, &binding.project, guard, request)?;
        if let Some(guard) = guard {
            super::producing_origin::check_cut_on(
                &transaction,
                &binding.project,
                &binding.store.cut,
            )?;
            require(
                binding.store.origin.as_ref() == guard.state().witness.as_ref().map(|w| &w.origin),
                "exchange binding must name the checked producing origin",
            )?;
        } else {
            require(
                binding.store.origin.is_none(),
                "exchange binding claims an unenrolled origin",
            )?;
        }
        let mut ledger = Ledger::load(&transaction)?;
        if let Some((id, _)) = ledger.bindings.iter().find(|(_, old)| *old == binding) {
            return Ok(id.clone());
        }
        // The repository enrollment is asserted by the caller, so the one
        // active binding per project is also keyed by the actual repository:
        // another enrollment for the same Git directory is a competing head.
        let mut superseded = ledger
            .bindings
            .values()
            .filter_map(|old| old.supersedes.clone())
            .collect::<BTreeSet<_>>();
        for (id, old) in &ledger.bindings {
            if recovery::quarantined(&transaction, id, old)? {
                superseded.insert(id.clone());
            }
        }
        require(
            binding.store.origin.is_none()
                || !ledger.bindings.iter().any(|(id, old)| {
                    !superseded.contains(id)
                        && binding.supersedes.as_ref() != Some(id)
                        && old.project == binding.project
                        && old.store.origin == binding.store.origin
                }),
            "origin has competing exchange bindings",
        )?;
        require(
            !ledger.bindings.iter().any(|(id, old)| {
                !superseded.contains(id)
                    && binding.supersedes.as_ref() != Some(id)
                    && old.project == binding.project
                    && old.repository.enrollment != binding.repository.enrollment
                    && same_directory(
                        &old.repository.git_directory,
                        &binding.repository.git_directory,
                    )
            }),
            "competing active exchange bindings for one repository",
        )?;
        let binding_object = CanonicalObject::mint(binding)?;
        let event = Event {
            schema_version: 1,
            binding: binding_object.key().clone(),
            sequence: 1,
            predecessor: None,
            actor: binding.actor.clone(),
            recorded_at: binding.created_at,
            transition: Transition::Activated,
        };
        let event_object = CanonicalObject::mint(&event)?;
        ledger.capacity(&[&binding_object, &event_object])?;
        ledger
            .bindings
            .insert(binding_object.key().clone(), binding.clone());
        ledger
            .events
            .insert(event_object.key().clone(), event.clone());
        ledger.validate(&transaction)?;
        Self::insert_object(&transaction, BINDING_KIND, &binding_object)?;
        Self::insert_object(&transaction, EVENT_KIND, &event_object)?;
        view::record(&transaction, &ledger, &event)?;
        transaction.commit()?;
        Ok(binding_object.key().clone())
    }

    /// Read one validated private binding; opaque legacy records are never rewritten.
    ///
    /// # Errors
    /// Refuses an inconsistent or oversized exchange ledger.
    pub fn task_sync_binding(&self, id: &ObjectId) -> Result<Option<Binding>, StoreError> {
        self.work_read_snapshot(|store| Ok(Ledger::load(&store.connection)?.bindings.remove(id)))
    }

    /// Read a binding's attributed progress in its explicit sequence order.
    /// Object identities and timestamps are not used as ordering cursors.
    ///
    /// # Errors
    /// Refuses an inconsistent or oversized exchange ledger.
    pub fn task_sync_exchange_events(
        &self,
        binding: &ObjectId,
    ) -> Result<Vec<(ObjectId, Event)>, StoreError> {
        self.work_read_snapshot(|store| {
            let ledger = Ledger::load(&store.connection)?;
            require(
                ledger.bindings.contains_key(binding),
                "exchange binding is missing",
            )?;
            let mut events = ledger
                .events
                .into_iter()
                .filter(|(_, event)| &event.binding == binding)
                .collect::<Vec<_>>();
            events.sort_by_key(|(_, event)| event.sequence);
            Ok(events)
        })
    }
}

#[derive(Default)]
struct Ledger {
    bindings: BTreeMap<ObjectId, Binding>,
    events: BTreeMap<ObjectId, Event>,
    bytes: usize,
}

impl Ledger {
    fn load(connection: &Connection) -> Result<Self, StoreError> {
        let ledger = Self::load_canonical(connection)?;
        view::verify(connection, &ledger)?;
        Ok(ledger)
    }

    fn load_canonical(connection: &Connection) -> Result<Self, StoreError> {
        let mut ledger = Self::default();
        let mut count = 0_usize;
        let mut total = 0_usize;
        let mut statement = connection.prepare(
            "SELECT object_id, object_kind, canonical_json, created_at FROM objects
             WHERE object_kind IN (?1, ?2)",
        )?;
        let mut rows = statement.query(params![BINDING_KIND, EVENT_KIND])?;
        while let Some(row) = rows.next()? {
            let id = ObjectId::from_stored(row.get::<_, String>(0)?)
                .ok_or_else(|| super::invalid("invalid exchange object id"))?;
            let kind: String = row.get(1)?;
            let bytes = row
                .get_ref(2)?
                .as_blob()
                .map_err(|_| super::invalid("exchange body is not a blob"))?;
            count += 1;
            total = total
                .checked_add(bytes.len())
                .ok_or_else(|| super::invalid("exchange discovery exceeds byte bound"))?;
            require(
                count <= MAX_RECORDS && total <= MAX_BYTES && bytes.len() <= MAX_RECORD_BYTES,
                "exchange discovery exceeds supported bound; preserve all records",
            )?;
            let object = CanonicalObject::stored(&id, bytes.to_vec())?;
            if kind == BINDING_KIND {
                let binding: Binding = object.decode()?;
                binding.validate()?;
                ledger.bindings.insert(id, binding);
            } else {
                let event: Event = object.decode()?;
                require(
                    event.schema_version == 1,
                    "unsupported exchange event schema",
                )?;
                attribution(&event.actor)?;
                require(event.sequence > 0, "invalid exchange sequence")?;
                ledger.events.insert(id, event);
            }
        }
        ledger.bytes = total;
        ledger.validate(connection)?;
        Ok(ledger)
    }

    fn capacity(&self, objects: &[&CanonicalObject]) -> Result<(), StoreError> {
        capacity::require(self.bindings.len() + self.events.len(), self.bytes, objects)
    }

    fn validate(&self, connection: &Connection) -> Result<(), StoreError> {
        let mut chains = BTreeMap::<ObjectId, BTreeMap<u64, (&ObjectId, &Event)>>::new();
        for (id, event) in &self.events {
            audit::retained::validate_checkpoint_on(connection, id, event)?;
            require(
                self.bindings.contains_key(&event.binding),
                "exchange binding is missing",
            )?;
            require(
                chains
                    .entry(event.binding.clone())
                    .or_default()
                    .insert(event.sequence, (id, event))
                    .is_none(),
                "exchange sequence forks",
            )?;
        }
        let mut settled = BTreeMap::new();
        for (id, binding) in &self.bindings {
            let chain = chains
                .get(id)
                .ok_or_else(|| super::invalid("exchange activation is missing"))?;
            let chain = chain.values().copied().collect::<Vec<_>>();
            let view = reduce::reduce(Some(connection), binding, &chain)?;
            settled.insert(id, view.head.stage == reduce::Stage::Settled);
        }
        let mut successors = BTreeSet::new();
        let mut heads = BTreeSet::new();
        for (id, binding) in &self.bindings {
            if let Some(previous) = &binding.supersedes {
                let old = self
                    .bindings
                    .get(previous)
                    .ok_or_else(|| super::invalid("superseded exchange binding is missing"))?;
                require(
                    old.project == binding.project
                        && old.repository.enrollment == binding.repository.enrollment,
                    "superseded exchange scope differs",
                )?;
                require(
                    successors.insert(previous),
                    "exchange binding activation forks",
                )?;
                require(
                    settled.get(previous) == Some(&true),
                    "binding bypasses unresolved exchange",
                )?;
                require(
                    chains
                        .get(previous)
                        .and_then(|chain| chain.last_key_value())
                        .map(|(_, (id, _))| *id)
                        == binding.superseded_tail.as_ref(),
                    "superseded binding tail changed",
                )?;
                let mut cursor = Some(id);
                let mut visited = BTreeSet::new();
                while let Some(current) = cursor {
                    require(visited.insert(current), "exchange binding cycle")?;
                    cursor = self
                        .bindings
                        .get(current)
                        .and_then(|value| value.supersedes.as_ref());
                }
            }
        }
        for (id, binding) in &self.bindings {
            if !successors.contains(id) && !recovery::quarantined(connection, id, binding)? {
                require(
                    heads.insert((&binding.project.0, binding.repository.enrollment)),
                    "competing active exchange bindings",
                )?;
            }
        }
        delta::validate_planning_owners_on(connection, self)?;
        Ok(())
    }
}

/// Selected canonical tail and operation facts, not a whole-ledger audit.
fn origin_state_on(
    connection: &Connection,
    project: &crate::ProjectId,
    origin: &ObjectId,
) -> Result<
    Option<(
        Binding,
        view::read::CheckedTail,
        crate::task_sync::origin::private::ExchangeAnchor,
    )>,
    StoreError,
> {
    let mut budget = view::read::ReadBudget::default();
    let mut selected = None;
    for id in view::select::bindings(connection, view::select::Bindings::Origin(project, origin))? {
        let binding = view::read::binding_on(connection, &id, &mut budget)?;
        require(
            &binding.project == project && binding.store.origin.as_ref() == Some(origin),
            "origin exchange binding scope differs",
        )?;
        if recovery::quarantined(connection, &id, &binding)? {
            continue;
        }
        if let Some(successor) = view::select::successor(connection, &id)? {
            let next = view::read::binding_on(connection, &successor, &mut budget)?;
            let tail = view::read::tail_on(connection, &id, &binding, &mut budget)?;
            require(
                next.project == binding.project
                    && next.repository.enrollment == binding.repository.enrollment
                    && next.supersedes.as_ref() == Some(&id),
                "superseded exchange scope differs",
            )?;
            require(
                settled_tail(&tail.event),
                "binding bypasses unresolved exchange",
            )?;
            require(
                next.superseded_tail.as_ref() == Some(&tail.id),
                "superseded binding tail changed",
            )?;
            continue;
        }
        require(selected.is_none(), "origin has competing exchange bindings")?;
        let tail = view::read::tail_on(connection, &id, &binding, &mut budget)?;
        if let Some(previous) = &binding.supersedes {
            let prior = view::read::binding_on(connection, previous, &mut budget)?;
            let prior_tail = view::read::tail_on(connection, previous, &prior, &mut budget)?;
            require(
                prior.project == binding.project
                    && prior.repository.enrollment == binding.repository.enrollment,
                "superseded exchange scope differs",
            )?;
            require(
                view::select::successor(connection, previous)?.as_ref() == Some(&id)
                    && binding.superseded_tail.as_ref() == Some(&prior_tail.id),
                "superseded binding tail changed",
            )?;
            require(
                settled_tail(&prior_tail.event),
                "binding bypasses unresolved exchange",
            )?;
        }
        let cut = if let Transition::CustodyCheckpoint { checkpoint } = &tail.event.transition {
            crate::task_sync::progress::ExportCut {
                position: checkpoint.local_cut.position.position,
                anchor: checkpoint.local_cut.anchor.clone(),
            }
        } else {
            tail.operation.as_ref().map_or_else(
                || binding.store.cut.clone(),
                |operation| operation.cut(&tail.event),
            )
        };
        let anchor = crate::task_sync::origin::private::ExchangeAnchor {
            binding: id,
            head: tail.id.clone(),
            sequence: tail.event.sequence,
            cut,
        };
        selected = Some((binding, tail, anchor));
    }
    Ok(selected)
}

pub(super) fn origin_anchor_on(
    connection: &Connection,
    project: &crate::ProjectId,
    origin: &ObjectId,
) -> Result<Option<crate::task_sync::origin::private::ExchangeAnchor>, StoreError> {
    Ok(origin_state_on(connection, project, origin)?.map(|(_, _, anchor)| anchor))
}

fn settled_tail(event: &Event) -> bool {
    matches!(
        event.transition,
        Transition::Activated
            | Transition::CustodyCheckpoint { .. }
            | Transition::ImportApplied { .. }
            | Transition::ReconciledImportApplied { .. }
            | Transition::DeltaImportApplied { .. }
            | Transition::PlanningDeferred { .. }
            | Transition::PlanningSuccessorApplied { .. }
            | Transition::EvidenceImportApplied { .. }
            | Transition::EvidenceStopped { .. }
            | Transition::PushObserved {
                observation: PublicationObservation::Confirmed { .. },
                ..
            }
    ) || matches!(
        &event.transition,
        Transition::OutgoingReconciled { reconciliation, .. }
            if matches!(reconciliation.as_ref(), crate::task_sync::ledger::OutgoingReconciliation::Stopped { .. })
    )
}

pub(super) fn require_project_settled_on(
    connection: &Connection,
    project: &crate::ProjectId,
) -> Result<(), StoreError> {
    let mut budget = view::read::ReadBudget::default();
    for id in view::select::bindings(connection, view::select::Bindings::Project(project))? {
        let binding = view::read::binding_on(connection, &id, &mut budget)?;
        require(
            &binding.project == project,
            "origin exchange binding scope differs",
        )?;
        let tail = view::read::tail_on(connection, &id, &binding, &mut budget)?;
        require(
            settled_tail(&tail.event),
            "origin enrollment cannot bypass unresolved exchange; retain original custody",
        )?;
    }
    Ok(())
}

pub(super) fn origin_request(
    action: &str,
    binding: &Binding,
    id: Option<&ObjectId>,
    intent: Option<&crate::task_sync::ledger::Intent>,
    progress: Option<&crate::task_sync::ledger::OutgoingProgress>,
) -> Result<ObjectId, StoreError> {
    Ok(CanonicalObject::freeze(&serde_json::json!({
        "action": action, "binding": binding, "binding_id": id,
        "intent": intent, "progress": progress,
    }))?
    .key()
    .clone())
}

/// Only one actual committed operation matching the retained exact request
/// can recover a stale private tail. Arbitrary later heads never count.
pub(super) fn matches_origin_recovery_on(
    connection: &Connection,
    project: &crate::ProjectId,
    origin: &ObjectId,
    before: Option<&crate::task_sync::origin::private::ExchangeAnchor>,
    request: &ObjectId,
) -> Result<bool, StoreError> {
    let Some((binding, tail, actual)) = origin_state_on(connection, project, origin)? else {
        return Ok(false);
    };
    let event = &tail.event;
    if let Transition::PlanningSuccessorApplied { prepared, .. } = &event.transition {
        let Some(before) = before else {
            return Ok(false);
        };
        let Some(operation) = tail
            .operation
            .as_ref()
            .filter(|operation| &operation.prepared == prepared)
        else {
            return Ok(false);
        };
        let view::read::CheckedRequest::Successor(recipe) = &operation.intent else {
            return Ok(false);
        };
        let Some((_, preparation)) = operation.events.iter().find(|(id, _)| id == prepared) else {
            return Ok(false);
        };
        let Transition::PlanningSuccessorPrepared { payload, .. } = &preparation.transition else {
            return Ok(false);
        };
        let retained = crate::task_sync::retention::successor::RetainedSuccessor {
            binding: actual.binding.clone(),
            recipe: recipe.as_ref().clone(),
            payload: payload.as_ref().clone(),
        };
        return Ok(actual.binding == before.binding
            && preparation.predecessor.as_ref() == Some(&before.head)
            && before.sequence.checked_add(1) == Some(preparation.sequence)
            && retained
                .prepared_event(&binding)
                .map_err(|error| super::invalid(error.to_string()))?
                == *preparation
            && retained
                .matches_outcome(&binding, event)
                .map_err(|error| super::invalid(error.to_string()))?
            && &retained
                .request_fingerprint()
                .map_err(|error| super::invalid(error.to_string()))?
                == request);
    }
    if let Transition::EvidenceImportApplied { prepared, .. }
    | Transition::EvidenceStopped { prepared, .. } = &event.transition
    {
        let Some(before) = before else {
            return Ok(false);
        };
        let Some(operation) = tail
            .operation
            .as_ref()
            .filter(|operation| &operation.prepared == prepared)
        else {
            return Ok(false);
        };
        let view::read::CheckedRequest::Evidence(_) = &operation.intent else {
            return Ok(false);
        };
        let Some((_, preparation)) = operation.events.iter().find(|(id, _)| id == prepared) else {
            return Ok(false);
        };
        let recipe = crate::task_sync::ledger::EvidenceImportRecipe::recover(
            prepared.clone(),
            preparation.clone(),
            tail.id.clone(),
            event,
        )?;
        recipe.validate(&actual.binding, &binding)?;
        return Ok(actual.binding == before.binding
            && preparation.predecessor.as_ref() == Some(&before.head)
            && before.sequence.checked_add(1) == Some(preparation.sequence)
            && &crate::task_sync::retention::evidence::request_fingerprint(&recipe)
                .map_err(|error| super::invalid(error.to_string()))?
                == request);
    }
    if let Transition::DeltaImportApplied { prepared, .. }
    | Transition::PlanningDeferred { prepared, .. } = &event.transition
    {
        let Some(before) = before else {
            return Ok(false);
        };
        let Some(operation) = tail
            .operation
            .as_ref()
            .filter(|operation| &operation.prepared == prepared)
        else {
            return Ok(false);
        };
        let view::read::CheckedRequest::Delta(intent) = &operation.intent else {
            return Ok(false);
        };
        let Some((_, preparation)) = operation.events.iter().find(|(id, _)| id == prepared) else {
            return Ok(false);
        };
        return Ok(actual.binding == before.binding
            && preparation.predecessor.as_ref() == Some(&before.head)
            && before.sequence.checked_add(1) == Some(preparation.sequence)
            && preparation.sequence.checked_add(1) == Some(event.sequence)
            && event.predecessor.as_ref() == Some(prepared)
            && &crate::task_sync::retention::delta::request_fingerprint(&actual.binding, intent)
                .map_err(|error| super::invalid(error.to_string()))?
                == request);
    }
    let follows = match (before, &event.transition) {
        // The first binding under a newly forked origin may explicitly name
        // the prior origin's settled binding. Ledger::validate checks that
        // predecessor/tail; the exact retained request still must match below.
        (None, Transition::Activated) => event.sequence == 1,
        (Some(before), Transition::Activated) => {
            binding.supersedes.as_ref() == Some(&before.binding)
                && binding.superseded_tail.as_ref() == Some(&before.head)
                && event.sequence == 1
        }
        (Some(before), _) => {
            actual.binding == before.binding
                && event.predecessor.as_ref() == Some(&before.head)
                && before.sequence.checked_add(1) == Some(event.sequence)
        }
        _ => false,
    };
    if !follows {
        return Ok(false);
    }
    if let Transition::CustodyCheckpoint { checkpoint } = &event.transition {
        return Ok(&checkpoint.request_fingerprint == request);
    }
    if let Transition::DeltaPrepared { intent, .. } = &event.transition {
        // Outgoing preparation commits one event. Incoming delta application
        // retains its atomic preparation + outcome recovery rule above.
        return Ok(intent.direction == Direction::Outgoing
            && &crate::task_sync::retention::delta::request_fingerprint(&actual.binding, intent)
                .map_err(|error| super::invalid(error.to_string()))?
                == request);
    }
    if let Transition::DeltaLocalCommitRecorded { prepared, commit } = &event.transition {
        let Some(operation) = tail
            .operation
            .as_ref()
            .filter(|operation| &operation.prepared == prepared)
        else {
            return Ok(false);
        };
        let view::read::CheckedRequest::Delta(intent) = &operation.intent else {
            return Ok(false);
        };
        return Ok(
            &crate::task_sync::retention::delta::commit_request_fingerprint(
                &actual.binding,
                intent,
                &crate::task_sync::delta::DeltaCommitSelection {
                    prepared: prepared.clone(),
                    commit: commit.clone(),
                },
            )
            .map_err(|error| super::invalid(error.to_string()))?
                == request,
        );
    }
    if let Some(operation) = &tail.operation
        && let view::read::CheckedRequest::Delta(intent) = &operation.intent
    {
        use crate::task_sync::ledger::OutgoingProgress;
        if let Some(recipe) =
            operation
                .events
                .iter()
                .find_map(|(_, candidate)| match &candidate.transition {
                    Transition::OutgoingReconciled { reconciliation, .. }
                        if matches!(
                            reconciliation.as_ref(),
                            crate::task_sync::ledger::OutgoingReconciliation::DeltaResolved { .. }
                        ) && candidate.sequence < event.sequence =>
                    {
                        Some(reconciliation)
                    }
                    _ => None,
                })
        {
            let progress = match &event.transition {
                Transition::OutgoingReconciled { reconciliation, .. } => {
                    match reconciliation.as_ref() {
                        crate::task_sync::ledger::OutgoingReconciliation::PushStarted {
                            ..
                        } => OutgoingProgress::PushStarted,
                        crate::task_sync::ledger::OutgoingReconciliation::Stopped { .. } => {
                            OutgoingProgress::Reconciled {
                                prepared: operation.prepared.clone(),
                                reconciliation: reconciliation.clone(),
                            }
                        }
                        _ => return Ok(false),
                    }
                }
                Transition::PushRejected { push_started, .. } => OutgoingProgress::PushRejected {
                    push_started: push_started.clone(),
                },
                Transition::PushObserved { observation, .. } => {
                    OutgoingProgress::Observed(observation.clone())
                }
                _ => return Ok(false),
            };
            let Some(commit) = operation
                .events
                .iter()
                .find_map(|(_, candidate)| match &candidate.transition {
                    Transition::DeltaLocalCommitRecorded { prepared, commit }
                        if prepared == &operation.prepared =>
                    {
                        Some(commit)
                    }
                    _ => None,
                })
            else {
                return Ok(false);
            };
            return Ok(
                &crate::task_sync::retention::delta::retry_request_fingerprint(
                    &actual.binding,
                    intent,
                    &crate::task_sync::delta::DeltaCommitSelection {
                        prepared: operation.prepared.clone(),
                        commit: commit.clone(),
                    },
                    recipe,
                    &progress,
                )
                .map_err(|error| super::invalid(error.to_string()))?
                    == request,
            );
        }
        let progress =
            match &event.transition {
                Transition::OutgoingReconciled { reconciliation, .. }
                    if matches!(
                        reconciliation.as_ref(),
                        crate::task_sync::ledger::OutgoingReconciliation::DeltaResolved { .. }
                    ) =>
                {
                    let Some(commit) = operation.events.iter().find_map(|(_, event)| match &event
                        .transition
                    {
                        Transition::DeltaLocalCommitRecorded { prepared, commit }
                            if prepared == &operation.prepared =>
                        {
                            Some(commit)
                        }
                        _ => None,
                    }) else {
                        return Ok(false);
                    };
                    return Ok(
                        &crate::task_sync::retention::delta::resolution_request_fingerprint(
                            &actual.binding,
                            intent,
                            &crate::task_sync::delta::DeltaCommitSelection {
                                prepared: operation.prepared.clone(),
                                commit: commit.clone(),
                            },
                            reconciliation,
                        )
                        .map_err(|error| super::invalid(error.to_string()))?
                            == request,
                    );
                }
                Transition::PushStarted { .. } => OutgoingProgress::PushStarted,
                Transition::PushRejected { push_started, .. } => OutgoingProgress::PushRejected {
                    push_started: push_started.clone(),
                },
                Transition::PushObserved { observation, .. } => {
                    OutgoingProgress::Observed(observation.clone())
                }
                _ => return Ok(false),
            };
        let Some(commit) = operation
            .events
            .iter()
            .find_map(|(_, event)| match &event.transition {
                Transition::DeltaLocalCommitRecorded { prepared, commit }
                    if prepared == &operation.prepared =>
                {
                    Some(commit)
                }
                _ => None,
            })
        else {
            return Ok(false);
        };
        return Ok(
            &crate::task_sync::retention::delta::progress_request_fingerprint(
                &actual.binding,
                intent,
                &crate::task_sync::delta::DeltaCommitSelection {
                    prepared: operation.prepared.clone(),
                    commit: commit.clone(),
                },
                &progress,
            )
            .map_err(|error| super::invalid(error.to_string()))?
                == request,
        );
    }
    let (action, intent, progress) = match &event.transition {
        Transition::Activated => ("activate", None, None),
        Transition::Prepared { intent } => ("prepare", Some(intent.as_ref()), None),
        transition => {
            use crate::task_sync::ledger::OutgoingProgress;
            let (action, prepared, progress) = match transition {
                Transition::ImportApplied { prepared, .. } => ("apply", prepared, None),
                Transition::LocalCommitRecorded { prepared } => (
                    "progress",
                    prepared,
                    Some(OutgoingProgress::LocalCommitRecorded),
                ),
                Transition::PushStarted { prepared } => {
                    ("progress", prepared, Some(OutgoingProgress::PushStarted))
                }
                Transition::PushRejected {
                    prepared,
                    push_started,
                } => (
                    "progress",
                    prepared,
                    Some(OutgoingProgress::PushRejected {
                        push_started: push_started.clone(),
                    }),
                ),
                Transition::PushObserved {
                    prepared,
                    observation,
                } => (
                    "progress",
                    prepared,
                    Some(OutgoingProgress::Observed(observation.clone())),
                ),
                Transition::OutgoingReconciled {
                    prepared,
                    reconciliation,
                } => (
                    "progress",
                    prepared,
                    Some(OutgoingProgress::Reconciled {
                        prepared: prepared.clone(),
                        reconciliation: reconciliation.clone(),
                    }),
                ),
                _ => return Ok(false),
            };
            let Some(intent) = tail
                .operation
                .as_ref()
                .filter(|operation| &operation.prepared == prepared)
                .and_then(view::read::CheckedOperation::progress_intent)
            else {
                return Ok(false);
            };
            (action, Some(intent), progress)
        }
    };
    let id = if action == "activate" {
        None
    } else {
        Some(&actual.binding)
    };
    Ok(&origin_request(action, &binding, id, intent, progress.as_ref())? == request)
}

/// Structural checks only: ids, paths, dispositions and provenance links.
/// Content fingerprints are compared on the write path, never here.
pub(super) fn validate_receipt(
    connection: &Connection,
    binding: &Binding,
    receipt: &ImportReceipt,
) -> Result<(), StoreError> {
    oid(&receipt.source_commit)?;
    oid(&receipt.source_tree)?;
    let mut seen = BTreeSet::new();
    for task in &receipt.tasks {
        require(
            seen.insert(task.task.0)
                && task.source_task_path == format!("tasks/{}.json", task.task.0),
            "invalid receipt task path or duplicate task",
        )?;
        require(
            match task.disposition {
                ImportDisposition::Created => task.before_basis.is_none(),
                ImportDisposition::ExactRepeat => {
                    task.before_basis.as_ref() == Some(&task.after_basis)
                }
                ImportDisposition::Reconciled => false,
            },
            "invalid import disposition basis",
        )?;
        require(
            task.local_record_ids.len() == 2,
            "receipt requires original origin and restored record",
        )?;
        let origin: super::ImportOrigin = crate::storage::SqliteStore::get_typed_object_on(
            connection,
            &task.local_record_ids[0],
            super::ORIGIN_KIND,
        )?
        .ok_or_else(|| super::invalid("receipt import origin is missing"))?;
        require(
            origin.project_id == binding.project
                && origin.task.id == task.task
                && origin.record_id == task.local_record_ids[1],
            "receipt provenance differs",
        )?;
        require(
            origin.notes_included == receipt.notes_included,
            "receipt note profile differs",
        )?;
    }
    Ok(())
}

/// Recomputes a receipt's content fingerprints with the running build. Only
/// the write path calls this, for the receipt it is about to commit: an older
/// stored receipt keeps the fingerprints its own build computed, and a later
/// change to task-document encoding must not make it look corrupt.
pub(super) fn verify_receipt_content(
    connection: &Connection,
    binding: &Binding,
    receipt: &ImportReceipt,
) -> Result<(), StoreError> {
    // The structural checks run first, so every task names exactly its two
    // record ids before they are read here.
    validate_receipt(connection, binding, receipt)?;
    let mut tasks = Vec::new();
    for task in &receipt.tasks {
        let origin: super::ImportOrigin = crate::storage::SqliteStore::get_typed_object_on(
            connection,
            &task.local_record_ids[0],
            super::ORIGIN_KIND,
        )?
        .ok_or_else(|| super::invalid("receipt import origin is missing"))?;
        require(
            CanonicalObject::freeze(&origin.task)?.key() == &task.after_basis,
            "receipt task basis differs from original import",
        )?;
        tasks.push(origin.task);
    }
    let tree = crate::task_sync::TaskTree {
        project: crate::task_sync::ProjectDocument {
            format: crate::task_sync::FORMAT.into(),
            project_id: binding.project.clone(),
            notes_included: receipt.notes_included,
        },
        tasks,
    };
    let basis = crate::task_sync::progress::documents_fingerprint(&tree.documents()?);
    require(
        receipt.after_basis.as_str() == basis,
        "receipt omits or changes admitted task content",
    )
}

/// Whether two recorded Git directories name the same directory now. A path
/// that cannot be resolved names no directory this check can compare.
pub(super) fn same_directory(left: &std::path::Path, right: &std::path::Path) -> bool {
    left == right
        || matches!(
            (std::fs::canonicalize(left), std::fs::canonicalize(right)),
            (Ok(left), Ok(right)) if left == right
        )
}

// Repair and doctor describe the same canonical failure identically, without
// reflecting values from malformed JSON into operator-facing labels.
fn failure_label(error: &StoreError) -> String {
    let reason =
        crate::storage::undecodable_record_reason(error).unwrap_or_else(|| error.to_string());
    format!("task_sync_exchange:{reason}")
}

pub(in crate::storage) fn verify_exchange(
    connection: &Connection,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) {
    match recovery::projection::verify(connection) {
        Ok(count) => *checked += count,
        Err(error) => invalid.push(failure_label(&error)),
    }
    match Ledger::load(connection) {
        Ok(ledger) => *checked += ledger.bindings.len() + ledger.events.len(),
        Err(error) => invalid.push(failure_label(&error)),
    }
}
