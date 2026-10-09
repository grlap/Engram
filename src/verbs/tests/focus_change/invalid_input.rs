//! Malformed explicit requests cannot redirect later ambient evidence.

use super::*;

fn refuses_without_navigation(call: impl FnOnce(&AgentVerbs, &str) -> Result<(), VerbError>) {
    refuses_without_navigation_given_record(|verbs, first, _| call(verbs, first));
}

/// As [`refuses_without_navigation`], also handing the call the id of a
/// record the store holds.
fn refuses_without_navigation_given_record(
    call: impl FnOnce(&AgentVerbs, &str, &str) -> Result<(), VerbError>,
) {
    refuses_without_navigation_given_focus(|verbs, first, _, record| call(verbs, first, record));
}

/// The shared fixture: the session holds `first` and `second` with focus on
/// `second` and a pending delivery, and the call gets both refs and the id of
/// a stored record. A refusal must leave focus, delivery and every row as
/// they were, and later ambient words must still reach `second`.
fn refuses_without_navigation_given_focus(
    call: impl FnOnce(&AgentVerbs, &str, &str, &str) -> Result<(), VerbError>,
) {
    let (agent, _peer, database) = sessions();
    // Two criteria, so a completion can supply one result per criterion and
    // still be malformed in how those results relate to each other.
    let first = agent
        .verbs
        .add(
            AddInput {
                title: "First".into(),
                acceptance: vec!["First works".into(), "First is reviewed".into()],
                ..AddInput::default()
            },
            at(1),
        )
        .expect("add first")
        .value["work"]["short_ref"]
        .as_str()
        .expect("ref")
        .to_owned();
    let second = add(&agent.verbs, "Second", None, 2);
    claim(&agent.verbs, &first, 3).expect("claim first");
    claim(&agent.verbs, &second, 4).expect("claim second");
    agent
        .verbs
        .next(&NextInput::default(), at(5))
        .expect("stage delivery");
    let store = SqliteStore::open(&database).expect("store");
    let project = ProjectId("focus-change".into());
    let session = SessionId("agent".into());
    let before = store
        .work_session_state(&project, &session, at(6))
        .expect("session");
    assert!(
        before.tentative_delivery_token.is_some(),
        "exercise pending delivery"
    );
    let connection = rusqlite::Connection::open(&database).expect("connection");
    let record: String = connection
        .query_row("SELECT object_id FROM objects LIMIT 1", [], |row| {
            row.get(0)
        })
        .expect("a stored record");
    let rows = crate::storage::test_database_shape_snapshot(&connection).expect("snapshot");

    let error = call(&agent.verbs, &first, &second, &record).expect_err("malformed input refuses");
    assert_eq!(error.focus_change_line(), None);
    assert_eq!(
        store.work_session_state(&project, &session, at(6)).unwrap(),
        before
    );
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&connection).unwrap(),
        rows
    );

    let receipt = agent
        .verbs
        .note(
            &NoteInput {
                work_ref: None,
                status: false,
                text: "still working on second".into(),
                refs: Vec::new(),
            },
            at(7),
        )
        .expect("ambient note");
    assert_eq!(receipt.value["work"]["short_ref"], second);
    let receipt = agent
        .verbs
        .gate(
            GateInput {
                work_ref: None,
                name: "still-second".into(),
                failed: Vec::new(),
                evidence_ref: None,
            },
            at(8),
        )
        .expect("ambient gate");
    assert_eq!(receipt.value["work"]["short_ref"], second);
}

#[test]
fn malformed_done_landing_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        verbs
            .done(
                DoneInput {
                    work_ref: Some(first.into()),
                    landing: Some(crate::domain::CompletionLanding {
                        commit: "1".repeat(40),
                        remote: "https://example.test/repo".into(),
                        branch: "master".into(),
                        pushed_at: at(6),
                        installed_build: None,
                    }),
                    ..DoneInput::default()
                },
                at(6),
            )
            .map(|_| ())
    });
}

#[test]
fn malformed_update_reason_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        verbs
            .update(
                UpdateInput {
                    work_ref: Some(first.into()),
                    action: UpdateAction::Blocked { detail: " ".into() },
                },
                at(6),
            )
            .map(|_| ())
    });
}

#[test]
fn malformed_evaluate_mode_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        verbs
            .evaluate(
                EvaluateInput {
                    work_ref: Some(first.into()),
                    mode: "unknown".into(),
                    acceptance_basis: 1,
                    evidence_basis: 0,
                    verdicts: Vec::new(),
                    attempt: None,
                    source_fingerprint: None,
                    model: None,
                    execution_identity: None,
                    parent_session: None,
                    supersedes: None,
                },
                at(6),
            )
            .map(|_| ())
    });
}

#[test]
fn malformed_evaluate_verdict_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        verbs
            .evaluate(
                EvaluateInput {
                    work_ref: Some(first.into()),
                    mode: "same_session".into(),
                    acceptance_basis: 1,
                    evidence_basis: 0,
                    verdicts: vec![crate::WorkCriterionVerdictInput {
                        criterion: 0,
                        verdict: "fail".into(),
                        basis: "judgment".into(),
                        rationale: "observed failure".into(),
                        evidence: Vec::new(),
                    }],
                    attempt: None,
                    source_fingerprint: None,
                    model: None,
                    execution_identity: None,
                    parent_session: None,
                    supersedes: None,
                },
                at(6),
            )
            .map(|_| ())
    });
}

#[test]
fn oversized_note_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        verbs
            .note(
                &NoteInput {
                    work_ref: Some(first.into()),
                    status: false,
                    text: "x".repeat(65_537),
                    refs: Vec::new(),
                },
                at(6),
            )
            .map(|_| ())
    });
}

#[test]
fn malformed_gate_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        verbs
            .gate(
                GateInput {
                    work_ref: Some(first.into()),
                    name: " ".into(),
                    failed: Vec::new(),
                    evidence_ref: None,
                },
                at(6),
            )
            .map(|_| ())
    });
}

#[test]
fn malformed_core_gate_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        verbs
            .service
            .work_gate_on(Some(first), " ", &[], None, at(6))
            .map(|_| ())
            .map_err(VerbError::from)
    });
}

#[test]
fn malformed_claim_ttl_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        verbs
            .claim(
                ClaimInput {
                    work_ref: first.into(),
                    ttl_seconds: Some(0),
                    recover: None,
                },
                at(6),
            )
            .map(|_| ())
    });
}

#[test]
fn malformed_child_claim_ttl_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        verbs
            .claim_under(
                ClaimUnderInput {
                    under: first.into(),
                    ttl_seconds: Some(0),
                    recover: None,
                },
                at(6),
            )
            .map(|_| ())
    });
}

#[test]
fn malformed_handoff_reason_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        verbs
            .handoff(
                HandoffInput {
                    work_ref: Some(first.into()),
                    action: HandoffAction::Cancel { reason: " ".into() },
                },
                at(6),
            )
            .map(|_| ())
    });
}

#[test]
fn malformed_handoff_ttl_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        verbs
            .handoff(
                HandoffInput {
                    work_ref: Some(first.into()),
                    action: HandoffAction::Offer {
                        to: "peer".into(),
                        summary: None,
                        ttl_seconds: Some(0),
                    },
                },
                at(6),
            )
            .map(|_| ())
    });
}

fn linked_done(first: &str, criterion: usize, locator: &str) -> DoneInput {
    DoneInput {
        work_ref: Some(first.into()),
        links: vec![crate::work_service::WorkCriterionLinkInput {
            criterion,
            locator: locator.into(),
        }],
        link_basis: Some(1),
        ..DoneInput::default()
    }
}

#[test]
fn malformed_done_link_locator_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        verbs
            .done(
                linked_done(first, 1, "https://example.test/evidence"),
                at(6),
            )
            .map(|_| ())
    });
}

#[test]
fn done_link_at_criterion_zero_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        verbs
            .done(linked_done(first, 0, "abcdef12"), at(6))
            .map(|_| ())
    });
}

#[test]
fn core_completion_with_acceptance_and_note_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        let input = serde_json::from_value(serde_json::json!({
            "acceptance": [{"criterion": null, "satisfied": true, "note": "met"}],
            "note": "shared",
        }))
        .expect("completion input");
        verbs
            .service
            .work_complete_on(Some(first), input, at(6))
            .map(|_| ())
            .map_err(VerbError::from)
    });
}

fn revision(action: serde_json::Value) -> UpdateAction {
    serde_json::from_value(action).expect("revision")
}

#[test]
fn bindings_bound_twice_leave_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        verbs
            .update(
                UpdateInput {
                    work_ref: Some(first.into()),
                    action: revision(serde_json::json!({
                        "action": "revise", "bindings": ["1=test", "1=lint"],
                    })),
                },
                at(6),
            )
            .map(|_| ())
    });
}

#[test]
fn blank_acceptance_replacement_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        verbs
            .update(
                UpdateInput {
                    work_ref: Some(first.into()),
                    action: revision(serde_json::json!({
                        "action": "revise", "acceptance": [" "],
                    })),
                },
                at(6),
            )
            .map(|_| ())
    });
}

fn keyed_evaluation(first: &str, attempt: String) -> EvaluateInput {
    EvaluateInput {
        work_ref: Some(first.into()),
        mode: "same_session".into(),
        acceptance_basis: 1,
        evidence_basis: 0,
        verdicts: vec![crate::WorkCriterionVerdictInput {
            criterion: 1,
            verdict: "fail".into(),
            basis: "judgment".into(),
            rationale: "not yet delivered".into(),
            evidence: Vec::new(),
        }],
        attempt: Some(attempt),
        source_fingerprint: None,
        model: None,
        execution_identity: None,
        parent_session: None,
        supersedes: None,
    }
}

#[test]
fn blank_evaluation_attempt_key_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        verbs
            .evaluate(keyed_evaluation(first, " ".into()), at(6))
            .map(|_| ())
    });
}

#[test]
fn oversized_evaluation_attempt_key_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        verbs
            .evaluate(keyed_evaluation(first, "k".repeat(257)), at(6))
            .map(|_| ())
    });
}

#[test]
fn child_with_blank_initial_note_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        verbs
            .add(
                AddInput {
                    title: "Child".into(),
                    under: Some(first.into()),
                    notes: vec![" ".into()],
                    ..AddInput::default()
                },
                at(6),
            )
            .map(|_| ())
    });
}

#[test]
fn core_completion_with_blank_criterion_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        let input = serde_json::from_value(serde_json::json!({
            "acceptance": [{"criterion": " ", "satisfied": true, "note": "met"}],
        }))
        .expect("completion input");
        verbs
            .service
            .work_complete_on(Some(first), input, at(6))
            .map(|_| ())
            .map_err(VerbError::from)
    });
}

fn core_update(verbs: &AgentVerbs, first: &str, input: serde_json::Value) -> Result<(), VerbError> {
    verbs
        .service
        .work_update_on(
            Some(first),
            serde_json::from_value(input).expect("update input"),
            at(6),
        )
        .map(|_| ())
        .map_err(VerbError::from)
}

/// A blank optional recovery reason is read only when a recovery applies,
/// which depends on the prior holder, so it is no malformed request: a
/// renewal that needs no recovery still admits it.
#[test]
fn core_claim_renewal_with_a_blank_recovery_reason_is_admitted() {
    let (agent, _peer, _database) = sessions();
    let item = add(&agent.verbs, "Item", None, 1);
    claim(&agent.verbs, &item, 2).expect("claim");
    core_update(
        &agent.verbs,
        &item,
        serde_json::json!({"kind": "claim", "recovery_reason": " "}),
    )
    .expect("a renewal ignores a blank recovery reason");
}

fn core_propose(
    verbs: &AgentVerbs,
    first: &str,
    input: serde_json::Value,
) -> Result<(), VerbError> {
    verbs
        .service
        .work_propose_on(
            Some(first),
            serde_json::from_value(input).expect("proposal input"),
            at(6),
        )
        .map(|_| ())
        .map_err(VerbError::from)
}

#[test]
fn core_decomposition_without_children_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        core_propose(
            verbs,
            first,
            serde_json::json!({"kind": "decompose", "children": []}),
        )
    });
}

#[test]
fn core_decomposition_with_blank_child_title_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        core_propose(
            verbs,
            first,
            serde_json::json!({"kind": "decompose", "children": [{
                "key": "child", "title": " ", "outcome": "done", "acceptance": ["done"],
            }]}),
        )
    });
}

#[test]
fn core_root_with_blank_title_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        core_propose(
            verbs,
            first,
            serde_json::json!({
                "kind": "root", "title": " ", "outcome": "done", "acceptance": ["done"],
            }),
        )
    });
}

#[test]
fn binding_pinned_to_a_record_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation_given_record(|verbs, first, record| {
        verbs
            .update(
                UpdateInput {
                    work_ref: Some(first.into()),
                    action: revision(serde_json::json!({
                        "action": "revise", "bindings": [format!("1=test:{record}")],
                    })),
                },
                at(6),
            )
            .map(|_| ())
    });
}

#[test]
fn child_binding_pinned_to_a_record_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation_given_record(|verbs, first, record| {
        verbs
            .add(
                AddInput {
                    title: "Child".into(),
                    under: Some(first.into()),
                    acceptance: vec!["Child is done".into()],
                    bindings: vec![format!("1=test:{record}")],
                    ..AddInput::default()
                },
                at(6),
            )
            .map(|_| ())
    });
}

#[test]
fn core_binding_at_criterion_zero_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        core_update(
            verbs,
            first,
            serde_json::json!({"kind": "revise", "patch": {
                "acceptance_bindings": [{"criterion": 0, "requirement": {"check_kind": "test"}}],
            }}),
        )
    });
}

/// One child keyed `a` and one prerequisite edge `work_key` ← `prerequisite`.
fn decomposition_with(work_key: &str, prerequisite: &str) -> serde_json::Value {
    serde_json::json!({"kind": "decompose", "children": [{
        "key": "a", "title": "Child", "outcome": "done", "acceptance": ["done"],
    }], "prerequisites": [{"work_key": work_key, "prerequisite": prerequisite}]})
}

#[test]
fn core_edge_from_an_unknown_child_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        core_propose(verbs, first, decomposition_with("missing", "a"))
    });
}

#[test]
fn core_edge_onto_its_own_child_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        core_propose(verbs, first, decomposition_with("a", "a"))
    });
}

#[test]
fn core_edge_onto_an_unknown_item_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        core_propose(verbs, first, decomposition_with("a", "w-000000000000"))
    });
}

#[test]
fn child_with_oversized_initial_note_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        verbs
            .add(
                AddInput {
                    title: "Child".into(),
                    under: Some(first.into()),
                    notes: vec!["x".repeat(65_537)],
                    ..AddInput::default()
                },
                at(6),
            )
            .map(|_| ())
    });
}

#[test]
fn core_root_with_oversized_initial_note_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        core_propose(
            verbs,
            first,
            serde_json::json!({
                "kind": "root", "title": "Root", "outcome": "done", "acceptance": ["done"],
                "notes": ["x".repeat(65_537)],
            }),
        )
    });
}

#[test]
fn core_decomposition_with_oversized_initial_note_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        core_propose(
            verbs,
            first,
            serde_json::json!({"kind": "decompose", "children": [{
                "key": "a", "title": "Child", "outcome": "done", "acceptance": ["done"],
                "notes": ["x".repeat(65_537)],
            }]}),
        )
    });
}

#[test]
fn core_cycle_between_sibling_children_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        let child = |key: &str| {
            serde_json::json!({
                "key": key, "title": "Child", "outcome": "done", "acceptance": ["done"],
            })
        };
        core_propose(
            verbs,
            first,
            serde_json::json!({"kind": "decompose",
            "children": [child("a"), child("b")],
            "prerequisites": [
                {"work_key": "a", "prerequisite": "b"},
                {"work_key": "b", "prerequisite": "a"},
            ]}),
        )
    });
}

/// A one-task plan: it binds no focus, so it is proposed with no target, and
/// a refusal must also leave no attempt behind.
fn core_plan(
    verbs: &AgentVerbs,
    task: &serde_json::Value,
    prerequisites: &serde_json::Value,
) -> Result<(), VerbError> {
    let mut task_value = serde_json::json!({
        "key": "t", "parent_key": null, "title": "Task", "outcome": "done", "acceptance": ["done"],
    });
    if let (Some(base), Some(extra)) = (task_value.as_object_mut(), task.as_object()) {
        base.extend(extra.clone());
    }
    verbs
        .service
        .work_propose_on(
            None,
            serde_json::from_value(serde_json::json!({"kind": "plan", "plan": {
                "tasks": [task_value],
                "prerequisites": prerequisites,
                "idempotency_key": "plan-1",
            }}))
            .expect("plan input"),
            at(6),
        )
        .map(|_| ())
        .map_err(VerbError::from)
}

#[test]
fn plan_with_oversized_initial_note_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, _| {
        core_plan(
            verbs,
            &serde_json::json!({"notes": ["x".repeat(65_537)]}),
            &serde_json::json!([]),
        )
    });
}

#[test]
fn plan_edge_onto_an_unknown_item_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, _| {
        core_plan(
            verbs,
            &serde_json::json!({}),
            &serde_json::json!([{"work_key": "t",
                "prerequisite": {"kind": "existing", "value": "w-000000000000"}}]),
        )
    });
}

#[test]
fn plan_binding_pinned_to_a_record_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation_given_record(|verbs, _, record| {
        core_plan(
            verbs,
            &serde_json::json!({"bindings": [
                {"criterion": 1, "check_kind": "test", "check_fingerprint": record},
            ]}),
            &serde_json::json!([]),
        )
    });
}

#[test]
fn core_completion_with_duplicate_criteria_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        let input = serde_json::from_value(serde_json::json!({
            "acceptance": [
                {"criterion": "First works", "satisfied": true, "note": "met"},
                {"criterion": " First works ", "satisfied": true, "note": "met"},
            ],
        }))
        .expect("completion input");
        verbs
            .service
            .work_complete_on(Some(first), input, at(6))
            .map(|_| ())
            .map_err(VerbError::from)
    });
}

#[test]
fn core_completion_with_duplicate_criteria_and_another_count_leaves_focus_unchanged() {
    refuses_without_navigation(|verbs, first| {
        let input = serde_json::from_value(serde_json::json!({
            "acceptance": [
                {"criterion": "First works", "satisfied": true, "note": "met"},
                {"criterion": "First works", "satisfied": true, "note": "met"},
                {"criterion": "First is reviewed", "satisfied": true, "note": "met"},
            ],
        }))
        .expect("completion input");
        let error = verbs
            .service
            .work_complete_on(Some(first), input, at(6))
            .expect_err("duplicate criteria refuse whatever the result count");
        assert!(
            matches!(
                &error,
                StoreError::WorkCompletionRefused { reason, .. }
                    if reason == "acceptance results contain a duplicate criterion"
            ),
            "{error:?}"
        );
        Err(VerbError::from(error))
    });
}

#[test]
fn done_link_basis_zero_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        let mut input = linked_done(first, 1, "abcdef12");
        input.link_basis = Some(0);
        verbs.done(input, at(6)).map(|_| ())
    });
}

fn bounded_evaluation(
    first: &str,
    rationale: String,
    acceptance_basis: i64,
    evidence_basis: i64,
) -> EvaluateInput {
    EvaluateInput {
        work_ref: Some(first.into()),
        mode: "same_session".into(),
        acceptance_basis,
        evidence_basis,
        verdicts: vec![crate::WorkCriterionVerdictInput {
            criterion: 1,
            verdict: "fail".into(),
            basis: "judgment".into(),
            rationale,
            evidence: Vec::new(),
        }],
        attempt: None,
        source_fingerprint: None,
        model: None,
        execution_identity: None,
        parent_session: None,
        supersedes: None,
    }
}

#[test]
fn oversized_evaluation_rationale_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        verbs
            .evaluate(bounded_evaluation(first, "x".repeat(65_537), 1, 0), at(6))
            .map(|_| ())
    });
}

/// Refuses `count` verdicts that each carry `rationale`, which fits the note
/// bound alone but not the record's byte cap together with the others.
fn refuses_rationales_over_the_record_cap(count: usize, rationale: &str) {
    refuses_without_navigation(|verbs, first| {
        let mut input = bounded_evaluation(first, rationale.to_owned(), 1, 0);
        let verdict = input.verdicts.remove(0);
        input.verdicts = (1..=count)
            .map(|criterion| crate::WorkCriterionVerdictInput {
                criterion,
                ..verdict.clone()
            })
            .collect();
        let error = verbs.evaluate(input, at(6)).expect_err("over the cap");
        let cap = format!(
            "would pass the {} byte cap",
            crate::domain::MAX_ACCEPTANCE_EVALUATION_BYTES
        );
        assert!(error.to_string().contains(&cap), "{error}");
        Err(error)
    });
}

/// Rationales whose bytes alone sum exactly to the cap still overflow it
/// once each is a canonical string in the record.
#[test]
fn evaluation_rationales_summing_to_the_record_cap_leave_focus_and_delivery_unchanged() {
    let rationale = "x".repeat(65_536);
    let count = crate::domain::MAX_ACCEPTANCE_EVALUATION_BYTES / rationale.len();
    refuses_rationales_over_the_record_cap(count, &rationale);
}

/// Escaping counts: quotation marks double in canonical form, so rationales
/// whose raw bytes fit the cap can still overflow it.
#[test]
fn escaped_evaluation_rationales_over_the_record_cap_leave_focus_and_delivery_unchanged() {
    let rationale = "\"".repeat(65_536);
    let count = crate::domain::MAX_ACCEPTANCE_EVALUATION_BYTES / (2 * rationale.len()) + 1;
    assert!(count * rationale.len() < crate::domain::MAX_ACCEPTANCE_EVALUATION_BYTES);
    refuses_rationales_over_the_record_cap(count, &rationale);
}

/// A malformed evaluation naming an unknown item is refused for its own
/// words, ahead of any refusal about the target.
#[test]
fn malformed_evaluation_refuses_before_its_target_is_resolved() {
    refuses_without_navigation(|verbs, _first| {
        let mut input = bounded_evaluation("w-000000000000", "not yet".into(), 1, 0);
        input.mode = "guesswork".into();
        let error = verbs.evaluate(input, at(6)).expect_err("unknown mode");
        assert!(
            error.to_string().contains("unknown evaluation mode"),
            "{error}"
        );
        Err(error)
    });
}

#[test]
fn malformed_core_evaluation_refuses_before_its_target_is_resolved() {
    refuses_without_navigation(|verbs, _first| {
        let mut input = bounded_evaluation("w-000000000000", "not yet".into(), 1, 0);
        input.mode = "guesswork".into();
        let error = verbs
            .service
            .work_evaluate_on(
                &crate::WorkEvaluateInput {
                    work_ref: input.work_ref,
                    mode: input.mode,
                    acceptance_basis: input.acceptance_basis,
                    evidence_basis: input.evidence_basis,
                    verdicts: input.verdicts,
                    attempt: None,
                    source_fingerprint: None,
                    model: None,
                    execution_identity: None,
                    parent_session: None,
                    supersedes: None,
                },
                at(6),
            )
            .expect_err("unknown mode");
        assert!(
            error.to_string().contains("unknown evaluation mode"),
            "{error}"
        );
        Err(VerbError::from(error))
    });
}

/// A bare `done` while two items are held cannot pick its target; a
/// malformed link is still refused for itself, ahead of that.
#[test]
fn malformed_bare_done_refuses_before_its_ambiguous_target() {
    refuses_without_navigation(|verbs, first| {
        let mut input = linked_done(first, 0, "abcdef12");
        input.work_ref = None;
        let error = verbs.done(input, at(6)).expect_err("criterion 0");
        assert!(
            matches!(error.error, StoreError::WorkCriterionLinkInvalid { .. }),
            "{error:?}"
        );
        Err(error)
    });
}

#[test]
fn negative_evidence_basis_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        verbs
            .evaluate(bounded_evaluation(first, "not yet".into(), 1, -1), at(6))
            .map(|_| ())
    });
}

#[test]
fn acceptance_basis_zero_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        verbs
            .evaluate(bounded_evaluation(first, "not yet".into(), 0, 0), at(6))
            .map(|_| ())
    });
}

#[test]
fn core_prerequisite_on_itself_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        core_update(
            verbs,
            first,
            serde_json::json!({"kind": "add_prerequisite", "prerequisite": first}),
        )
    });
}

#[test]
fn core_prerequisite_removed_from_itself_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        core_update(
            verbs,
            first,
            serde_json::json!({"kind": "remove_prerequisite", "prerequisite": first}),
        )
    });
}

#[test]
fn core_supersede_by_itself_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        core_update(
            verbs,
            first,
            serde_json::json!({"kind": "supersede", "replacement": first, "reason": "replaced"}),
        )
    });
}

#[test]
fn core_edge_onto_the_named_parent_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        core_propose(verbs, first, decomposition_with("a", first))
    });
}

#[test]
fn ambient_edge_onto_the_focused_parent_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation_given_focus(|verbs, _, second, _| {
        verbs
            .service
            .work_propose_on(
                None,
                serde_json::from_value(decomposition_with("a", second)).expect("proposal"),
                at(6),
            )
            .map(|_| ())
            .map_err(VerbError::from)
    });
}

#[test]
fn ambient_prerequisite_on_the_focus_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation_given_focus(|verbs, _, second, _| {
        verbs
            .service
            .work_update_on(
                None,
                serde_json::from_value(
                    serde_json::json!({"kind": "add_prerequisite", "prerequisite": second}),
                )
                .expect("update"),
                at(6),
            )
            .map(|_| ())
            .map_err(VerbError::from)
    });
}

/// Refuses `acceptance` as a core completion naming `first`, asserting the
/// refusal is `expected`.
fn refuses_explicit_acceptance(
    evidence: &[String],
    acceptance: &serde_json::Value,
    expected: fn(&StoreError) -> bool,
) {
    refuses_without_navigation(|verbs, first| {
        let input = serde_json::from_value(serde_json::json!({
            "evidence": evidence,
            "acceptance": acceptance,
        }))
        .expect("completion input");
        let error = verbs
            .service
            .work_complete_on(Some(first), input, at(6))
            .expect_err("refused");
        assert!(expected(&error), "{error:?}");
        Err(VerbError::from(error))
    });
}

fn cites_outside_the_requested_basis(error: &StoreError) -> bool {
    matches!(error, StoreError::WorkCompletionRefused { reason, .. }
        if reason.contains("outside the requested completion basis"))
}

/// A citation outside the evidence the request names is refused whatever the
/// item holds, so neither an unmet criterion nor another result count lets
/// it move focus first.
#[test]
fn core_completion_citing_outside_its_evidence_with_an_unmet_criterion_leaves_focus_unchanged() {
    refuses_explicit_acceptance(
        &["1".repeat(64)],
        &serde_json::json!([
            {"criterion": "First works", "satisfied": false, "evidence": ["2".repeat(64)], "note": "no"},
            {"criterion": "First is reviewed", "satisfied": true, "note": "met"},
        ]),
        cites_outside_the_requested_basis,
    );
}

#[test]
fn core_completion_citing_outside_its_evidence_with_another_count_leaves_focus_unchanged() {
    refuses_explicit_acceptance(
        &["1".repeat(64)],
        &serde_json::json!([
            {"criterion": "First works", "satisfied": true, "evidence": ["2".repeat(64)], "note": "met"},
            {"criterion": "First is reviewed", "satisfied": true, "note": "met"},
            {"criterion": "First ships", "satisfied": true, "note": "met"},
        ]),
        cites_outside_the_requested_basis,
    );
}

#[test]
fn core_completion_with_an_unnamed_result_beside_another_leaves_focus_unchanged() {
    refuses_explicit_acceptance(
        &[],
        &serde_json::json!([
            {"satisfied": true, "note": "met"},
            {"criterion": "First works", "satisfied": true, "note": "met"},
        ]),
        |error| {
            matches!(error, StoreError::InvalidWork(reason)
                if reason == "each acceptance result needs its criterion when more than one is given")
        },
    );
}

#[test]
fn plan_naming_one_item_twice_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        // The same existing item by its short ref and by its full id, read
        // through the read-only inspect path.
        let view = serde_json::to_value(verbs.service.work_inspect(first, at(6)).expect("inspect"))
            .expect("view");
        let full = view["status"]["work"]["work_id"]
            .as_str()
            .expect("work id")
            .to_owned();
        core_plan(
            verbs,
            &serde_json::json!({}),
            &serde_json::json!([
                {"work_key": "t", "prerequisite": {"kind": "existing", "value": first}},
                {"work_key": "t", "prerequisite": {"kind": "existing", "value": full}},
            ]),
        )
    });
}

#[test]
fn core_waiver_of_itself_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        core_update(
            verbs,
            first,
            serde_json::json!({"kind": "waive_required_child", "child": first, "reason": "why"}),
        )
    });
}

#[test]
fn ambient_waiver_of_the_focus_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation_given_focus(|verbs, _, second, _| {
        verbs
            .service
            .work_update_on(
                None,
                serde_json::from_value(serde_json::json!({
                    "kind": "waive_required_child", "child": second, "reason": "why",
                }))
                .expect("update"),
                at(6),
            )
            .map(|_| ())
            .map_err(VerbError::from)
    });
}

#[test]
fn acceptance_citing_evidence_outside_the_named_set_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        let input = serde_json::from_value(serde_json::json!({
            "evidence": ["1".repeat(64)],
            "acceptance": [
                {
                    "criterion": "First works", "satisfied": true,
                    "evidence": ["2".repeat(64)], "note": "met",
                },
                {"criterion": "First is reviewed", "satisfied": true, "note": "met"},
            ],
        }))
        .expect("completion input");
        verbs
            .service
            .work_complete_on(Some(first), input, at(6))
            .map(|_| ())
            .map_err(VerbError::from)
    });
}

#[test]
fn duplicate_criteria_refusal_keeps_its_completion_code() {
    refuses_without_navigation(|verbs, first| {
        let input = serde_json::from_value(serde_json::json!({
            "acceptance": [
                {"criterion": "First works", "satisfied": true, "note": "met"},
                {"criterion": "First works", "satisfied": true, "note": "met"},
            ],
        }))
        .expect("completion input");
        let error = verbs
            .service
            .work_complete_on(Some(first), input, at(6))
            .expect_err("duplicate criteria refuse");
        assert!(
            matches!(error, StoreError::WorkCompletionRefused { .. }),
            "{error:?}"
        );
        Err(VerbError::from(error))
    });
}

fn cited_evaluation(first: &str, citation: String) -> EvaluateInput {
    EvaluateInput {
        work_ref: Some(first.into()),
        mode: "same_session".into(),
        acceptance_basis: 1,
        evidence_basis: 0,
        verdicts: vec![crate::WorkCriterionVerdictInput {
            criterion: 1,
            verdict: "pass".into(),
            basis: "observed".into(),
            rationale: "citation checked".into(),
            evidence: vec![citation],
        }],
        attempt: None,
        source_fingerprint: None,
        model: None,
        execution_identity: None,
        parent_session: None,
        supersedes: None,
    }
}

#[test]
fn malformed_evaluate_citation_leaves_focus_and_delivery_unchanged() {
    refuses_without_navigation(|verbs, first| {
        verbs
            .evaluate(
                cited_evaluation(first, "https://example.test/evidence".into()),
                at(6),
            )
            .map(|_| ())
    });
}

#[test]
fn valid_evaluation_with_foreign_run_citation_retains_disclosed_move() {
    let (agent, _peer, database) = sessions();
    let first = add(&agent.verbs, "First", None, 1);
    let second = add(&agent.verbs, "Second", None, 2);
    claim(&agent.verbs, &first, 3).expect("claim first");
    claim(&agent.verbs, &second, 4).expect("claim second");
    note(&agent.verbs, &second, 5);
    let store = SqliteStore::open(&database).expect("store");
    let run = store
        .resolve_work_ref(&ProjectId("focus-change".into()), &second)
        .expect("second")
        .active_run_id
        .expect("run");
    let citation = store.work_run_evidence(run).expect("evidence")[0]
        .as_str()
        .to_owned();
    let error = agent
        .verbs
        .evaluate(cited_evaluation(&first, citation), at(6))
        .expect_err("citation belongs to another run");
    assert!(matches!(
        error.error,
        StoreError::AcceptanceEvaluationAdmissionRefused { .. }
    ));
    let value = agent
        .verbs
        .project_error(&error, crate::store_error_value(&error.error));
    assert_eq!(value["focus_change"]["from"], second);
    assert_eq!(value["focus_change"]["to"], first);
    assert_eq!(
        store
            .work_session_state(
                &ProjectId("focus-change".into()),
                &SessionId("agent".into()),
                at(6)
            )
            .expect("session")
            .focused_work_id,
        Some(
            store
                .resolve_work_ref(&ProjectId("focus-change".into()), &first)
                .expect("first")
                .work_id
        )
    );
}

/// A blank rationale keeps storage's refusal, which names its criterion and
/// the item, and still leaves focus where it was.
#[test]
fn blank_evaluation_rationale_keeps_its_typed_refusal_and_focus() {
    refuses_without_navigation(|verbs, first| {
        let error = verbs
            .evaluate(bounded_evaluation(first, " ".into(), 1, 0), at(6))
            .expect_err("blank rationale");
        assert!(
            matches!(&error.error, StoreError::AcceptanceEvaluationRefused { reason, .. }
                if reason == "criterion 1 needs a rationale"),
            "{error:?}"
        );
        Err(error)
    });
}

/// An evaluated policy reads no explicit results, so it admits a note beside
/// an empty result list; the request check lets that through to it.
#[test]
fn completion_note_beside_an_empty_result_list_passes_the_request_check() {
    let input: crate::WorkCompleteInput = serde_json::from_value(serde_json::json!({
        "acceptance": [],
        "note": "accepted under the evaluated policy",
    }))
    .expect("completion input");
    crate::work_service::validate_completion_request(&input).expect("admissible shape");
}

/// Moves this session's focus to `other`.
fn focus_on(agent: &Session, other: &str) {
    agent
        .verbs
        .service
        .work_focus(other, at(5))
        .expect("focus moves");
}

/// An exact keyed resend of an admitted update that named no item replays,
/// even once the focus has moved onto the item it named as a prerequisite.
#[test]
fn ambient_keyed_prerequisite_replays_after_focus_moves_onto_it() {
    let (agent, _peer, _database) = sessions();
    let item = add(&agent.verbs, "Item", None, 1);
    let prerequisite = add(&agent.verbs, "Prerequisite", None, 2);
    claim(&agent.verbs, &item, 3).expect("claim");
    let input = serde_json::json!({
        "kind": "add_prerequisite", "prerequisite": prerequisite, "idempotency_key": "edge",
    });
    let first = agent
        .verbs
        .service
        .work_update_on(None, serde_json::from_value(input.clone()).unwrap(), at(4))
        .expect("edge added");
    focus_on(&agent, &prerequisite);
    let replay = agent
        .verbs
        .service
        .work_update_on(None, serde_json::from_value(input).unwrap(), at(6))
        .expect("exact resend replays");
    assert_eq!(
        serde_json::to_value(first).unwrap(),
        serde_json::to_value(replay).unwrap()
    );
}

/// The same for a keyed decomposition of the focus whose child waits on an
/// existing item that later becomes the focus.
#[test]
fn ambient_keyed_decomposition_replays_after_focus_moves_onto_its_prerequisite() {
    let (agent, _peer, _database) = sessions();
    let parent = add(&agent.verbs, "Parent", None, 1);
    let prerequisite = add(&agent.verbs, "Prerequisite", None, 2);
    claim(&agent.verbs, &parent, 3).expect("claim");
    let mut input = decomposition_with("a", &prerequisite);
    input["idempotency_key"] = serde_json::json!("decomposition");
    let first = agent
        .verbs
        .service
        .work_propose_on(None, serde_json::from_value(input.clone()).unwrap(), at(4))
        .expect("decomposed");
    focus_on(&agent, &prerequisite);
    let replay = agent
        .verbs
        .service
        .work_propose_on(None, serde_json::from_value(input).unwrap(), at(6))
        .expect("exact resend replays");
    assert_eq!(
        serde_json::to_value(first).unwrap(),
        serde_json::to_value(replay).unwrap()
    );
}
