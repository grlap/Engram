//! `show`, `show --full`, the `evaluate` refusal and its receipt disclose a
//! failing evaluation whose criteria the executor revised.

use super::*;

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one agent walk shows the failure, the revision, both reads, the refusal and the acknowledged record"
)]
fn a_carried_failure_is_shown_refused_until_named_and_named_in_the_receipt() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("work.sqlite3");
    let project = ProjectId("carried-word".into());
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
                title: "Carried item".into(),
                acceptance: vec!["the report lists every store".into()],
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
    enable(
        &database,
        &[
            AcceptanceEvaluationMode::SameSession,
            AcceptanceEvaluationMode::SubAgent,
            AcceptanceEvaluationMode::IndependentSession,
        ],
        3,
    );
    let run_id = SqliteStore::open(&database)
        .expect("store")
        .resolve_work_ref(&project, &work_ref)
        .expect("resolve work")
        .active_run_id
        .expect("active run");
    let gate = SqliteStore::open(&database)
        .expect("store")
        .work_run_evidence(run_id)
        .expect("run evidence")
        .into_iter()
        .map(|hash| hash.as_str().to_owned())
        .collect::<Vec<_>>();
    let bases = || {
        let shown = verbs.show(&work_ref, at(4)).expect("show");
        (
            shown.value["acceptance_basis"]
                .as_i64()
                .expect("acceptance basis"),
            shown.value["evidence_basis"]
                .as_i64()
                .expect("evidence basis"),
        )
    };
    let submission = |verdict_word: &str, supersedes: Option<&str>| {
        let (acceptance, evidence) = bases();
        let citations = if verdict_word == "pass" {
            gate.clone()
        } else {
            Vec::new()
        };
        // The holder's own evaluation: a sub-agent under its session, the
        // executor-affiliated mode an unmarked task still admits.
        EvaluateInput {
            mode: "sub_agent".into(),
            execution_identity: Some("agent-evaluator".into()),
            parent_session: Some("agent".into()),
            acceptance_basis: acceptance,
            supersedes: supersedes.map(str::to_owned),
            ..evaluate_input(
                &work_ref,
                evidence,
                vec![verdict(1, verdict_word, "judgment", &citations)],
            )
        }
    };

    // The holder's sub-agent: a child session of its own, never a holder.
    let agent_child = AgentVerbs::new(
        database.clone(),
        project.clone(),
        "agent-child".into(),
        SessionId("agent-child".into()),
        None,
    );
    let failed = agent_child
        .evaluate(submission("fail", None), at(5))
        .expect("failing evaluation");
    let failed_id = failed.value["evaluation"]["evaluation"]
        .as_str()
        .expect("record id")
        .to_owned();
    verbs
        .update(
            UpdateInput {
                work_ref: Some(work_ref.clone()),
                action: UpdateAction::Revise {
                    external: None,
                    clear_external: false,
                    title: None,
                    outcome: None,
                    acceptance: Some(vec!["the report lists some stores".into()]),
                    bindings: None,
                    assignee: None,
                    priority: None,
                    defer: None,
                    kind: None,
                    labels: Vec::new(),
                    unlabels: Vec::new(),
                },
            },
            at(6),
        )
        .expect("the executor rewords the failed criterion");

    let shown = verbs.show(&work_ref, at(7)).expect("show");
    let carried = &shown.value["acceptance_evaluation"]["carried_failure"];
    assert_eq!(carried["evaluation"], failed_id.as_str(), "{carried}");
    assert_eq!(carried["revised_by"], "executor");
    assert_eq!(carried["failing"], 1);
    assert_eq!(carried["supersedes_required"], true);
    assert!(carried["judged_revision"].as_i64().is_some());
    assert!(
        shown.text().contains(&format!(
            "carried failure: evaluation {failed_id} did not pass 1 of the criteria"
        )) && shown.text().contains(&format!(
            "the next evaluation must name it, from an evaluator that never held the run: --supersedes {failed_id}"
        )),
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
            at(7),
        )
        .expect("show --full");
    let complete = &full.value["work"]["evaluation"];
    assert_eq!(evaluation_record_id(complete), failed_id.as_str());
    evaluation_record_id(&complete["carried_failure"]);
    assert_eq!(
        complete["carried_failure"]["evaluation"],
        failed_id.as_str()
    );
    assert_eq!(
        complete["carried_failure"]["blocking"],
        serde_json::json!([{
            "criterion": 1,
            "verdict": "fail",
            "rationale": "criterion 1: fail because the gate says so",
        }])
    );
    assert_eq!(
        complete["carried_failure"]["judged_criteria"],
        serde_json::json!(["the report lists every store"])
    );
    // The judged criterion had no binding, and still has none.
    assert_eq!(
        complete["carried_failure"]["judged_bindings"],
        serde_json::json!([])
    );
    assert!(full.value["work"].get("acceptance_bindings").is_none());
    assert!(
        full.text().contains("  judged bindings: none"),
        "{}",
        full.text()
    );
    // The criteria before (as judged) and after, side by side.
    assert_eq!(
        complete["verdicts"][0]["criterion"],
        "the report lists every store"
    );
    assert_eq!(
        full.value["work"]["acceptance"],
        serde_json::json!(["the report lists some stores"])
    );

    let refused = verbs
        .evaluate(submission("pass", None), at(8))
        .expect_err("a pass that ignores the failure is refused");
    let error = crate::store_error_value(&refused.error);
    assert_eq!(error["error"]["code"], "acceptance_evaluation_refused");
    assert_eq!(
        error["error"]["details"]["reason"],
        "carried_failure_unacknowledged"
    );
    assert_eq!(
        error["error"]["details"]["failed_evaluation"],
        failed_id.as_str()
    );
    assert!(
        error["error"]["details"]["remedy"]
            .as_str()
            .is_some_and(|remedy| remedy.contains("--supersedes RECORD_ID")),
        "{error}"
    );

    // The executor naming its own failure is refused, with every way out.
    let self_named = verbs
        .evaluate(submission("pass", Some(&failed_id)), at(9))
        .expect_err("the executor may not acknowledge its own failure");
    let error = crate::store_error_value(&self_named.error);
    assert_eq!(error["error"]["code"], "acceptance_evaluation_refused");
    assert_eq!(
        error["error"]["details"]["reason"],
        "carried_failure_self_acknowledged"
    );
    assert_eq!(
        error["error"]["details"]["failed_evaluation"],
        failed_id.as_str()
    );
    assert!(
        error["error"]["details"]["remedy"]
            .as_str()
            .is_some_and(|remedy| remedy.contains("independent_session")
                && remedy.contains("sub_agent under its own host-issued session")),
        "{error}"
    );

    let reviewer = AgentVerbs::new(
        database.clone(),
        project.clone(),
        "reviewer".into(),
        SessionId("reviewer".into()),
        None,
    );
    let recorded = reviewer
        .evaluate(
            EvaluateInput {
                mode: "independent_session".into(),
                execution_identity: None,
                parent_session: None,
                ..submission("pass", Some(&failed_id))
            },
            at(9),
        )
        .expect("the reviewer's acknowledged pass is recorded");
    assert_eq!(
        recorded.value["evaluation"]["supersedes"],
        failed_id.as_str()
    );
    assert!(
        recorded
            .text()
            .contains(&format!("superseding the carried failure {failed_id}")),
        "{}",
        recorded.text()
    );
    let after = verbs.show(&work_ref, at(10)).expect("show");
    let block = &after.value["acceptance_evaluation"];
    assert!(block.get("carried_failure").is_none(), "{block}");
    assert_eq!(block["supersedes"], failed_id.as_str());
    assert!(
        after
            .text()
            .contains(&format!("supersedes the carried failure {failed_id}")),
        "{}",
        after.text()
    );
    let nothing = verbs
        .evaluate(submission("pass", Some(&failed_id)), at(11))
        .expect_err("nothing is carried any more");
    assert_eq!(
        crate::store_error_value(&nothing.error)["error"]["details"]["reason"],
        "nothing_to_supersede"
    );
    verbs
        .done(
            DoneInput {
                source_fingerprint: None,
                landing: None,
                links: Vec::new(),
                link_basis: None,
                work_ref: Some(work_ref.clone()),
                summary: Some("delivered after superseding the failure".into()),
                note: None,
            },
            at(12),
        )
        .expect("the acknowledged pass completes");
}

// B56: a binding-only revision leaves the criterion's text as it was, so
// `show --full` gives the bindings the failure was judged under beside the
// current ones.
#[test]
fn a_binding_only_revision_shows_the_judged_bindings_beside_the_current_ones() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("work.sqlite3");
    let verbs = AgentVerbs::new(
        database.clone(),
        ProjectId("carried-bindings".into()),
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let added = verbs
        .add(
            AddInput {
                title: "Bound item".into(),
                acceptance: vec!["the tests pass".into()],
                bindings: vec!["1=test".into()],
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
    enable(&database, &[AcceptanceEvaluationMode::SameSession], 2);
    let shown = verbs.show(&work_ref, at(3)).expect("show");
    verbs
        .evaluate(
            EvaluateInput {
                acceptance_basis: shown.value["acceptance_basis"]
                    .as_i64()
                    .expect("acceptance basis"),
                ..evaluate_input(
                    &work_ref,
                    shown.value["evidence_basis"]
                        .as_i64()
                        .expect("evidence basis"),
                    vec![verdict(1, "fail", "judgment", &[])],
                )
            },
            at(4),
        )
        .expect("failing evaluation");
    verbs
        .update(
            UpdateInput {
                work_ref: Some(work_ref.clone()),
                action: UpdateAction::Revise {
                    external: None,
                    clear_external: false,
                    title: None,
                    outcome: None,
                    acceptance: None,
                    bindings: Some(vec!["1=review".into()]),
                    assignee: None,
                    priority: None,
                    defer: None,
                    kind: None,
                    labels: Vec::new(),
                    unlabels: Vec::new(),
                },
            },
            at(5),
        )
        .expect("the executor binds the criterion to another kind");
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
    assert_eq!(
        full.value["work"]["evaluation"]["carried_failure"]["judged_bindings"],
        serde_json::json!([{ "criterion": 1, "requirement": { "check_kind": "test" } }])
    );
    assert_eq!(
        full.value["work"]["acceptance_bindings"],
        serde_json::json!([{ "criterion": 1, "requirement": { "check_kind": "review" } }])
    );
    assert!(
        full.text()
            .contains("  judged bindings: criterion 1 [requires host test verification]")
            && full
                .text()
                .contains("1. the tests pass  [requires host review verification]"),
        "{}",
        full.text()
    );
}

// B58: after a reviewer names the failure and fails the revised criteria,
// `show --full` still gives the original failure's criteria and verdicts as
// the before side, beside the newest evaluation's criteria and the current
// ones.
#[test]
fn full_readback_keeps_the_original_failure_beside_a_failing_review() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("work.sqlite3");
    let project = ProjectId("carried-readback".into());
    let verbs = AgentVerbs::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let work_ref = verbs
        .add(
            AddInput {
                title: "Readback item".into(),
                acceptance: vec!["the report lists every store".into()],
                ..AddInput::default()
            },
            at(0),
        )
        .expect("add")
        .value["work"]["short_ref"]
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
    enable(
        &database,
        &[
            AcceptanceEvaluationMode::SameSession,
            AcceptanceEvaluationMode::SubAgent,
            AcceptanceEvaluationMode::IndependentSession,
        ],
        2,
    );
    let submission = |verdict_word: &str, mode: &str, supersedes: Option<&str>| {
        let shown = verbs.show(&work_ref, at(3)).expect("show");
        let own = mode == "sub_agent";
        EvaluateInput {
            mode: mode.into(),
            execution_identity: own.then(|| "agent-evaluator".into()),
            parent_session: own.then(|| "agent".into()),
            acceptance_basis: shown.value["acceptance_basis"]
                .as_i64()
                .expect("acceptance basis"),
            supersedes: supersedes.map(str::to_owned),
            ..evaluate_input(
                &work_ref,
                shown.value["evidence_basis"]
                    .as_i64()
                    .expect("evidence basis"),
                vec![verdict(1, verdict_word, "judgment", &[])],
            )
        }
    };
    // The holder's sub-agent: a child session of its own, never a holder.
    let agent_child = AgentVerbs::new(
        database.clone(),
        project.clone(),
        "agent-child".into(),
        SessionId("agent-child".into()),
        None,
    );
    let failed = agent_child
        .evaluate(submission("fail", "sub_agent", None), at(4))
        .expect("failing evaluation");
    let failed_id = failed.value["evaluation"]["evaluation"]
        .as_str()
        .expect("record id")
        .to_owned();
    verbs
        .update(
            UpdateInput {
                work_ref: Some(work_ref.clone()),
                action: UpdateAction::Revise {
                    external: None,
                    clear_external: false,
                    title: None,
                    outcome: None,
                    acceptance: Some(vec!["the report lists some stores".into()]),
                    bindings: None,
                    assignee: None,
                    priority: None,
                    defer: None,
                    kind: None,
                    labels: Vec::new(),
                    unlabels: Vec::new(),
                },
            },
            at(5),
        )
        .expect("the executor rewords the failed criterion");
    let reviewer = AgentVerbs::new(
        database.clone(),
        project.clone(),
        "reviewer".into(),
        SessionId("reviewer".into()),
        None,
    );
    let review = reviewer
        .evaluate(
            submission("fail", "independent_session", Some(&failed_id)),
            at(6),
        )
        .expect("the reviewer names the failure and fails the revision");
    let review_id = review.value["evaluation"]["evaluation"]
        .as_str()
        .expect("record id")
        .to_owned();

    let full = verbs
        .show_records(
            &work_ref,
            &ShowInput {
                full: true,
                ..ShowInput::default()
            },
            at(7),
        )
        .expect("show --full");
    let evaluation = &full.value["work"]["evaluation"];
    assert_eq!(evaluation_record_id(evaluation), review_id.as_str());
    // The newest evaluation is the review, on the reworded criterion.
    assert_eq!(evaluation["evaluation"], review_id.as_str());
    assert_eq!(
        evaluation["verdicts"][0]["criterion"],
        "the report lists some stores"
    );
    // The carried failure is still the original, with its own criteria and
    // verdict as the before side.
    let carried = &evaluation["carried_failure"];
    assert_eq!(carried["evaluation"], failed_id.as_str());
    assert_eq!(
        carried["judged_criteria"],
        serde_json::json!(["the report lists every store"])
    );
    assert_eq!(
        carried["blocking"],
        serde_json::json!([{
            "criterion": 1,
            "verdict": "fail",
            "rationale": "criterion 1: fail because the gate says so",
        }])
    );
    assert_eq!(
        full.value["work"]["acceptance"],
        serde_json::json!(["the report lists some stores"])
    );
    // The newest evaluation judged no binding; the original judged none.
    assert_eq!(carried["newest_judged_bindings"], serde_json::json!([]));
    assert_eq!(carried["judged_bindings"], serde_json::json!([]));
    let text = full.text();
    assert!(
        text.contains("  bindings the newest evaluation judged: none"),
        "{text}"
    );
    assert!(
        text.contains(&format!("  criteria evaluation {failed_id} judged:"))
            && text.contains("    1. the report lists every store")
            && text.contains("       fail: criterion 1: fail because the gate says so")
            && text.contains(&format!(
                "  criteria the newest evaluation {review_id} judged:"
            ))
            && text.contains("    1. the report lists some stores"),
        "{text}"
    );
}
