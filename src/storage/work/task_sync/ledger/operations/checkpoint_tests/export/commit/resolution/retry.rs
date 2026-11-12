//! Durable once-only permission and uncertainty; no fixture dispatches Git.

use super::*;

mod driver;

fn admitted(f: &mut ExportFixture) -> InspectedDeltaResolution {
    let input = resolution(f, true);
    f.store
        .record_task_sync_delta_resolution(&input, at(43))
        .unwrap();
    input
}

fn selected_commit(input: &InspectedDeltaResolution) -> &RetainedCommit {
    let OutgoingReconciliation::DeltaResolved { commit, .. } = &input.recipe else {
        unreachable!()
    };
    commit
}

fn request(input: &InspectedDeltaResolution, progress: &OutgoingProgress) -> ObjectId {
    retention::delta::retry_request_fingerprint(
        &input.original.binding_id,
        &input.original.intent,
        &input.selection,
        &input.recipe,
        progress,
    )
    .unwrap()
}

#[test]
fn delta_retry_progress_start_once_unknown_restart_and_exact_confirmation_keep_original_pending() {
    let mut f = ExportFixture::new(2);
    let input = admitted(&mut f);
    let (start, fresh) = f
        .store
        .record_task_sync_delta_retry_once(&input, &OutgoingProgress::PushStarted, at(44))
        .unwrap();
    assert!(fresh);
    let unknown =
        OutgoingProgress::Observed(PublicationObservation::Unknown { observed_tip: None });
    let (observed, fresh) = f
        .store
        .record_task_sync_delta_retry_once(&input, &unknown, at(45))
        .unwrap();
    assert!(fresh);
    let witness = retention::read_publication_witness(
        &f.binding.repository.git_directory,
        input.operation().unwrap(),
    )
    .unwrap()
    .unwrap();
    assert_eq!(witness.event_id, start);
    drop(f.store);
    let mut store = SqliteStore::open(&f.binding.store.database).unwrap();
    let before = state(&store);
    assert_eq!(
        store
            .record_task_sync_delta_retry_once(&input, &OutgoingProgress::PushStarted, at(46))
            .unwrap(),
        (start, false)
    );
    assert_eq!(
        store
            .record_task_sync_delta_retry_once(&input, &unknown, at(47))
            .unwrap(),
        (observed, false)
    );
    assert_eq!(state(&store), before);
    let wrong = OutgoingProgress::Observed(PublicationObservation::Confirmed {
        observed_tip: input.selection.commit.oid.clone(),
    });
    assert!(
        store
            .record_task_sync_delta_retry_once(&input, &wrong, at(48))
            .is_err()
    );
    assert_eq!(state(&store), before);
    let confirmed = OutgoingProgress::Observed(PublicationObservation::Confirmed {
        observed_tip: selected_commit(&input).oid.clone(),
    });
    let (outcome, fresh) = store
        .record_task_sync_delta_retry_once(&input, &confirmed, at(49))
        .unwrap();
    assert!(fresh);
    let before = state(&store);
    assert_eq!(
        store
            .record_task_sync_delta_retry_once(&input, &confirmed, at(50))
            .unwrap(),
        (outcome, false)
    );
    assert_eq!(state(&store), before);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn delta_retry_progress_recovers_exact_start_and_observation_ack_without_new_permission() {
    for observed in [false, true] {
        let mut f = ExportFixture::new(2);
        let input = admitted(&mut f);
        if observed {
            f.store
                .record_task_sync_delta_retry_once(&input, &OutgoingProgress::PushStarted, at(44))
                .unwrap();
        }
        let progress = if observed {
            OutgoingProgress::Observed(PublicationObservation::Unknown { observed_tip: None })
        } else {
            OutgoingProgress::PushStarted
        };
        let before = private::read(&f.binding.store.database, &f.binding.project)
            .unwrap()
            .witness
            .unwrap();
        let (event, fresh) = f
            .store
            .record_task_sync_delta_retry_once(&input, &progress, at(45))
            .unwrap();
        assert!(fresh);
        let after = private::read(&f.binding.store.database, &f.binding.project)
            .unwrap()
            .witness
            .unwrap();
        let mut pending = before;
        pending.pending_exchange = Some(request(&input, &progress));
        private::advance(&f.binding.store.database, &after, &pending).unwrap();
        drop(f.store);
        let mut store = SqliteStore::open(&f.binding.store.database).unwrap();
        let before = state(&store);
        assert_eq!(
            store
                .record_task_sync_delta_retry_once(&input, &progress, at(46))
                .unwrap(),
            (event, false)
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
}

#[test]
fn delta_retry_progress_commit_failure_keeps_ahead_witness_and_refuses_another_start() {
    let mut f = ExportFixture::new(2);
    let input = admitted(&mut f);
    let clean = state(&f.store);
    // Fail COMMIT after append and witness flush, using a deferred foreign key.
    f.store.connection.execute_batch("CREATE TABLE retry_commit_failure (missing TEXT REFERENCES objects(object_id) DEFERRABLE INITIALLY DEFERRED);
        CREATE TEMP TRIGGER fail_retry_commit AFTER INSERT ON objects
        WHEN NEW.object_kind='task_sync_exchange_event' AND json_extract(NEW.canonical_json,'$.transition.reconciliation.step')='push_started'
        BEGIN INSERT INTO retry_commit_failure VALUES('missing-fixture-object'); END;").unwrap();
    let before = state(&f.store);
    assert!(
        f.store
            .record_task_sync_delta_retry_once(&input, &OutgoingProgress::PushStarted, at(44))
            .is_err()
    );
    assert_eq!(state(&f.store), before);
    let witness = retention::read_publication_witness(
        &f.binding.repository.git_directory,
        input.operation().unwrap(),
    )
    .unwrap()
    .unwrap();
    f.store
        .connection
        .execute_batch("DROP TRIGGER fail_retry_commit; DROP TABLE retry_commit_failure;")
        .unwrap();
    assert_eq!(state(&f.store), clean);
    let before = state(&f.store);
    assert!(
        f.store
            .record_task_sync_delta_retry_once(&input, &OutgoingProgress::PushStarted, at(45))
            .is_err()
    );
    assert_eq!(state(&f.store), before);
    assert_eq!(
        retention::read_publication_witness(
            &f.binding.repository.git_directory,
            input.operation().unwrap()
        )
        .unwrap(),
        Some(witness)
    );
    input.recheck_files().unwrap();
}

#[test]
fn delta_retry_progress_second_known_failure_stops_without_confirmation_or_extra_permission() {
    let mut f = ExportFixture::new(2);
    let input = admitted(&mut f);
    let (start, fresh) = f
        .store
        .record_task_sync_delta_retry_once(&input, &OutgoingProgress::PushStarted, at(44))
        .unwrap();
    assert!(fresh);
    let stop = OutgoingProgress::Reconciled {
        prepared: input.selection.prepared.clone(),
        reconciliation: Box::new(OutgoingReconciliation::Stopped {
            failure: OutgoingFailure::NonFastForward {
                push_started: start.clone(),
                fetched_tip: CanonicalObject::freeze(&"second fetched tip")
                    .unwrap()
                    .key()
                    .to_string(),
            },
        }),
    };
    let before = state(&f.store);
    let witness = retention::read_publication_witness(
        &f.binding.repository.git_directory,
        input.operation().unwrap(),
    )
    .unwrap();
    assert!(
        f.store
            .record_task_sync_delta_retry_once(&input, &stop, at(45))
            .is_err()
    );
    assert_eq!(state(&f.store), before);
    assert_eq!(
        retention::read_publication_witness(
            &f.binding.repository.git_directory,
            input.operation().unwrap()
        )
        .unwrap(),
        witness
    );
    f.store
        .record_task_sync_delta_retry_once(
            &input,
            &OutgoingProgress::PushRejected {
                push_started: start.clone(),
            },
            at(45),
        )
        .unwrap();
    let (stopped, fresh) = f
        .store
        .record_task_sync_delta_retry_once(&input, &stop, at(45))
        .unwrap();
    assert!(fresh);
    let before = state(&f.store);
    assert_eq!(
        f.store
            .record_task_sync_delta_retry_once(&input, &stop, at(46))
            .unwrap(),
        (stopped, false)
    );
    assert_eq!(
        f.store
            .record_task_sync_delta_retry_once(&input, &OutgoingProgress::PushStarted, at(47))
            .unwrap(),
        (start, false)
    );
    let confirmed = OutgoingProgress::Observed(PublicationObservation::Confirmed {
        observed_tip: selected_commit(&input).oid.clone(),
    });
    assert!(
        f.store
            .record_task_sync_delta_retry_once(&input, &confirmed, at(48))
            .is_err()
    );
    assert_eq!(state(&f.store), before);
    let completed:Option<String>=f.store.connection.query_row("SELECT last_outgoing_completed_id FROM work_task_sync_exchange_heads WHERE binding_id=?1",[f.id.as_str()],|row|row.get(0)).unwrap();
    assert_eq!(completed, None);
    assert!(f.store.verify_all().unwrap().is_healthy());
}
