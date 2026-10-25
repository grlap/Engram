//! Stored-state refusals preserve navigation; committed success stays visible.

use super::*;
use std::{path::Path, sync::mpsc, time::Duration};

pub(super) fn assert_focus(database: &Path, expected: &str) {
    let store = SqliteStore::open(database).unwrap();
    let project = ProjectId("focus-change".into());
    let work = store.resolve_work_ref(&project, expected).unwrap();
    let state = store
        .work_session_state(&project, &SessionId("agent".into()), at(20))
        .unwrap();
    assert_eq!(state.focused_work_id, Some(work.work_id));
}

struct Fixture {
    peer: Session,
    // Close the peer's store before the agent drops their shared test home.
    agent: Session,
    database: std::path::PathBuf,
    target: String,
    focus: String,
}

impl Fixture {
    fn new(offered: bool) -> Self {
        let (agent, peer, database) = sessions();
        let target = add(&agent.verbs, "Target", None, 1);
        let focus = add(&agent.verbs, "Focus", None, 2);
        claim(&agent.verbs, &target, 3).unwrap();
        claim(&agent.verbs, &focus, 4).unwrap();
        if offered {
            update_handoff::handoff(
                &agent.verbs,
                &target,
                HandoffAction::Offer {
                    to: "peer".into(),
                    summary: Some("handoff checkpoint".into()),
                    ttl_seconds: Some(3_600),
                },
                5,
            )
            .unwrap();
        }
        agent.verbs.service.select_work(&focus, at(6)).unwrap();
        agent.verbs.next(&NextInput::default(), at(7)).unwrap();
        let fixture = Self {
            peer,
            agent,
            database,
            target,
            focus,
        };
        assert!(fixture.navigation().0.tentative_delivery_token.is_some());
        fixture
    }

    fn navigation(&self) -> (crate::domain::WorkSessionState, Option<Vec<u8>>) {
        let store = SqliteStore::open(&self.database).unwrap();
        let state = store
            .work_session_state(
                &ProjectId("focus-change".into()),
                &SessionId("agent".into()),
                at(20),
            )
            .unwrap();
        let connection = rusqlite::Connection::open(&self.database).unwrap();
        let payload = connection
            .query_row(
                "SELECT tentative_delivery_payload FROM work_session_state
             WHERE project_id = 'focus-change' AND session_id = 'agent'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        (state, payload)
    }

    fn assert_no_move(&self, error: &VerbError) {
        assert_eq!(error.focus_change_line(), None, "{error:?}");
        let value = self
            .agent
            .verbs
            .project_error(error, crate::store_error_value(&error.error));
        assert!(value.get("focus_change").is_none(), "{value}");
        assert_focus(&self.database, &self.focus);
    }
}

#[test]
fn root_creation_selects_focus_before_projecting_its_work_memory() {
    let (agent, _peer, database) = sessions();
    let previous = add(&agent.verbs, "Previous root", None, 1);
    let created = agent
        .verbs
        .add(
            AddInput {
                title: "New root with guarded memory view".into(),
                acceptance: vec!["its memory view is readable".into()],
                ..AddInput::default()
            },
            at(2),
        )
        .expect("creation must select the root before its guarded memory projection");
    let target = created.value["work"]["short_ref"].as_str().unwrap();
    assert_ne!(target, previous);
    assert_eq!(
        moved(&created),
        Some((json!(previous), json!(target), false))
    );
    assert_focus(&database, target);
}

#[test]
fn refused_holder_note_and_status_preserve_focus_delivery_and_run() {
    for status in [false, true] {
        let fixture = Fixture::new(true);
        let before = fixture.navigation();
        let store = SqliteStore::open(&fixture.database).unwrap();
        let work = store
            .resolve_work_ref(&ProjectId("focus-change".into()), &fixture.target)
            .unwrap();
        let run = store.latest_work_run(work.work_id).unwrap().unwrap();
        let evidence = store.work_run_evidence(run.run_id).unwrap();
        let error = fixture
            .agent
            .verbs
            .note(
                &NoteInput {
                    work_ref: Some(fixture.target.clone()),
                    status,
                    text: "refused while offered".into(),
                    refs: Vec::new(),
                },
                at(8),
            )
            .unwrap_err();
        assert!(matches!(error.error, StoreError::WorkHandoffPending { .. }));
        fixture.assert_no_move(&error);
        assert_eq!(fixture.navigation(), before);
        assert_eq!(store.work_run_evidence(run.run_id).unwrap(), evidence);
        assert_eq!(
            store
                .latest_work_run(work.work_id)
                .unwrap()
                .unwrap()
                .last_checkpoint,
            run.last_checkpoint
        );
        let bare = fixture
            .agent
            .verbs
            .note(
                &NoteInput {
                    work_ref: None,
                    status: false,
                    text: "still on focus".into(),
                    refs: Vec::new(),
                },
                at(9),
            )
            .unwrap();
        assert_eq!(bare.value["work"]["short_ref"], fixture.focus);
    }
}

#[test]
fn refused_gate_done_and_renew_preserve_staged_navigation() {
    for operation in ["gate", "done", "claim"] {
        let fixture = Fixture::new(true);
        let before = fixture.navigation();
        let error = match operation {
            "gate" => fixture
                .agent
                .verbs
                .gate(
                    GateInput {
                        work_ref: Some(fixture.target.clone()),
                        name: "checked".into(),
                        failed: Vec::new(),
                        evidence_ref: None,
                    },
                    at(8),
                )
                .unwrap_err(),
            "done" => fixture
                .agent
                .verbs
                .done(
                    DoneInput {
                        work_ref: Some(fixture.target.clone()),
                        summary: Some("delivered".into()),
                        ..DoneInput::default()
                    },
                    at(8),
                )
                .unwrap_err(),
            _ => claim(&fixture.agent.verbs, &fixture.target, 8).unwrap_err(),
        };
        assert!(
            matches!(error.error, StoreError::WorkHandoffPending { .. }),
            "{operation}: {error:?}"
        );
        fixture.assert_no_move(&error);
        assert_eq!(fixture.navigation(), before, "{operation}");
    }
}

#[test]
fn typed_completion_refusal_preserves_focus_and_delivery() {
    let fixture = Fixture::new(false);
    super::super::evaluate::enable(
        &fixture.database,
        &[crate::domain::AcceptanceEvaluationMode::SameSession],
        8,
    );
    let before = fixture.navigation();
    let result = fixture
        .agent
        .verbs
        .service
        .work_complete_on(
            Some(&fixture.target),
            serde_json::from_value(json!({})).unwrap(),
            at(9),
        )
        .unwrap();
    assert!(matches!(
        result,
        crate::work_service::WorkCompleteResult::Refused(_)
    ));
    assert_eq!(fixture.navigation(), before);
    let receipt = fixture
        .agent
        .verbs
        .done(
            DoneInput {
                work_ref: Some(fixture.target.clone()),
                ..DoneInput::default()
            },
            at(10),
        )
        .unwrap();
    assert!(receipt.value.get("focus_change").is_none());
    assert!(!receipt.text().contains("focus moved"));
    assert_eq!(fixture.navigation(), before);
}

#[test]
fn every_refused_update_family_leaves_navigation_unchanged() {
    for input in [
        json!({"kind":"claim"}),
        json!({"kind":"claim_next_ready"}),
        json!({"kind":"release","reason":"pause"}),
        json!({"kind":"checkpoint","summary":"checkpoint"}),
        json!({"kind":"evidence","summary":"evidence"}),
        json!({"kind":"block","blocker_kind":"manual","detail":"blocked"}),
        json!({"kind":"unblock"}),
        json!({"kind":"revise","patch":{"title":"new title"}}),
        json!({"kind":"add_prerequisite","prerequisite":"FOCUS"}),
        json!({"kind":"remove_prerequisite","prerequisite":"FOCUS"}),
        json!({"kind":"reopen","reason":"again"}),
        json!({"kind":"cancel","reason":"cancel"}),
        json!({"kind":"reject","reason":"reject"}),
        json!({"kind":"supersede","replacement":"FOCUS","reason":"replace"}),
        json!({"kind":"detach","reason":"detach"}),
        json!({"kind":"waive_required_child","child":"FOCUS","reason":"waive"}),
    ] {
        let fixture = Fixture::new(false);
        // The target belongs to another session; shape and references are valid.
        update_handoff::handoff(
            &fixture.agent.verbs,
            &fixture.target,
            HandoffAction::Offer {
                to: "peer".into(),
                summary: Some("transfer".into()),
                ttl_seconds: None,
            },
            8,
        )
        .unwrap();
        update_handoff::handoff(
            &fixture.peer.verbs,
            &fixture.target,
            HandoffAction::Accept,
            9,
        )
        .unwrap();
        fixture
            .agent
            .verbs
            .service
            .select_work(&fixture.focus, at(10))
            .unwrap();
        fixture
            .agent
            .verbs
            .next(&NextInput::default(), at(11))
            .unwrap();
        let before = fixture.navigation();
        let input = serde_json::to_string(&input)
            .unwrap()
            .replace("FOCUS", &fixture.focus);
        let parsed = serde_json::from_str(&input).unwrap();
        let error = fixture
            .agent
            .verbs
            .disclosing_focus(|| {
                fixture
                    .agent
                    .verbs
                    .service
                    .work_update_on(Some(&fixture.target), parsed, at(12))
                    .map(|result| {
                        Receipt::assemble(Vec::new(), Guidance::default(), json!(result), false)
                    })
                    .map_err(VerbError::from)
            })
            .expect_err(&input);
        fixture.assert_no_move(&error);
        assert_eq!(fixture.navigation(), before, "{input}");
    }
}

#[test]
fn refused_handoff_actions_and_claim_under_preserve_navigation() {
    for action in [
        HandoffAction::Accept,
        HandoffAction::Cancel {
            reason: "cancel".into(),
        },
    ] {
        let fixture = Fixture::new(false);
        let before = fixture.navigation();
        let error =
            update_handoff::handoff(&fixture.agent.verbs, &fixture.target, action, 8).unwrap_err();
        fixture.assert_no_move(&error);
        assert_eq!(fixture.navigation(), before);
    }
    let fixture = Fixture::new(true);
    let before = fixture.navigation();
    let error = update_handoff::handoff(
        &fixture.agent.verbs,
        &fixture.target,
        HandoffAction::Offer {
            to: "peer".into(),
            summary: Some("another offer".into()),
            ttl_seconds: None,
        },
        8,
    )
    .unwrap_err();
    fixture.assert_no_move(&error);
    assert_eq!(fixture.navigation(), before);
    let fixture = Fixture::new(false);
    let before = fixture.navigation();
    let error = fixture
        .agent
        .verbs
        .claim_under(
            crate::verbs::ClaimUnderInput {
                under: fixture.target.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(8),
        )
        .unwrap_err();
    fixture.assert_no_move(&error);
    assert_eq!(fixture.navigation(), before);
}

#[test]
fn a_refused_decomposition_does_not_select_its_parent() {
    let fixture = Fixture::new(false);
    update_handoff::handoff(
        &fixture.agent.verbs,
        &fixture.target,
        HandoffAction::Offer {
            to: "peer".into(),
            summary: Some("transfer".into()),
            ttl_seconds: None,
        },
        8,
    )
    .unwrap();
    update_handoff::handoff(
        &fixture.peer.verbs,
        &fixture.target,
        HandoffAction::Accept,
        9,
    )
    .unwrap();
    fixture
        .agent
        .verbs
        .service
        .select_work(&fixture.focus, at(10))
        .unwrap();
    fixture
        .agent
        .verbs
        .next(&NextInput::default(), at(11))
        .unwrap();
    let before = fixture.navigation();
    let error = fixture
        .agent
        .verbs
        .add(
            AddInput {
                title: "Refused child".into(),
                acceptance: vec!["child works".into()],
                under: Some(fixture.target.clone()),
                ..AddInput::default()
            },
            at(12),
        )
        .unwrap_err();
    assert!(matches!(
        error.error,
        StoreError::WorkPeerDecompositionRefused { .. }
    ));
    fixture.assert_no_move(&error);
    assert_eq!(fixture.navigation(), before);
}

#[test]
fn an_evaluation_refused_after_input_checks_preserves_navigation() {
    let fixture = Fixture::new(false);
    super::super::evaluate::enable(
        &fixture.database,
        &[crate::domain::AcceptanceEvaluationMode::SameSession],
        8,
    );
    let before = fixture.navigation();
    let error = fixture
        .agent
        .verbs
        .evaluate(
            EvaluateInput {
                supersedes: None,
                work_ref: Some(fixture.target.clone()),
                mode: "same_session".into(),
                acceptance_basis: 2,
                evidence_basis: 0,
                verdicts: vec![crate::WorkCriterionVerdictInput {
                    criterion: 1,
                    verdict: "fail".into(),
                    basis: "judgment".into(),
                    rationale: "negative assessment on a stale revision".into(),
                    evidence: Vec::new(),
                }],
                attempt: Some("refused-evaluation".into()),
                source_fingerprint: None,
                model: None,
                execution_identity: None,
                parent_session: None,
            },
            at(9),
        )
        .unwrap_err();
    assert!(
        matches!(
            error.error,
            StoreError::WorkRevisionConflict {
                expected: 2,
                current: 1,
                ..
            }
        ),
        "{error:?}"
    );
    fixture.assert_no_move(&error);
    assert_eq!(fixture.navigation(), before);
}

#[test]
fn successful_handoff_replay_still_selects_its_explicit_target() {
    let fixture = Fixture::new(false);
    let input = || crate::work_service::WorkHandoffInput::Offer {
        to: "peer".into(),
        ttl_seconds: None,
        checkpoint_summary: "transfer".into(),
        idempotency_key: "retained-handoff".into(),
    };
    let first = fixture
        .agent
        .verbs
        .service
        .work_handoff_on(Some(&fixture.target), input(), at(8))
        .unwrap();
    fixture
        .agent
        .verbs
        .service
        .select_work(&fixture.focus, at(9))
        .unwrap();
    let replay = fixture
        .agent
        .verbs
        .service
        .work_handoff_on(Some(&fixture.target), input(), at(10))
        .unwrap();
    assert_eq!(
        serde_json::to_value(first).unwrap(),
        serde_json::to_value(replay).unwrap()
    );
    assert_focus(&fixture.database, &fixture.target);
}

#[test]
fn a_refusal_cannot_restore_focus_over_another_connection() {
    let fixture = Fixture::new(true);
    let third = add(&fixture.agent.verbs, "Concurrent focus", None, 8);
    fixture
        .agent
        .verbs
        .service
        .select_work(&fixture.focus, at(9))
        .unwrap();
    let database = fixture.database.clone();
    let target = fixture.target.clone();
    let (arrived, arrival) = mpsc::channel();
    let (resume, resumed) = mpsc::channel();
    let worker = std::thread::spawn(move || {
        let verbs = AgentVerbs::new(
            database,
            ProjectId("focus-change".into()),
            "agent".into(),
            SessionId("agent".into()),
            None,
        );
        crate::work_service::before_note_capture(move || {
            arrived.send(()).unwrap();
            resumed.recv_timeout(Duration::from_secs(30)).unwrap();
        });
        verbs
            .note(
                &NoteInput {
                    work_ref: Some(target),
                    status: false,
                    text: "refused note".into(),
                    refs: Vec::new(),
                },
                at(10),
            )
            .unwrap_err()
    });
    arrival.recv_timeout(Duration::from_secs(30)).unwrap();
    fixture
        .agent
        .verbs
        .service
        .select_work(&third, at(11))
        .unwrap();
    let after_concurrent_selection = fixture.navigation();
    resume.send(()).unwrap();
    let error = worker.join().unwrap();
    assert!(matches!(error.error, StoreError::WorkHandoffPending { .. }));
    assert_eq!(error.focus_change_line(), None);
    assert_eq!(fixture.navigation(), after_concurrent_selection);
    assert_focus(&fixture.database, &third);
}

#[test]
fn post_success_projection_error_discloses_the_committed_move() {
    let fixture = Fixture::new(false);
    let error = fixture
        .agent
        .verbs
        .disclosing_focus(|| {
            fixture.agent.verbs.service.work_note_on(
                Some(&fixture.target),
                "committed note",
                &[],
                at(8),
            )?;
            Err(VerbError::from(StoreError::InvalidWorkProjection(
                "post-success projection".into(),
            )))
        })
        .unwrap_err();
    let value = fixture
        .agent
        .verbs
        .project_error(&error, crate::store_error_value(&error.error));
    assert_eq!(value["focus_change"]["from"], fixture.focus);
    assert_eq!(value["focus_change"]["to"], fixture.target);
    assert!(error.focus_change_line().is_some());
    assert_focus(&fixture.database, &fixture.target);
}
