use super::*;
use crate::domain::{
    AcceptanceBinding, ReviseWorkRequest, VerificationRequirement, WorkPlanningAuthority,
};

fn bound(criterion: usize, kind: VerificationKind) -> AcceptanceBinding {
    AcceptanceBinding {
        criterion,
        requirement: VerificationRequirement {
            check_kind: kind,
            check_fingerprint: None,
            required_environment: None,
        },
    }
}

/// A root whose first criterion requires host test verification.
fn bound_root(store: &mut SqliteStore, project: &str) -> WorkItem {
    let mut request = root_request(project, "create-bound-work", 1);
    request.acceptance = vec!["run tests".into(), "write docs".into()];
    request.acceptance_bindings = vec![bound(1, VerificationKind::Test)];
    store
        .create_work(&request, &DevelopmentNoopRedactor)
        .expect("create bound work")
}

fn revise_bound(
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    acceptance: Option<Vec<&str>>,
    bindings: Option<Vec<AcceptanceBinding>>,
    key: &str,
    second: i64,
) -> WorkItem {
    let patch = crate::domain::WorkRevisionPatch {
        acceptance: acceptance.map(|list| list.into_iter().map(str::to_owned).collect()),
        acceptance_bindings: bindings,
        ..crate::domain::WorkRevisionPatch::default()
    };
    store
        .revise_work(
            &ReviseWorkRequest {
                work_id: work.work_id,
                expected_revision: work.revision,
                patch,
                authority: WorkPlanningAuthority::Claim {
                    run_id: claim.run_id,
                    holder: claim.holder.clone(),
                    claim_id: claim.claim_id,
                    claim_fence: claim.fence,
                },
                actor: actor(&claim.holder.0),
                idempotency_key: key.into(),
                updated_at: at(second),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("revise bound work")
}

#[test]
fn a_bound_criterion_opens_an_obligation_and_completes_only_on_host_verification_of_its_kind() {
    let directory = crate::test_support::temp_home().expect("temporary directory");
    let mut store = SqliteStore::open(directory.path().join("engram.sqlite3")).expect("store");
    let work = bound_root(&mut store, "project-bound-criterion");
    let run_id = work.active_run_id.expect("active run");
    // Creation opened the obligation: the run owes a test verification for
    // criterion 1, triggered by the creation event on its own feed.
    let opened = store.work_run_obligations(run_id).expect("obligations");
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0].state, WorkObligationState::Open);
    assert_eq!(
        opened[0].obligation.rule.rule_id,
        "acceptance_criterion_requires_verification:1"
    );
    assert_eq!(
        opened[0].obligation.requirement.check_kind,
        VerificationKind::Test
    );
    assert_eq!(
        opened[0].obligation.trigger_position.feed,
        FeedId::RunExecution(run_id)
    );

    // A claim on the same run does not open it twice.
    let claim = claim(&mut store, &work, "runner", "claim-bound", 2, 300);
    assert_eq!(
        store
            .work_run_obligations(run_id)
            .expect("obligations")
            .len(),
        1
    );

    let generic = evidence(&mut store, &work, &claim, "runner", "generic-bound", 3);
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-bound-open",
        4,
        std::slice::from_ref(&generic),
    );
    let refused = complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &generic,
        "complete-bound-open",
        5,
    );
    let Err(StoreError::OpenWorkObligations { obligations, .. }) = refused else {
        panic!("a bound criterion without verification must refuse completion: {refused:?}");
    };
    assert_eq!(obligations[0].required_check, VerificationKind::Test);

    // A passing check of another kind satisfies nothing.
    let build = host_verification(
        &mut store,
        &work,
        &claim,
        "runner",
        "build-bound",
        VerificationKind::Build,
        VerificationResult::Passed,
        6,
    );
    assert_eq!(
        store.work_run_obligations(run_id).expect("obligations")[0].state,
        WorkObligationState::Open
    );

    // A passing test verification does, although the run observed no source
    // mutation at all: the source as it stands was verified.
    let verification = host_verification(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-bound",
        VerificationKind::Test,
        VerificationResult::Passed,
        7,
    );
    let terminal = store.work_run_obligations(run_id).expect("obligations");
    assert_eq!(terminal.len(), 1);
    assert_eq!(terminal[0].state, WorkObligationState::Satisfied);
    assert!(matches!(
        terminal[0].resolution.as_ref().map(|event| &event.resolution),
        Some(WorkObligationResolution::Satisfied { evidence, .. }) if evidence == &verification
    ));

    let all = store.work_run_evidence(run_id).expect("run evidence");
    assert!(all.contains(&build) && all.contains(&verification));
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-bound-verified",
        8,
        &all,
    );
    let mut request = completion_request(&work, &claim, "runner", &generic, "complete-bound", 9);
    request.evidence.push(verification.clone());
    let seal = store
        .complete_work(&request, &DevelopmentNoopRedactor)
        .expect("complete once the bound criterion is verified");
    // The seal binds the obligation, and the bound criterion cites the record
    // that satisfied it; the free-text criterion is as the author left it.
    assert_eq!(seal.obligations.len(), 1);
    assert_eq!(
        seal.obligations[0].obligation_id,
        terminal[0].obligation.obligation_id
    );
    assert!(seal.acceptance[0].evidence.contains(&verification));
    assert_eq!(seal.acceptance[1].evidence, vec![generic.clone()]);
    assert!(store.verify_all().expect("doctor").is_healthy());
}

#[test]
fn a_newer_failed_verification_contradicts_a_satisfied_bound_criterion() {
    let directory = crate::test_support::temp_home().expect("temporary directory");
    let mut store = SqliteStore::open(directory.path().join("engram.sqlite3")).expect("store");
    let work = bound_root(&mut store, "project-bound-contradiction");
    let run_id = work.active_run_id.expect("active run");
    let claim = claim(&mut store, &work, "runner", "claim-contradiction", 2, 300);
    let generic = evidence(
        &mut store,
        &work,
        &claim,
        "runner",
        "generic-contradiction",
        3,
    );
    let passed = host_verification(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-passed",
        VerificationKind::Test,
        VerificationResult::Passed,
        4,
    );
    assert_eq!(
        store.work_run_obligations(run_id).expect("obligations")[0].state,
        WorkObligationState::Satisfied
    );
    // The host then observes the same kind of check failing. The obligation
    // stays satisfied as a record, but completion holds the criterion to the
    // newest verification of its kind.
    let failed = host_verification(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-failed",
        VerificationKind::Test,
        VerificationResult::Failed,
        5,
    );
    let all = store.work_run_evidence(run_id).expect("run evidence");
    assert!(all.contains(&passed) && all.contains(&failed));
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-contradiction",
        6,
        &all,
    );
    let refused = complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &generic,
        "complete-contradicted",
        7,
    );
    let Err(StoreError::WorkCompletionRefused { reason, .. }) = refused else {
        panic!("a newer failed check must refuse completion: {refused:?}");
    };
    assert!(
        reason.contains("criterion 1 requires test verification"),
        "{reason}"
    );
    assert!(
        reason.contains("contradicted by newer verification evidence"),
        "{reason}"
    );
    assert!(reason.contains(failed.as_str()), "{reason}");
}

#[test]
fn revising_the_acceptance_waives_the_bindings_it_drops_and_opens_the_ones_it_adds() {
    let directory = crate::test_support::temp_home().expect("temporary directory");
    let mut store = SqliteStore::open(directory.path().join("engram.sqlite3")).expect("store");
    let work = bound_root(&mut store, "project-bound-revision");
    let run_id = work.active_run_id.expect("active run");
    let claim = claim(&mut store, &work, "runner", "claim-revision", 2, 300);

    // Replacing the acceptance list without restating the bindings drops
    // them: the obligation is waived in the revising actor's name.
    let revised = revise_bound(
        &mut store,
        &work,
        &claim,
        Some(vec!["run tests", "write docs", "zap lint"]),
        None,
        "revise-drop",
        3,
    );
    assert!(revised.acceptance_bindings.is_empty());
    let after_drop = store.work_run_obligations(run_id).expect("obligations");
    assert_eq!(after_drop.len(), 1);
    assert_eq!(after_drop[0].state, WorkObligationState::Waived);
    assert!(matches!(
        after_drop[0].resolution.as_ref().map(|event| &event.resolution),
        Some(WorkObligationResolution::Waived { waived_by, reason })
            if waived_by == "runner" && reason.contains("revision 2") && reason.contains("criterion 1")
    ));

    // Binding two criteria opens one obligation each, from the revision that
    // authored them, on the same run.
    let rebound = revise_bound(
        &mut store,
        &revised,
        &claim,
        None,
        Some(vec![
            bound(3, VerificationKind::Lint),
            bound(1, VerificationKind::Test),
        ]),
        "revise-rebind",
        4,
    );
    assert_eq!(
        rebound
            .acceptance_bindings
            .iter()
            .map(|binding| binding.criterion)
            .collect::<Vec<_>>(),
        vec![1, 3],
        "bindings are kept in position order"
    );
    let after_rebind = store.work_run_obligations(run_id).expect("obligations");
    let open = after_rebind
        .iter()
        .filter(|record| record.state == WorkObligationState::Open)
        .map(|record| {
            (
                record.obligation.rule.rule_id.clone(),
                record.obligation.requirement.check_kind,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(after_rebind.len(), 3);
    assert!(open.contains(&(
        "acceptance_criterion_requires_verification:1".into(),
        VerificationKind::Test
    )));
    assert!(open.contains(&(
        "acceptance_criterion_requires_verification:3".into(),
        VerificationKind::Lint
    )));
    assert!(store.verify_all().expect("doctor").is_healthy());
}

#[test]
fn bindings_are_read_from_the_shell_form_and_admitted_against_the_list() {
    let parsed = AcceptanceBinding::parse(" 2 = test ").expect("plain binding");
    assert_eq!(parsed.criterion, 2);
    assert_eq!(parsed.requirement.check_kind, VerificationKind::Test);
    assert_eq!(parsed.requirement.check_fingerprint, None);
    let fingerprint = check_fingerprint("pinned");
    let pinned =
        AcceptanceBinding::parse(&format!("1=build:{}", fingerprint.as_str())).expect("pinned");
    assert_eq!(pinned.requirement.check_kind, VerificationKind::Build);
    assert_eq!(pinned.requirement.check_fingerprint, Some(fingerprint));
    for text in ["test", "0=test", "x=test", "1=magic", "1=test:not-an-id"] {
        assert!(
            AcceptanceBinding::parse(text).is_err(),
            "{text:?} must be refused"
        );
    }
    // Shape cannot tell a command fingerprint from a record id, so any well
    // formed value parses; storage refuses a stored record's id on admission.
    assert!(AcceptanceBinding::parse(&format!("1=test:{}", "a".repeat(32))).is_ok());
    let sorted = crate::domain::normalize_acceptance_bindings(
        3,
        &[
            bound(3, VerificationKind::Lint),
            bound(1, VerificationKind::Test),
        ],
    )
    .expect("in range");
    assert_eq!(
        sorted
            .iter()
            .map(|binding| binding.criterion)
            .collect::<Vec<_>>(),
        vec![1, 3]
    );
    let twice = crate::domain::normalize_acceptance_bindings(
        3,
        &[
            bound(2, VerificationKind::Lint),
            bound(2, VerificationKind::Test),
        ],
    )
    .expect_err("bound twice");
    assert!(twice.contains("criterion 2 is bound twice"), "{twice}");
    let outside =
        crate::domain::normalize_acceptance_bindings(1, &[bound(2, VerificationKind::Test)])
            .expect_err("outside the list");
    assert!(outside.contains("names criterion 2"), "{outside}");
}

#[test]
fn a_bound_child_opens_its_obligations_when_the_parent_is_decomposed() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(
            &root_request("project-bound-child", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let mut first = child("first", ChildRequirement::Required, "First");
    first.acceptance = vec!["run tests".into(), "write docs".into()];
    first.acceptance_bindings = vec![
        bound(1, VerificationKind::Test),
        bound(2, VerificationKind::Review),
    ];
    let plain = child("plain", ChildRequirement::Required, "Plain");
    let decomposition = store
        .decompose_work(
            &DecomposeWorkRequest {
                parent_id: root.work_id,
                expected_parent_revision: root.revision,
                children: vec![first, plain],
                prerequisites: Vec::new(),
                authority: delegated(&root.project_id.0, "planner"),
                actor: actor("planner"),
                idempotency_key: "decompose-bound".into(),
                created_at: at(1),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("decompose");
    let run_of = |title: &str| {
        decomposition
            .children
            .iter()
            .find(|child| child.title == title)
            .and_then(|child| child.active_run_id)
            .expect("child run")
    };
    let opened = store
        .work_run_obligations(run_of("First"))
        .expect("obligations");
    assert!(
        opened
            .iter()
            .all(|record| record.state == WorkObligationState::Open)
    );
    let mut owed = opened
        .iter()
        .map(|record| {
            format!(
                "{} {:?}",
                record.obligation.rule.rule_id, record.obligation.requirement.check_kind
            )
        })
        .collect::<Vec<_>>();
    owed.sort();
    assert_eq!(
        owed,
        vec![
            "acceptance_criterion_requires_verification:1 Test",
            "acceptance_criterion_requires_verification:2 Review",
        ]
    );
    assert!(
        store
            .work_run_obligations(run_of("Plain"))
            .expect("obligations")
            .is_empty()
    );
    assert!(store.verify_all().expect("doctor").is_healthy());
}

#[test]
fn a_pinned_binding_is_satisfied_only_by_the_check_it_pins() {
    let directory = crate::test_support::temp_home().expect("temporary directory");
    let mut store = SqliteStore::open(directory.path().join("engram.sqlite3")).expect("store");
    let mut request = root_request("project-pinned-criterion", "create-pinned-work", 1);
    request.acceptance = vec!["run tests".into()];
    let mut pinned = bound(1, VerificationKind::Test);
    pinned.requirement.check_fingerprint = Some(check_fingerprint("pinned-test"));
    request.acceptance_bindings = vec![pinned];
    let work = store
        .create_work(&request, &DevelopmentNoopRedactor)
        .expect("create pinned work");
    let run_id = work.active_run_id.expect("active run");
    let claim = claim(&mut store, &work, "runner", "claim-pinned", 2, 300);

    // Another passing test is not the check the binding names.
    host_verification(
        &mut store,
        &work,
        &claim,
        "runner",
        "other-test",
        VerificationKind::Test,
        VerificationResult::Passed,
        3,
    );
    assert_eq!(
        store.work_run_obligations(run_id).expect("obligations")[0].state,
        WorkObligationState::Open
    );

    let exact = host_verification(
        &mut store,
        &work,
        &claim,
        "runner",
        "pinned-test",
        VerificationKind::Test,
        VerificationResult::Passed,
        4,
    );
    let terminal = store.work_run_obligations(run_id).expect("obligations");
    assert_eq!(terminal.len(), 1);
    assert!(matches!(
        terminal[0].resolution.as_ref().map(|event| &event.resolution),
        Some(WorkObligationResolution::Satisfied { evidence, .. }) if evidence == &exact
    ));
    assert!(store.verify_all().expect("doctor").is_healthy());
}

#[test]
fn rewriting_a_bound_criterion_owes_its_verification_again() {
    let directory = crate::test_support::temp_home().expect("temporary directory");
    let mut store = SqliteStore::open(directory.path().join("engram.sqlite3")).expect("store");
    let work = bound_root(&mut store, "project-rewritten-criterion");
    let run_id = work.active_run_id.expect("active run");
    let claim = claim(&mut store, &work, "runner", "claim-rewritten", 2, 300);
    host_verification(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-before-rewrite",
        VerificationKind::Test,
        VerificationResult::Passed,
        3,
    );
    assert_eq!(
        store.work_run_obligations(run_id).expect("obligations")[0].state,
        WorkObligationState::Satisfied
    );

    // The sentence at position 1 changes and the binding is restated as it
    // was: the pass recorded for the old sentence does not carry over.
    let revised = revise_bound(
        &mut store,
        &work,
        &claim,
        Some(vec!["run all tests twice", "write docs"]),
        Some(vec![bound(1, VerificationKind::Test)]),
        "rewrite-bound-criterion",
        4,
    );
    let after = store.work_run_obligations(run_id).expect("obligations");
    assert_eq!(after.len(), 2);
    let reopened = after
        .iter()
        .find(|record| record.state == WorkObligationState::Open)
        .expect("the rewritten criterion owes its verification again");
    assert_eq!(reopened.obligation.work_revision, revised.revision);
    assert_eq!(
        reopened.obligation.rule.rule_id,
        "acceptance_criterion_requires_verification:1"
    );

    let claim = store
        .current_work_claim(revised.work_id)
        .expect("claim")
        .expect("live claim");
    let fresh = host_verification(
        &mut store,
        &revised,
        &claim,
        "runner",
        "test-after-rewrite",
        VerificationKind::Test,
        VerificationResult::Passed,
        5,
    );
    let terminal = store.work_run_obligations(run_id).expect("obligations");
    assert!(
        terminal
            .iter()
            .all(|record| { record.state == WorkObligationState::Satisfied })
    );
    assert!(terminal.iter().any(|record| matches!(
        record.resolution.as_ref().map(|event| &event.resolution),
        Some(WorkObligationResolution::Satisfied { evidence, .. }) if evidence == &fresh
    )));
    assert!(store.verify_all().expect("doctor").is_healthy());
}

#[test]
fn a_verification_older_than_the_latest_source_change_no_longer_carries_its_criterion() {
    let directory = crate::test_support::temp_home().expect("temporary directory");
    let mut store = SqliteStore::open(directory.path().join("engram.sqlite3")).expect("store");
    let mut request = root_request("project-stale-verification", "create-stale-work", 1);
    request.acceptance = vec!["build is clean".into(), "write docs".into()];
    request.acceptance_bindings = vec![bound(1, VerificationKind::Build)];
    let work = store
        .create_work(&request, &DevelopmentNoopRedactor)
        .expect("create bound work");
    let run_id = work.active_run_id.expect("active run");
    let claim = claim(&mut store, &work, "runner", "claim-stale", 2, 300);
    let build = host_verification(
        &mut store,
        &work,
        &claim,
        "runner",
        "build-before",
        VerificationKind::Build,
        VerificationResult::Passed,
        3,
    );

    // The source then changes. The builtin rule asks for a test, which is
    // given; nothing asks for the build again, yet the build on record
    // certifies code that has since moved.
    source_mutation(
        &mut store,
        &work,
        &claim,
        "runner",
        "change",
        4,
        Some("revision-after-change"),
    );
    host_verification_of(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-after-change",
        VerificationKind::Test,
        VerificationResult::Passed,
        5,
        "revision-after-change",
    );
    assert!(
        store
            .work_run_obligations(run_id)
            .expect("obligations")
            .iter()
            .all(|record| record.state == WorkObligationState::Satisfied)
    );
    let generic = evidence(&mut store, &work, &claim, "runner", "generic-stale", 6);
    let all = store.work_run_evidence(run_id).expect("run evidence");
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-stale",
        7,
        &all,
    );
    let mut stale = completion_request(&work, &claim, "runner", &generic, "complete-stale", 8);
    stale.evidence.push(build.clone());
    let refused = store.complete_work(&stale, &DevelopmentNoopRedactor);
    let Err(StoreError::WorkCompletionRefused { reason, .. }) = refused else {
        panic!("a build older than the latest source change must not seal: {refused:?}");
    };
    assert!(
        reason.contains("criterion 1 requires build verification")
            && reason.contains("does not verify the run's latest source change"),
        "{reason}"
    );

    // Recording order proves nothing about what was checked: a build recorded
    // after the change, but of the source as it stood before it, is as stale.
    let old_source = host_verification_of(
        &mut store,
        &work,
        &claim,
        "runner",
        "build-of-the-old-source",
        VerificationKind::Build,
        VerificationResult::Passed,
        9,
        "revision-as-it-stands",
    );
    let all = store.work_run_evidence(run_id).expect("run evidence");
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-old-source",
        10,
        &all,
    );
    let mut rerecorded =
        completion_request(&work, &claim, "runner", &generic, "complete-old-source", 11);
    rerecorded.evidence.push(old_source);
    let refused = store.complete_work(&rerecorded, &DevelopmentNoopRedactor);
    let Err(StoreError::WorkCompletionRefused { reason, .. }) = refused else {
        panic!("a build of the older source must not seal: {refused:?}");
    };
    assert!(
        reason.contains("does not verify the run's latest source change")
            && reason.contains("stale_source_revision"),
        "{reason}"
    );

    let rebuilt = host_verification_of(
        &mut store,
        &work,
        &claim,
        "runner",
        "build-after",
        VerificationKind::Build,
        VerificationResult::Passed,
        12,
        "revision-after-change",
    );
    let all = store.work_run_evidence(run_id).expect("run evidence");
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-rebuilt",
        13,
        &all,
    );
    let mut fresh = completion_request(&work, &claim, "runner", &generic, "complete-rebuilt", 14);
    fresh.evidence.push(rebuilt.clone());
    let seal = store
        .complete_work(&fresh, &DevelopmentNoopRedactor)
        .expect("a build of the changed source carries the criterion");
    // The criterion cites the verification the freshness rule accepted, not
    // the older record that first satisfied the obligation.
    assert!(seal.acceptance[0].evidence.contains(&rebuilt));
    assert!(!seal.acceptance[0].evidence.contains(&build));
    assert!(store.verify_all().expect("doctor").is_healthy());
}

#[test]
fn a_binding_dropped_and_added_again_owes_its_verification_from_the_new_authoring() {
    let directory = crate::test_support::temp_home().expect("temporary directory");
    let mut store = SqliteStore::open(directory.path().join("engram.sqlite3")).expect("store");
    let work = bound_root(&mut store, "project-rebound-criterion");
    let run_id = work.active_run_id.expect("active run");
    let claim = claim(&mut store, &work, "runner", "claim-rebound", 2, 300);
    host_verification(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-first-authoring",
        VerificationKind::Test,
        VerificationResult::Passed,
        3,
    );

    // Rewrite the sentence under the binding, drop the binding, add it back.
    let rewritten = revise_bound(
        &mut store,
        &work,
        &claim,
        Some(vec!["run the whole suite", "write docs"]),
        Some(vec![bound(1, VerificationKind::Test)]),
        "rebound-rewrite",
        4,
    );
    let dropped = revise_bound(
        &mut store,
        &rewritten,
        &claim,
        None,
        Some(Vec::new()),
        "rebound-drop",
        5,
    );
    let rebound = revise_bound(
        &mut store,
        &dropped,
        &claim,
        None,
        Some(vec![bound(1, VerificationKind::Test)]),
        "rebound-add",
        6,
    );

    // The first authoring's pass and the rewrite's waived obligation are
    // history; the binding authored now owes its own verification.
    let records = store.work_run_obligations(run_id).expect("obligations");
    let states = records
        .iter()
        .map(|record| (record.obligation.work_revision, record.state))
        .collect::<Vec<_>>();
    assert_eq!(records.len(), 3, "{states:?}");
    let open = records
        .iter()
        .filter(|record| record.state == WorkObligationState::Open)
        .collect::<Vec<_>>();
    assert_eq!(open.len(), 1, "{states:?}");
    assert_eq!(open[0].obligation.work_revision, rebound.revision);

    let claim = store
        .current_work_claim(rebound.work_id)
        .expect("claim")
        .expect("live claim");
    let fresh = host_verification(
        &mut store,
        &rebound,
        &claim,
        "runner",
        "test-new-authoring",
        VerificationKind::Test,
        VerificationResult::Passed,
        7,
    );
    let terminal = store.work_run_obligations(run_id).expect("obligations");
    assert!(terminal.iter().any(|record| {
        record.obligation.work_revision == rebound.revision
            && matches!(
                record.resolution.as_ref().map(|event| &event.resolution),
                Some(WorkObligationResolution::Satisfied { evidence, .. }) if evidence == &fresh
            )
    }));
    assert!(store.verify_all().expect("doctor").is_healthy());
}

#[test]
fn rewriting_a_bound_criterion_whose_obligation_is_open_waives_it_by_name() {
    let directory = crate::test_support::temp_home().expect("temporary directory");
    let mut store = SqliteStore::open(directory.path().join("engram.sqlite3")).expect("store");
    let work = bound_root(&mut store, "project-open-rewrite");
    let run_id = work.active_run_id.expect("active run");
    let claim = claim(&mut store, &work, "runner", "claim-open-rewrite", 2, 300);
    let revised = revise_bound(
        &mut store,
        &work,
        &claim,
        Some(vec!["run the whole suite", "write docs"]),
        Some(vec![bound(1, VerificationKind::Test)]),
        "open-rewrite",
        3,
    );
    let records = store.work_run_obligations(run_id).expect("obligations");
    assert_eq!(records.len(), 2);
    let waived = records
        .iter()
        .find(|record| record.state == WorkObligationState::Waived)
        .expect("the old sentence's open obligation is waived");
    assert_eq!(waived.obligation.work_revision, work.revision);
    let Some(WorkObligationResolution::Waived { waived_by, reason }) =
        waived.resolution.as_ref().map(|event| &event.resolution)
    else {
        panic!("expected an attributed waiver");
    };
    assert_eq!(waived_by, "runner");
    assert!(
        reason.contains("criterion 1 was rewritten, and its verification is owed again"),
        "{reason}"
    );
    let open = records
        .iter()
        .filter(|record| record.state == WorkObligationState::Open)
        .collect::<Vec<_>>();
    assert_eq!(open.len(), 1);
    assert_eq!(open[0].obligation.work_revision, revised.revision);
    assert!(store.verify_all().expect("doctor").is_healthy());
}

#[test]
fn a_pin_that_is_a_stored_record_id_is_refused_where_it_is_authored() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    store
        .create_work(
            &root_request("project-record-id-pin", "first-root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("a first root, so the store holds records");
    let stored: String = store
        .connection
        .query_row("SELECT object_hash FROM objects LIMIT 1", [], |row| {
            row.get(0)
        })
        .expect("a stored record id");
    let mut request = root_request("project-record-id-pin", "pinned-root", 1);
    request.acceptance = vec!["run tests".into()];
    let mut pinned = bound(1, VerificationKind::Test);
    pinned.requirement.check_fingerprint =
        Some(ObjectHash::from_stored(stored).expect("stored id shape"));
    request.acceptance_bindings = vec![pinned];
    let refused = store.create_work(&request, &DevelopmentNoopRedactor);
    let Err(StoreError::InvalidWork(reason)) = refused else {
        panic!("a stored record's id must not be admitted as a pin: {refused:?}");
    };
    assert!(
        reason.contains("criterion 1 pins") && reason.contains("names a stored record"),
        "{reason}"
    );
}

#[test]
fn a_revision_cannot_pin_a_stored_record_with_or_without_a_new_list() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let work = bound_root(&mut store, "project-record-id-revision");
    let claim = claim(
        &mut store,
        &work,
        "runner",
        "claim-record-id-revision",
        2,
        300,
    );
    let stored: String = store
        .connection
        .query_row("SELECT object_hash FROM objects LIMIT 1", [], |row| {
            row.get(0)
        })
        .expect("a stored record id");
    let mut pinned = bound(1, VerificationKind::Test);
    pinned.requirement.check_fingerprint =
        Some(ObjectHash::from_stored(stored).expect("stored id shape"));
    // Bindings authored with a replacement list, and bindings revised alone
    // against the stored list, reach the guard by different routes.
    for (acceptance, key) in [
        (
            Some(vec!["run tests".to_owned(), "write docs".to_owned()]),
            "pin-with-a-list",
        ),
        (None, "pin-alone"),
    ] {
        let refused = store.revise_work(
            &ReviseWorkRequest {
                work_id: work.work_id,
                expected_revision: work.revision,
                patch: crate::domain::WorkRevisionPatch {
                    acceptance,
                    acceptance_bindings: Some(vec![pinned.clone()]),
                    ..crate::domain::WorkRevisionPatch::default()
                },
                authority: WorkPlanningAuthority::Claim {
                    run_id: claim.run_id,
                    holder: claim.holder.clone(),
                    claim_id: claim.claim_id,
                    claim_fence: claim.fence,
                },
                actor: actor("runner"),
                idempotency_key: key.into(),
                updated_at: at(3),
            },
            &DevelopmentNoopRedactor,
        );
        let Err(StoreError::InvalidWork(reason)) = refused else {
            panic!("{key}: a stored record must not be admitted as a pin: {refused:?}");
        };
        assert!(reason.contains("names a stored record"), "{key}: {reason}");
    }
    assert_eq!(
        store.get_work_item(work.work_id).expect("item").revision,
        work.revision,
        "a refused revision changes nothing"
    );
}

#[test]
fn a_source_change_recorded_without_a_revision_is_judged_by_recording_order() {
    use crate::domain::{ExecutionObservation, VerificationEvidenceMismatch};
    use crate::storage::work::feeds::{load_typed_work_object, run_feed_position_for_object_on};
    let directory = crate::test_support::temp_home().expect("temporary directory");
    let mut store = SqliteStore::open(directory.path().join("engram.sqlite3")).expect("store");
    let mut request = root_request("project-bare-change", "create-bare-change-work", 1);
    request.acceptance = vec!["build is clean".into()];
    request.acceptance_bindings = vec![bound(1, VerificationKind::Build)];
    let work = store
        .create_work(&request, &DevelopmentNoopRedactor)
        .expect("create bound work");
    let claim = claim(&mut store, &work, "runner", "claim-bare-change", 2, 300);
    let before = host_verification(
        &mut store,
        &work,
        &claim,
        "runner",
        "build-before-bare-change",
        VerificationKind::Build,
        VerificationResult::Passed,
        3,
    );
    // A host that observed no source basis records the change with neither a
    // revision nor a time. The matching rule has nothing to compare then, and
    // would refuse every verification forever.
    let change = source_mutation(&mut store, &work, &claim, "runner", "bare", 4, None);
    let after = host_verification(
        &mut store,
        &work,
        &claim,
        "runner",
        "build-after-bare-change",
        VerificationKind::Build,
        VerificationResult::Passed,
        5,
    );

    let position = |hash: &ObjectHash| {
        run_feed_position_for_object_on(&store.connection, claim.run_id, hash)
            .expect("run-feed position")
            .position
    };
    let mutation: ExecutionObservation =
        load_typed_work_object(&store.connection, &change, "execution_observation")
            .expect("the recorded change");
    assert!(mutation.source_basis.is_none() && mutation.observed_at.is_none());
    let judge = |hash: &ObjectHash| {
        let evidence: VerificationEvidence =
            load_typed_work_object(&store.connection, hash, "verification_evidence")
                .expect("verification evidence");
        let producer: ExecutionObservation = load_typed_work_object(
            &store.connection,
            &evidence.producer_observation,
            "execution_observation",
        )
        .expect("producer observation");
        super::super::binding_freshness_mismatch(
            (&mutation, position(&change)),
            (&evidence, position(hash)),
            &producer,
            &bound(1, VerificationKind::Build).requirement,
        )
    };
    assert_eq!(
        judge(&before),
        Some(VerificationEvidenceMismatch::NotAfterMutation)
    );
    assert_eq!(judge(&after), None);
}

#[test]
fn a_binding_follows_its_criterion_into_the_stored_order() {
    let directory = crate::test_support::temp_home().expect("temporary directory");
    let mut store = SqliteStore::open(directory.path().join("engram.sqlite3")).expect("store");
    // The list is stored sorted, as `show` numbers it; the binding was
    // authored against the list as typed and lands on the same criterion.
    let mut request = root_request("project-bound-order", "create-reordered-work", 1);
    request.acceptance = vec!["zeta: tests pass".into(), "alpha: docs updated".into()];
    request.acceptance_bindings = vec![bound(1, VerificationKind::Test)];
    let work = store
        .create_work(&request, &DevelopmentNoopRedactor)
        .expect("create reordered work");
    assert_eq!(
        work.acceptance,
        vec!["alpha: docs updated", "zeta: tests pass"]
    );
    assert_eq!(work.acceptance_bindings.len(), 1);
    assert_eq!(work.acceptance_bindings[0].criterion, 2);
    let opened = store
        .work_run_obligations(work.active_run_id.expect("active run"))
        .expect("obligations");
    assert_eq!(
        opened[0].obligation.rule.rule_id,
        "acceptance_criterion_requires_verification:2"
    );
    // A position past the list as typed refuses before any effect.
    request.idempotency_key = "create-overbound-work".into();
    request.acceptance_bindings = vec![bound(3, VerificationKind::Test)];
    let refused = store
        .create_work(&request, &DevelopmentNoopRedactor)
        .expect_err("a binding past the list");
    assert!(
        matches!(&refused, StoreError::InvalidWork(reason) if reason.contains("names criterion 3")),
        "{refused:?}"
    );
}
