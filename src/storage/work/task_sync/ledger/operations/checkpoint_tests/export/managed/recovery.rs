use super::*;
use crate::task_sync::retention::audit::AuditReservation;
use std::sync::{Arc, Mutex};

#[test]
fn committed_push_start_holds_the_custody_reservation_across_its_post_commit_check() {
    use crate::storage::work::task_sync::producing_origin::AFTER_POST_COMMIT_CAPTURE;
    let mut f = fixture();
    let (input, selection) = commit::selected(&mut f);
    f.store
        .record_task_sync_delta_export_commit(&input, &selection, at(41))
        .unwrap();
    let root = f.binding.repository.git_directory.clone();
    let checked = std::rc::Rc::new(std::cell::Cell::new(0_usize));
    let seen = checked.clone();
    // Between the committed start's custody capture and its check, no other
    // writer can take the repository's reservation, so none moves the
    // journal or writes an artifact there.
    AFTER_POST_COMMIT_CAPTURE.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            seen.set(seen.get() + 1);
            assert!(AuditReservation::acquire(&root).is_err());
        }));
    });
    let result = f.store.record_task_sync_delta_progress_once(
        &input,
        &selection,
        OutgoingProgress::PushStarted,
        at(42),
    );
    AFTER_POST_COMMIT_CAPTURE.with(|hook| hook.borrow_mut().take());
    let (started, fresh) = result.unwrap();
    assert_eq!(checked.get(), 1, "one fenced capture confirms the start");
    assert!(
        fresh,
        "the first committed start authorizes its one dispatch"
    );
    assert_eq!(
        f.store
            .record_task_sync_delta_progress_once(
                &input,
                &selection,
                OutgoingProgress::PushStarted,
                at(43)
            )
            .unwrap(),
        (started, false)
    );
}

#[test]
fn committed_push_start_rereads_a_journal_another_writer_moved_while_holding_its_reservation() {
    use crate::storage::work::task_sync::producing_origin::AFTER_POST_COMMIT_CAPTURE;
    let mut f = fixture();
    let (input, selection) = commit::selected(&mut f);
    f.store
        .record_task_sync_delta_export_commit(&input, &selection, at(41))
        .unwrap();
    let held = Arc::new(Mutex::new(None::<AuditReservation>));
    let retained = held.clone();
    let root = f.binding.repository.git_directory.clone();
    // Another writer takes the repository reservation as the start commits
    // and keeps it past the post-commit fence's wait, so the check runs
    // unfenced; once, between its capture and its check, that writer
    // reserves a write of its own and moves the journal.
    f.store
        .connection
        .commit_hook(Some(move || {
            *retained.lock().unwrap() = Some(AuditReservation::acquire(&root).unwrap());
            false
        }))
        .unwrap();
    let writer = held.clone();
    let moved = std::rc::Rc::new(std::cell::Cell::new(false));
    let seen = moved.clone();
    AFTER_POST_COMMIT_CAPTURE.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            if !seen.replace(true) {
                writer
                    .lock()
                    .unwrap()
                    .as_ref()
                    .unwrap()
                    .reserve_artifact(
                        uuid::Uuid::new_v4(),
                        crate::task_sync::retention::audit::journal::Family::Commit,
                        &"another writer",
                    )
                    .unwrap();
            }
        }));
    });
    let result = f.store.record_task_sync_delta_progress_once(
        &input,
        &selection,
        OutgoingProgress::PushStarted,
        at(42),
    );
    AFTER_POST_COMMIT_CAPTURE.with(|hook| hook.borrow_mut().take());
    f.store
        .connection
        .commit_hook(None::<fn() -> bool>)
        .unwrap();
    drop(held.lock().unwrap().take());
    let (started, fresh) = result.unwrap();
    assert!(moved.get());
    assert!(
        fresh,
        "the first committed start authorizes its one dispatch"
    );
    // The exact retry never dispatches again.
    assert_eq!(
        f.store
            .record_task_sync_delta_progress_once(
                &input,
                &selection,
                OutgoingProgress::PushStarted,
                at(43)
            )
            .unwrap(),
        (started, false)
    );
}

#[test]
fn a_committed_start_that_returned_no_success_carries_its_dispatch_to_the_exact_retry() {
    use crate::storage::work::task_sync::producing_origin::AFTER_POST_COMMIT_CAPTURE;
    let mut f = fixture();
    let (input, selection) = commit::selected(&mut f);
    f.store
        .record_task_sync_delta_export_commit(&input, &selection, at(41))
        .unwrap();
    let held = Arc::new(Mutex::new(None::<AuditReservation>));
    let retained = held.clone();
    let root = f.binding.repository.git_directory.clone();
    // Another writer holds the repository reservation from the start's
    // commit on, past the fence's wait, and moves the journal after every
    // post-commit capture, more often than the recaptures allow.
    f.store
        .connection
        .commit_hook(Some(move || {
            *retained.lock().unwrap() = Some(AuditReservation::acquire(&root).unwrap());
            false
        }))
        .unwrap();
    let writer = held.clone();
    AFTER_POST_COMMIT_CAPTURE.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            writer
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .reserve_artifact(
                    uuid::Uuid::new_v4(),
                    crate::task_sync::retention::audit::journal::Family::Commit,
                    &"another writer",
                )
                .unwrap();
        }));
    });
    let result = f.store.record_task_sync_delta_progress_once(
        &input,
        &selection,
        OutgoingProgress::PushStarted,
        at(42),
    );
    AFTER_POST_COMMIT_CAPTURE.with(|hook| hook.borrow_mut().take());
    f.store
        .connection
        .commit_hook(None::<fn() -> bool>)
        .unwrap();
    drop(held.lock().unwrap().take());
    assert!(
        result.is_err(),
        "the start committed but its custody was never confirmed"
    );
    // Retries of the same pending request never run at once: while another
    // holds the project's origin continuity, as a concurrent retry would, this
    // one refuses as busy and receives no dispatch.
    let database = std::path::PathBuf::from(f.store.connection.path().unwrap());
    let concurrent = crate::task_sync::origin::continuity::PreparedContinuity::acquire(
        &database,
        &f.binding.project,
    )
    .unwrap();
    let busy = f
        .store
        .record_task_sync_delta_progress_once(
            &input,
            &selection,
            OutgoingProgress::PushStarted,
            at(43),
        )
        .unwrap_err();
    assert!(busy.to_string().contains("busy"), "{busy}");
    drop(concurrent);
    // The call that committed the start returned no success, so it handed
    // out no dispatch: the exact retry carries it, once.
    let (started, owed) = f
        .store
        .record_task_sync_delta_progress_once(
            &input,
            &selection,
            OutgoingProgress::PushStarted,
            at(43),
        )
        .unwrap();
    assert!(owed, "the exact retry authorizes the one dispatch");
    assert_eq!(
        f.store
            .record_task_sync_delta_progress_once(
                &input,
                &selection,
                OutgoingProgress::PushStarted,
                at(44)
            )
            .unwrap(),
        (started, false)
    );
}

#[test]
fn a_start_whose_witness_advanced_before_its_write_reported_failure_still_dispatches() {
    use crate::task_sync::origin::private::FAIL_AFTER_WITNESS_ADVANCE;
    let mut f = fixture();
    let (input, selection) = commit::selected(&mut f);
    f.store
        .record_task_sync_delta_export_commit(&input, &selection, at(41))
        .unwrap();
    // The witness replacement lands, then its write reports failure, as a
    // failed directory sync after the rename: the witness already records
    // the start, so its one dispatch is handed out rather than lost.
    FAIL_AFTER_WITNESS_ADVANCE.with(|fail| fail.set(true));
    let result = f.store.record_task_sync_delta_progress_once(
        &input,
        &selection,
        OutgoingProgress::PushStarted,
        at(42),
    );
    let consumed = !FAIL_AFTER_WITNESS_ADVANCE.with(|fail| fail.replace(false));
    assert!(consumed, "the start advanced the witness");
    let (started, fresh) = result.unwrap();
    assert!(
        fresh,
        "the first committed start authorizes its one dispatch"
    );
    assert_eq!(
        f.store
            .record_task_sync_delta_progress_once(
                &input,
                &selection,
                OutgoingProgress::PushStarted,
                at(43)
            )
            .unwrap(),
        (started, false)
    );
}

#[test]
fn a_failed_replay_of_an_acknowledged_start_never_dispatches_it_again() {
    use crate::storage::work::task_sync::producing_origin::AFTER_POST_COMMIT_CAPTURE;
    let mut f = fixture();
    let (input, selection) = commit::selected(&mut f);
    f.store
        .record_task_sync_delta_export_commit(&input, &selection, at(41))
        .unwrap();
    let (started, fresh) = f
        .store
        .record_task_sync_delta_progress_once(
            &input,
            &selection,
            OutgoingProgress::PushStarted,
            at(42),
        )
        .unwrap();
    assert!(
        fresh,
        "the first committed start authorizes its one dispatch"
    );
    // A replay of that acknowledged start reserves its request again, then
    // fails after its effect: another writer holds the repository
    // reservation past the fence's wait and moves the journal after every
    // post-commit capture.
    let root = f.binding.repository.git_directory.clone();
    let writer = Arc::new(Mutex::new(Some(AuditReservation::acquire(&root).unwrap())));
    let moving = writer.clone();
    AFTER_POST_COMMIT_CAPTURE.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            moving
                .lock()
                .unwrap()
                .as_ref()
                .unwrap()
                .reserve_artifact(
                    uuid::Uuid::new_v4(),
                    crate::task_sync::retention::audit::journal::Family::Commit,
                    &"another writer",
                )
                .unwrap();
        }));
    });
    let replay = f.store.record_task_sync_delta_progress_once(
        &input,
        &selection,
        OutgoingProgress::PushStarted,
        at(43),
    );
    AFTER_POST_COMMIT_CAPTURE.with(|hook| hook.borrow_mut().take());
    drop(writer.lock().unwrap().take());
    assert!(replay.is_err(), "the replay's custody was never confirmed");
    // The failed replay left its own request pending, as an interrupted first
    // start would; the dispatch was already handed out, so none is owed now.
    let database = std::path::PathBuf::from(f.store.connection.path().unwrap());
    assert!(
        crate::task_sync::origin::private::read(&database, &f.binding.project)
            .unwrap()
            .witness
            .unwrap()
            .pending_exchange
            .is_some(),
        "the failed replay left its request pending"
    );
    assert_eq!(
        f.store
            .record_task_sync_delta_progress_once(
                &input,
                &selection,
                OutgoingProgress::PushStarted,
                at(44)
            )
            .unwrap(),
        (started, false)
    );
}

#[test]
fn committed_push_start_survives_a_busy_settlement_and_never_redispatches() {
    let mut f = fixture();
    let (input, selection) = commit::selected(&mut f);
    f.store
        .record_task_sync_delta_export_commit(&input, &selection, at(41))
        .unwrap();
    let held = Arc::new(Mutex::new(None::<AuditReservation>));
    let retained = held.clone();
    let root = f.binding.repository.git_directory.clone();
    // Hold the real repository reservation after the publication witness is
    // durable and as SQLite commits. Reads must not take this exclusive lock;
    // settlement can retry later but cannot hide the committed dispatch result.
    f.store
        .connection
        .commit_hook(Some(move || {
            *retained.lock().unwrap() = Some(AuditReservation::acquire(&root).unwrap());
            false
        }))
        .unwrap();
    let (started, fresh) = f
        .store
        .record_task_sync_delta_progress_once(
            &input,
            &selection,
            OutgoingProgress::PushStarted,
            at(42),
        )
        .unwrap();
    assert!(
        fresh,
        "the first committed start authorizes its one dispatch"
    );
    assert!(held.lock().unwrap().is_some());
    let pending = journal_of(&f);
    assert!(!pending.writes.is_empty());
    f.store
        .connection
        .commit_hook(None::<fn() -> bool>)
        .unwrap();
    drop(held.lock().unwrap().take());
    assert_eq!(
        f.store
            .record_task_sync_delta_progress_once(
                &input,
                &selection,
                OutgoingProgress::PushStarted,
                at(43)
            )
            .unwrap(),
        (started, false)
    );
    assert!(journal_of(&f).writes.is_empty());
    assert!(f.store.verify_all().unwrap().is_healthy());
}
