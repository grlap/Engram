//! The `evaluate` word and its `show`, `update`, and `done` disclosures.

mod review;

use super::*;
use crate::domain::{
    AcceptanceEvaluationMode, AcceptanceEvaluationPolicy, ActorContext, AssuranceLevel,
    MechanicalBasis, ProvenanceLink,
};
use crate::{
    ClaimInput, DevelopmentNoopRedactor, DoneInput, EvaluateInput, GateInput, SqliteStore,
    WorkCriterionVerdictInput,
};

fn enable(database: &std::path::Path, modes: &[AcceptanceEvaluationMode], second: i64) {
    SqliteStore::open(database)
        .expect("store")
        .set_acceptance_evaluation_policy(
            &AcceptanceEvaluationPolicy {
                allowed_modes: modes.to_vec(),
                mechanical_basis: MechanicalBasis::Asserted,
                require_source_freshness: false,
            },
            &ActorContext {
                actor_id: "policy-admin".into(),
                actor_kind: "host_operator".into(),
                assurance: AssuranceLevel::Asserted,
                run_id: None,
                session_id: None,
                source_tool: Some("verbs_test".into()),
                source_skill: None,
                provenance_chain: Vec::<ProvenanceLink>::new(),
                reason: "enable acceptance evaluation for the verbs test".into(),
            },
            "evaluated completion for this test project",
            "enable-evaluated-completion",
            None,
            at(second),
            &DevelopmentNoopRedactor,
        )
        .expect("activate the acceptance evaluation policy");
}

fn verdict(
    position: usize,
    verdict: &str,
    basis: &str,
    evidence: &[String],
) -> WorkCriterionVerdictInput {
    WorkCriterionVerdictInput {
        criterion: position,
        verdict: verdict.into(),
        basis: basis.into(),
        rationale: format!("criterion {position}: {verdict} because the gate says so"),
        evidence: evidence.to_vec(),
    }
}

fn evaluate_input(
    work_ref: &str,
    evidence_basis: i64,
    verdicts: Vec<WorkCriterionVerdictInput>,
) -> EvaluateInput {
    EvaluateInput {
        work_ref: Some(work_ref.into()),
        mode: "same_session".into(),
        acceptance_basis: 1,
        evidence_basis,
        verdicts,
        attempt: None,
        source_fingerprint: None,
        model: None,
        execution_identity: None,
        parent_session: None,
    }
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one agent walk shows the refusal, the record, the show disclosure, the done refusal, and the seal"
)]
fn evaluate_word_records_verdicts_and_the_other_words_disclose_them() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("work.sqlite3");
    let project = ProjectId("evaluate-word".into());
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
                title: "Evaluated item".into(),
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
    assert_eq!(gate_hash.len(), 1);
    // What show prints as the evidence basis: the run-feed head at read time.
    let basis = || {
        SqliteStore::open(&database)
            .expect("store")
            .work_feed_head(&crate::domain::FeedId::RunExecution(run_id))
            .expect("run feed head")
    };

    let refused = verbs
        .evaluate(
            evaluate_input(
                &work_ref,
                basis(),
                vec![
                    verdict(1, "pass", "asserted", &gate_hash),
                    verdict(2, "pass", "judgment", &gate_hash),
                ],
            ),
            at(3),
        )
        .expect_err("the legacy policy refuses evaluate");
    assert!(
        matches!(
            refused.error,
            StoreError::AcceptanceEvaluationRefused { .. }
        ),
        "{refused:?}"
    );
    assert!(
        refused
            .guidance()
            .reminders
            .iter()
            .any(|reminder| reminder.contains("does not enable acceptance evaluation")),
        "{:?}",
        refused.guidance()
    );

    enable(&database, &[AcceptanceEvaluationMode::SameSession], 4);
    let pinned = verbs
        .update(
            UpdateInput {
                work_ref: Some(work_ref.clone()),
                action: UpdateAction::EvaluationMode {
                    mode: Some("same_session".into()),
                },
            },
            at(5),
        )
        .expect("pin the evaluation mode");
    assert!(
        pinned.text().contains(&format!(
            "pinned evaluation mode same_session on {work_ref}"
        )),
        "{}",
        pinned.text()
    );
    let stale_basis = verbs
        .evaluate(
            evaluate_input(
                &work_ref,
                basis(),
                vec![verdict(1, "pass", "asserted", &gate_hash)],
            ),
            at(6),
        )
        .expect_err("the pin bumped the revision; the printed basis is stale");
    assert!(
        matches!(stale_basis.error, StoreError::WorkRevisionConflict { .. }),
        "{stale_basis:?}"
    );
    let shown = verbs.show(&work_ref, at(6)).expect("show");
    assert!(
        shown.text().contains("evaluation mode: same_session"),
        "{}",
        shown.text()
    );
    assert_eq!(
        shown.value["status"]["work"]["evaluation_mode"], "same_session",
        "a host selects the evaluator from the JSON, so the pin must be there too"
    );
    let acceptance = shown.value["acceptance_basis"].as_i64().expect("basis");
    assert_eq!(acceptance, 2);
    assert_eq!(shown.value["evidence_basis"], basis());
    assert!(
        shown.text().contains(&format!(
            "evidence basis: {} (pass --evidence-basis",
            basis()
        )),
        "{}",
        shown.text()
    );

    let failing = verbs
        .evaluate(
            EvaluateInput {
                acceptance_basis: acceptance,
                ..evaluate_input(
                    &work_ref,
                    basis(),
                    vec![
                        verdict(1, "pass", "asserted", &gate_hash),
                        verdict(2, "fail", "judgment", &[]),
                    ],
                )
            },
            at(7),
        )
        .expect("record a failing evaluation");
    assert!(
        failing.text().contains(&format!(
            "recorded same_session evaluation on {work_ref} \"Evaluated item\": 1/2 pass, fail on \"beta: docs updated\""
        )),
        "{}",
        failing.text()
    );
    assert_eq!(failing.value["evaluation"]["passed"], 1);
    assert_eq!(failing.value["evaluation"]["verdicts_total"], 2);
    assert_eq!(failing.value["evaluation"]["verdicts_omitted"], 0);
    assert_eq!(failing.value["evaluation"]["blocking"]["position"], 2);
    assert_eq!(failing.value["evaluation"]["replayed"], false);
    let shown = verbs.show(&work_ref, at(8)).expect("show");
    let text = shown.text();
    assert!(
        text.contains("evaluation: same_session ") && text.contains("1/2 pass, fresh"),
        "{text}"
    );
    assert!(text.contains("  1. pass (asserted)"), "{text}");
    assert!(text.contains("  2. fail (judgment)"), "{text}");
    assert_eq!(shown.value["acceptance_evaluation"]["passed"], 1);
    assert_eq!(
        shown.value["acceptance_evaluation"]["verdicts"][1]["verdict"],
        "fail"
    );

    let refused_done = verbs
        .done(
            DoneInput {
                source_fingerprint: None,
                links: Vec::new(),
                link_basis: None,
                work_ref: Some(work_ref.clone()),
                summary: Some("finished".into()),
                note: None,
            },
            at(9),
        )
        .expect("done returns a recovery receipt");
    assert!(refused_done.owed);
    assert_eq!(refused_done.value["code"], "acceptance_failed");
    assert!(
        refused_done
            .next
            .iter()
            .any(|command| command.contains("engram work show")),
        "{:?}",
        refused_done.next
    );

    // An identical resend carries the same evidence basis; a resend with a
    // fresh basis is a deliberate re-evaluation and records a new object.
    let passing_basis = basis();
    let passing = verbs
        .evaluate(
            EvaluateInput {
                acceptance_basis: acceptance,
                ..evaluate_input(
                    &work_ref,
                    passing_basis,
                    vec![
                        verdict(1, "pass", "asserted", &gate_hash),
                        verdict(2, "pass", "judgment", &gate_hash),
                    ],
                )
            },
            at(10),
        )
        .expect("record a passing evaluation");
    assert!(
        passing.text().contains("2/2 pass, all criteria pass"),
        "{}",
        passing.text()
    );
    assert_eq!(passing.value["evaluation"]["evaluated_cut"], passing_basis);
    let replayed = verbs
        .evaluate(
            EvaluateInput {
                acceptance_basis: acceptance,
                ..evaluate_input(
                    &work_ref,
                    passing_basis,
                    vec![
                        verdict(1, "pass", "asserted", &gate_hash),
                        verdict(2, "pass", "judgment", &gate_hash),
                    ],
                )
            },
            at(11),
        )
        .expect("identical resend replays");
    assert_eq!(replayed.value["evaluation"]["replayed"], true);
    assert_eq!(
        replayed.value["evaluation"]["hash"],
        passing.value["evaluation"]["hash"]
    );
    let completed = verbs
        .done(
            DoneInput {
                source_fingerprint: None,
                links: Vec::new(),
                link_basis: None,
                work_ref: Some(work_ref.clone()),
                summary: Some("finished".into()),
                note: None,
            },
            at(12),
        )
        .expect("done after a passing evaluation");
    assert!(!completed.owed, "{}", completed.text());
    assert!(
        completed.text().contains("completed"),
        "{}",
        completed.text()
    );
    let cleared = verbs
        .update(
            UpdateInput {
                work_ref: Some(work_ref.clone()),
                action: UpdateAction::EvaluationMode { mode: None },
            },
            at(13),
        )
        .expect_err("completed work is frozen");
    assert!(
        matches!(
            cleared.error,
            StoreError::InvalidWork(ref reason)
                if reason == crate::work_service::COMPLETED_WORK_LATE_FINDING_REFUSAL
        ),
        "{cleared:?}"
    );
}

// Many criteria: the evaluate receipt and ordinary show stay within the agent
// budget by omitting trailing verdict rows with exact counts, and
// `show --full` returns every row with its rationale.
#[test]
fn many_criteria_fit_receipts_by_explicit_omission_and_full_show_is_complete() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("work.sqlite3");
    let project = ProjectId("evaluate-many-criteria".into());
    let verbs = AgentVerbs::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let criteria = (0..300)
        .map(|index| format!("criterion {index:03} holds"))
        .collect::<Vec<_>>();
    let added = verbs
        .add(
            AddInput {
                title: "Many criteria".into(),
                acceptance: criteria,
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
    let store = SqliteStore::open(&database).expect("store");
    let gate_hash = store
        .work_run_evidence(run_id)
        .expect("run evidence")
        .into_iter()
        .map(|hash| hash.as_str().to_owned())
        .collect::<Vec<_>>();
    let basis = store
        .work_feed_head(&crate::domain::FeedId::RunExecution(run_id))
        .expect("run feed head");
    drop(store);
    let verdicts = (1..=300)
        .map(|position| verdict(position, "pass", "asserted", &gate_hash))
        .collect::<Vec<_>>();
    let receipt = verbs
        .evaluate(evaluate_input(&work_ref, basis, verdicts), at(4))
        .expect("evaluate 300 criteria");
    let value_bytes = serde_json::to_vec(&receipt.value).expect("value").len();
    let text_bytes = format!("{}\n", receipt.text()).len();
    // The shared rule is strict: the budget is never reached exactly.
    assert!(
        value_bytes < MAX_AGENT_WORK_RESPONSE_BYTES && text_bytes < MAX_AGENT_WORK_RESPONSE_BYTES,
        "{value_bytes} value bytes, {text_bytes} text bytes"
    );
    let evaluation = &receipt.value["evaluation"];
    assert_eq!(evaluation["verdicts_total"], 300);
    assert_eq!(evaluation["passed"], 300);
    let omitted = evaluation["verdicts_omitted"].as_u64().expect("omitted");
    let shown = evaluation["verdicts"].as_array().expect("rows").len() as u64;
    assert!(omitted > 0, "{evaluation}");
    assert_eq!(shown + omitted, 300);
    assert!(
        receipt.text().contains("300/300 pass, all criteria pass"),
        "{}",
        receipt.text()
    );

    let shown = verbs.show(&work_ref, at(5)).expect("show");
    let text = shown.text();
    assert!(
        format!("{text}\n").len() < MAX_AGENT_WORK_RESPONSE_BYTES,
        "{} bytes",
        text.len()
    );
    assert!(
        text.contains("more verdicts not shown; engram work show"),
        "{text}"
    );
    let block = &shown.value["acceptance_evaluation"];
    assert_eq!(block["criteria"], 300);
    let omitted = block["verdicts_omitted"].as_u64().expect("omitted");
    let visible = block["verdicts"].as_array().expect("rows").len() as u64;
    assert!(omitted > 0, "{block}");
    assert_eq!(visible + omitted, 300);
    // Agent-facing commands quote the ref, as every other show command does.
    assert_eq!(
        block["full_detail"],
        format!("engram work show '{work_ref}' --full")
    );

    let full = verbs
        .show_records(
            &work_ref,
            &ShowInput {
                full: true,
                ..ShowInput::default()
            },
            at(6),
        )
        .expect("show --full");
    let complete = &full.value["work"]["evaluation"];
    assert_eq!(complete["verdicts"].as_array().expect("rows").len(), 300);
    assert_eq!(complete["passed"], 300);
    assert_eq!(complete["verdicts"][299]["position"], 300);
    assert!(
        complete["verdicts"][0]["rationale"]
            .as_str()
            .expect("rationale")
            .contains("because the gate says so")
    );
    assert!(full.text().contains("(complete)"), "{}", full.text());
    assert!(
        full.text().contains("  300. pass (asserted)"),
        "{}",
        full.text()
    );
}

// `add --evaluation-mode` pins the mode from creation for roots and children;
// an unknown word refuses before anything is created.
#[test]
fn add_pins_the_evaluation_mode_from_creation() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("work.sqlite3");
    let verbs = AgentVerbs::new(
        database,
        ProjectId("evaluation-mode-add".into()),
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let unknown = verbs
        .add(
            AddInput {
                title: "Never created".into(),
                evaluation_mode: Some("telepathy".into()),
                ..AddInput::default()
            },
            at(0),
        )
        .expect_err("an unknown mode word refuses");
    assert!(
        matches!(unknown.error, StoreError::InvalidWork(ref reason) if reason.contains("independent_session")),
        "{unknown:?}"
    );
    let root = verbs
        .add(
            AddInput {
                title: "Pinned root".into(),
                evaluation_mode: Some("sub-agent".into()),
                ..AddInput::default()
            },
            at(1),
        )
        .expect("add a pinned root");
    let root_ref = root.value["work"]["short_ref"]
        .as_str()
        .expect("root ref")
        .to_owned();
    assert!(
        verbs
            .show(&root_ref, at(2))
            .expect("show root")
            .text()
            .contains("evaluation mode: sub_agent")
    );
    let child = verbs
        .add(
            AddInput {
                title: "Pinned child".into(),
                under: Some(root_ref),
                evaluation_mode: Some("independent_session".into()),
                ..AddInput::default()
            },
            at(3),
        )
        .expect("add a pinned child");
    let child_ref = child.value["work"]["short_ref"]
        .as_str()
        .expect("child ref")
        .to_owned();
    assert!(
        verbs
            .show(&child_ref, at(4))
            .expect("show child")
            .text()
            .contains("evaluation mode: independent_session")
    );
}

#[test]
fn evaluation_mode_update_validates_the_word_and_clears() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("work.sqlite3");
    let verbs = AgentVerbs::new(
        database,
        ProjectId("evaluation-mode-update".into()),
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let added = verbs
        .add(
            AddInput {
                title: "Mode item".into(),
                ..AddInput::default()
            },
            at(0),
        )
        .expect("add");
    let work_ref = added.value["work"]["short_ref"]
        .as_str()
        .expect("work ref")
        .to_owned();
    let unknown = verbs
        .update(
            UpdateInput {
                work_ref: Some(work_ref.clone()),
                action: UpdateAction::EvaluationMode {
                    mode: Some("telepathy".into()),
                },
            },
            at(1),
        )
        .expect_err("an unknown mode word refuses");
    assert!(
        matches!(unknown.error, StoreError::InvalidWork(ref reason) if reason.contains("independent_session")),
        "{unknown:?}"
    );
    let pinned = verbs
        .update(
            UpdateInput {
                work_ref: Some(work_ref.clone()),
                action: UpdateAction::EvaluationMode {
                    mode: Some("independent-session".into()),
                },
            },
            at(2),
        )
        .expect("hyphenated words are accepted");
    assert!(
        pinned
            .text()
            .contains("pinned evaluation mode independent_session on"),
        "{}",
        pinned.text()
    );
    let shown = verbs.show(&work_ref, at(3)).expect("show");
    assert!(
        shown
            .text()
            .contains("evaluation mode: independent_session"),
        "{}",
        shown.text()
    );
    assert_eq!(shown.value["acceptance_basis"], 2);
    let cleared = verbs
        .update(
            UpdateInput {
                work_ref: Some(work_ref.clone()),
                action: UpdateAction::EvaluationMode { mode: None },
            },
            at(4),
        )
        .expect("clear the mode");
    assert!(
        cleared
            .text()
            .contains(&format!("cleared evaluation mode on {work_ref}")),
        "{}",
        cleared.text()
    );
    assert!(
        !verbs
            .show(&work_ref, at(5))
            .expect("show")
            .text()
            .contains("evaluation mode:")
    );
}
