//! A standing blocking evaluation, as the agent words disclose it: `show`,
//! `show --full` and `next` carry the same typed cause a refused record
//! carries, before any evaluator is started.

use super::*;
use crate::NextInput;

#[test]
fn show_full_show_and_next_disclose_the_standing_cause_a_refusal_carries() {
    let directory = crate::test_support::temp_home().expect("temporary directory");
    let database = directory.path().join("work.sqlite3");
    let project = ProjectId("evaluate-reroll".into());
    let verbs = AgentVerbs::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let added = verbs
        .add(
            AddInput {
                title: "Rerolled item".into(),
                acceptance: vec!["alpha: tests pass".into(), "beta: docs updated".into()],
                ..AddInput::default()
            },
            at(0),
        )
        .expect("add");
    let work_ref = added.value["work"]["short_ref"]
        .as_str()
        .expect("work ref")
        .to_owned();
    verbs
        .claim(
            ClaimInput {
                work_ref: work_ref.clone(),
                ttl_seconds: Some(3_600),
                recover: None,
            },
            at(1),
        )
        .expect("claim");
    verbs
        .gate(
            GateInput {
                work_ref: Some(work_ref.clone()),
                name: "cargo-test".into(),
                failed: Vec::new(),
                evidence_ref: None,
            },
            at(2),
        )
        .expect("gate");
    enable(&database, &[AcceptanceEvaluationMode::SameSession], 3);
    let run_id = SqliteStore::open(&database)
        .expect("store")
        .resolve_work_ref(&project, &work_ref)
        .expect("resolve work")
        .active_run_id
        .expect("active run");
    let gate_hash = SqliteStore::open(&database)
        .expect("store")
        .work_run_evidence(run_id)
        .expect("run evidence")
        .into_iter()
        .map(|hash| hash.as_str().to_owned())
        .collect::<Vec<_>>();
    let basis = || {
        SqliteStore::open(&database)
            .expect("store")
            .work_feed_head(&crate::domain::FeedId::RunExecution(run_id))
            .expect("run feed head")
    };
    let failed_at = basis();
    let failing = verbs
        .evaluate(
            evaluate_input(
                &work_ref,
                failed_at,
                vec![
                    verdict(1, "pass", "asserted", &gate_hash),
                    verdict(2, "fail", "judgment", &[]),
                ],
            ),
            at(4),
        )
        .expect("record a failing evaluation");
    let failed = failing.value["evaluation"]["hash"]
        .as_str()
        .expect("record id")
        .to_owned();
    let head = basis();

    // The documented shape: the refusal family's tag, the run feed as its
    // own typed identity, an exclusive and an inclusive position, and the
    // one-based criterion with the verdict's wire word.
    let expected = serde_json::json!({
        "kind": "reroll",
        "mismatch": "blocking_evaluation_stands",
        "evaluation": failed,
        "feed": {"kind": "run_execution", "id": run_id.0.to_string()},
        "after_position": failed_at,
        "through_position": head,
        "criterion": 2,
        "verdict": "fail",
        "remedy": "record_new_evidence_then_evaluate",
    });
    let shown = verbs.show(&work_ref, at(5)).expect("show");
    assert_eq!(shown.value["acceptance_evaluation"]["reroll"], expected);
    assert!(
        shown
            .text()
            .contains("re-roll refused: fail on criterion 2 stands"),
        "{}",
        shown.text()
    );
    let full = verbs
        .show_records(
            &work_ref,
            &ShowInput {
                full: true,
                ..ShowInput::default()
            },
            at(5),
        )
        .expect("show --full");
    assert_eq!(full.value["work"]["evaluation"]["reroll"], expected);
    assert!(
        full.text()
            .contains("re-roll refused: fail on criterion 2 stands"),
        "{}",
        full.text()
    );
    let next = verbs
        .next(
            &NextInput {
                peek: true,
                ..NextInput::default()
            },
            at(5),
        )
        .expect("next");
    assert_eq!(next.value["focus"]["evaluation"]["reroll"], expected);
    // The full cause still fits the compact budget, value and text alike.
    let value_bytes = serde_json::to_vec(&next.value).expect("value").len();
    let text_bytes = format!("{}\n", next.text()).len();
    assert!(
        value_bytes < MAX_AGENT_WORK_RESPONSE_BYTES && text_bytes < MAX_AGENT_WORK_RESPONSE_BYTES,
        "{value_bytes} value bytes, {text_bytes} text bytes"
    );
    assert!(
        next.text().contains("re-roll needs new evidence"),
        "{}",
        next.text()
    );

    // A re-roll on the same evidence is refused with exactly that cause, and
    // its error keeps the evaluation refusal's code.
    let refused = verbs
        .evaluate(
            evaluate_input(
                &work_ref,
                head,
                vec![
                    verdict(1, "pass", "asserted", &gate_hash),
                    verdict(2, "pass", "judgment", &gate_hash),
                ],
            ),
            at(6),
        )
        .expect_err("a re-roll on the same evidence is refused");
    let value = crate::mcp::store_error_value(&refused.error);
    assert_eq!(value["error"]["code"], "acceptance_evaluation_refused");
    assert_eq!(value["error"]["details"]["cause"], expected);
    assert_eq!(
        value["error"]["details"]["remedy"],
        crate::domain::RerollAdmissionCause::REMEDY
    );

    // A correction clears it: the status shows nothing standing, and the
    // next evaluation on a basis that includes it records.
    verbs
        .note(
            &NoteInput {
                status: false,
                work_ref: Some(work_ref.clone()),
                text: "correction: the docs are updated".into(),
                refs: Vec::new(),
            },
            at(7),
        )
        .expect("correction note");
    let shown = verbs.show(&work_ref, at(8)).expect("show");
    assert!(
        shown.value["acceptance_evaluation"].get("reroll").is_none(),
        "{}",
        shown.value["acceptance_evaluation"]
    );
    verbs
        .evaluate(
            evaluate_input(
                &work_ref,
                basis(),
                vec![
                    verdict(1, "pass", "asserted", &gate_hash),
                    verdict(2, "pass", "judgment", &gate_hash),
                ],
            ),
            at(9),
        )
        .expect("an evaluation after the correction records");
}
