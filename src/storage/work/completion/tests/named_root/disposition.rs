use super::*;

/// A displaced change reaches the agent: after `done`, the item's
/// obligation page, which `done` and `show` render, names the change the
/// named root displaced with its workspace and revision, and counts it.
#[test]
fn the_obligation_page_names_a_displaced_change() {
    let directory = crate::test_support::temp_home().expect("directory");
    let database = directory.path().join("named-root.db");
    let (mut store, work, claim, _) = named_work_in(SqliteStore::open(&database).expect("store"));
    let checked = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-in-B",
        VerificationKind::Test,
        VerificationResult::Passed,
        5,
        basis("workspace-B", "B5", Some(9)),
    );
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-B",
        6,
        std::slice::from_ref(&checked),
    );
    complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &checked,
        "complete-B",
        7,
    )
    .expect("the B check completes the work");
    drop(store);
    let service = LocalWorkService::new(
        database,
        work.project_id.clone(),
        "reader".into(),
        SessionId("reader".into()),
        None,
    );
    let focus = service
        .work_focus(&work.short_ref, at(8))
        .expect("focus the completed item");
    let page = &focus.obligation_page;
    assert_eq!(page.displaced_total, 1);
    assert_eq!(
        page.items
            .iter()
            .filter_map(|item| item.displaced_change.clone())
            .collect::<Vec<_>>(),
        vec![crate::DisplacedSourceChange {
            observation_id: "write-foreign-before-name".into(),
            workspace_id: "workspace-A".into(),
            source_revision: Some("A3".into()),
            source_id_summary: None,
        }]
    );
    assert_eq!(page.untested_total, 0, "a displaced change is not untested");
}

/// When the stock rule and an operator rule both open an obligation for one
/// change, both are displaced, and the seal and the obligation page count
/// the change once.
#[test]
fn a_change_two_rules_displace_is_disclosed_once() {
    let directory = crate::test_support::temp_home().expect("directory");
    let database = directory.path().join("named-root.db");
    let mut store = SqliteStore::open(&database).expect("store");
    let mut rules = crate::control::builtin_obligation_rule_set().rules;
    rules.push(pinned_rule("pinned-suite"));
    select_rules(&mut store, rules);
    let (mut store, work, claim, earlier) = named_work_in(store);
    let checked = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-in-B",
        VerificationKind::Test,
        VerificationResult::Passed,
        5,
        basis("workspace-B", "B5", Some(9)),
    );
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-B",
        6,
        std::slice::from_ref(&checked),
    );
    let seal = complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &checked,
        "complete-B",
        7,
    )
    .expect("the B check completes the work");
    assert_eq!(seal.foreign_workspace_changes, vec![earlier.clone()]);
    let displaced = store
        .work_run_obligations(claim.run_id)
        .expect("obligations")
        .into_iter()
        .filter(|record| record.obligation.triggering_observation == earlier)
        .map(|record| record.state)
        .collect::<Vec<_>>();
    assert_eq!(displaced, vec![WorkObligationState::Displaced; 2]);
    let report = store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
    drop(store);
    let service = LocalWorkService::new(
        database,
        work.project_id.clone(),
        "reader".into(),
        SessionId("reader".into()),
        None,
    );
    let focus = service
        .work_focus(&work.short_ref, at(8))
        .expect("focus the completed item");
    assert_eq!(focus.obligation_page.displaced_total, 1);
}

/// An operator rule that pins its check is held to the same principle as the
/// stock rule. A change from workspace A before the claim named any root is
/// displaced and disclosed, and the doctor accepts that displacement. A
/// change recorded after its generation was released is unbound: a matching
/// check in the current root after it satisfies it, but without one it stays
/// open for an operator waiver instead of being waived as untested.
#[test]
fn an_operator_rule_is_displaced_and_satisfied_like_the_stock_rule() {
    let (mut store, work, claim, earlier) = named_work_in(pinned_store("pinned-suite"));
    release(&mut store, &work, &claim, "release-runner", 5);
    let work = store.get_work_item(work.work_id).expect("item");
    let claim = super::claim(&mut store, &work, "runner", "reclaim-runner", 6, 300);
    name_root(&mut store, &work, &claim, 10, 7);
    let stale = source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "stale-generation-in-D",
        8,
        Some(basis("workspace-D", "D8", Some(9))),
        None,
    );
    let unpinned = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "unpinned-check",
        VerificationKind::Test,
        VerificationResult::Passed,
        9,
        basis("workspace-B", "B9", Some(10)),
    );
    assert_eq!(
        obligation(&store, &claim, &Pick::Criterion),
        (WorkObligationState::Satisfied, Some(unpinned.clone()))
    );
    let open_stale = operator_obligation(&store, &claim, &stale);
    assert_eq!(open_stale.state, WorkObligationState::Open);
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-unpinned",
        10,
        std::slice::from_ref(&unpinned),
    );
    let refused = complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &unpinned,
        "complete-unpinned",
        11,
    );
    assert!(refused_on(&refused, &open_stale), "{refused:?}");
    assert_eq!(
        operator_obligation(&store, &claim, &stale).state,
        WorkObligationState::Open,
        "an operator obligation is never waived as untested"
    );
    let pinned = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "pinned-suite",
        VerificationKind::Test,
        VerificationResult::Passed,
        12,
        basis("workspace-B", "B9", Some(10)),
    );
    let satisfied_stale = operator_obligation(&store, &claim, &stale);
    assert_eq!(satisfied_stale.state, WorkObligationState::Satisfied);
    assert!(matches!(
        satisfied_stale.resolution.map(|event| event.resolution),
        Some(WorkObligationResolution::Satisfied { evidence, .. }) if evidence == pinned
    ));
    assert_eq!(
        operator_obligation(&store, &claim, &earlier).state,
        WorkObligationState::Open,
        "no check in the root is credited for a change in workspace A"
    );
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-pinned",
        13,
        &[unpinned, pinned.clone()],
    );
    let seal = complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &pinned,
        "complete-pinned",
        14,
    )
    .expect("the pinned check completes the work");
    assert_eq!(seal.foreign_workspace_changes, vec![earlier.clone()]);
    assert!(matches!(
        operator_obligation(&store, &claim, &earlier)
            .resolution
            .map(|event| event.resolution),
        Some(WorkObligationResolution::Displaced { .. })
    ));
    let report = store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
}

/// A change workspace A reports while B is bound stays foreign under an
/// operator rule too: the pinned check in B does not satisfy it, and `done`
/// neither displaces nor waives it.
#[test]
fn an_operator_rule_keeps_a_foreign_change_under_a_bound_name_open() {
    let (mut store, work, claim, _) = named_work_in(pinned_store("pinned-suite"));
    let foreign = source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "foreign-under-9",
        5,
        Some(basis("workspace-A", "A5", Some(9))),
        None,
    );
    let pinned = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "pinned-suite",
        VerificationKind::Test,
        VerificationResult::Passed,
        6,
        basis("workspace-B", "B6", Some(9)),
    );
    let open = operator_obligation(&store, &claim, &foreign);
    assert_eq!(open.state, WorkObligationState::Open);
    assert_eq!(
        store
            .work_obligation_completion_actions(&[&open.obligation])
            .expect("operator guidance")[0],
        WorkObligationCompletionAction::WaiverOnly
    );
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-pinned",
        7,
        std::slice::from_ref(&pinned),
    );
    let refused = complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &pinned,
        "complete-pinned",
        8,
    );
    assert!(refused_on(&refused, &open), "{refused:?}");
    assert_eq!(
        operator_obligation(&store, &claim, &foreign).state,
        WorkObligationState::Open
    );
    let report = store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
}

/// Under an active named root, `done` still waives the stock rule's untested
/// changes that the root gives no other disposition: a change in the root's
/// own workspace and an unbound change, each with no later check, are
/// recorded as untested, and work with no bound criterion completes.
#[test]
fn done_waives_untested_changes_the_root_gives_no_other_disposition() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let mut request = root_request("project-a", "named-root-work", 1);
    request.acceptance = vec!["describe the change".into()];
    let work = store
        .create_work(&request, &DevelopmentNoopRedactor)
        .expect("work");
    let claim = claim(&mut store, &work, "runner", "named-root-claim", 2, 300);
    name_root(&mut store, &work, &claim, 9, 3);
    release(&mut store, &work, &claim, "release-runner", 4);
    let work = store.get_work_item(work.work_id).expect("item");
    let claim = super::claim(&mut store, &work, "runner", "reclaim-runner", 5, 300);
    name_root(&mut store, &work, &claim, 10, 6);
    let checked = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-in-B-10",
        VerificationKind::Test,
        VerificationResult::Passed,
        7,
        basis("workspace-B", "B7", Some(10)),
    );
    let in_root = source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "change-in-B",
        8,
        Some(basis("workspace-B", "B8", Some(10))),
        None,
    );
    let unbound = source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "stale-generation-in-D",
        9,
        Some(basis("workspace-D", "D9", Some(9))),
        None,
    );
    for change in [&in_root, &unbound] {
        assert_eq!(
            obligation(&store, &claim, &Pick::TriggeredBy(change)),
            (WorkObligationState::Open, None)
        );
        assert_eq!(
            stock_completion_action(&store, &claim, change),
            WorkObligationCompletionAction::DoneWaives
        );
    }
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-B-10",
        10,
        std::slice::from_ref(&checked),
    );
    let seal = complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &checked,
        "complete-B-10",
        11,
    )
    .expect("the untested changes are waived and the work completes");
    assert!(
        seal.foreign_workspace_changes.is_empty(),
        "{:?}",
        seal.foreign_workspace_changes
    );
    let records = store
        .work_run_obligations(claim.run_id)
        .expect("terminal obligations");
    for change in [&in_root, &unbound] {
        let resolution = records
            .iter()
            .find(|record| &record.obligation.triggering_observation == change)
            .and_then(|record| record.resolution.as_ref())
            .map(|event| event.resolution.clone());
        assert!(
            matches!(resolution, Some(WorkObligationResolution::Waived { .. })),
            "{change}: {resolution:?}"
        );
    }
    let report = store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
}
