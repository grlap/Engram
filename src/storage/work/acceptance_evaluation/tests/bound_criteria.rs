//! Coverage for criteria bound to typed verification requirements: exact
//! citation sealing, check-fingerprint pinning, and verification-kind
//! matching for acceptance evaluation.

use super::*;

// An evaluation's citations are sealed as recorded. The obligation was first
// satisfied by one check; the evaluator cites a later recheck. Completion must
// not add the earlier record to the evaluated criterion, or the seal would no
// longer derive from the evaluation it binds.
#[test]
fn an_evaluated_bound_criterion_seals_with_exactly_the_citations_it_was_judged_on() {
    let mut fixture = fixture("project-bound-evaluated-completion");
    let claim = fixture.claim.clone();
    let work = revise(
        &mut fixture.store,
        &fixture.work,
        &claim,
        WorkRevisionPatch {
            acceptance: Some(vec!["run tests".into(), "write docs".into()]),
            acceptance_bindings: Some(vec![crate::domain::AcceptanceBinding {
                criterion: 1,
                requirement: crate::domain::VerificationRequirement {
                    check_kind: VerificationKind::Test,
                    check_fingerprint: None,
                    required_environment: None,
                },
            }]),
            ..empty_patch()
        },
        "bind-for-completion",
        5,
    )
    .expect("bind the first criterion");
    enable(
        &mut fixture.store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable-bound-completion",
        6,
    );
    let generic = fixture.evidence.clone();
    let first = host_verification(
        &mut fixture.store,
        &work,
        &claim,
        "runner",
        "first-test",
        VerificationKind::Test,
        VerificationResult::Passed,
        7,
    );
    let recheck = host_verification(
        &mut fixture.store,
        &work,
        &claim,
        "runner",
        "recheck-test",
        VerificationKind::Test,
        VerificationResult::Passed,
        8,
    );
    let through = cut(&fixture.store, &work);
    record(
        &mut fixture.store,
        &request(
            &work,
            through,
            "runner",
            Mode::SameSession,
            vec![
                verdict(
                    1,
                    AcceptanceVerdict::Pass,
                    AcceptanceBasis::Observed,
                    std::slice::from_ref(&recheck),
                ),
                verdict(
                    2,
                    AcceptanceVerdict::Pass,
                    AcceptanceBasis::Judgment,
                    std::slice::from_ref(&generic),
                ),
            ],
            9,
        ),
    )
    .expect("the evaluation cites the recheck");

    // Ordinary completion carries every run evidence record, the first check
    // included.
    let all = fixture
        .store
        .work_run_evidence(claim.run_id)
        .expect("run evidence");
    assert!(all.contains(&first) && all.contains(&recheck));
    let seal = checkpoint_then_complete(
        &mut fixture.store,
        &work,
        &claim,
        "runner",
        &all,
        false,
        None,
        "complete-bound-evaluated",
        10,
    )
    .expect("an evaluated completion after a recheck seals");
    assert!(seal.acceptance[0].evidence.contains(&recheck));
    assert!(!seal.acceptance[0].evidence.contains(&first));
}

// A binding that pins one check is met only by verification of that check:
// another passing check of the same kind is not the evidence it names.
#[test]
fn a_pinned_bound_criterion_passes_only_on_the_check_it_pins() {
    let mut fixture = fixture("project-pinned-evaluation");
    let claim = fixture.claim.clone();
    let work = revise(
        &mut fixture.store,
        &fixture.work,
        &claim,
        WorkRevisionPatch {
            acceptance: Some(vec!["run tests".into(), "write docs".into()]),
            acceptance_bindings: Some(vec![crate::domain::AcceptanceBinding {
                criterion: 1,
                requirement: crate::domain::VerificationRequirement {
                    check_kind: VerificationKind::Test,
                    check_fingerprint: Some(check_fingerprint("pinned-check")),
                    required_environment: None,
                },
            }]),
            ..empty_patch()
        },
        "pin-criterion",
        5,
    )
    .expect("pin the first criterion");
    enable(
        &mut fixture.store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable-pinned-evaluation",
        6,
    );
    let generic = fixture.evidence.clone();

    let other = host_verification(
        &mut fixture.store,
        &work,
        &claim,
        "runner",
        "other-check",
        VerificationKind::Test,
        VerificationResult::Passed,
        7,
    );
    let through = cut(&fixture.store, &work);
    let wrong_check = refusal(record(
        &mut fixture.store,
        &request(
            &work,
            through,
            "runner",
            Mode::SameSession,
            vec![
                verdict(
                    1,
                    AcceptanceVerdict::Pass,
                    AcceptanceBasis::Observed,
                    std::slice::from_ref(&other),
                ),
                verdict(
                    2,
                    AcceptanceVerdict::Pass,
                    AcceptanceBasis::Judgment,
                    std::slice::from_ref(&generic),
                ),
            ],
            8,
        ),
    ));
    assert!(
        wrong_check.contains("is not passed host-minted verification evidence of that kind"),
        "{wrong_check}"
    );

    let exact = host_verification(
        &mut fixture.store,
        &work,
        &claim,
        "runner",
        "pinned-check",
        VerificationKind::Test,
        VerificationResult::Passed,
        9,
    );
    let through = cut(&fixture.store, &work);
    record(
        &mut fixture.store,
        &request(
            &work,
            through,
            "runner",
            Mode::SameSession,
            vec![
                verdict(
                    1,
                    AcceptanceVerdict::Pass,
                    AcceptanceBasis::Observed,
                    std::slice::from_ref(&exact),
                ),
                verdict(
                    2,
                    AcceptanceVerdict::Pass,
                    AcceptanceBasis::Judgment,
                    std::slice::from_ref(&generic),
                ),
            ],
            10,
        ),
    )
    .expect("the pinned check passes the bound criterion");
}

// A criterion bound to typed verification passes only on an observed basis
// citing host-minted verification of that kind with a passed result; judgment
// and an asserted gate are refused for it and stay available to the free-text
// criterion beside it.
#[test]
fn a_bound_criterion_passes_only_on_observed_verification_of_its_kind() {
    let mut fixture = fixture("project-bound-evaluation");
    let claim = fixture.claim.clone();
    let work = revise(
        &mut fixture.store,
        &fixture.work,
        &claim,
        WorkRevisionPatch {
            acceptance: Some(vec!["run tests".into(), "write docs".into()]),
            acceptance_bindings: Some(vec![crate::domain::AcceptanceBinding {
                criterion: 1,
                requirement: crate::domain::VerificationRequirement {
                    check_kind: VerificationKind::Test,
                    check_fingerprint: None,
                    required_environment: None,
                },
            }]),
            ..empty_patch()
        },
        "bind-criterion",
        5,
    )
    .expect("bind the first criterion");
    enable(
        &mut fixture.store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable-bound-evaluation",
        6,
    );
    let generic = fixture.evidence.clone();
    let gate = gate(&mut fixture.store, &work, &claim, "runner", "tests", &[], 7);

    let through = cut(&fixture.store, &work);
    let judgment = refusal(record(
        &mut fixture.store,
        &request(
            &work,
            through,
            "runner",
            Mode::SameSession,
            vec![
                verdict(
                    1,
                    AcceptanceVerdict::Pass,
                    AcceptanceBasis::Judgment,
                    std::slice::from_ref(&generic),
                ),
                verdict(
                    2,
                    AcceptanceVerdict::Pass,
                    AcceptanceBasis::Judgment,
                    std::slice::from_ref(&generic),
                ),
            ],
            8,
        ),
    ));
    assert!(
        judgment.contains("criterion 1 is bound to test verification"),
        "{judgment}"
    );

    let through = cut(&fixture.store, &work);
    let asserted = refusal(record(
        &mut fixture.store,
        &request(
            &work,
            through,
            "runner",
            Mode::SameSession,
            vec![
                verdict(
                    1,
                    AcceptanceVerdict::Pass,
                    AcceptanceBasis::Asserted,
                    std::slice::from_ref(&gate),
                ),
                verdict(
                    2,
                    AcceptanceVerdict::Pass,
                    AcceptanceBasis::Judgment,
                    std::slice::from_ref(&generic),
                ),
            ],
            9,
        ),
    ));
    assert!(
        asserted.contains("criterion 1 is bound to test verification"),
        "{asserted}"
    );

    let build = host_verification(
        &mut fixture.store,
        &work,
        &claim,
        "runner",
        "build-check",
        VerificationKind::Build,
        VerificationResult::Passed,
        10,
    );
    let through = cut(&fixture.store, &work);
    let wrong_kind = refusal(record(
        &mut fixture.store,
        &request(
            &work,
            through,
            "runner",
            Mode::SameSession,
            vec![
                verdict(
                    1,
                    AcceptanceVerdict::Pass,
                    AcceptanceBasis::Observed,
                    std::slice::from_ref(&build),
                ),
                verdict(
                    2,
                    AcceptanceVerdict::Pass,
                    AcceptanceBasis::Judgment,
                    std::slice::from_ref(&generic),
                ),
            ],
            11,
        ),
    ));
    assert!(
        wrong_kind.contains("is not passed host-minted verification evidence of that kind"),
        "{wrong_kind}"
    );

    let test = host_verification(
        &mut fixture.store,
        &work,
        &claim,
        "runner",
        "test-check",
        VerificationKind::Test,
        VerificationResult::Passed,
        12,
    );
    let through = cut(&fixture.store, &work);
    let accepted = record(
        &mut fixture.store,
        &request(
            &work,
            through,
            "runner",
            Mode::SameSession,
            vec![
                verdict(
                    1,
                    AcceptanceVerdict::Pass,
                    AcceptanceBasis::Observed,
                    std::slice::from_ref(&test),
                ),
                verdict(
                    2,
                    AcceptanceVerdict::Pass,
                    AcceptanceBasis::Judgment,
                    &[generic],
                ),
            ],
            13,
        ),
    )
    .expect("an observed test verification passes the bound criterion");
    assert!(accepted.record.all_pass());
    assert_eq!(accepted.record.verdicts[0].evidence, vec![test]);
}
