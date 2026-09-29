use super::*;

#[test]
fn operator_basisless_latest_change_needs_a_named_root_check_or_waiver() {
    let directory = crate::test_support::temp_home().expect("temporary directory");
    let database = directory.path().join("operator-guidance.sqlite3");
    let mut store = SqliteStore::open(&database).expect("store");
    select_rules(&mut store, vec![pinned_rule("pinned-suite")]);
    store
        .set_acceptance_evaluation_policy(
            &crate::domain::AcceptanceEvaluationPolicy {
                allowed_modes: vec![crate::domain::AcceptanceEvaluationMode::SameSession],
                mechanical_basis: crate::domain::MechanicalBasis::Asserted,
                require_source_freshness: false,
            },
            &actor("policy-admin"),
            "enable-operator-evaluation",
            None,
            at(1),
            &DevelopmentNoopRedactor,
        )
        .expect("evaluated policy");
    let work = store
        .create_work(
            &root_request("project-a", "basisless-operator-guidance", 2),
            &DevelopmentNoopRedactor,
        )
        .expect("work");
    let claim = claim(&mut store, &work, "runner", "basisless-claim", 3, 300);
    let change = source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "basisless-change",
        4,
        None,
        None,
    );
    let open = operator_obligation(&store, &claim, &change);
    assert_eq!(open.state, WorkObligationState::Open);
    assert_eq!(
        store
            .work_obligation_completion_actions(&[&open.obligation])
            .expect("actual remedy"),
        [WorkObligationCompletionAction::NameRootCheckOrWaiver]
    );
    let verbs = crate::verbs::AgentVerbs::new(
        database,
        work.project_id.clone(),
        "runner".into(),
        SessionId("runner".into()),
        None,
    );
    let advisory = verbs
        .show(&work.short_ref, at(5))
        .expect("agent guidance")
        .value["evaluation_obligations"]
        .clone();
    assert_eq!(advisory["action_required_total"], 1);
    assert_eq!(
        advisory["items"][0]["action_required_before_evaluation"],
        true
    );
    assert!(
        advisory["items"][0]["remedy"]
            .as_str()
            .expect("remedy")
            .contains("name a source root and run its credited check")
    );

    host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "pinned-suite",
        VerificationKind::Test,
        VerificationResult::Passed,
        5,
        basis("workspace-B", "B5", None),
    );
    assert_eq!(
        operator_obligation(&store, &claim, &change).state,
        WorkObligationState::Open,
        "a check with no named root cannot credit the basisless latest change"
    );

    name_root(&mut store, &work, &claim, 9, 6);
    host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "pinned-suite",
        VerificationKind::Test,
        VerificationResult::Passed,
        7,
        basis("workspace-B", "B7", Some(9)),
    );
    assert_eq!(
        operator_obligation(&store, &claim, &change).state,
        WorkObligationState::Satisfied,
        "a fresh check in the named root credits the previously unlocated change"
    );
    assert!(store.verify_all().expect("doctor").is_healthy());
}
