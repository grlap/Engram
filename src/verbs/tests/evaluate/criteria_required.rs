//! `done` on an item with no acceptance criteria, under a policy that requires
//! an acceptance evaluation, is refused by name through the word, the same
//! way on a retry, and completes once criteria are added and evaluated.

use super::*;
use crate::domain::{ChildRequirement, CreateWorkRequest, WorkItemKind, WorkOrigin};

fn done(verbs: &AgentVerbs, work_ref: &str, second: i64) -> Result<Receipt, VerbError> {
    verbs.done(
        DoneInput {
            source_fingerprint: None,
            landing: None,
            links: Vec::new(),
            link_basis: None,
            work_ref: Some(work_ref.into()),
            summary: Some("delivered".into()),
            note: None,
        },
        at(second),
    )
}

// B52: the refusal through the word, its retry, and the way through to a seal.
#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one agent walk shows the refusal, its retry, and the way through to a seal"
)]
fn done_without_criteria_is_refused_by_name_until_criteria_are_added_and_evaluated() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("work.sqlite3");
    let project = ProjectId("criteria-required-word".into());
    let verbs = AgentVerbs::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    // An item without criteria, as a migration imports one: the add word
    // would substitute the title as a criterion, so the core creates it.
    let item = SqliteStore::open(&database)
        .expect("store")
        .create_work(
            &CreateWorkRequest {
                acceptance_bindings: Vec::new(),
                evaluation_mode: None,
                external_ref: None,
                notes: Vec::new(),
                project_id: project.clone(),
                parent_id: None,
                child_requirement: ChildRequirement::Required,
                title: "Imported item".into(),
                outcome: "Imported without acceptance criteria".into(),
                acceptance: Vec::new(),
                kind: WorkItemKind::Task,
                priority: 2,
                labels: Vec::new(),
                assigned_to: None,
                deferred_until: None,
                origin: WorkOrigin::Local,
                source_snapshot_id: None,
                actor: ActorContext {
                    actor_id: "importer".into(),
                    actor_kind: "host_operator".into(),
                    assurance: AssuranceLevel::Asserted,
                    run_id: None,
                    session_id: Some(SessionId("importer".into())),
                    source_tool: Some("verbs_test".into()),
                    source_skill: None,
                    provenance_chain: Vec::<ProvenanceLink>::new(),
                    reason: "import an item without acceptance criteria".into(),
                },
                idempotency_key: "create-criteria-less".into(),
                created_at: at(0),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("create an item without criteria");
    assert!(item.acceptance.is_empty(), "{:?}", item.acceptance);
    let work_ref = item.short_ref.clone();
    enable(&database, &[AcceptanceEvaluationMode::SameSession], 1);
    verbs
        .claim(
            ClaimInput {
                work_ref: work_ref.clone(),
                ttl_seconds: Some(3_600),
                recover: None,
            },
            at(2),
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
            at(3),
        )
        .expect("gate");
    let run_id = item.active_run_id.expect("active run");
    let head = || {
        SqliteStore::open(&database)
            .expect("store")
            .work_feed_head(&crate::domain::FeedId::RunExecution(run_id))
            .expect("run feed head")
    };
    let before = head();

    // The first attempt and an identical retry are both refused by name and
    // capture nothing: no evidence, checkpoint or seal reaches the run.
    for (attempt, second) in [(1, 4), (2, 5)] {
        let refused = done(&verbs, &work_ref, second).expect_err("nothing to evaluate");
        assert!(
            matches!(refused.error, StoreError::AcceptanceCriteriaRequired { work } if work == item.work_id),
            "attempt {attempt}: {refused:?}"
        );
        assert_eq!(
            crate::store_error_value(&refused.error)["error"]["code"],
            "acceptance_criteria_required"
        );
        let guidance = refused.guidance();
        assert!(
            guidance
                .reminders
                .iter()
                .any(|reminder| reminder.contains("no acceptance criteria")),
            "{guidance:?}"
        );
        assert_eq!(
            guidance.next,
            vec![
                format!("engram work update {work_ref} --accept \"…\""),
                format!("engram work done {work_ref}"),
            ]
        );
        assert_eq!(head(), before, "attempt {attempt} must append nothing");
    }

    // With a criterion added, done names the missing evaluation instead.
    verbs
        .update(
            UpdateInput {
                work_ref: Some(work_ref.clone()),
                action: UpdateAction::Revise {
                    external: None,
                    clear_external: false,
                    title: None,
                    outcome: None,
                    acceptance: Some(vec!["the imported work is delivered".into()]),
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
        .expect("add a criterion");
    let owed = done(&verbs, &work_ref, 7).expect("done names what is owed");
    assert!(owed.owed, "{}", owed.text());
    assert_eq!(owed.value["code"], "missing_acceptance_evaluation");

    // An evaluation of that criterion lets done seal the item.
    let evidence = SqliteStore::open(&database)
        .expect("store")
        .work_run_evidence(run_id)
        .expect("run evidence")
        .into_iter()
        .map(|hash| hash.as_str().to_owned())
        .collect::<Vec<_>>();
    let shown = verbs.show(&work_ref, at(8)).expect("show");
    let acceptance_basis = shown.value["acceptance_basis"]
        .as_i64()
        .expect("acceptance basis");
    verbs
        .evaluate(
            EvaluateInput {
                acceptance_basis,
                ..evaluate_input(
                    &work_ref,
                    head(),
                    vec![verdict(1, "pass", "judgment", &evidence)],
                )
            },
            at(9),
        )
        .expect("record a passing evaluation");
    let completed = done(&verbs, &work_ref, 10).expect("done after the evaluation");
    assert!(!completed.owed, "{}", completed.text());
}
