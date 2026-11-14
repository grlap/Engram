//! Real SQL/private custody, substituting only the already inspected Git facts.
//! Actual Git snapshot admission has separate Git fixtures; this is no push.

use super::*;
use crate::task_sync::{
    exchange::InspectedDeltaResolution,
    ledger::{OutgoingFailure, OutgoingProgress, OutgoingReconciliation, PublicationObservation},
};

mod retry;

#[test]
fn delta_resolution_without_recorded_rejection_preserves_unknown_dispatch() {
    let mut f = ExportFixture::new(2);
    let input = resolution_with_rejection(&mut f, true, false);
    let before = state(&f.store);
    let witness = private::read(&f.binding.store.database, &f.binding.project).unwrap();
    let inventory = retention::inventory(&f.binding.repository.git_directory).unwrap();
    let error = f
        .store
        .record_task_sync_delta_resolution(&input, at(43))
        .unwrap_err();
    assert!(
        error.to_string().contains("uncertainty stays unresolved"),
        "{error}"
    );
    assert_eq!(state(&f.store), before);
    assert_eq!(
        private::read(&f.binding.store.database, &f.binding.project).unwrap(),
        witness
    );
    assert_eq!(
        retention::inventory(&f.binding.repository.git_directory).unwrap(),
        inventory
    );
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[cfg(windows)]
#[tokio::test]
async fn delta_resolution_and_retry_keep_abandoned_git_outside_the_writer() {
    use super::super::super::writer_probe::{Probe, abandoned};
    let (home, store, binding, id) = abandoned().await;
    let mut f = ExportFixture::from_store(home, store, binding, id, 1);
    let probe = Probe::new(&f.binding.store.database);
    let input = resolution(&mut f, true);
    f.store
        .record_task_sync_delta_resolution(&input, at(43))
        .unwrap();
    let (start, _) = f
        .store
        .record_task_sync_delta_retry_once(&input, &OutgoingProgress::PushStarted, at(44))
        .unwrap();
    f.store
        .record_task_sync_delta_retry_once(
            &input,
            &OutgoingProgress::PushRejected {
                push_started: start.clone(),
            },
            at(45),
        )
        .unwrap();
    let stop = OutgoingProgress::Reconciled {
        prepared: input.selection.prepared.clone(),
        reconciliation: Box::new(OutgoingReconciliation::Stopped {
            failure: OutgoingFailure::NonFastForward {
                push_started: start,
                fetched_tip: CanonicalObject::freeze(&"second competing tip")
                    .unwrap()
                    .key()
                    .to_string(),
            },
        }),
    };
    f.store
        .record_task_sync_delta_retry_once(&input, &stop, at(46))
        .unwrap();
    probe.assert_observed();
    assert!(f.store.verify_all().unwrap().is_healthy());
}

fn resolution(f: &mut ExportFixture, pushed: bool) -> InspectedDeltaResolution {
    resolution_with_rejection(f, pushed, true)
}

fn resolution_with_rejection(
    f: &mut ExportFixture,
    pushed: bool,
    rejected: bool,
) -> InspectedDeltaResolution {
    let (original, selection) = progress::committed(f);
    let tip = CanonicalObject::freeze(&"concurrent fetched tip")
        .unwrap()
        .key()
        .to_string();
    let failure = if pushed {
        let (push_started, fresh) = f
            .store
            .record_task_sync_delta_progress_once(
                &original,
                &selection,
                OutgoingProgress::PushStarted,
                at(42),
            )
            .unwrap();
        assert!(fresh);
        if rejected {
            f.store
                .record_task_sync_delta_progress_once(
                    &original,
                    &selection,
                    OutgoingProgress::PushRejected {
                        push_started: push_started.clone(),
                    },
                    at(42),
                )
                .unwrap();
        }
        OutgoingFailure::NonFastForward {
            push_started,
            fetched_tip: tip.clone(),
        }
    } else {
        OutgoingFailure::AncestryChanged {
            fetched_tip: tip.clone(),
        }
    };
    let mut resolved = original.intent.clone();
    resolved.operation = uuid::Uuid::new_v4();
    resolved.key = resolved.operation.to_string();
    resolved.expected_head = Some(selection.commit.oid.clone());
    resolved.candidate.operation = resolved.operation;
    resolved.candidate.directory = retention::delta::candidate_directory(
        &f.binding.repository.git_directory,
        resolved.operation,
    );
    let DeltaBasis::Outgoing { export_before, .. } = &mut resolved.basis else {
        unreachable!()
    };
    export_before.anchor = PreparedExportAnchor::Prepared {
        event: selection.prepared.clone(),
    };
    export_before.membership = original.intent.candidate.output.clone();
    export_before.cut = original.intent.local_cut.clone();
    let mut task = original
        .payload
        .documents
        .values()
        .find_map(Option::as_ref)
        .unwrap()
        .clone();
    task.id = crate::WorkId::new();
    task.title = "Concurrent remote task".into();
    task.activity.clear();
    task.notes.clear();
    resolved.candidate.changed = BTreeSet::from([task.id]);
    resolved.candidate.output.inventory_root = original
        .payload
        .output
        .inventory_root
        .including(&task)
        .unwrap();
    let payload = DeltaCandidatePayload {
        format: DELTA_PAYLOAD_FORMAT.into(),
        project_id: f.binding.project.clone(),
        operation: resolved.operation,
        output: resolved.candidate.output.clone(),
        documents: BTreeMap::from([(task.id, Some(task))]),
    };
    resolved.candidate.content_fingerprint =
        CanonicalObject::freeze(&payload).unwrap().key().clone();
    retention::delta::retain_candidate(&f.id, &f.binding, &resolved, &payload).unwrap();
    let commit = RetainedCommit {
        oid: CanonicalObject::freeze(&"merged commit")
            .unwrap()
            .key()
            .to_string(),
        tree: CanonicalObject::freeze(&"merged tree")
            .unwrap()
            .key()
            .to_string(),
        parents: vec![selection.commit.oid.clone(), tip],
        retention_ref: format!(
            "refs/engram/task-sync/exchange/{}/commit",
            resolved.operation
        ),
        raw_bytes_fingerprint: CanonicalObject::freeze(&"merged raw bytes")
            .unwrap()
            .key()
            .clone(),
    };
    {
        let reservation =
            retention::audit::AuditReservation::acquire(&f.binding.repository.git_directory)
                .unwrap();
        reservation
            .reserve_artifact(
                resolved.operation,
                retention::audit::journal::Family::Commit,
                &commit,
            )
            .unwrap();
    }
    std::fs::write(
        f.binding
            .repository
            .git_directory
            .join("engram-task-sync-commits")
            .join(format!("{}.json", resolved.operation)),
        crate::canonical::canonical_bytes(&commit).unwrap(),
    )
    .unwrap();
    InspectedDeltaResolution {
        original,
        selection,
        recipe: OutgoingReconciliation::DeltaResolved {
            failure,
            resolved: Box::new(resolved),
            payload: Box::new(payload),
            commit: Box::new(commit),
        },
    }
}

#[test]
fn managed_resolution_and_retry_keep_one_canonical_operation_and_dispatch() {
    let mut f = super::super::managed::fixture();
    let input = resolution(&mut f, true);
    let recipe = f
        .store
        .record_task_sync_delta_resolution(&input, at(43))
        .unwrap();
    assert_eq!(
        f.store
            .record_task_sync_delta_resolution(&input, at(44))
            .unwrap(),
        recipe
    );
    let (start, fresh) = f
        .store
        .record_task_sync_delta_retry_once(&input, &OutgoingProgress::PushStarted, at(45))
        .unwrap();
    assert!(fresh);
    assert_eq!(
        f.store
            .record_task_sync_delta_retry_once(&input, &OutgoingProgress::PushStarted, at(46))
            .unwrap(),
        (start, false)
    );
    let OutgoingReconciliation::DeltaResolved { commit, .. } = &input.recipe else {
        unreachable!()
    };
    let confirmed = OutgoingProgress::Observed(PublicationObservation::Confirmed {
        observed_tip: commit.oid.clone(),
    });
    let (outcome, fresh) = f
        .store
        .record_task_sync_delta_retry_once(&input, &confirmed, at(47))
        .unwrap();
    assert!(fresh);
    let before = state(&f.store);
    assert_eq!(
        f.store
            .record_task_sync_delta_retry_once(&input, &confirmed, at(48))
            .unwrap(),
        (outcome, false)
    );
    assert_eq!(state(&f.store), before);
    assert!(
        retention::audit::journal::observe(&f.binding.repository.git_directory)
            .unwrap()
            .unwrap()
            .writes
            .is_empty()
    );
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[test]
fn managed_resolution_refuses_a_missing_original_publication_witness() {
    let mut f = super::super::managed::fixture();
    let input = resolution(&mut f, true);
    let root = &f.binding.repository.git_directory;
    let path = root
        .join("engram-task-sync-publication-attempts")
        .join(format!("{}.json", input.original.intent.operation));
    // Simulate damage to a required original witness after its publication was
    // recorded. The resolved operation's complete artifacts do not replace it.
    std::fs::remove_file(&path).unwrap();
    let before = state(&f.store);
    let pending = retention::audit::journal::observe(root).unwrap();
    let error = f
        .store
        .record_task_sync_delta_resolution(&input, at(43))
        .unwrap_err();
    assert!(
        error.to_string().contains("publication witness is missing"),
        "{error}"
    );
    assert_eq!(state(&f.store), before);
    assert_eq!(retention::audit::journal::observe(root).unwrap(), pending);
    assert!(!path.exists());
}

#[test]
fn delta_recipe_admission_records_one_original_pending_operation_and_exact_replay() {
    for pushed in [false, true] {
        let mut f = ExportFixture::new(2);
        let input = resolution(&mut f, pushed);
        let native = f.intent.clone();
        let recipe = f
            .store
            .record_task_sync_delta_resolution(&input, at(43))
            .unwrap();
        let events = f
            .store
            .task_sync_delta_outgoing_events(&f.id, &f.intent)
            .unwrap();
        assert_eq!(events.last().unwrap().0, recipe);
        assert_eq!(
            events
                .iter()
                .filter(|(_, event)| matches!(&event.transition, Transition::DeltaPrepared { .. }))
                .count(),
            1
        );
        let pending: Option<String> = f
            .store
            .connection
            .query_row(
                "SELECT pending_prepared_id FROM work_task_sync_exchange_heads WHERE binding_id=?1",
                [f.id.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(pending.as_deref(), Some(input.selection.prepared.as_str()));
        assert_eq!(f.intent, native);
        let before = state(&f.store);
        assert_eq!(
            f.store
                .record_task_sync_delta_resolution(&input, at(44))
                .unwrap(),
            recipe
        );
        assert_eq!(state(&f.store), before);
        assert!(f.store.verify_all().unwrap().is_healthy());
    }
}

#[test]
fn delta_recipe_admission_recovers_only_the_whole_committed_request_after_lost_ack() {
    let mut f = ExportFixture::new(2);
    let input = resolution(&mut f, true);
    let prior = private::read(&f.binding.store.database, &f.binding.project)
        .unwrap()
        .witness
        .unwrap();
    let recipe = f
        .store
        .record_task_sync_delta_resolution(&input, at(43))
        .unwrap();
    let after = private::read(&f.binding.store.database, &f.binding.project)
        .unwrap()
        .witness
        .unwrap();
    let mut pending = prior;
    pending.pending_exchange = Some(input.request_fingerprint().unwrap());
    private::advance(&f.binding.store.database, &after, &pending).unwrap();
    drop(f.store);
    let mut store = SqliteStore::open(&f.binding.store.database).unwrap();
    let before = state(&store);
    let mut changed = input.clone();
    let OutgoingReconciliation::DeltaResolved { failure, .. } = &mut changed.recipe else {
        unreachable!()
    };
    let OutgoingFailure::NonFastForward { push_started, .. } = failure else {
        unreachable!()
    };
    *push_started = ObjectId::mint();
    assert_ne!(
        changed.request_fingerprint().unwrap(),
        input.request_fingerprint().unwrap()
    );
    assert!(
        store
            .record_task_sync_delta_resolution(&changed, at(44))
            .is_err()
    );
    assert_eq!(state(&store), before);
    assert_eq!(
        private::read(&f.binding.store.database, &f.binding.project)
            .unwrap()
            .witness,
        Some(pending)
    );
    assert_eq!(
        store
            .record_task_sync_delta_resolution(&input, at(45))
            .unwrap(),
        recipe
    );
    assert_eq!(state(&store), before);
    assert_eq!(
        private::read(&f.binding.store.database, &f.binding.project)
            .unwrap()
            .witness,
        Some(after)
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn delta_recipe_admission_sql_rollback_preserves_artifacts_and_allows_only_exact_recipe() {
    let mut f = ExportFixture::new(2);
    let input = resolution(&mut f, false);
    let before = state(&f.store);
    f.store.connection.execute_batch("CREATE TEMP TRIGGER fail_resolution BEFORE INSERT ON objects
        WHEN NEW.object_kind='task_sync_exchange_event' AND json_extract(NEW.canonical_json,'$.transition.reconciliation.step')='delta_resolved'
        BEGIN SELECT RAISE(ABORT,'fixture resolution rollback'); END;").unwrap();
    let error = f
        .store
        .record_task_sync_delta_resolution(&input, at(43))
        .unwrap_err();
    assert!(
        error.to_string().contains("fixture resolution rollback"),
        "{error}"
    );
    assert_eq!(state(&f.store), before);
    input.recheck_files().unwrap();
    // The ordinary native operation cannot adopt the unrecorded merged files.
    assert!(
        f.store
            .record_task_sync_delta_progress_once(
                &input.original,
                &input.selection,
                OutgoingProgress::PushStarted,
                at(44)
            )
            .is_err()
    );
    assert_eq!(state(&f.store), before);
    f.store
        .connection
        .execute_batch("DROP TRIGGER fail_resolution")
        .unwrap();
    f.store
        .record_task_sync_delta_resolution(&input, at(45))
        .unwrap();
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[test]
fn delta_recipe_admission_refuses_unknown_and_changed_retained_commit_without_sql_changes() {
    let mut f = ExportFixture::new(2);
    let input = resolution_with_rejection(&mut f, true, false);
    // Insert an Unknown and align its private witness to exercise the reducer's
    // uncertainty check independently of the separate rewind/custody refusal.
    let mut ledger = Ledger::load(&f.store.connection).unwrap();
    let transaction = f.store.begin_work_mutation().unwrap();
    let previous = ledger
        .events
        .iter()
        .filter(|(_, event)| event.binding == f.id)
        .max_by_key(|(_, event)| event.sequence)
        .unwrap();
    let object = CanonicalObject::mint(&Event {
        schema_version: 1,
        binding: f.id.clone(),
        sequence: previous.1.sequence + 1,
        predecessor: Some(previous.0.clone()),
        actor: f.intent.actor.clone(),
        recorded_at: at(43),
        transition: Transition::PushObserved {
            prepared: input.selection.prepared.clone(),
            observation: PublicationObservation::Unknown { observed_tip: None },
        },
    })
    .unwrap();
    crate::storage::work::task_sync::ledger::operations::store_event(
        &transaction,
        &mut ledger,
        &object,
    )
    .unwrap();
    transaction.commit().unwrap();
    let prior = private::read(&f.binding.store.database, &f.binding.project)
        .unwrap()
        .witness
        .unwrap();
    let mut after = prior.clone();
    after.exchange = crate::storage::work::task_sync::ledger::origin_anchor_on(
        &f.store.connection,
        &f.binding.project,
        f.binding.store.origin.as_ref().unwrap(),
    )
    .unwrap();
    private::advance(&f.binding.store.database, &prior, &after).unwrap();
    let before = state(&f.store);
    let error = f
        .store
        .record_task_sync_delta_resolution(&input, at(44))
        .unwrap_err();
    assert!(
        error.to_string().contains("uncertainty stays unresolved"),
        "{error}"
    );
    assert_eq!(state(&f.store), before);
    input.recheck_files().unwrap();
    let mut changed = input.clone();
    let OutgoingReconciliation::DeltaResolved { commit, .. } = &mut changed.recipe else {
        unreachable!()
    };
    commit.tree = CanonicalObject::freeze(&"changed retained tree")
        .unwrap()
        .key()
        .to_string();
    assert!(
        f.store
            .record_task_sync_delta_resolution(&changed, at(45))
            .is_err()
    );
    assert_eq!(state(&f.store), before);
}
