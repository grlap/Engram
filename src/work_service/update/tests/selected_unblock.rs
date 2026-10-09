//! A selected unblock is one intent for its one blocker: repeating it after a
//! lost answer returns the recorded result with one clear and one event,
//! whatever else changed on the item; an attempt that never reached its clear
//! refuses once the item changed; a bare unblock keeps its own identity.

use super::*;

mod retirement;

/// Fields drop in declaration order, so the service releases the store
/// before its directory is removed.
struct Fixture {
    database: std::path::PathBuf,
    project: ProjectId,
    session: SessionId,
    service: LocalWorkService,
    work: WorkItemSummary,
    _directory: crate::test_support::TempHome,
}

fn fixture(name: &str) -> Fixture {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId(format!("selected-unblock-{name}"));
    let session = SessionId(format!("selected-unblock-{name}-session"));
    let service = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        session.clone(),
        Some("protocol-test".into()),
    );
    let work = proposed_root(
        service
            .work_propose(root_input("Blocked twice", &format!("{name}-root")), at(0))
            .expect("root"),
    );
    Fixture {
        database,
        project,
        session,
        service,
        work,
        _directory: directory,
    }
}

impl Fixture {
    fn target(&self) -> String {
        self.work.work_id.0.to_string()
    }

    fn block(&self, kind: WorkBlockerKind, detail: &str, now: i64) {
        self.service
            .work_update_on(
                Some(&self.target()),
                WorkUpdateInput::Block {
                    blocker_kind: kind,
                    detail: detail.into(),
                    idempotency_key: String::new(),
                },
                at(now),
            )
            .expect("block");
    }

    fn active(&self) -> Vec<String> {
        SqliteStore::open(&self.database)
            .expect("store")
            .inspect_work(self.work.work_id, at(100))
            .expect("status")
            .blockers
            .into_iter()
            .map(|blocker| blocker.blocker_id)
            .collect()
    }

    fn revision(&self) -> i64 {
        SqliteStore::open(&self.database)
            .expect("store")
            .get_work_item(self.work.work_id)
            .expect("item")
            .revision
    }

    fn clear_events(&self) -> i64 {
        rusqlite::Connection::open(&self.database)
            .expect("connection")
            .query_row(
                "SELECT count(*) FROM objects WHERE object_kind = 'work_event'
                 AND json_extract(CAST(canonical_json AS TEXT), '$.transition.kind') = 'unblocked'
                 AND json_extract(CAST(canonical_json AS TEXT), '$.work_id') = ?1",
                [self.target()],
                |row| row.get(0),
            )
            .expect("count clear events")
    }

    fn unblock(blocker_id: &str) -> WorkUpdateInput {
        WorkUpdateInput::Unblock {
            blocker_id: Some(blocker_id.to_owned()),
            idempotency_key: String::new(),
        }
    }

    fn selected(&self, blocker_id: &str, now: i64) -> Result<WorkUpdateResult, StoreError> {
        self.service
            .work_update_on(Some(&self.target()), Self::unblock(blocker_id), at(now))
    }
}

#[test]
fn a_repeated_selected_clear_replays_one_clear_after_unrelated_changes() {
    let fixture = fixture("replay");
    fixture.block(WorkBlockerKind::Manual, "Same reason", 1);
    fixture.block(WorkBlockerKind::HumanDecision, "Same reason", 2);
    let [first, second] = fixture.active().try_into().expect("two blockers");

    let cleared = fixture.selected(&second, 3).expect("clear the second");
    let revision = fixture.revision();
    assert_eq!(fixture.active(), std::slice::from_ref(&first));
    assert_eq!(fixture.clear_events(), 1);

    // An unrelated blocker bumps the revision the clear was made at; the
    // repeated call still finds its recorded attempt.
    fixture.block(WorkBlockerKind::ExternalInput, "Another reason", 4);
    let third = fixture
        .active()
        .into_iter()
        .find(|blocker| blocker != &first)
        .expect("third blocker");
    let replayed = fixture.selected(&second, 5).expect("replay");
    assert_eq!(
        serde_json::to_value(&replayed).expect("replay"),
        serde_json::to_value(&cleared).expect("original")
    );
    assert_eq!(fixture.clear_events(), 1);
    assert_eq!(fixture.revision(), revision + 1);
    let mut remaining = vec![first, third];
    remaining.sort();
    assert_eq!(fixture.active(), remaining);
}

#[test]
fn a_clear_committed_before_its_answer_was_recorded_is_recovered_once() {
    let fixture = fixture("recovered");
    fixture.block(WorkBlockerKind::Manual, "Same reason", 1);
    fixture.block(WorkBlockerKind::Manual, "Same reason", 2);
    let [first, second] = fixture.active().try_into().expect("two blockers");
    let input = Fixture::unblock(&first);

    // The attempt begins and the core clear commits, but the answer is lost
    // before the protocol result is recorded.
    let mut store = SqliteStore::open(&fixture.database).expect("store");
    store
        .focus_work_session(
            &fixture.project,
            &fixture.session,
            fixture.work.work_id,
            at(3),
        )
        .expect("focus");
    let basis = fixture
        .service
        .protocol_basis(&store, true, false, Some(fixture.work.work_id), at(3))
        .expect("basis");
    let intent = fixture.service.protocol_intent(&input);
    let key = fixture
        .service
        .selected_unblock_idempotency_key(&basis, &first, &intent)
        .expect("selected key");
    store
        .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
            project_id: &fixture.project,
            session_id: &fixture.session,
            operation: "work_update:unblock",
            idempotency_key: &key,
            intent: &intent,
            basis: &basis,
            now: at(3),
        })
        .expect("begin attempt");
    let work = basis.focused_work.clone().expect("bound item");
    store
        .clear_work_blocker(
            &ClearWorkBlockerRequest {
                work_id: work.work_id,
                expected_work_revision: work.revision,
                blocker_id: first.clone(),
                authority: fixture
                    .service
                    .planning_authority(basis.claim.as_ref(), &work, at(3)),
                actor: fixture.service.actor("test", "commit only the core clear"),
                idempotency_key: fixture
                    .service
                    .core_operation_key("work_update:unblock", &key, "clear_work_blocker")
                    .expect("scoped key"),
                cleared_at: at(3),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("core clear without protocol result");
    drop(store);
    // The item changes again before the caller repeats itself: an unrelated
    // blocker, then another session's clear of the second blocker.
    fixture.block(WorkBlockerKind::Policy, "Later reason", 4);
    let peer = LocalWorkService::new(
        fixture.database.clone(),
        fixture.project.clone(),
        "peer".into(),
        SessionId("selected-unblock-recovered-peer".into()),
        Some("protocol-test".into()),
    );
    peer.work_update_on(
        Some(&fixture.target()),
        WorkUpdateInput::Unblock {
            blocker_id: Some(second.clone()),
            idempotency_key: String::new(),
        },
        at(5),
    )
    .expect("peer clears the second");

    // The same session repeats its word; the answer names the blocker its
    // own committed clear removed, not the later change.
    let verbs = crate::verbs::AgentVerbs::new(
        fixture.database.clone(),
        fixture.project.clone(),
        "agent".into(),
        fixture.session.clone(),
        Some("protocol-test".into()),
    );
    let selector = crate::work_service::blocker_selector::encode(&first);
    let repeat_word = || {
        verbs
            .update(
                crate::verbs::UpdateInput {
                    work_ref: Some(fixture.target()),
                    action: crate::verbs::UpdateAction::Unblock {
                        blocker: Some(selector.clone()),
                    },
                },
                at(6),
            )
            .expect("recover the clear")
    };
    let recovered = repeat_word();
    assert_eq!(recovered.value["operation"], "unblock");
    assert_eq!(recovered.value["cleared_blocker"], selector.as_str());
    assert!(
        recovered.text().contains(&format!(
            "cleared blocker {selector} (manual) \"Same reason\""
        )),
        "{}",
        recovered.text()
    );
    let replayed = repeat_word();
    assert_eq!(replayed.value["cleared_blocker"], selector.as_str());
    assert_eq!(fixture.clear_events(), 2);
    assert!(!fixture.active().contains(&first));
    assert!(!fixture.active().contains(&second));
    assert_eq!(fixture.active().len(), 1);
}

#[test]
fn an_unfinished_selected_clear_refuses_once_the_item_changed() {
    let fixture = fixture("unfinished");
    fixture.block(WorkBlockerKind::Manual, "First reason", 1);
    fixture.block(WorkBlockerKind::Manual, "Second reason", 2);
    let [first, _second] = fixture.active().try_into().expect("two blockers");
    let input = Fixture::unblock(&first);

    // The attempt begins, and nothing else happens before the answer is lost.
    let mut store = SqliteStore::open(&fixture.database).expect("store");
    store
        .focus_work_session(
            &fixture.project,
            &fixture.session,
            fixture.work.work_id,
            at(3),
        )
        .expect("focus");
    let basis = fixture
        .service
        .protocol_basis(&store, true, false, Some(fixture.work.work_id), at(3))
        .expect("basis");
    let intent = fixture.service.protocol_intent(&input);
    let key = fixture
        .service
        .selected_unblock_idempotency_key(&basis, &first, &intent)
        .expect("selected key");
    store
        .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
            project_id: &fixture.project,
            session_id: &fixture.session,
            operation: "work_update:unblock",
            idempotency_key: &key,
            intent: &intent,
            basis: &basis,
            now: at(3),
        })
        .expect("begin attempt");
    drop(store);
    fixture.block(WorkBlockerKind::Manual, "Third reason", 4);
    let before = fixture.active();
    let revision = fixture.revision();

    let refused = fixture.selected(&first, 5).expect_err("changed basis");
    assert!(
        matches!(&refused, StoreError::InvalidWork(words)
            if words.contains("never answered") && words.contains("engram work show")),
        "{refused:?}"
    );
    let verbs = crate::verbs::AgentVerbs::new(
        fixture.database.clone(),
        fixture.project.clone(),
        "agent".into(),
        fixture.session.clone(),
        Some("protocol-test".into()),
    );
    let selector = crate::work_service::blocker_selector::encode(&first);
    let error = verbs
        .update(
            crate::verbs::UpdateInput {
                work_ref: Some(fixture.target()),
                action: crate::verbs::UpdateAction::Unblock {
                    blocker: Some(selector),
                },
            },
            at(5),
        )
        .expect_err("interrupted word refusal");
    assert!(verbs.error_message(&error).contains("selector"));
    assert_eq!(
        verbs.error_guidance(&error).next,
        [format!("engram work show {}", fixture.work.short_ref)]
    );
    assert_eq!(fixture.active(), before);
    assert_eq!(fixture.revision(), revision);
    assert_eq!(fixture.clear_events(), 0);
}

#[test]
fn a_bare_unblock_keeps_its_meaning_for_one_blocker_and_refuses_for_two() {
    let fixture = fixture("bare");
    fixture.block(WorkBlockerKind::Manual, "Only reason", 1);
    let bare = WorkUpdateInput::Unblock {
        blocker_id: None,
        idempotency_key: String::new(),
    };
    fixture
        .service
        .work_update_on(Some(&fixture.target()), bare.clone(), at(2))
        .expect("bare unblock clears the only blocker");
    let observed = fixture.active();
    assert!(observed.is_empty(), "{observed:?}");

    fixture.block(WorkBlockerKind::Manual, "One of two", 3);
    fixture.block(WorkBlockerKind::Manual, "Two of two", 4);
    let before = fixture.active();
    let refused = fixture
        .service
        .work_update_on(Some(&fixture.target()), bare, at(5))
        .expect_err("two blockers");
    assert!(
        refused.to_string().contains("multiple active blockers"),
        "{refused}"
    );
    assert_eq!(fixture.active(), before);
}

#[test]
fn a_selected_clear_refused_under_a_peer_claim_clears_when_repeated_after_release() {
    let fixture = fixture("refused");
    let peer = LocalWorkService::new(
        fixture.database.clone(),
        fixture.project.clone(),
        "peer".into(),
        SessionId("selected-unblock-refused-peer".into()),
        Some("protocol-test".into()),
    );
    // The peer holds the item and raises both blockers itself.
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
    for (index, detail) in ["First reason", "Second reason"].into_iter().enumerate() {
        peer.work_update_on(
            Some(&fixture.target()),
            WorkUpdateInput::Block {
                blocker_kind: WorkBlockerKind::Manual,
                detail: detail.into(),
                idempotency_key: String::new(),
            },
            at(2 + i64::try_from(index).expect("small")),
        )
        .expect("peer blocks");
    }
    let [first, second] = fixture.active().try_into().expect("two blockers");
    let refused = fixture
        .selected(&first, 4)
        .expect_err("a peer's live claim refuses project planning");
    assert!(
        !matches!(refused, StoreError::WorkOperationIdempotencyConflict { .. }),
        "{refused:?}"
    );
    assert_eq!(fixture.active(), [first.clone(), second.clone()]);

    peer.work_update_on(
        Some(&fixture.target()),
        WorkUpdateInput::Release {
            reason: "handing back".into(),
            waiver_reason: Some("handing back".into()),
            idempotency_key: String::new(),
        },
        at(5),
    )
    .expect("peer releases");
    // The same command, repeated once the item changed, is admitted afresh.
    fixture.selected(&first, 6).expect("clears after release");
    assert_eq!(fixture.active(), std::slice::from_ref(&second));
    assert_eq!(fixture.clear_events(), 1);
}
