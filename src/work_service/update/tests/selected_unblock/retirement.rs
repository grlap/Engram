//! Deterministic competing-connection checks for refused selected clears.

use super::*;
use crate::storage::PendingWorkProtocolAttempt;

const OPERATION: &str = "work_update:unblock";
const CORE_OPERATION: &str = "clear_work_blocker";

fn admit(
    fixture: &Fixture,
    blocker: &str,
    now: i64,
) -> (PendingWorkProtocolAttempt, ClearWorkBlockerRequest, String) {
    let mut store = SqliteStore::open(&fixture.database).expect("store");
    let basis = fixture
        .service
        .protocol_basis(&store, true, false, Some(fixture.work.work_id), at(now))
        .expect("basis");
    let input = Fixture::unblock(blocker);
    let intent = fixture.service.protocol_intent(&input);
    let key = fixture
        .service
        .selected_unblock_idempotency_key(&basis, blocker, &intent)
        .expect("key");
    let attempt = store
        .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
            project_id: &fixture.project,
            session_id: &fixture.session,
            operation: OPERATION,
            idempotency_key: &key,
            intent: &intent,
            basis: &basis,
            now: at(now),
        })
        .expect("admit");
    let guard = PendingWorkProtocolAttempt::new(
        &fixture.project,
        &fixture.session,
        OPERATION,
        &key,
        &intent,
        attempt.basis.as_ref().expect("recorded basis"),
    )
    .expect("guard");
    let work = basis.focused_work.as_ref().expect("work");
    let request = ClearWorkBlockerRequest {
        work_id: work.work_id,
        expected_work_revision: work.revision,
        blocker_id: blocker.into(),
        authority: fixture
            .service
            .planning_authority(basis.claim.as_ref(), work, at(now)),
        actor: fixture
            .service
            .actor("work_update", "clear an ambient work blocker"),
        idempotency_key: fixture
            .service
            .core_operation_key(OPERATION, &key, CORE_OPERATION)
            .expect("core key"),
        cleared_at: at(now),
    };
    (guard, request, key)
}

fn pending_basis(fixture: &Fixture, key: &str) -> Option<Vec<u8>> {
    use rusqlite::OptionalExtension;
    rusqlite::Connection::open(&fixture.database)
        .expect("connection")
        .query_row(
            "SELECT basis_json FROM work_protocol_attempts
         WHERE project_id = ?1 AND session_id = ?2 AND operation = ?3
           AND idempotency_key = ?4 AND result_id IS NULL AND result_json IS NULL",
            rusqlite::params![fixture.project.0, fixture.session.0, OPERATION, key],
            |row| row.get(0),
        )
        .optional()
        .expect("pending basis")
}

#[test]
fn a_competing_core_commit_before_retirement_preserves_the_pending_basis() {
    let fixture = fixture("retirement-core-first");
    let peer = LocalWorkService::new(
        fixture.database.clone(),
        fixture.project.clone(),
        "peer".into(),
        SessionId("retirement-peer".into()),
        None,
    );
    peer.work_update_on(
        Some(&fixture.target()),
        WorkUpdateInput::Claim {
            ttl_seconds: Some(300),
            recovery_reason: None,
            idempotency_key: String::new(),
        },
        at(1),
    )
    .expect("peer claims");
    for (now, detail) in [(2, "first"), (3, "second")] {
        peer.work_update_on(
            Some(&fixture.target()),
            WorkUpdateInput::Block {
                blocker_kind: WorkBlockerKind::Manual,
                detail: detail.into(),
                idempotency_key: String::new(),
            },
            at(now),
        )
        .expect("peer blocks");
    }
    let blocker = fixture.active()[0].clone();
    let (guard, mut request, key) = admit(&fixture, &blocker, 4);
    let recorded_basis = pending_basis(&fixture, &key).expect("pending");
    let revision = fixture.revision();
    // A's clear is refused before expiry. B commits only the core result
    // after expiry, precisely between A's refusal and retirement transaction.
    request.cleared_at = at(4000);
    let database = fixture.database.clone();
    crate::storage::before_refused_retirement(move || {
        SqliteStore::open(database)
            .expect("second connection")
            .clear_work_blocker_with_attempt(&request, &DevelopmentNoopRedactor, Some(&guard))
            .expect("competing core clear");
    });
    let refusal = fixture.selected(&blocker, 4).expect_err("live peer claim");
    assert!(!matches!(
        refusal,
        StoreError::WorkOperationIdempotencyConflict { .. }
    ));
    assert_eq!(pending_basis(&fixture, &key), Some(recorded_basis));
    assert_eq!(fixture.revision(), revision + 1);
    assert_eq!(fixture.clear_events(), 1);
    let core_key = fixture
        .service
        .core_operation_key(OPERATION, &key, CORE_OPERATION)
        .expect("key");
    let core = SqliteStore::open(&fixture.database)
        .expect("store")
        .work_operation_result_value(CORE_OPERATION, &core_key)
        .expect("core");
    let recovered = fixture
        .selected(&blocker, 4001)
        .expect("recover core result");
    let replay = fixture.selected(&blocker, 4002).expect("exact repeat");
    assert_eq!(
        serde_json::to_value(recovered).unwrap(),
        serde_json::to_value(replay).unwrap()
    );
    assert_eq!(
        SqliteStore::open(&fixture.database)
            .expect("store")
            .work_operation_result_value(CORE_OPERATION, &core_key)
            .expect("core"),
        core
    );
    assert_eq!(fixture.clear_events(), 1);
}

#[test]
fn retirement_before_a_stale_clear_prevents_mutation_and_a_fresh_attempt_can_clear() {
    let fixture = fixture("retirement-first");
    fixture.block(WorkBlockerKind::Manual, "first", 1);
    let blocker = fixture.active()[0].clone();
    let (guard, mut request, key) = admit(&fixture, &blocker, 2);
    let revision = fixture.revision();
    let mut store = SqliteStore::open(&fixture.database).expect("store");
    store
        .retire_refused_work_protocol_attempt(&guard, CORE_OPERATION, &request.idempotency_key)
        .expect("retire");
    assert!(pending_basis(&fixture, &key).is_none());
    assert!(matches!(
        store.clear_work_blocker_with_attempt(&request, &DevelopmentNoopRedactor, Some(&guard),),
        Err(StoreError::InvalidWork(reason))
            if reason == crate::storage::STALE_SELECTED_UNBLOCK_REFUSAL
    ));
    assert_eq!(fixture.revision(), revision);
    assert_eq!(fixture.clear_events(), 0);
    assert_eq!(fixture.active().as_slice(), std::slice::from_ref(&blocker));
    fixture
        .selected(&blocker, 3)
        .expect("fresh admission clears");
    assert_eq!(fixture.clear_events(), 1);
    // A committed core replay remains valid after the protocol row completed.
    request.cleared_at = at(3);
    store
        .clear_work_blocker_with_attempt(&request, &DevelopmentNoopRedactor, Some(&guard))
        .expect("core replay precedes the retired-attempt guard");
}

#[test]
fn late_retirement_preserves_a_replacement_attempt_on_a_new_basis() {
    let fixture = fixture("retirement-replacement");
    fixture.block(WorkBlockerKind::Manual, "first", 1);
    let blocker = fixture.active()[0].clone();
    let (old, request, key) = admit(&fixture, &blocker, 2);
    let mut store = SqliteStore::open(&fixture.database).expect("store");
    store
        .retire_refused_work_protocol_attempt(&old, CORE_OPERATION, &request.idempotency_key)
        .expect("retire old");
    fixture.block(WorkBlockerKind::Manual, "second", 3);
    let (_new, _request, replacement_key) = admit(&fixture, &blocker, 4);
    assert_eq!(replacement_key, key);
    let replacement = pending_basis(&fixture, &key).expect("replacement basis");
    store
        .retire_refused_work_protocol_attempt(&old, CORE_OPERATION, &request.idempotency_key)
        .expect("late retirement is a no-op");
    assert_eq!(pending_basis(&fixture, &key), Some(replacement.clone()));
    assert!(matches!(
        store.clear_work_blocker_with_attempt(&request, &DevelopmentNoopRedactor, Some(&old)),
        Err(StoreError::InvalidWork(reason))
            if reason == crate::storage::STALE_SELECTED_UNBLOCK_REFUSAL
    ));
    assert_eq!(pending_basis(&fixture, &key), Some(replacement));
    assert_eq!(fixture.clear_events(), 0);
    fixture
        .selected(&blocker, 5)
        .expect("replacement remains usable");
    assert_eq!(fixture.clear_events(), 1);
}

#[test]
fn a_mismatched_request_cannot_retire_or_use_an_existing_pending_attempt() {
    let fixture = fixture("retirement-wrong-request");
    fixture.block(WorkBlockerKind::Manual, "first", 1);
    let blocker = fixture.active()[0].clone();
    let (_guard, request, key) = admit(&fixture, &blocker, 2);
    let original = pending_basis(&fixture, &key).expect("basis");
    let basis: serde_json::Value = serde_json::from_slice(&original).expect("basis JSON");
    let wrong = PendingWorkProtocolAttempt::new(
        &fixture.project,
        &fixture.session,
        OPERATION,
        &key,
        &serde_json::json!({"another": "intent"}),
        &basis,
    )
    .expect("different request guard");
    let mut store = SqliteStore::open(&fixture.database).expect("store");
    store
        .retire_refused_work_protocol_attempt(&wrong, CORE_OPERATION, &request.idempotency_key)
        .expect("mismatch is a no-op");
    assert_eq!(pending_basis(&fixture, &key), Some(original));
    let revision = fixture.revision();
    assert!(matches!(
        store.clear_work_blocker_with_attempt(&request, &DevelopmentNoopRedactor, Some(&wrong),),
        Err(StoreError::WorkOperationIdempotencyConflict { .. })
    ));
    assert_eq!(fixture.revision(), revision);
    assert_eq!(fixture.clear_events(), 0);
}

#[test]
fn a_concurrent_identical_service_refusal_retires_the_stale_call_without_a_clear() {
    let fixture = fixture("service-retired");
    let peer = LocalWorkService::new(
        fixture.database.clone(),
        fixture.project.clone(),
        "peer".into(),
        SessionId("live-peer".into()),
        None,
    );
    peer.work_update_on(
        Some(&fixture.target()),
        WorkUpdateInput::Claim {
            ttl_seconds: Some(300),
            recovery_reason: None,
            idempotency_key: String::new(),
        },
        at(1),
    )
    .expect("peer claim");
    peer.work_update_on(
        Some(&fixture.target()),
        WorkUpdateInput::Block {
            blocker_kind: WorkBlockerKind::Manual,
            detail: "waiting".into(),
            idempotency_key: String::new(),
        },
        at(2),
    )
    .expect("block");
    let blocker = fixture.active()[0].clone();
    let competing = LocalWorkService::new(
        fixture.database.clone(),
        fixture.project.clone(),
        "agent".into(),
        fixture.session.clone(),
        Some("protocol-test".into()),
    );
    let target = fixture.target();
    let selected = blocker.clone();
    before_selected_clear(move || {
        let error = competing
            .work_update_on(Some(&target), Fixture::unblock(&selected), at(3))
            .expect_err("identical call refused under peer claim");
        assert!(!matches!(
            error,
            StoreError::WorkOperationIdempotencyConflict { .. }
        ));
    });
    let revision = fixture.revision();
    let error = fixture.selected(&blocker, 3).expect_err("retired attempt");
    assert!(matches!(error, StoreError::InvalidWork(reason)
        if reason.starts_with(crate::storage::STALE_SELECTED_UNBLOCK_REFUSAL)
        && reason.contains(&format!("engram work show {}", fixture.work.short_ref))));
    assert_eq!(fixture.revision(), revision);
    assert_eq!(fixture.active(), std::slice::from_ref(&blocker));
    assert_eq!(fixture.clear_events(), 0);
    peer.work_update_on(
        Some(&fixture.target()),
        WorkUpdateInput::Release {
            reason: "returning work".into(),
            waiver_reason: Some("returning work".into()),
            idempotency_key: String::new(),
        },
        at(4),
    )
    .expect("release");
    fixture.selected(&blocker, 5).expect("fresh attempt");
    assert_eq!(fixture.clear_events(), 1);
}

#[test]
fn a_completed_wrapper_without_a_core_replay_cannot_admit_a_new_clear() {
    let fixture = fixture("completed-guard");
    fixture.block(WorkBlockerKind::Manual, "first", 1);
    let blocker = fixture.active()[0].clone();
    let (guard, mut request, _key) = admit(&fixture, &blocker, 2);
    fixture.selected(&blocker, 3).expect("complete wrapper");
    // An unused core key reaches the guard rather than the preceding replay.
    request.idempotency_key.push_str(":unused");
    let mut store = SqliteStore::open(&fixture.database).expect("store");
    let revision = fixture.revision();
    assert!(matches!(store.clear_work_blocker_with_attempt(
        &request, &DevelopmentNoopRedactor, Some(&guard)),
        Err(StoreError::InvalidWork(reason)) if reason == crate::storage::STALE_SELECTED_UNBLOCK_REFUSAL));
    assert_eq!(fixture.revision(), revision);
    assert_eq!(fixture.active(), [] as [String; 0]);
    assert_eq!(fixture.clear_events(), 1);
}

#[test]
fn committed_core_replay_mismatches_remain_conflicts_before_the_stale_guard() {
    let fixture = fixture("core-conflict");
    fixture.block(WorkBlockerKind::Manual, "first", 1);
    let blocker = fixture.active()[0].clone();
    let (guard, mut request, key) = admit(&fixture, &blocker, 2);
    let mut store = SqliteStore::open(&fixture.database).expect("store");
    store
        .retire_refused_work_protocol_attempt(&guard, CORE_OPERATION, &request.idempotency_key)
        .expect("retire guard");
    store
        .clear_work_blocker(&request, &DevelopmentNoopRedactor)
        .expect("commit core");
    let core = store
        .work_operation_result_value(CORE_OPERATION, &request.idempotency_key)
        .expect("result");
    request.blocker_id = "different intent".into();
    assert!(matches!(
        store.clear_work_blocker_with_attempt(&request, &DevelopmentNoopRedactor, Some(&guard)),
        Err(StoreError::WorkOperationIdempotencyConflict { .. })
    ));
    assert_eq!(
        store
            .work_operation_result_value(CORE_OPERATION, &request.idempotency_key)
            .expect("result"),
        core
    );
    assert!(pending_basis(&fixture, &key).is_none());
    assert_eq!(fixture.clear_events(), 1);
}

#[test]
fn public_clear_refusals_name_raw_ids_and_leave_blockers_unchanged() {
    let fixture = fixture("raw-id");
    fixture.block(WorkBlockerKind::Manual, "first", 1);
    fixture.block(WorkBlockerKind::Manual, "second", 2);
    let blocker = fixture.active()[0].clone();
    let (_guard, mut request, _key) = admit(&fixture, &blocker, 3);
    let mut store = SqliteStore::open(&fixture.database).expect("store");
    let before = fixture.active();
    let revision = fixture.revision();
    for invalid in ["unknown".to_owned(), blocker_selector::encode(&blocker)] {
        request.blocker_id = invalid;
        let error = store
            .clear_work_blocker(&request, &DevelopmentNoopRedactor)
            .expect_err("raw id required");
        assert!(matches!(error, StoreError::InvalidWork(reason)
            if reason == crate::storage::UNKNOWN_BLOCKER_REFUSAL
            && reason.contains("blocker_id") && !reason.contains("selector")));
        assert_eq!(fixture.active(), before);
        assert_eq!(fixture.revision(), revision);
        assert_eq!(fixture.clear_events(), 0);
    }
    let error = fixture
        .service
        .work_update_on(
            Some(&fixture.target()),
            WorkUpdateInput::Unblock {
                blocker_id: None,
                idempotency_key: String::new(),
            },
            at(4),
        )
        .expect_err("multiple blockers");
    assert!(matches!(error, StoreError::InvalidWork(reason)
        if reason == MULTIPLE_BLOCKERS_REFUSAL && reason.contains("blocker_id")));
    assert_eq!(fixture.active(), before);
}

#[test]
fn an_explicit_unblock_key_still_refuses_a_changed_selector_intent() {
    let fixture = fixture("explicit-conflict");
    fixture.block(WorkBlockerKind::Manual, "first", 1);
    fixture.block(WorkBlockerKind::Manual, "second", 2);
    let [first, second] = fixture.active().try_into().expect("two blockers");
    let input = |blocker| WorkUpdateInput::Unblock {
        blocker_id: Some(blocker),
        idempotency_key: "explicit-clear".into(),
    };
    fixture
        .service
        .work_update_on(Some(&fixture.target()), input(first.clone()), at(3))
        .expect("first explicit clear");
    let revision = fixture.revision();
    let error = fixture
        .service
        .work_update_on(Some(&fixture.target()), input(second.clone()), at(4))
        .expect_err("changed intent");
    assert!(matches!(
        error,
        StoreError::WorkOperationIdempotencyConflict { .. }
    ));
    assert_eq!(fixture.revision(), revision);
    assert_eq!(fixture.active(), [second]);
    assert_eq!(fixture.clear_events(), 1);
    fixture
        .service
        .work_update_on(Some(&fixture.target()), input(first), at(5))
        .expect("original intent replays");
    assert_eq!(fixture.clear_events(), 1);
}
