//! Outgoing inspection observes selected native bytes; it admits no write.

use super::*;
use crate::task_sync::{delta::*, retention};
use std::collections::{BTreeMap, BTreeSet};

mod commit;
mod confirmed;
mod recovery;

/// Fields drop in declaration order: the store closes its database before the
/// temporary home is removed, so Windows never meets an open file on cleanup.
struct ExportFixture {
    store: SqliteStore,
    binding: Binding,
    id: ObjectId,
    intent: DeltaIntent,
    payload: DeltaCandidatePayload,
    home: crate::test_support::TempHome,
}

impl ExportFixture {
    fn new(count: usize) -> Self {
        let (home, mut store, binding, _) = initial_fixture();
        let id = store.activate_task_sync_binding(&binding).unwrap();
        Self::from_store(home, store, binding, id, count)
    }

    fn from_store(
        home: crate::test_support::TempHome,
        mut store: SqliteStore,
        binding: Binding,
        id: ObjectId,
        count: usize,
    ) -> Self {
        let request = audit_request(&store, &binding, &id, CustodySeedSelection::LocalExport);
        let audited = store
            .audit_task_sync_custody(&request, &binding.actor, at(30))
            .unwrap();
        let event: Event =
            SqliteStore::get_typed_object_on(&store.connection, &audited.checkpoint, EVENT_KIND)
                .unwrap()
                .unwrap();
        let Transition::CustodyCheckpoint { checkpoint } = event.transition else {
            panic!("checkpoint expected")
        };
        let Some(CheckpointSeed::PreparedExport { membership, .. }) = checkpoint.seed else {
            panic!("export seed expected")
        };
        let export_before = PreparedExportBasis {
            anchor: PreparedExportAnchor::Bootstrap {
                checkpoint: audited.checkpoint,
            },
            membership: membership.clone(),
            cut: checkpoint.local_cut,
        };
        let mut selected = BTreeSet::new();
        for index in 0..count {
            let task = store
                .create_work(
                    &crate::storage::work::test_support::root_request(
                        &binding.project.0,
                        &format!("Outgoing native task {index}"),
                        35,
                    ),
                    &crate::DevelopmentNoopRedactor,
                )
                .unwrap();
            selected.insert(task.work_id);
        }
        let captured = store
            .capture_task_documents(&binding.project, &binding.profile, selected.clone())
            .unwrap();
        let documents = captured
            .documents
            .into_iter()
            .map(|(id, task)| {
                let crate::task_sync::CapturedTask::Present(task) = task else {
                    panic!("new native document expected")
                };
                (id, Some(*task))
            })
            .collect::<BTreeMap<_, _>>();
        let output = OutputMembership {
            inventory_root: documents
                .values()
                .flatten()
                .try_fold(membership.inventory_root, InventoryRoot::including)
                .unwrap(),
            profile: binding.profile.clone(),
        };
        let operation = uuid::Uuid::new_v4();
        let payload = DeltaCandidatePayload {
            format: DELTA_PAYLOAD_FORMAT.into(),
            project_id: binding.project.clone(),
            operation,
            output: output.clone(),
            documents,
        };
        let cut = producing_origin::cut_on(&store.connection, &binding.project).unwrap();
        let intent = DeltaIntent {
            operation,
            key: operation.to_string(),
            direction: Direction::Outgoing,
            actor: binding.actor.clone(),
            producer: CanonicalObject::freeze(&"outgoing observation producer")
                .unwrap()
                .key()
                .clone(),
            expected_head: None,
            common_commit: None,
            local_cut: OriginQualifiedCut {
                binding: id.clone(),
                origin: binding.store.origin.clone().unwrap(),
                restore_occurrence: binding.store.restore_occurrence,
                position: FeedPosition {
                    feed: FeedId::Project(binding.project.clone()),
                    position: cut.position,
                },
                anchor: cut.anchor,
            },
            basis: DeltaBasis::Outgoing {
                export_before: Box::new(export_before),
                confirmed_before: None,
            },
            candidate: RetainedDeltaCandidate {
                operation,
                directory: retention::delta::candidate_directory(
                    &binding.repository.git_directory,
                    operation,
                ),
                content_fingerprint: CanonicalObject::freeze(&payload).unwrap().key().clone(),
                output,
                changed: selected,
            },
        };
        payload.validate(&id, &binding, &intent).unwrap();
        Self {
            store,
            binding,
            id,
            intent,
            payload,
            home,
        }
    }
}

#[test]
fn outgoing_delta_preparation_recovers_only_its_exact_committed_witness_after_reopen() {
    use crate::task_sync::origin::private;
    let mut f = ExportFixture::new(2);
    let prior = private::read(&f.binding.store.database, &f.binding.project)
        .unwrap()
        .witness
        .unwrap();
    let request = retention::delta::request_fingerprint(&f.id, &f.intent).unwrap();
    let prepared = f
        .store
        .prepare_task_sync_delta_export(&f.id, &f.intent, at(40))
        .unwrap();
    let completed = private::read(&f.binding.store.database, &f.binding.project)
        .unwrap()
        .witness
        .unwrap();
    let origin = f.binding.store.origin.as_ref().unwrap();
    assert!(
        super::super::super::matches_origin_recovery_on(
            &f.store.connection,
            &f.binding.project,
            origin,
            prior.exchange.as_ref(),
            &request,
        )
        .unwrap()
    );
    assert!(
        !super::super::super::matches_origin_recovery_on(
            &f.store.connection,
            &f.binding.project,
            origin,
            prior.exchange.as_ref(),
            &ObjectId::mint(),
        )
        .unwrap()
    );
    let mut wrong_before = prior.exchange.clone().unwrap();
    wrong_before.head = ObjectId::mint();
    assert!(
        !super::super::super::matches_origin_recovery_on(
            &f.store.connection,
            &f.binding.project,
            origin,
            Some(&wrong_before),
            &request,
        )
        .unwrap()
    );
    // Model process loss after SQL committed but before the private witness
    // acknowledged it. The original request and retained bytes survive.
    let mut reserved = prior;
    reserved.pending_exchange = Some(request);
    private::advance(&f.binding.store.database, &completed, &reserved).unwrap();
    let before = state(&f.store);
    drop(f.store);
    let mut store = SqliteStore::open(&f.binding.store.database).unwrap();
    assert_eq!(
        store
            .prepare_task_sync_delta_export(&f.id, &f.intent, at(50))
            .unwrap(),
        prepared
    );
    assert_eq!(state(&store), before);
    assert_eq!(
        private::read(&f.binding.store.database, &f.binding.project)
            .unwrap()
            .witness,
        Some(completed)
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn outgoing_delta_preparation_retains_selected_bytes_and_replays_after_native_edits_and_reopen() {
    let mut f = ExportFixture::new(3);
    let prepared = f
        .store
        .prepare_task_sync_delta_export(&f.id, &f.intent, at(40))
        .unwrap();
    assert_eq!(
        retention::delta::read_candidate(&f.id, &f.binding, &f.intent).unwrap(),
        Some(f.payload.clone())
    );
    let original: Event =
        SqliteStore::get_typed_object_on(&f.store.connection, &prepared, EVENT_KIND)
            .unwrap()
            .unwrap();
    f.store
        .create_work(
            &crate::storage::work::test_support::root_request(
                &f.binding.project.0,
                "Later native task",
                50,
            ),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let before = state(&f.store);
    assert_eq!(
        f.store
            .prepare_task_sync_delta_export(&f.id, &f.intent, at(60))
            .unwrap(),
        prepared
    );
    assert_eq!(state(&f.store), before);
    assert_eq!(
        SqliteStore::get_typed_object_on::<Event>(&f.store.connection, &prepared, EVENT_KIND)
            .unwrap()
            .unwrap(),
        original
    );
    assert!(f.store.verify_all().unwrap().is_healthy());
    drop(f.store);
    let mut store = SqliteStore::open(&f.binding.store.database).unwrap();
    assert_eq!(
        store
            .prepare_task_sync_delta_export(&f.id, &f.intent, at(70))
            .unwrap(),
        prepared
    );
    assert_eq!(
        SqliteStore::get_typed_object_on::<Event>(&store.connection, &prepared, EVENT_KIND)
            .unwrap()
            .unwrap(),
        original
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn outgoing_delta_preparation_refuses_drift_and_key_reuse_without_replacing_retained_bytes() {
    let mut f = ExportFixture::new(2);
    let mut changed = f.intent.clone();
    changed.local_cut.position.position += 1;
    let before = state(&f.store);
    assert!(
        f.store
            .prepare_task_sync_delta_export(&f.id, &changed, at(40))
            .is_err()
    );
    assert_eq!(state(&f.store), before);
    assert!(!f.intent.candidate.directory.exists());
    let prepared = f
        .store
        .prepare_task_sync_delta_export(&f.id, &f.intent, at(41))
        .unwrap();
    let before = state(&f.store);
    let mut changed = f.intent.clone();
    changed.actor.reason.push_str(" changed attribution");
    assert!(
        f.store
            .prepare_task_sync_delta_export(&f.id, &changed, at(42))
            .is_err()
    );
    assert_eq!(state(&f.store), before);
    assert_eq!(
        retention::delta::read_candidate(&f.id, &f.binding, &f.intent).unwrap(),
        Some(f.payload.clone())
    );
    assert_eq!(
        f.store
            .prepare_task_sync_delta_export(&f.id, &f.intent, at(43))
            .unwrap(),
        prepared
    );
}

#[test]
fn outgoing_delta_preparation_keeps_exact_retention_after_sql_failure_for_same_intent_retry() {
    let mut f = ExportFixture::new(2);
    let before = state(&f.store);
    f.store.connection.execute_batch("CREATE TEMP TRIGGER fail_delta_preparation BEFORE INSERT ON objects
        WHEN NEW.object_kind='task_sync_exchange_event' AND json_extract(NEW.canonical_json,'$.transition.transition')='delta_prepared'
        BEGIN SELECT RAISE(ABORT,'fixture preparation failure'); END;").unwrap();
    assert!(
        f.store
            .prepare_task_sync_delta_export(&f.id, &f.intent, at(40))
            .is_err()
    );
    assert_eq!(state(&f.store), before);
    assert_eq!(
        retention::delta::read_candidate(&f.id, &f.binding, &f.intent).unwrap(),
        Some(f.payload.clone())
    );
    f.store
        .connection
        .execute_batch("DROP TRIGGER fail_delta_preparation")
        .unwrap();
    let prepared = f
        .store
        .prepare_task_sync_delta_export(&f.id, &f.intent, at(41))
        .unwrap();
    assert_eq!(
        f.store
            .prepare_task_sync_delta_export(&f.id, &f.intent, at(42))
            .unwrap(),
        prepared
    );
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[test]
fn outgoing_delta_inspection_two_thousand_native_members_is_read_only_and_creates_no_artifacts() {
    let f = ExportFixture::new(2_000);
    let before = state(&f.store);
    let private_before =
        crate::task_sync::origin::private::read(&f.binding.store.database, &f.binding.project)
            .unwrap();
    assert_eq!(
        f.store
            .inspect_task_sync_delta_export(&f.id, &f.intent)
            .unwrap(),
        f.payload
    );
    assert_eq!(state(&f.store), before);
    assert_eq!(
        crate::task_sync::origin::private::read(&f.binding.store.database, &f.binding.project)
            .unwrap(),
        private_before
    );
    assert!(!f.intent.candidate.directory.exists());
    let complete = f.store.export_task_tree(&f.binding.project).unwrap();
    assert_eq!(
        complete
            .tasks
            .iter()
            .try_fold(InventoryRoot::ZERO, InventoryRoot::including)
            .unwrap(),
        f.intent.candidate.output.inventory_root
    );
    let source_count: i64 = f.store.connection.query_row("SELECT count(*) FROM work_task_sync_graph_nodes WHERE view_kind='source' AND scope_id=?1", [f.id.as_str()], |row|row.get(0)).unwrap();
    assert_eq!(source_count, 0);
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[test]
fn outgoing_delta_inspection_refuses_changed_membership_cut_payload_and_missing_export_basis() {
    let f = ExportFixture::new(2);
    let before = state(&f.store);
    let mut changed = f.intent.clone();
    changed.local_cut.position.position += 1;
    assert!(
        f.store
            .inspect_task_sync_delta_export(&f.id, &changed)
            .is_err()
    );
    let mut changed = f.intent.clone();
    changed.candidate.content_fingerprint = CanonicalObject::freeze(&"different selected bytes")
        .unwrap()
        .key()
        .clone();
    assert!(
        f.store
            .inspect_task_sync_delta_export(&f.id, &changed)
            .is_err()
    );
    let mut changed = f.intent.clone();
    let DeltaBasis::Outgoing { export_before, .. } = &mut changed.basis else {
        unreachable!()
    };
    export_before.anchor = PreparedExportAnchor::Bootstrap {
        checkpoint: ObjectId::mint(),
    };
    assert!(
        f.store
            .inspect_task_sync_delta_export(&f.id, &changed)
            .is_err()
    );
    assert_eq!(state(&f.store), before);
    f.store
        .connection
        .execute(
            "DELETE FROM work_task_sync_source_heads WHERE binding_id=?1 AND role='export'",
            [f.id.as_str()],
        )
        .unwrap();
    let damaged = state(&f.store);
    assert!(
        f.store
            .inspect_task_sync_delta_export(&f.id, &f.intent)
            .is_err()
    );
    assert_eq!(state(&f.store), damaged);
    assert!(!f.intent.candidate.directory.exists());
}

#[test]
fn outgoing_delta_inspection_uses_shared_restore_owner_without_settling_custody() {
    let f = ExportFixture::new(1);
    let path = crate::backup::restore::restore_record_path(f.home.path(), &f.binding.project);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, b"unreadable restore provenance").unwrap();
    let before = state(&f.store);
    let private_before =
        crate::task_sync::origin::private::read(&f.binding.store.database, &f.binding.project)
            .unwrap();
    let error = f
        .store
        .inspect_task_sync_delta_export(&f.id, &f.intent)
        .unwrap_err();
    assert!(error.to_string().contains("restore occurrence"), "{error}");
    assert_eq!(state(&f.store), before);
    assert_eq!(
        crate::task_sync::origin::private::read(&f.binding.store.database, &f.binding.project)
            .unwrap(),
        private_before
    );
    assert_eq!(
        std::fs::read(&path).unwrap(),
        b"unreadable restore provenance"
    );
    assert!(!f.intent.candidate.directory.exists());
}

impl ExportFixture {
    // Construct canonical historical fixtures through the real reducer and
    // projection owner. This is not a Git inspection or a writable sync API.
    fn event(&mut self, transition: Transition) -> ObjectId {
        let transaction = self.store.begin_work_mutation().unwrap();
        let mut ledger = Ledger::load(&transaction).unwrap();
        let (previous, tail) = ledger
            .events
            .iter()
            .filter(|(_, event)| event.binding == self.id)
            .max_by_key(|(_, event)| event.sequence)
            .unwrap();
        let event = Event {
            schema_version: 1,
            binding: self.id.clone(),
            sequence: tail.sequence + 1,
            predecessor: Some(previous.clone()),
            actor: self.intent.actor.clone(),
            recorded_at: at(40),
            transition,
        };
        let object = CanonicalObject::mint(&event).unwrap();
        super::super::store_event(&transaction, &mut ledger, &object).unwrap();
        transaction.commit().unwrap();
        object.key().clone()
    }

    fn confirmed(&mut self) -> ConfirmedExportBasis {
        let prepared = self.event(Transition::DeltaPrepared {
            intent: Box::new(self.intent.clone()),
            payload: Box::new(self.payload.clone()),
        });
        let commit = crate::task_sync::ledger::RetainedCommit {
            oid: CanonicalObject::freeze(&"historical outgoing commit")
                .unwrap()
                .key()
                .to_string(),
            tree: CanonicalObject::freeze(&"historical outgoing tree")
                .unwrap()
                .key()
                .to_string(),
            parents: vec![],
            retention_ref: format!(
                "refs/engram/task-sync/exchange/{}/commit",
                self.intent.operation
            ),
            raw_bytes_fingerprint: CanonicalObject::freeze(&"historical commit bytes")
                .unwrap()
                .key()
                .clone(),
        };
        self.event(Transition::DeltaLocalCommitRecorded {
            prepared: prepared.clone(),
            commit: commit.clone(),
        });
        self.event(Transition::PushStarted {
            prepared: prepared.clone(),
        });
        let outcome = self.event(Transition::PushObserved {
            prepared: prepared.clone(),
            observation: crate::task_sync::ledger::PublicationObservation::Confirmed {
                observed_tip: commit.oid.clone(),
            },
        });
        ConfirmedExportBasis {
            outcome,
            prepared,
            snapshot: MembershipSnapshot {
                commit,
                membership: self.intent.candidate.output.clone(),
            },
        }
    }

    fn next_intent(
        &self,
        confirmed: &ConfirmedExportBasis,
    ) -> (DeltaIntent, DeltaCandidatePayload) {
        let mut intent = self.intent.clone();
        intent.operation = uuid::Uuid::new_v4();
        intent.key = intent.operation.to_string();
        intent.candidate.operation = intent.operation;
        intent.candidate.directory = retention::delta::candidate_directory(
            &self.binding.repository.git_directory,
            intent.operation,
        );
        intent.candidate.changed.clear();
        intent.expected_head = Some(confirmed.snapshot.commit.oid.clone());
        intent.basis = DeltaBasis::Outgoing {
            export_before: Box::new(PreparedExportBasis {
                anchor: PreparedExportAnchor::Prepared {
                    event: confirmed.prepared.clone(),
                },
                membership: confirmed.snapshot.membership.clone(),
                cut: intent.local_cut.clone(),
            }),
            confirmed_before: Some(Box::new(confirmed.clone())),
        };
        let payload = DeltaCandidatePayload {
            format: DELTA_PAYLOAD_FORMAT.into(),
            project_id: self.binding.project.clone(),
            operation: intent.operation,
            output: intent.candidate.output.clone(),
            documents: BTreeMap::new(),
        };
        intent.candidate.content_fingerprint =
            CanonicalObject::freeze(&payload).unwrap().key().clone();
        (intent, payload)
    }
}

#[test]
fn outgoing_delta_history_validates_confirmed_basis_after_newer_preparation() {
    let mut f = ExportFixture::new(2);
    let confirmed = f.confirmed();
    let (intent, payload) = f.next_intent(&confirmed);
    assert_eq!(
        f.store
            .inspect_task_sync_delta_export(&f.id, &intent)
            .unwrap(),
        payload
    );
    f.intent = intent;
    f.payload = payload;
    let prepared = f.event(Transition::DeltaPrepared {
        intent: Box::new(f.intent.clone()),
        payload: Box::new(f.payload.clone()),
    });
    // Historical validation uses immutable anchors, not the newer export head.
    assert!(f.store.verify_all().unwrap().is_healthy());
    drop(f.store);
    SqliteStore::repair_rebuildable_projections(&f.binding.store.database).unwrap();
    let store = SqliteStore::open(&f.binding.store.database).unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
    let selected = crate::storage::work::task_sync::ledger::view::read::operation_on(
        &store.connection,
        &f.id,
        &f.binding,
        &prepared,
        &mut crate::storage::work::task_sync::ledger::view::read::ReadBudget::default(),
    )
    .unwrap();
    assert_eq!(selected.prepared, prepared);
    assert!(matches!(selected.intent,
        crate::storage::work::task_sync::ledger::view::read::CheckedRequest::Delta(intent)
        if intent.as_ref() == &f.intent));
    assert!(!f.intent.candidate.directory.exists());
}

#[test]
fn outgoing_delta_history_refuses_changed_confirmed_snapshot_without_writes() {
    let mut f = ExportFixture::new(1);
    let confirmed = f.confirmed();
    let (intent, _) = f.next_intent(&confirmed);
    let before = state(&f.store);
    for scenario in ["prepared", "outcome", "commit", "membership", "absent"] {
        let mut changed = intent.clone();
        let DeltaBasis::Outgoing {
            confirmed_before, ..
        } = &mut changed.basis
        else {
            unreachable!()
        };
        let basis = confirmed_before.as_mut().unwrap();
        match scenario {
            "prepared" => basis.prepared = ObjectId::mint(),
            "outcome" => basis.outcome = ObjectId::mint(),
            "commit" => {
                basis.snapshot.commit.raw_bytes_fingerprint =
                    CanonicalObject::freeze(&"changed commit bytes")
                        .unwrap()
                        .key()
                        .clone();
            }
            "membership" => basis.snapshot.membership.inventory_root = InventoryRoot::ZERO,
            "absent" => *confirmed_before = None,
            _ => unreachable!(),
        }
        assert!(
            f.store
                .inspect_task_sync_delta_export(&f.id, &changed)
                .is_err(),
            "{scenario}"
        );
        assert_eq!(state(&f.store), before, "{scenario}");
    }
}
