use super::*;
use crate::task_sync::ledger::{OutgoingProgress, PublicationObservation};

pub(super) fn committed(f: &mut ExportFixture) -> (InspectedDelta, DeltaCommitSelection) {
    let (input, selection) = selected(f);
    f.store
        .record_task_sync_delta_export_commit(&input, &selection, at(41))
        .unwrap();
    (input, selection)
}

#[test]
fn delta_publication_start_is_once_and_unknown_survives_restart_until_exact_confirmation() {
    let mut f = ExportFixture::new(2);
    let (input, selection) = committed(&mut f);
    let (start, fresh) = f
        .store
        .record_task_sync_delta_progress_once(
            &input,
            &selection,
            OutgoingProgress::PushStarted,
            at(42),
        )
        .unwrap();
    assert!(fresh);
    let unknown =
        OutgoingProgress::Observed(PublicationObservation::Unknown { observed_tip: None });
    let (unknown_id, fresh) = f
        .store
        .record_task_sync_delta_progress_once(&input, &selection, unknown.clone(), at(43))
        .unwrap();
    assert!(fresh);
    drop(f.store);
    let mut store = SqliteStore::open(&f.binding.store.database).unwrap();
    store
        .create_work(
            &crate::storage::work::test_support::root_request(
                &f.binding.project.0,
                "Later native edit",
                44,
            ),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let before = state(&store);
    assert_eq!(
        store
            .record_task_sync_delta_progress_once(
                &input,
                &selection,
                OutgoingProgress::PushStarted,
                at(45),
            )
            .unwrap(),
        (start.clone(), false)
    );
    assert_eq!(
        store
            .record_task_sync_delta_progress_once(&input, &selection, unknown, at(46),)
            .unwrap(),
        (unknown_id, false)
    );
    let wrong = OutgoingProgress::Observed(PublicationObservation::Confirmed {
        observed_tip: CanonicalObject::freeze(&"different remote tip")
            .unwrap()
            .key()
            .to_string(),
    });
    assert!(
        store
            .record_task_sync_delta_progress_once(&input, &selection, wrong, at(47))
            .is_err()
    );
    assert_eq!(state(&store), before);
    let exact = OutgoingProgress::Observed(PublicationObservation::Confirmed {
        observed_tip: selection.commit.oid.clone(),
    });
    let (confirmed, fresh) = store
        .record_task_sync_delta_progress_once(&input, &selection, exact.clone(), at(48))
        .unwrap();
    assert!(fresh);
    let before = state(&store);
    assert_eq!(
        store
            .record_task_sync_delta_progress_once(&input, &selection, exact, at(49),)
            .unwrap(),
        (confirmed.clone(), false)
    );
    assert_eq!(
        store
            .record_task_sync_delta_progress_once(
                &input,
                &selection,
                OutgoingProgress::PushStarted,
                at(50),
            )
            .unwrap(),
        (start, false)
    );
    assert_eq!(state(&store), before);
    let events = store
        .task_sync_delta_outgoing_events(&f.id, &f.intent)
        .unwrap();
    assert_eq!(events.last().unwrap().0, confirmed);
    assert!(matches!(
        &events.last().unwrap().1.transition,
        Transition::PushObserved {
            observation: PublicationObservation::Confirmed { .. },
            ..
        }
    ));
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn delta_publication_recovers_only_its_exact_committed_private_ack() {
    for progress in [
        OutgoingProgress::PushStarted,
        OutgoingProgress::Observed(PublicationObservation::Unknown { observed_tip: None }),
    ] {
        let mut f = ExportFixture::new(2);
        let (input, selection) = committed(&mut f);
        if matches!(&progress, OutgoingProgress::Observed(_)) {
            f.store
                .record_task_sync_delta_progress_once(
                    &input,
                    &selection,
                    OutgoingProgress::PushStarted,
                    at(42),
                )
                .unwrap();
        }
        let before = private::read(&f.binding.store.database, &f.binding.project)
            .unwrap()
            .witness
            .unwrap();
        let (event, fresh) = f
            .store
            .record_task_sync_delta_progress_once(&input, &selection, progress.clone(), at(43))
            .unwrap();
        assert!(fresh);
        let after = private::read(&f.binding.store.database, &f.binding.project)
            .unwrap()
            .witness
            .unwrap();
        let mut pending = before;
        pending.pending_exchange = Some(
            retention::delta::progress_request_fingerprint(&f.id, &f.intent, &selection, &progress)
                .unwrap(),
        );
        private::advance(&f.binding.store.database, &after, &pending).unwrap();
        let before = state(&f.store);
        drop(f.store);
        let mut store = SqliteStore::open(&f.binding.store.database).unwrap();
        assert_eq!(
            store
                .record_task_sync_delta_progress_once(&input, &selection, progress, at(44),)
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
    }
}

#[test]
fn delta_publication_rollback_retains_ahead_witness_and_never_returns_fresh_permission() {
    let mut f = ExportFixture::new(2);
    let (input, selection) = committed(&mut f);
    let before = state(&f.store);
    {
        let transaction = f.store.begin_work_mutation().unwrap();
        let mut ledger = Ledger::load(&transaction).unwrap();
        let (previous, tail) = ledger
            .events
            .iter()
            .filter(|(_, event)| event.binding == f.id)
            .max_by_key(|(_, event)| event.sequence)
            .unwrap();
        let object = CanonicalObject::mint(&Event {
            schema_version: 1,
            binding: f.id.clone(),
            sequence: tail.sequence + 1,
            predecessor: Some(previous.clone()),
            actor: f.intent.actor.clone(),
            recorded_at: at(42),
            transition: Transition::PushStarted {
                prepared: selection.prepared.clone(),
            },
        })
        .unwrap();
        crate::storage::work::task_sync::ledger::operations::store_event(
            &transaction,
            &mut ledger,
            &object,
        )
        .unwrap();
        retention::retain_publication_witness(
            &f.binding.repository.git_directory,
            &retention::PublicationWitness {
                operation: f.intent.operation,
                event_id: object.key().clone(),
                event: object.decode().unwrap(),
            },
        )
        .unwrap();
        // Drop the writer after the external witness was flushed.
    }
    let path = f
        .binding
        .repository
        .git_directory
        .join("engram-task-sync-publication-attempts")
        .join(format!("{}.json", f.intent.operation));
    let retained = std::fs::read(&path).unwrap();
    assert!(
        f.store
            .record_task_sync_delta_progress_once(
                &input,
                &selection,
                OutgoingProgress::PushStarted,
                at(43),
            )
            .is_err()
    );
    assert_eq!(state(&f.store), before);
    assert_eq!(std::fs::read(path).unwrap(), retained);
}

#[test]
fn delta_publication_refuses_uncommitted_changed_or_unsupported_steps_without_sql_changes() {
    let mut f = ExportFixture::new(2);
    let (input, selection) = selected(&mut f);
    let before = state(&f.store);
    assert!(
        f.store
            .record_task_sync_delta_progress_once(
                &input,
                &selection,
                OutgoingProgress::PushStarted,
                at(41),
            )
            .is_err()
    );
    assert_eq!(state(&f.store), before);
    f.store
        .record_task_sync_delta_export_commit(&input, &selection, at(42))
        .unwrap();
    let before = state(&f.store);
    assert!(
        f.store
            .record_task_sync_delta_progress_once(
                &input,
                &selection,
                OutgoingProgress::LocalCommitRecorded,
                at(43),
            )
            .is_err()
    );
    let mut changed = selection.clone();
    changed.prepared = ObjectId::mint();
    assert!(
        f.store
            .record_task_sync_delta_progress_once(
                &input,
                &changed,
                OutgoingProgress::PushStarted,
                at(44),
            )
            .is_err()
    );
    assert_eq!(state(&f.store), before);
}
