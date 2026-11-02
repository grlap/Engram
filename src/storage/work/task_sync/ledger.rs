//! One bounded discovery of private exchange records per transaction/batch.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Connection, params};

use crate::storage::SqliteStore;
use crate::task_sync::ledger::{
    BINDING_KIND, Binding, Direction, EVENT_KIND, Event, ImportDisposition, ImportReceipt,
    PublicationObservation, Transition, attribution, oid, require,
};
use crate::{CanonicalObject, ObjectId, storage::StoreError};

const MAX_RECORDS: usize = 100_000;
const MAX_BYTES: usize = 128 * 1024 * 1024;
const MAX_RECORD_BYTES: usize = 16 * 1024 * 1024;

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
            && matches!(&event.transition, Transition::Prepared{intent} if intent.direction == Direction::Incoming),
        "hold requires the current prepared incoming exchange",
    )
}

mod operations;

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
        let mut ledger = Ledger::load(&transaction)?;
        if let Some((id, _)) = ledger.bindings.iter().find(|(_, old)| *old == binding) {
            return Ok(id.clone());
        }
        // The repository enrollment is asserted by the caller, so the one
        // active binding per project is also keyed by the actual repository:
        // another enrollment for the same Git directory is a competing head.
        let superseded = ledger
            .bindings
            .values()
            .filter_map(|old| old.supersedes.clone())
            .collect::<BTreeSet<_>>();
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
        ledger.events.insert(event_object.key().clone(), event);
        ledger.validate(&transaction)?;
        Self::insert_object(&transaction, BINDING_KIND, &binding_object)?;
        Self::insert_object(&transaction, EVENT_KIND, &event_object)?;
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
        require(
            self.bindings.len() + self.events.len() + objects.len() <= MAX_RECORDS
                && objects
                    .iter()
                    .all(|object| object.bytes().len() <= MAX_RECORD_BYTES)
                && objects
                    .iter()
                    .try_fold(self.bytes, |total, object| {
                        total.checked_add(object.bytes().len())
                    })
                    .is_some_and(|total| total <= MAX_BYTES),
            "exchange discovery capacity exceeded; preserve retained input",
        )
    }

    fn validate(&self, connection: &Connection) -> Result<(), StoreError> {
        let mut chains = BTreeMap::<ObjectId, BTreeMap<u64, (&ObjectId, &Event)>>::new();
        for (id, event) in &self.events {
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
            let mut previous = None;
            let mut keys = BTreeSet::new();
            let mut operations = BTreeSet::new();
            let mut pending = None;
            let mut stage = Stage::Settled;
            for (index, (event_id, event)) in chain.values().enumerate() {
                require(
                    event.sequence
                        == u64::try_from(index + 1).map_err(|_| {
                            super::invalid("exchange sequence exceeds supported bound")
                        })?
                        && event.predecessor.as_ref() == previous,
                    "exchange predecessor chain is broken",
                )?;
                if index == 0 {
                    require(
                        matches!(event.transition, Transition::Activated)
                            && event.actor == binding.actor,
                        "invalid exchange activation",
                    )?;
                } else {
                    match &event.transition {
                        Transition::Activated => {
                            return Err(super::invalid("duplicate exchange activation"));
                        }
                        Transition::Prepared { intent } => {
                            intent.validate()?;
                            require(
                                stage == Stage::Settled,
                                "new exchange bypasses unresolved operation",
                            )?;
                            require(
                                keys.insert(&intent.key) && operations.insert(intent.operation),
                                "duplicate exchange request key or operation",
                            )?;
                            require(
                                event.actor == intent.actor,
                                "exchange intent attribution differs",
                            )?;
                            pending = Some((*event_id, intent));
                            stage = Stage::Prepared;
                        }
                        transition => {
                            let (prepared, intent) = pending.ok_or_else(|| {
                                super::invalid("exchange transition has no intent")
                            })?;
                            require(
                                event.actor == intent.actor,
                                "exchange transition attribution differs",
                            )?;
                            stage = match transition {
                                Transition::LocalCommitRecorded { prepared: selected } => {
                                    require(
                                        selected == prepared
                                            && intent.direction == Direction::Outgoing
                                            && stage == Stage::Prepared,
                                        "invalid local commit transition",
                                    )?;
                                    Stage::Committed
                                }
                                Transition::ImportApplied {
                                    prepared: selected,
                                    receipt,
                                } => {
                                    require(
                                        selected == prepared
                                            && intent.direction == Direction::Incoming
                                            && stage == Stage::Prepared,
                                        "invalid import transition",
                                    )?;
                                    require(
                                        receipt.source_commit == intent.commit.oid
                                            && receipt.source_tree == intent.commit.tree
                                            && receipt.before_basis == intent.local_planning_basis
                                            && receipt.after_basis
                                                == intent.candidate.planning_fingerprint,
                                        "import receipt differs from intent",
                                    )?;
                                    validate_receipt(connection, binding, receipt)?;
                                    Stage::Settled
                                }
                                Transition::ReconciledImportApplied {
                                    prepared: selected,
                                    receipt,
                                } => {
                                    require(
                                        selected == prepared
                                            && intent.direction == Direction::Incoming
                                            && stage == Stage::Prepared,
                                        "invalid reconciled import transition",
                                    )?;
                                    super::reconciliation::validate_receipt_on(
                                        connection, event_id, binding, intent, receipt,
                                    )?;
                                    Stage::Settled
                                }
                                Transition::PushStarted { prepared: selected } => {
                                    require(
                                        selected == prepared
                                            && intent.direction == Direction::Outgoing
                                            && stage == Stage::Committed,
                                        "invalid push transition",
                                    )?;
                                    Stage::Pushing
                                }
                                Transition::PushObserved {
                                    prepared: selected,
                                    observation,
                                } => {
                                    require(
                                        selected == prepared
                                            && matches!(stage, Stage::Pushing | Stage::Unknown),
                                        "invalid publication observation",
                                    )?;
                                    match observation {
                                        PublicationObservation::Confirmed { observed_tip } => {
                                            oid(observed_tip)?;
                                            require(
                                                observed_tip == &intent.commit.oid,
                                                "publication observation requires the exact retained commit",
                                            )?;
                                            Stage::Settled
                                        }
                                        PublicationObservation::Unknown { observed_tip } => {
                                            if let Some(tip) = observed_tip {
                                                oid(tip)?;
                                            }
                                            Stage::Unknown
                                        }
                                    }
                                }
                                // The outer match handles these; refuse rather than panic.
                                Transition::Activated | Transition::Prepared { .. } => {
                                    return Err(super::invalid("unexpected exchange transition"));
                                }
                            };
                        }
                    }
                }
                previous = Some(*event_id);
            }
            settled.insert(id, stage == Stage::Settled);
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
            if !successors.contains(id) {
                require(
                    heads.insert((&binding.project.0, binding.repository.enrollment)),
                    "competing active exchange bindings",
                )?;
            }
        }
        Ok(())
    }
}

#[derive(Clone, Copy, Eq, PartialEq)]
enum Stage {
    Settled,
    Prepared,
    Committed,
    Pushing,
    Unknown,
}

/// Structural checks only: ids, paths, dispositions and provenance links.
/// Content fingerprints are compared on the write path, never here.
fn validate_receipt(
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

pub(in crate::storage) fn verify_exchange(
    connection: &Connection,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) {
    match Ledger::load(connection) {
        Ok(ledger) => *checked += ledger.bindings.len() + ledger.events.len(),
        Err(error) => invalid.push(format!("task_sync_exchange:{error}")),
    }
}
