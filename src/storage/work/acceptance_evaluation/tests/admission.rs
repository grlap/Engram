use super::*;
use crate::domain::{AcceptWorkHandoffRequest, EvaluationAdmissionRemedy, OfferWorkHandoffRequest};
use chrono::{DateTime, Utc};

pub(crate) struct AdmissionTransportFixture {
    // Keep the store and its repository-local temporary home alive.
    _owner: Fixture,
    pub database: std::path::PathBuf,
    pub work: WorkItem,
    pub input: crate::WorkEvaluateInput,
    pub family: &'static str,
    pub mismatch: &'static str,
}

/// Real recorded histories used by both the service and MCP handler checks.
pub(crate) fn admission_transport_fixture(
    case: &str,
    now: DateTime<Utc>,
) -> AdmissionTransportFixture {
    let mut fixture = fixture("admission-transports");
    let second = (now - at(0)).num_seconds() - 60;
    let mut work = fixture.work.clone();
    let store = &mut fixture.store;
    let mut claim = claim(
        store,
        &work,
        "runner",
        "renew-transport-claim",
        second,
        3_600,
    );
    enable(
        store,
        if case == "eligibility" {
            &[Mode::SameSession, Mode::IndependentSession]
        } else {
            &[Mode::SameSession]
        },
        MechanicalBasis::Observed,
        false,
        "transport-policy",
        second + 1,
    );
    if matches!(
        case,
        "wrong_run" | "beyond_cut" | "wrong_source" | "wrong_basis"
    ) {
        work = revise(
            store,
            &work,
            &claim,
            WorkRevisionPatch {
                acceptance_bindings: Some(vec![AcceptanceBinding {
                    criterion: 1,
                    requirement: crate::domain::VerificationRequirement {
                        check_kind: VerificationKind::Test,
                        check_fingerprint: None,
                    },
                }]),
                ..empty_patch()
            },
            "transport-binding",
            second + 2,
        )
        .unwrap();
        claim = store
            .current_work_claim(work.work_id)
            .expect("read revised claim")
            .expect("claim retained after the revision");
    }
    let mut citation = fixture.evidence.clone();
    let mut basis = cut(store, &work);
    let mut declared = None;
    let (family, mismatch) = match case {
        "eligibility" => ("eligibility", "same_session_unmarked"),
        "source_root" => {
            let host = HostSession::bind(store, &work, &claim, second + 3);
            store
                .bind_named_root(
                    &work.project_id,
                    &host.session_id,
                    &host.connection_token,
                    &host.routing_token,
                    claim.claim_id,
                    claim.fence,
                    "workspace-B",
                    9,
                    at(second + 4),
                    crate::domain::NamedRootBindingKind::Bound,
                    None,
                    &mut actor("runner"),
                    "transport-root",
                    at(second + 4),
                )
                .expect("bind the named root in the fixture's project");
            basis = cut(store, &work);
            declared = Some("transport-R".into());
            ("source_root", "no_initial_sighting")
        }
        "wrong_run" => {
            let foreign = store
                .create_work(
                    &root_request("admission-transports", "foreign-transport", second + 3),
                    &DevelopmentNoopRedactor,
                )
                .unwrap();
            let foreign_claim = super::claim(
                store,
                &foreign,
                "foreign-runner",
                "foreign-claim",
                second + 4,
                3_600,
            );
            citation = host_verification(
                store,
                &foreign,
                &foreign_claim,
                "foreign-runner",
                "foreign-check",
                VerificationKind::Test,
                VerificationResult::Passed,
                second + 5,
            );
            basis = cut(store, &work);
            ("citation", "not_on_run")
        }
        "beyond_cut" | "wrong_source" => {
            let mut host = HostSession::bind(store, &work, &claim, second + 3);
            host.basis.source_revision = "transport-R".into();
            basis = cut(store, &work);
            citation = host
                .checkpoint(
                    store,
                    true,
                    Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
                    second + 10,
                )
                .pop()
                .unwrap();
            declared = Some("transport-R".into());
            if case == "wrong_source" {
                host.basis.source_revision = "transport-S".into();
                host.checkpoint(store, true, None, second + 20);
                basis = cut(store, &work);
                declared = Some("transport-S".into());
                ("citation", "wrong_source")
            } else {
                ("citation", "beyond_cut")
            }
        }
        // A valid passed check of the bound kind, cited under the wrong
        // basis: the basis is the fault, and no citation is named.
        "wrong_basis" => {
            citation = host_verification(
                store,
                &work,
                &claim,
                "runner",
                "transport-test",
                VerificationKind::Test,
                VerificationResult::Passed,
                second + 3,
            );
            basis = cut(store, &work);
            ("citation", "observed_basis_required")
        }
        _ => panic!("unknown transport fixture"),
    };
    let input = crate::WorkEvaluateInput {
        work_ref: Some(work.short_ref.clone()),
        mode: "same_session".into(),
        acceptance_basis: work.revision,
        evidence_basis: basis,
        verdicts: vec![crate::WorkCriterionVerdictInput {
            criterion: 1,
            verdict: "pass".into(),
            basis: if family == "citation" && case != "wrong_basis" {
                "observed"
            } else {
                "judgment"
            }
            .into(),
            rationale: "the cited evidence supports the criterion".into(),
            evidence: vec![citation.as_str().into()],
        }],
        source_fingerprint: declared,
        attempt: None,
        model: None,
        execution_identity: None,
        parent_session: None,
        supersedes: None,
    };
    let database = fixture.directory.path().join("engram.sqlite3");
    AdmissionTransportFixture {
        _owner: fixture,
        database,
        work,
        input,
        family,
        mismatch,
    }
}

/// Check the shared CLI/MCP error shape and word advice for an actual core refusal.
pub(super) fn typed_refusal(
    result: Result<AcceptanceEvaluationReceipt, StoreError>,
) -> (String, AcceptanceEvaluationAdmissionCause) {
    let error = result.expect_err("admission refuses");
    let StoreError::AcceptanceEvaluationAdmissionRefused {
        work,
        reason,
        cause,
    } = &error
    else {
        panic!("expected typed admission refusal, got {error:?}");
    };
    let legacy = StoreError::AcceptanceEvaluationRefused {
        work: *work,
        reason: reason.clone(),
    };
    assert_eq!(error.to_string(), legacy.to_string());
    let value = crate::mcp::store_error_value(&error);
    assert_eq!(value["error"]["code"], "acceptance_evaluation_refused");
    assert_eq!(value["error"]["message"], legacy.to_string());
    assert_eq!(value["error"]["details"]["reason"], *reason);
    assert_eq!(
        value["error"]["details"]["work_id"],
        serde_json::to_value(work).unwrap()
    );
    let decoded: AcceptanceEvaluationAdmissionCause =
        serde_json::from_value(value["error"]["details"]["cause"].clone()).unwrap();
    assert_eq!(&decoded, cause.as_ref());
    let result = (reason.clone(), decoded);
    let guidance = crate::verbs::VerbError::from(error).guidance();
    assert_eq!(
        guidance.reminders,
        vec![value["error"]["details"]["remedy"].as_str().unwrap()]
    );
    assert!(
        guidance
            .next
            .iter()
            .all(|command| command.starts_with("engram work show "))
    );
    result
}

/// What a typed admission refusal must name: its family's mismatch and the
/// remedy that mismatch selects.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Typed {
    Eligibility(EvaluationEligibilityMismatch, EvaluationAdmissionRemedy),
    Root(EvaluationRootMismatch, EvaluationAdmissionRemedy),
    Citation(EvaluationCitationMismatch, EvaluationAdmissionRemedy),
}

/// The reason of a refusal, after checking its shared CLI/MCP shape, its
/// word advice and that its typed cause names `expected`.
pub(super) fn typed_cause(
    result: Result<AcceptanceEvaluationReceipt, StoreError>,
    expected: Typed,
) -> String {
    let (reason, cause) = typed_refusal(result);
    let found = match cause {
        AcceptanceEvaluationAdmissionCause::Eligibility(cause) => {
            Typed::Eligibility(cause.mismatch, cause.remedy)
        }
        AcceptanceEvaluationAdmissionCause::SourceRoot(cause) => {
            Typed::Root(cause.mismatch, cause.remedy)
        }
        AcceptanceEvaluationAdmissionCause::Citation(cause) => {
            Typed::Citation(cause.mismatch, cause.remedy)
        }
    };
    assert_eq!(found, expected, "{reason}");
    reason
}

/// As `typed_cause`, for the refusal `attempt` meets, which must also leave
/// the store unchanged.
pub(super) fn typed_reason(
    store: &mut SqliteStore,
    expected: Typed,
    attempt: impl FnOnce(&mut SqliteStore) -> Result<AcceptanceEvaluationReceipt, StoreError>,
) -> String {
    let snapshot = test_database_shape_snapshot(&store.connection).unwrap();
    let result = attempt(store);
    assert_eq!(
        test_database_shape_snapshot(&store.connection).unwrap(),
        snapshot,
        "a refused evaluation changes nothing"
    );
    typed_cause(result, expected)
}

// Every citation mismatch selects its remedy by name. Unverifiable source is
// pinned here: admission never meets it, since a pass on a bound criterion
// cites only passed checks, each produced by a sighting with a revision
// before it.
#[test]
fn every_citation_mismatch_selects_its_remedy() {
    let fixture = fixture("citation-remedies");
    let context = CitationContext {
        item: &fixture.work,
        run_id: fixture.claim.run_id,
        cut: 1,
        criterion: 1,
        citation: "citation",
        position: None,
    };
    for (mismatch, remedy) in [
        (
            EvaluationCitationMismatch::NotOnRun,
            EvaluationAdmissionRemedy::ReadRunEvidence,
        ),
        (
            EvaluationCitationMismatch::BeyondCut,
            EvaluationAdmissionRemedy::ReadCurrentCut,
        ),
        (
            EvaluationCitationMismatch::ObservedBasisRequired,
            EvaluationAdmissionRemedy::ReadRunEvidence,
        ),
        (
            EvaluationCitationMismatch::PassedVerificationRequired,
            EvaluationAdmissionRemedy::ReadRunEvidence,
        ),
        (
            EvaluationCitationMismatch::ObservedPolicyRequired,
            EvaluationAdmissionRemedy::ReadRunEvidence,
        ),
        (
            EvaluationCitationMismatch::PassingGateRequired,
            EvaluationAdmissionRemedy::ReadRunEvidence,
        ),
        (
            EvaluationCitationMismatch::BoundVerificationMismatch,
            EvaluationAdmissionRemedy::ReadRunEvidence,
        ),
        (
            EvaluationCitationMismatch::WrongSource,
            EvaluationAdmissionRemedy::RunCurrentCheckAndEvaluate,
        ),
        (
            EvaluationCitationMismatch::SourceMovedAfterCheck,
            EvaluationAdmissionRemedy::RunCurrentCheckAndEvaluate,
        ),
        (
            EvaluationCitationMismatch::UnverifiableSource,
            EvaluationAdmissionRemedy::RunCurrentCheckAndEvaluate,
        ),
    ] {
        assert_eq!(context.cause(mismatch).remedy, remedy, "{mismatch:?}");
    }
}

// B01, B05, B32, B63: existing policy, default-independence and mark guards
// supply distinct typed causes without appending an evaluation or any row.
#[test]
fn admission_eligibility_causes_preserve_precedence_text_and_database() {
    let mut fixture = fixture("admission-eligibility");
    let work = fixture.work.clone();
    let note = fixture.evidence.clone();
    let store = &mut fixture.store;
    let make = |store: &SqliteStore, work: &WorkItem, mode| {
        request(
            work,
            cut(store, work),
            "runner",
            mode,
            vec![verdict(
                1,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::Judgment,
                std::slice::from_ref(&note),
            )],
            20,
        )
    };
    let snapshot = test_database_shape_snapshot(&store.connection).unwrap();
    let (reason, cause) = typed_refusal(record(store, &make(store, &work, Mode::SameSession)));
    assert_eq!(
        reason,
        "the project policy does not enable acceptance evaluation; completion stays self-asserted"
    );
    let AcceptanceEvaluationAdmissionCause::Eligibility(cause) = cause else {
        panic!("eligibility");
    };
    assert_eq!(
        cause.mismatch,
        EvaluationEligibilityMismatch::EvaluationDisabled
    );
    assert_eq!(
        cause.remedy,
        EvaluationAdmissionRemedy::UseSelfAssertedCompletion
    );
    assert_eq!(
        test_database_shape_snapshot(&store.connection).unwrap(),
        snapshot
    );

    enable(
        store,
        &[Mode::IndependentSession],
        MechanicalBasis::Asserted,
        false,
        "independent-only",
        5,
    );
    let snapshot = test_database_shape_snapshot(&store.connection).unwrap();
    let (reason, cause) = typed_refusal(record(store, &make(store, &work, Mode::SameSession)));
    assert_eq!(
        reason,
        "mode same_session is not allowed by the project policy; allowed: independent_session"
    );
    let AcceptanceEvaluationAdmissionCause::Eligibility(cause) = cause else {
        panic!("eligibility");
    };
    assert_eq!(
        cause.mismatch,
        EvaluationEligibilityMismatch::ModeDisallowed
    );
    assert_eq!(cause.admitted_modes, vec![Mode::IndependentSession]);
    assert_eq!(cause.task_mark, None);
    assert_eq!(
        test_database_shape_snapshot(&store.connection).unwrap(),
        snapshot
    );

    enable(
        store,
        &[Mode::SameSession, Mode::IndependentSession],
        MechanicalBasis::Asserted,
        false,
        "both",
        6,
    );
    let snapshot = test_database_shape_snapshot(&store.connection).unwrap();
    let (_, cause) = typed_refusal(record(store, &make(store, &work, Mode::SameSession)));
    let AcceptanceEvaluationAdmissionCause::Eligibility(cause) = cause else {
        panic!("eligibility");
    };
    assert_eq!(
        cause.mismatch,
        EvaluationEligibilityMismatch::SameSessionUnmarked
    );
    assert_eq!(
        test_database_shape_snapshot(&store.connection).unwrap(),
        snapshot
    );

    let work = revise(
        store,
        &work,
        &fixture.claim,
        WorkRevisionPatch {
            evaluation_mode: Some(Mode::SameSession),
            ..empty_patch()
        },
        "self-mark",
        7,
    )
    .unwrap();
    let snapshot = test_database_shape_snapshot(&store.connection).unwrap();
    for (mode, mismatch) in [
        (
            Mode::SameSession,
            EvaluationEligibilityMismatch::MarkAuthorAffiliated,
        ),
        (
            Mode::IndependentSession,
            EvaluationEligibilityMismatch::TaskPinMismatch,
        ),
    ] {
        let (_, cause) = typed_refusal(record(store, &make(store, &work, mode)));
        let AcceptanceEvaluationAdmissionCause::Eligibility(cause) = cause else {
            panic!("eligibility");
        };
        assert_eq!(cause.mismatch, mismatch);
        assert_eq!(cause.task_mark, Some(Mode::SameSession));
        if mode == Mode::SameSession {
            assert_eq!(cause.mark_author, Some(SessionId("runner".into())));
        }
        assert_eq!(
            test_database_shape_snapshot(&store.connection).unwrap(),
            snapshot
        );
    }
}

// B63: a legitimately shaped sub_agent request, with a parent session and an
// execution identity, recorded from the run's current holder or from a former
// one, is the executor's own evaluation. Its typed cause points at the
// evaluator binding, not at another mode, and nothing is appended.
#[test]
fn a_sub_agent_request_from_a_current_or_former_holder_points_at_the_evaluator_binding() {
    let mut fixture = fixture("admission-sub-agent-affiliated");
    let work = fixture.work.clone();
    let first = fixture.claim.clone();
    let note = fixture.evidence.clone();
    let store = &mut fixture.store;
    enable(
        store,
        &[Mode::SubAgent, Mode::IndependentSession],
        MechanicalBasis::Asserted,
        false,
        "sub-agent-affiliated",
        5,
    );
    let child = |store: &SqliteStore, work: &WorkItem, session: &str, parent: &SessionId| {
        RecordAcceptanceEvaluationRequest {
            execution_identity: Some("child-execution".into()),
            parent_session: Some(parent.clone()),
            ..request(
                work,
                cut(store, work),
                session,
                Mode::SubAgent,
                vec![verdict(
                    1,
                    AcceptanceVerdict::Pass,
                    AcceptanceBasis::Judgment,
                    std::slice::from_ref(&note),
                )],
                20,
            )
        }
    };
    let refused = |store: &mut SqliteStore, request: &RecordAcceptanceEvaluationRequest| {
        let snapshot = test_database_shape_snapshot(&store.connection).unwrap();
        let (reason, cause) = typed_refusal(record(store, request));
        assert!(
            reason.starts_with(
                "a sub_agent evaluation must be recorded from a distinct child session"
            ),
            "{reason}"
        );
        let AcceptanceEvaluationAdmissionCause::Eligibility(cause) = cause else {
            panic!("eligibility");
        };
        assert_eq!(
            cause.mismatch,
            EvaluationEligibilityMismatch::SubAgentEvaluatorAffiliated
        );
        assert_eq!(
            cause.remedy,
            EvaluationAdmissionRemedy::InspectEvaluatorBinding
        );
        assert_eq!(
            test_database_shape_snapshot(&store.connection).unwrap(),
            snapshot
        );
    };
    // The current holder, naming itself as the parent.
    refused(store, &child(store, &work, "runner", &first.holder));

    // A handoff to "second" leaves "runner" a former holder.
    let offer = store
        .offer_work_handoff(
            &OfferWorkHandoffRequest {
                work_id: work.work_id,
                run_id: first.run_id,
                expected_work_revision: work.revision,
                from: first.holder.clone(),
                to: SessionId("second".into()),
                claim_id: first.claim_id,
                claim_fence: first.fence,
                ttl_seconds: 300,
                checkpoint_summary: "handing the run to second".into(),
                actor: actor("runner"),
                idempotency_key: "offer-to-second".into(),
                offered_at: at(6),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("offer handoff");
    let second = store
        .accept_work_handoff(
            &AcceptWorkHandoffRequest {
                work_id: work.work_id,
                offer_id: offer.offer_id,
                to: SessionId("second".into()),
                actor: actor("second"),
                idempotency_key: "accept-as-second".into(),
                accepted_at: at(7),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("accept handoff");
    let work = store
        .get_work_item(work.work_id)
        .expect("item after handoff");
    refused(store, &child(store, &work, "runner", &second.holder));
    // A distinct child of the current holder records.
    record(store, &child(store, &work, "child", &second.holder)).expect("a distinct child records");
}
