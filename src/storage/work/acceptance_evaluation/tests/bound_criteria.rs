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

/// The citation context of an admission refusal, with its reason.
fn citation_refusal(
    result: Result<AcceptanceEvaluationReceipt, StoreError>,
) -> (String, crate::domain::CitationAdmissionCause) {
    match result {
        Err(StoreError::AcceptanceEvaluationAdmissionRefused { reason, cause, .. }) => match *cause
        {
            crate::domain::AcceptanceEvaluationAdmissionCause::Citation(context) => {
                (reason, *context)
            }
            other => panic!("expected a citation cause, got {other:?}"),
        },
        other => panic!("expected an admission refusal, got {other:?}"),
    }
}

// A refusal names the deciding fault. A pass on a bound criterion with the
// wrong basis names no citation, even when its first citation is a valid
// passed verification of the bound kind; an observed pass that also cites a
// note names the note, in whichever order they were submitted; and the
// remedy states the admissible pass.
#[test]
fn a_bound_criterion_refusal_never_names_a_valid_verification_as_its_fault() {
    let mut fixture = fixture("project-bound-attribution");
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
        "enable-bound-attribution",
        6,
    );
    let note = fixture.evidence.clone();
    let test = host_verification(
        &mut fixture.store,
        &work,
        &claim,
        "runner",
        "test-check",
        VerificationKind::Test,
        VerificationResult::Passed,
        7,
    );
    let docs = verdict(
        2,
        AcceptanceVerdict::Pass,
        AcceptanceBasis::Judgment,
        std::slice::from_ref(&note),
    );
    let submit = |store: &mut SqliteStore, first: CriterionVerdictInput, second: i64| {
        let through = cut(store, &work);
        record(
            store,
            &request(
                &work,
                through,
                "runner",
                Mode::SameSession,
                vec![first, docs.clone()],
                second,
            ),
        )
    };
    let admissible = "a pass on a criterion bound to a check uses basis observed, and every citation of it is a passed host-minted verification of the bound kind, matching any pinned check, cited by its full record id";

    // Wrong basis, with the valid verification submitted first, then with a
    // note alone: the basis decides, and no citation is named.
    for (case, evidence) in [
        ("valid verification first", vec![test.clone(), note.clone()]),
        ("note only", vec![note.clone()]),
    ] {
        let (reason, cause) = citation_refusal(submit(
            &mut fixture.store,
            CriterionVerdictInput {
                criterion: 1,
                verdict: AcceptanceVerdict::Pass,
                basis: AcceptanceBasis::Judgment,
                rationale: "criterion 1: pass".into(),
                evidence,
            },
            8,
        ));
        assert_eq!(
            cause.mismatch,
            crate::domain::EvaluationCitationMismatch::ObservedBasisRequired,
            "{case}"
        );
        assert_eq!(cause.citation, "", "{case}: no citation was at fault");
        assert_eq!(cause.citation_position, None, "{case}");
        assert_eq!(cause.criterion, 1, "{case}");
        assert!(
            reason.contains("criterion 1 is bound to test verification"),
            "{case}: {reason}"
        );
        let remedy = crate::work_service::evaluation_admission_remedy(
            &crate::domain::AcceptanceEvaluationAdmissionCause::Citation(Box::new(cause)),
        );
        assert!(remedy.ends_with(admissible), "{case}: {remedy}");
    }

    // Observed, with a note beside the valid verification: the note is the
    // fault, whichever comes first.
    for (case, evidence) in [
        ("verification first", vec![test.clone(), note.clone()]),
        ("note first", vec![note.clone(), test.clone()]),
    ] {
        let (_, cause) = citation_refusal(submit(
            &mut fixture.store,
            CriterionVerdictInput {
                criterion: 1,
                verdict: AcceptanceVerdict::Pass,
                basis: AcceptanceBasis::Observed,
                rationale: "criterion 1: pass".into(),
                evidence,
            },
            9,
        ));
        assert_eq!(
            cause.mismatch,
            crate::domain::EvaluationCitationMismatch::PassedVerificationRequired,
            "{case}"
        );
        assert_eq!(cause.citation, note.as_str(), "{case}: the note is named");
    }

    // A verdict that does not pass carries no basis requirement: a judgment
    // on the bound criterion citing the note is admitted.
    let admitted = submit(
        &mut fixture.store,
        CriterionVerdictInput {
            criterion: 1,
            verdict: AcceptanceVerdict::InsufficientEvidence,
            basis: AcceptanceBasis::Judgment,
            rationale: "criterion 1: insufficient evidence".into(),
            evidence: vec![note.clone()],
        },
        10,
    )
    .expect("a non-pass verdict needs no observed basis");
    assert!(!admitted.record.all_pass());
}

// Under an observed mechanical policy an asserted pass is refused for its
// basis: the cause names no citation, even when the cited record is a valid
// passed verification.
#[test]
fn an_asserted_pass_under_an_observed_policy_names_no_citation() {
    let mut fixture = fixture("project-observed-policy-attribution");
    let claim = fixture.claim.clone();
    let work = fixture.work.clone();
    enable(
        &mut fixture.store,
        &[Mode::SameSession],
        MechanicalBasis::Observed,
        false,
        "enable-observed-policy",
        5,
    );
    let test = host_verification(
        &mut fixture.store,
        &work,
        &claim,
        "runner",
        "test-check",
        VerificationKind::Test,
        VerificationResult::Passed,
        6,
    );
    let through = cut(&fixture.store, &work);
    let (reason, cause) = citation_refusal(record(
        &mut fixture.store,
        &request(
            &work,
            through,
            "runner",
            Mode::SameSession,
            vec![verdict(
                1,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::Asserted,
                std::slice::from_ref(&test),
            )],
            7,
        ),
    ));
    assert_eq!(
        cause.mismatch,
        crate::domain::EvaluationCitationMismatch::ObservedPolicyRequired
    );
    assert_eq!(cause.citation, "");
    assert_eq!(cause.citation_position, None);
    assert!(
        reason.contains("the project policy requires observed check evidence"),
        "{reason}"
    );
}
