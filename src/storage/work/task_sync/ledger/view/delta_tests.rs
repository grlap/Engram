use super::*;
use crate::storage::work::{
    task_sync::tests::{candidate, ledger::binding, state},
    test_support::at,
};
use crate::task_sync::{
    delta::*,
    inventory::InventoryRoot,
    ledger::{BINDING_KIND, RetainedCommit},
};
use std::collections::{BTreeMap, BTreeSet};

struct Fixture {
    _home: crate::test_support::TempHome,
    store: SqliteStore,
    ledger: Ledger,
    binding: ObjectId,
    intent: DeltaIntent,
    prepared: ObjectId,
    outcome: ObjectId,
    event: Event,
}

impl Fixture {
    fn new(count: usize) -> Self {
        let home = crate::test_support::temp_home().unwrap();
        let mut basis = binding(home.path());
        basis.store.origin = Some(ObjectId::mint());
        let store = SqliteStore::open_in_memory().unwrap();
        let object = crate::CanonicalObject::mint(&basis).unwrap();
        let binding = object.key().clone();
        Self::put(&store, BINDING_KIND, &object);
        let mut ledger = Ledger::default();
        ledger.bindings.insert(binding.clone(), basis.clone());
        let activated = Event {
            schema_version: 1,
            binding: binding.clone(),
            sequence: 1,
            predecessor: None,
            actor: basis.actor.clone(),
            recorded_at: at(10),
            transition: Transition::Activated,
        };
        let activation = Self::event(&store, &mut ledger, activated);
        let operation = uuid::Uuid::new_v4();
        let cut = OriginQualifiedCut {
            binding: binding.clone(),
            origin: basis.store.origin.clone().unwrap(),
            restore_occurrence: None,
            position: crate::FeedPosition {
                feed: crate::FeedId::Project(basis.project.clone()),
                position: 0,
            },
            anchor: None,
        };
        let mut template = candidate()
            .tasks
            .into_iter()
            .find(|task| task.lifecycle == crate::WorkLifecycle::Open && task.parent_id.is_none())
            .expect("fixture has an Open root");
        template.external_ref = None;
        template.parent_id = None;
        template.prerequisites.clear();
        template.blockers.clear();
        template.superseded_by = None;
        template.notes.clear();
        template.activity.clear();
        let documents = (0..count)
            .map(|index| {
                let mut task = template.clone();
                task.id = crate::WorkId::new();
                task.title = format!("Selected payload {index}");
                (task.id, Some(task))
            })
            .collect::<BTreeMap<_, _>>();
        let changed = documents.keys().copied().collect::<BTreeSet<_>>();
        let output = OutputMembership {
            inventory_root: documents
                .values()
                .flatten()
                .try_fold(InventoryRoot::ZERO, InventoryRoot::including)
                .unwrap(),
            profile: basis.profile.clone(),
        };
        let payload = DeltaCandidatePayload {
            format: DELTA_PAYLOAD_FORMAT.into(),
            project_id: basis.project.clone(),
            operation,
            output: output.clone(),
            documents,
        };
        let fingerprint = crate::CanonicalObject::freeze(&"Git metadata")
            .unwrap()
            .key()
            .clone();
        let commit = RetainedCommit {
            oid: fingerprint.as_str()[..40].into(),
            tree: fingerprint.as_str()[..40].into(),
            parents: Vec::new(),
            retention_ref: format!("refs/engram/task-sync/exchange/{operation}/commit"),
            raw_bytes_fingerprint: fingerprint.clone(),
        };
        let empty = OutputMembership {
            inventory_root: InventoryRoot::ZERO,
            profile: basis.profile.clone(),
        };
        let intent = DeltaIntent {
            operation,
            key: operation.to_string(),
            direction: Direction::Incoming,
            actor: basis.actor.clone(),
            producer: fingerprint,
            expected_head: None,
            common_commit: None,
            local_cut: cut.clone(),
            basis: DeltaBasis::Incoming {
                source_before: Box::new(IncomingSourceBasis {
                    anchor: IncomingSourceAnchor::Bootstrap {
                        checkpoint: ObjectId::mint(),
                    },
                    snapshot: MembershipSnapshot {
                        commit: commit.clone(),
                        membership: empty.clone(),
                    },
                }),
                source_after: Box::new(MembershipSnapshot {
                    commit,
                    membership: output.clone(),
                }),
                source_changed: changed.clone(),
                local_before: Box::new(DestinationPlanningBasis {
                    baseline: ObjectId::mint(),
                    membership: empty,
                }),
            },
            candidate: RetainedDeltaCandidate {
                operation,
                directory: home.path().join("absent-retained-candidate"),
                content_fingerprint: crate::CanonicalObject::freeze(&payload)
                    .unwrap()
                    .key()
                    .clone(),
                output,
                changed,
            },
        };
        payload.validate(&binding, &basis, &intent).unwrap();
        let preparation = Event {
            schema_version: 1,
            binding: binding.clone(),
            sequence: 2,
            predecessor: Some(activation),
            actor: basis.actor.clone(),
            recorded_at: at(11),
            transition: Transition::DeltaPrepared {
                intent: Box::new(intent.clone()),
                payload: Box::new(payload),
            },
        };
        let prepared = Self::event(&store, &mut ledger, preparation);
        let event = Event {
            schema_version: 1,
            binding: binding.clone(),
            sequence: 3,
            predecessor: Some(prepared.clone()),
            actor: basis.actor,
            recorded_at: at(12),
            transition: Transition::DeltaImportApplied {
                prepared: prepared.clone(),
                receipt: Box::new(DeltaImportReceipt {
                    planning: ObjectId::mint(),
                    after_cut: cut,
                    before: Vec::new(),
                    tasks: Vec::new(),
                    holds: Vec::new(),
                    terminal_evidence: Vec::new(),
                }),
            },
        };
        let outcome = Self::event(&store, &mut ledger, event.clone());
        Self {
            _home: home,
            store,
            ledger,
            binding,
            intent,
            prepared,
            outcome,
            event,
        }
    }

    fn put(store: &SqliteStore, kind: &str, object: &crate::CanonicalObject) {
        store.connection.execute("INSERT INTO objects(object_id,object_kind,canonical_json,created_at) VALUES(?1,?2,?3,'fixture')",
            params![object.key().as_str(),kind,object.bytes()]).unwrap();
    }

    fn event(store: &SqliteStore, ledger: &mut Ledger, event: Event) -> ObjectId {
        // Pure historical lookup fixtures, deliberately without graph admission.
        // They exercise canonical metadata and keyed views, never a writer or
        // a manufactured applied-receipt API.
        let object = crate::CanonicalObject::mint(&event).unwrap();
        Self::put(store, super::super::EVENT_KIND, &object);
        ledger.events.insert(object.key().clone(), event);
        record(&store.connection, ledger, &ledger.events[object.key()]).unwrap();
        object.key().clone()
    }

    fn moved(&mut self) {
        let checkpoint = ManagedCustodyCheckpoint {
            operation: uuid::Uuid::new_v4(),
            key: "later audit".into(),
            request_fingerprint: crate::CanonicalObject::freeze(&"later request")
                .unwrap()
                .key()
                .clone(),
            previous_checkpoint: None,
            local_cut: self.intent.local_cut.clone(),
            reason: CheckpointReason::NamedAudit,
            seed: None,
            artifacts: Vec::new(),
        };
        let event = Event {
            schema_version: 1,
            binding: self.binding.clone(),
            sequence: 4,
            predecessor: Some(self.outcome.clone()),
            actor: self.intent.actor.clone(),
            recorded_at: at(13),
            transition: Transition::CustodyCheckpoint {
                checkpoint: Box::new(checkpoint),
            },
        };
        Self::event(&self.store, &mut self.ledger, event);
    }
}

#[test]
fn delta_receipt_lookup_reads_no_prepared_task_documents_after_later_movement() {
    let mut f = Fixture::new(2_000);
    f.moved();
    let before = state(&f.store);
    READS.with(|count| count.set(0));
    let actual = f
        .store
        .completed_task_sync_delta_import(&f.binding, &f.intent)
        .unwrap()
        .unwrap();
    assert_eq!(
        actual,
        CompletedExchange {
            prepared: f.prepared.clone(),
            outcome: f.outcome.clone(),
            event: f.event.clone()
        }
    );
    // Exactly the binding and compact outcome are decoded. The prepared
    // payload and predecessor checkpoint documents never enter the decoder.
    assert_eq!(READS.with(std::cell::Cell::get), 2);
    assert_eq!(state(&f.store), before);
    assert!(!f.intent.candidate.directory.exists());
    let sql = crate::storage::work::task_sync::closure::membership::head::select_sql();
    let header: String = f
        .store
        .connection
        .query_row(&sql, [f.prepared.as_str()], |row| row.get(0))
        .unwrap();
    assert!(
        header.len() < 8192,
        "ordinary head metadata grew with the old operation's changed-key set: {}",
        header.len()
    );
    let plan = f
        .store
        .connection
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .unwrap()
        .query_map([f.prepared.as_str()], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .join(" ");
    assert!(
        plan.contains("objects_task_sync_membership_checkpoint"),
        "{plan}"
    );
    assert!(!plan.contains("SCAN"), "{plan}");
}

#[test]
fn delta_receipt_lookup_refuses_changed_intent_and_stale_or_missing_links_without_writes() {
    let f = Fixture::new(2);
    let before = state(&f.store);
    let mut changed = f.intent.clone();
    changed.key.push_str(" changed");
    assert!(
        f.store
            .completed_task_sync_delta_import(&f.binding, &changed)
            .is_err()
    );
    changed = f.intent.clone();
    changed.expected_head = Some(
        crate::CanonicalObject::freeze(&"changed head")
            .unwrap()
            .key()
            .as_str()[..40]
            .into(),
    );
    assert!(
        f.store
            .completed_task_sync_delta_import(&f.binding, &changed)
            .is_err()
    );
    assert_eq!(state(&f.store), before);
    f.store
        .connection
        .execute(
            "DELETE FROM work_task_sync_exchange_operations WHERE binding_id=?1",
            [f.binding.as_str()],
        )
        .unwrap();
    let damaged = state(&f.store);
    assert!(
        f.store
            .completed_task_sync_delta_import(&f.binding, &f.intent)
            .is_err()
    );
    assert_eq!(state(&f.store), damaged);

    let f = Fixture::new(1);
    let predecessor = f.ledger.events[&f.prepared].predecessor.as_ref().unwrap();
    f.store
        .connection
        .execute(
            "DELETE FROM objects WHERE object_id=?1",
            [predecessor.as_str()],
        )
        .unwrap();
    let damaged = state(&f.store);
    assert!(
        f.store
            .completed_task_sync_delta_import(&f.binding, &f.intent)
            .is_err()
    );
    assert_eq!(state(&f.store), damaged);
}
