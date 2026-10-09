use super::*;
use crate::storage::{
    WorkWriterAdmissionReason, last_writer_admission_allowance, with_writer_admission_test_policy,
};
use std::sync::mpsc;
use std::time::{Duration as WallDuration, Instant};

fn input(target: &str, text: &str) -> NoteInput {
    NoteInput {
        work_ref: Some(target.into()),
        text: text.into(),
        status: false,
        refs: vec![],
    }
}

fn claim(verbs: &AgentVerbs, target: &str, time: i64) {
    verbs
        .claim(
            ClaimInput {
                work_ref: target.into(),
                ttl_seconds: Some(3600),
                recover: None,
            },
            at(time),
        )
        .unwrap();
}

fn count_notes(verbs: &AgentVerbs, target: &str, text: &str) -> usize {
    verbs.show_with_notes(target, true, at(20)).unwrap().value["notes"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|note| note["summary"] == text)
        .count()
}

#[test]
fn note_writer_admission_holder_and_non_holder_outlive_old_timeout_exactly_once() {
    let (_directory, holder, path, project) = fixture();
    let target = add(&holder, "Contended note target", None, false, 0);
    claim(&holder, &target, 1);
    let other = add(&holder, "Holder's prior focus", None, false, 2);
    claim(&holder, &other, 3);
    let peer = AgentVerbs::new(
        path.clone(),
        project.clone(),
        "peer".into(),
        SessionId("peer".into()),
        None,
    );
    let peer_focus = add(&peer, "Peer's own focus", None, false, 4);
    claim(&peer, &peer_focus, 5);
    let store = SqliteStore::open(&path).unwrap();
    let target_id = store.resolve_work_ref(&project, &target).unwrap().work_id;
    let other_id = store.resolve_work_ref(&project, &other).unwrap().work_id;
    let peer_id = store
        .resolve_work_ref(&project, &peer_focus)
        .unwrap()
        .work_id;
    let other_claim = store.current_work_claim(other_id).unwrap();
    let peer_claim = store.current_work_claim(peer_id).unwrap();
    let mut blocker = rusqlite::Connection::open(&path).unwrap();
    let old_timeout: u32 = blocker
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .unwrap();
    assert_eq!(old_timeout, 5000);
    let transaction = blocker
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    let (ready, started) = mpsc::channel();
    std::thread::scope(|scope| {
        let owner_ready = ready.clone();
        let owner_target = target.clone();
        let holder_ref = &holder;
        let owner = scope.spawn(move || {
            let mut first = true;
            with_writer_admission_test_policy(
                None,
                move || {
                    if first {
                        first = false;
                        owner_ready.send(()).unwrap();
                    }
                },
                || {
                    let began = Instant::now();
                    let result = holder_ref
                        .note(&input(&owner_target, "Holder contended note"), at(6))
                        .unwrap();
                    assert!(began.elapsed() >= WallDuration::from_millis(u64::from(old_timeout)));
                    assert!(!result.text().contains("observation, no run credit"));
                },
            );
        });
        let peer_target = target.clone();
        let peer_ref = &peer;
        let observer = scope.spawn(move || {
            let mut first = true;
            with_writer_admission_test_policy(
                None,
                move || {
                    if first {
                        first = false;
                        ready.send(()).unwrap();
                    }
                },
                || {
                    let began = Instant::now();
                    let result = peer_ref
                        .note(&input(&peer_target, "Peer contended note"), at(6))
                        .unwrap();
                    assert!(began.elapsed() >= WallDuration::from_millis(u64::from(old_timeout)));
                    assert!(result.text().contains("observation, no run credit"));
                },
            );
        });
        started.recv_timeout(WallDuration::from_secs(10)).unwrap();
        started.recv_timeout(WallDuration::from_secs(10)).unwrap();
        // Both real BEGINs face the same real writer beyond the production 5 s
        // timeout. This is the only long control; all negative cases are short.
        std::thread::sleep(WallDuration::from_millis(u64::from(old_timeout) + 250));
        transaction.commit().unwrap();
        owner.join().unwrap();
        observer.join().unwrap();
    });
    assert_eq!(count_notes(&holder, &target, "Holder contended note"), 1);
    assert_eq!(count_notes(&peer, &target, "Peer contended note"), 1);
    assert_eq!(
        store
            .work_session_state(&project, &SessionId("agent".into()), at(20))
            .unwrap()
            .focused_work_id,
        Some(target_id)
    );
    assert_eq!(
        store
            .work_session_state(&project, &SessionId("peer".into()), at(20))
            .unwrap()
            .focused_work_id,
        Some(peer_id)
    );
    assert_eq!(store.current_work_claim(other_id).unwrap(), other_claim);
    assert_eq!(store.current_work_claim(peer_id).unwrap(), peer_claim);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn note_writer_admission_exhaustion_preserves_non_holder_focus_and_absence() {
    let (_directory, verbs, path, project) = fixture();
    let focus = add(&verbs, "Existing focus", None, false, 0);
    claim(&verbs, &focus, 1);
    let target = add(&verbs, "Unheld target", None, false, 2);
    let store = SqliteStore::open(&path).unwrap();
    let before = store
        .work_session_state(&project, &SessionId("agent".into()), at(3))
        .unwrap();
    let blocker = rusqlite::Connection::open(&path).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let error = with_writer_admission_test_policy(
        Some(WallDuration::from_millis(30)),
        || {},
        || verbs.note(&input(&target, "Unadmitted note"), at(3)),
    )
    .unwrap_err();
    let value = crate::store_error_value(&error.error);
    assert_eq!(value["error"]["code"], "engram_store_error");
    assert_eq!(value["error"]["details"]["reason"], "busy_budget_exhausted");
    assert_eq!(
        value["error"]["details"]["certainty"],
        "acquisition_not_started"
    );
    assert_eq!(
        value["error"]["details"]["sqlite_primary_code"],
        rusqlite::ffi::SQLITE_BUSY
    );
    blocker.execute_batch("COMMIT").unwrap();
    assert_eq!(count_notes(&verbs, &target, "Unadmitted note"), 0);
    assert_eq!(
        store
            .work_session_state(&project, &SessionId("agent".into()), at(3))
            .unwrap(),
        before
    );
}

#[test]
fn note_writer_admission_failure_after_capture_does_not_replay_or_claim_absence() {
    let (_directory, verbs, path, project) = fixture();
    let target = add(&verbs, "Post-capture contention", None, false, 0);
    let before = SqliteStore::open(&path)
        .unwrap()
        .work_session_state(&project, &SessionId("agent".into()), at(1))
        .unwrap();
    let blocker = rusqlite::Connection::open(&path).unwrap();
    let release = rusqlite::Connection::open(&path).unwrap();
    let mut acquisition = 0;
    let error = with_writer_admission_test_policy(
        Some(WallDuration::from_millis(30)),
        move || {
            acquisition += 1;
            // Non-holder: attempt, capture, then result recording.
            if acquisition == 3 {
                blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
            }
        },
        || verbs.note(&input(&target, "Already committed note"), at(1)),
    )
    .unwrap_err();
    assert!(matches!(
        &error.error,
        StoreError::WorkWriterAdmissionRefused {
            reason: WorkWriterAdmissionReason::BusyBudgetExhausted,
            ..
        }
    ));
    assert_eq!(
        crate::store_error_value(&error.error)["error"]["details"]["certainty"],
        "acquisition_not_started"
    );
    // Dropping the test policy closed the blocker and rolled back its lock.
    release.execute_batch("BEGIN IMMEDIATE; ROLLBACK").unwrap();
    assert_eq!(count_notes(&verbs, &target, "Already committed note"), 1);
    let replay = verbs
        .note(&input(&target, "Already committed note"), at(1))
        .unwrap();
    assert!(replay.text().contains("observation, no run credit"));
    assert_eq!(count_notes(&verbs, &target, "Already committed note"), 1);
    assert!(
        SqliteStore::open(&path)
            .unwrap()
            .verify_all()
            .unwrap()
            .is_healthy()
    );
    assert_eq!(
        SqliteStore::open(&path)
            .unwrap()
            .work_session_state(&project, &SessionId("agent".into()), at(2))
            .unwrap()
            .focused_work_id,
        before.focused_work_id
    );
}

#[test]
fn note_writer_admission_does_not_refresh_a_changed_claim_basis() {
    let (_directory, verbs, path, project) = fixture();
    let target = add(&verbs, "Original authority basis", None, false, 0);
    claim(&verbs, &target, 1);
    let other_connection = AgentVerbs::new(
        path.clone(),
        project,
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let changed_target = target.clone();
    let mut first = true;
    let error = with_writer_admission_test_policy(
        Some(WallDuration::from_millis(30)),
        move || {
            if first {
                first = false;
                other_connection
                    .update(
                        UpdateInput {
                            work_ref: Some(changed_target.clone()),
                            action: UpdateAction::Release {
                                reason: Some("changed authority during admission".into()),
                            },
                        },
                        at(2),
                    )
                    .unwrap();
            }
        },
        || verbs.note(&input(&target, "Stale holder note"), at(2)),
    )
    .unwrap_err();
    assert!(
        matches!(
            error.error,
            StoreError::WorkRevisionConflict { .. }
                | StoreError::WorkClaimMismatch { .. }
                | StoreError::WorkClaimLapsed { .. }
        ),
        "{error:?}"
    );
    assert_eq!(count_notes(&verbs, &target, "Stale holder note"), 0);
    assert!(
        SqliteStore::open(&path)
            .unwrap()
            .verify_all()
            .unwrap()
            .is_healthy()
    );
}

#[test]
fn note_writer_admission_fresh_process_registration_uses_the_short_window() {
    let (_directory, owner, path, project) = fixture();
    let target = add(&owner, "Fresh note process", None, false, 0);
    let timestamp = uuid::Timestamp::from_unix(
        uuid::NoContext,
        u64::try_from(at(0).timestamp()).unwrap(),
        0,
    );
    let session = SessionId(format!(
        "local-process-v1-42-{}",
        uuid::Uuid::new_v7(timestamp)
    ));
    let fresh = AgentVerbs::new_with_attribution(
        path.clone(),
        project,
        "fresh".into(),
        session.clone(),
        None,
        None,
        crate::work_service::WorkAttributionDefaults {
            actor: None,
            session: true,
        },
    );
    let inspect = rusqlite::Connection::open(&path).unwrap();
    inspect.execute_batch("BEGIN IMMEDIATE").unwrap();
    let error = with_writer_admission_test_policy(
        Some(WallDuration::from_millis(30)),
        || {},
        || fresh.note(&input(&target, "Fresh contended note"), at(1)),
    )
    .unwrap_err();
    let wire = crate::store_error_value(&error.error);
    assert_eq!(wire["error"]["details"]["reason"], "busy_budget_exhausted");
    assert!(wire["error"]["details"]["budget_ms"].as_u64().unwrap() <= 30);
    inspect.execute_batch("COMMIT").unwrap();
    assert_eq!(
        inspect
            .query_row(
                "SELECT COUNT(*) FROM work_session_state WHERE session_id = ?1",
                [&session.0],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        0
    );
    assert_eq!(count_notes(&owner, &target, "Fresh contended note"), 0);
    fresh
        .note(&input(&target, "Fresh contended note"), at(1))
        .unwrap();
    assert_eq!(count_notes(&owner, &target, "Fresh contended note"), 1);
}

#[test]
fn note_writer_admission_word_scope_restores_after_return_and_unwind() {
    let (_directory, verbs, _path, _project) = fixture();
    let target = add(&verbs, "Scope restoration", None, false, 0);
    with_writer_admission_test_policy(
        Some(WallDuration::from_millis(30)),
        || {},
        || {
            verbs.note(&input(&target, "Normal note"), at(1)).unwrap();
            assert!(last_writer_admission_allowance().unwrap() <= WallDuration::from_millis(30));
            add(&verbs, "Ordinary word after note", None, false, 2);
            assert_eq!(
                last_writer_admission_allowance(),
                Some(WallDuration::from_secs(5))
            );

            let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                verbs.service.note_writer_admission_word(|| {
                    verbs
                        .note(&input(&target, "Note before unwind"), at(3))
                        .unwrap();
                    panic!("unwind the note word scope");
                });
            }));
            assert!(panic.is_err());
            add(&verbs, "Ordinary word after unwind", None, false, 4);
            assert_eq!(
                last_writer_admission_allowance(),
                Some(WallDuration::from_secs(5))
            );
        },
    );
    assert_eq!(count_notes(&verbs, &target, "Normal note"), 1);
    assert_eq!(count_notes(&verbs, &target, "Note before unwind"), 1);
}

#[test]
fn note_writer_admission_nested_word_reuses_exhausted_window() {
    let (_directory, verbs, path, _project) = fixture();
    let target = add(&verbs, "Nested note scope", None, false, 0);
    let blocker = rusqlite::Connection::open(path).unwrap();
    with_writer_admission_test_policy(
        Some(WallDuration::from_millis(30)),
        || {},
        || {
            verbs.service.note_writer_admission_word(|| {
                verbs
                    .note(&input(&target, "First nested note"), at(1))
                    .unwrap();
                std::thread::sleep(WallDuration::from_millis(35));
                blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
                let error = verbs
                    .note(&input(&target, "Second nested note"), at(2))
                    .unwrap_err();
                assert!(matches!(
                    error.error,
                    StoreError::WorkWriterAdmissionRefused {
                        reason: WorkWriterAdmissionReason::BusyBudgetExhausted,
                        budget_ms: 0,
                        ..
                    }
                ));
                blocker.execute_batch("COMMIT").unwrap();
            });
        },
    );
    assert_eq!(count_notes(&verbs, &target, "First nested note"), 1);
    assert_eq!(count_notes(&verbs, &target, "Second nested note"), 0);
}
