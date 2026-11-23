use super::*;
use crate::domain::{
    AcceptWorkHandoffRequest, EvaluationAdmissionRemedy, OfferWorkHandoffRequest,
    RegisteredVerificationOutcome,
};
use chrono::{DateTime, Utc};

#[test]
fn new_fingerprint_evaluation_is_refused_without_recording() {
    let mut fixture = fixture("retired-evaluation-source");
    let (host, source_id) = shared_host(
        &mut fixture.store,
        &fixture.work,
        &fixture.claim,
        &fixture.directory.path().join("source"),
        6,
    );
    let check = shared_check(
        &mut fixture.store,
        &host,
        &source_id,
        "before-retirement",
        10,
    );
    let mut input = request(
        &fixture.work,
        cut(&fixture.store, &fixture.work),
        "runner",
        Mode::SameSession,
        vec![verdict(
            1,
            AcceptanceVerdict::Pass,
            AcceptanceBasis::Observed,
            &[check],
        )],
        20,
    );
    input.source_basis = Some(AcceptanceSourceBasis::legacy(
        None,
        "retained-source".into(),
    ));
    let before = test_database_shape_snapshot(&fixture.store.connection).unwrap();
    let reads = crate::committed_input::read_count();
    let error = fixture
        .store
        .record_acceptance_evaluation(&input, &DevelopmentNoopRedactor)
        .expect_err("new legacy declaration");
    assert_eq!(
        crate::host::store_error_code(&error),
        "acceptance_evaluation_refused"
    );
    assert!(
        error
            .to_string()
            .contains("legacy source declarations are retired")
    );
    assert_eq!(
        test_database_shape_snapshot(&fixture.store.connection).unwrap(),
        before
    );
    assert_eq!(crate::committed_input::read_count(), reads);
}

#[test]
fn retained_legacy_source_requires_a_shared_declaration_for_new_verdicts() {
    for outcome in [
        AcceptanceVerdict::Pass,
        AcceptanceVerdict::Fail,
        AcceptanceVerdict::InsufficientEvidence,
        AcceptanceVerdict::NeedsHuman,
    ] {
        let mut fixture = fixture("retained-evaluation-source");
        enable(
            &mut fixture.store,
            &[Mode::SameSession],
            MechanicalBasis::Asserted,
            false,
            "enable",
            5,
        );
        let mut host = HostSession::bind(&mut fixture.store, &fixture.work, &fixture.claim, 6);
        host.install_source_records(&mut fixture.store, &[(false, Some("historical-source"))], 7);
        let mut input = request(
            &fixture.work,
            cut(&fixture.store, &fixture.work),
            "runner",
            Mode::SameSession,
            vec![verdict(
                1,
                outcome,
                AcceptanceBasis::Judgment,
                std::slice::from_ref(&fixture.evidence),
            )],
            10,
        );
        let before = test_database_shape_snapshot(&fixture.store.connection).unwrap();
        let error = fixture
            .store
            .record_acceptance_evaluation(&input, &DevelopmentNoopRedactor)
            .expect_err("an undeclared evaluation cannot infer a retired source");
        assert!(
            matches!(&error, StoreError::AcceptanceEvaluationRefused { reason, .. }
                if reason.contains("retained source is legacy")),
            "{error}"
        );
        assert_eq!(
            test_database_shape_snapshot(&fixture.store.connection).unwrap(),
            before
        );

        if outcome != AcceptanceVerdict::Pass {
            input.source_basis =
                AcceptanceSourceBasis::from_declaration(Some(measured_elsewhere()));
            let recorded = fixture
                .store
                .record_acceptance_evaluation(&input, &DevelopmentNoopRedactor)
                .expect("shared declaration records the non-pass judgment");
            assert_eq!(recorded.record.verdicts[0].verdict, outcome);
            assert!(recorded.record.registered_input_decision.is_none());
        }
    }
}

pub(crate) struct AdmissionTransportFixture {
    // Keep the store and its repository-local temporary home alive.
    _owner: Fixture,
    pub database: std::path::PathBuf,
    pub work: WorkItem,
    pub input: crate::WorkEvaluateInput,
    pub family: &'static str,
    pub mismatch: &'static str,
    pub citable_record: Option<ObjectId>,
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
        "wrong_run"
            | "wrong_basis"
            | "beyond_cut"
            | "beyond_cut_without_match"
            | "wrong_source"
            | "wrong_source_without_match"
            | "wrong_source_with_superseded_match"
            | "wrong_source_with_distinct_current_match"
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
    let mut citable_record = None;
    let transport_source = |letter: &str| crate::source::SourceId {
        git_delta: Some(crate::source::GitDelta {
            sha256: letter.repeat(64),
            dirty: false,
            gitlinks: vec![],
        }),
        commit: letter.repeat(40),
        tree: letter.repeat(40),
        changes: vec![],
        submodules: vec![],
    };
    let (family, mismatch) = match case {
        "eligibility" => ("eligibility", "same_session_unmarked"),
        "source_root" => {
            let host = HostSession::bind(store, &work, &claim, second + 3);
            store
                .bind_named_root(
                    &work.project_id,
                    &host.session_id,
                    &host.connection_token,
                    crate::storage::HostSessionAuthority::Work,
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
            declared = Some(transport_source("a"));
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
        "beyond_cut"
        | "beyond_cut_without_match"
        | "wrong_source"
        | "wrong_source_without_match"
        | "wrong_source_with_superseded_match"
        | "wrong_source_with_distinct_current_match" => {
            let (host, source_id) = shared_host(
                store,
                &work,
                &claim,
                &fixture.directory.path().join("source"),
                second + 3,
            );
            if matches!(case, "beyond_cut" | "beyond_cut_without_match") {
                if case == "beyond_cut" {
                    shared_check(store, &host, &source_id, "initial", second + 5);
                }
                basis = cut(store, &work);
                citation = shared_check(store, &host, &source_id, "later", second + 10);
                declared = Some(source_id);
                ("citation", "beyond_cut")
            } else {
                let mut other = source_id.clone();
                other.tree = ObjectId::from_canonical_bytes(b"other source").as_str()[..40].into();
                citation = shared_check(store, &host, &other, "other", second + 10);
                if case == "wrong_source" {
                    citable_record = Some(shared_check(
                        store,
                        &host,
                        &source_id,
                        "current",
                        second + 20,
                    ));
                } else if case == "wrong_source_with_superseded_match" {
                    shared_check(store, &host, &source_id, "current", second + 20);
                    shared_check_with_outcome(
                        store,
                        &host,
                        &source_id,
                        "newer-failure",
                        second + 25,
                        RegisteredVerificationOutcome::Failed,
                        "cargo test --lib",
                    );
                } else if case == "wrong_source_with_distinct_current_match" {
                    shared_check(store, &host, &source_id, "older", second + 20);
                    shared_check(store, &host, &source_id, "superseded", second + 22);
                    citable_record = Some(shared_check_with_outcome(
                        store,
                        &host,
                        &source_id,
                        "distinct-check",
                        second + 24,
                        RegisteredVerificationOutcome::Passed,
                        "cargo test --lib distinct",
                    ));
                    shared_check_with_outcome(
                        store,
                        &host,
                        &source_id,
                        "superseding-failure",
                        second + 25,
                        RegisteredVerificationOutcome::Failed,
                        "cargo test --lib",
                    );
                }
                basis = cut(store, &work);
                declared = Some(source_id);
                ("citation", "wrong_source")
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
        source_id: declared,
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
        citable_record,
    }
}

/// Bind a real Git source for current registered evidence shared by word fixtures.
pub(super) fn shared_host(
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    source_root: &std::path::Path,
    second: i64,
) -> (HostSession, crate::source::SourceId) {
    // A real named Git root lets the current SourceId producer and
    // its live currency checks participate in both transports.
    std::fs::create_dir(source_root).unwrap();
    for args in [
        vec!["init", "-q"],
        vec![
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "--no-gpg-sign",
            "--allow-empty",
            "-qm",
            "fixture",
        ],
    ] {
        let output = std::process::Command::new("git")
            .args(args)
            .current_dir(source_root)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let source_id = crate::source::inspect(source_root, crate::source::MAX_BUDGET)
        .source_id
        .expect("complete fixture source");
    let mut host = HostSession::bind(store, work, claim, second);
    host.basis.workspace_id = source_root.canonicalize().unwrap().to_string_lossy().into();
    host.basis.source_root_generation = Some(1);
    host.basis.source_root_state = Some(crate::domain::SourceRootState::Named);
    store
        .bind_named_root(
            &work.project_id,
            &host.session_id,
            &host.connection_token,
            crate::storage::HostSessionAuthority::Work,
            claim.claim_id,
            claim.fence,
            &host.basis.workspace_id,
            1,
            at(second + 1),
            crate::domain::NamedRootBindingKind::Bound,
            None,
            &mut actor("runner"),
            "transport-shared-root",
            at(second + 1),
        )
        .unwrap();
    (host, source_id)
}

/// Install a check through the current registered `SourceId` owner, rather
/// than seeding a legacy check that new callers can no longer submit.
pub(super) fn shared_check(
    store: &mut SqliteStore,
    host: &HostSession,
    source_id: &crate::source::SourceId,
    key: &str,
    second: i64,
) -> ObjectId {
    shared_check_with_outcome(
        store,
        host,
        source_id,
        key,
        second,
        RegisteredVerificationOutcome::Passed,
        "cargo test --lib",
    )
}

fn shared_check_with_outcome(
    store: &mut SqliteStore,
    host: &HostSession,
    source_id: &crate::source::SourceId,
    key: &str,
    second: i64,
    outcome: RegisteredVerificationOutcome,
    command: &str,
) -> ObjectId {
    use crate::domain::{
        RegisteredVerificationAssociation, RegisteredVerificationCheck,
        RegisteredVerificationInput, RegisteredVerificationPostRun, RegisteredVerificationRequest,
        RegisteredVerificationRoot, RegisteredVerificationRun, SourceRootState,
    };
    let request = RegisteredVerificationRequest {
        operation_key: format!("termal-run:test-transport-{key}"),
        registration: RegisteredVerificationAssociation {
            host_run_id: format!("test-transport-{key}"),
            request_identity: format!("transport-request-{key}"),
            registered_at: at(second),
            initiating_session_id: host.session_id.clone(),
            owning_session_id: None,
            binding: host.binding.clone(),
            root: RegisteredVerificationRoot {
                workspace_id: host.basis.workspace_id.clone(),
                generation: 1,
                state: SourceRootState::Named,
            },
        },
        input: RegisteredVerificationInput::SourceId {
            source_id: source_id.clone(),
        },
        run: RegisteredVerificationRun {
            mode: "focused".into(),
            command: command.into(),
            started_at: at(second),
            finished_at: at(second + 1),
            outcome,
        },
        checks: vec![RegisteredVerificationCheck {
            stage: "focused".into(),
            check_kind: VerificationKind::Test,
            command: command.into(),
            check_fingerprint: check_fingerprint(command),
            started_at: Some(at(second)),
            completed_at: Some(at(second + 1)),
            outcome,
            summary: Some("current source test".into()),
            refs: vec![],
            counts: None,
        }],
        post_run: RegisteredVerificationPostRun::SourceId {
            source_id: source_id.clone(),
        },
    };
    store
        .record_registered_verification(
            &host.project_id,
            &host.session_id,
            &host.connection_token,
            crate::storage::HostSessionAuthority::Work,
            &actor(&host.session_id.0),
            &request,
            at(second + 2),
        )
        .expect("registered shared check")
        .verifications
        .remove(0)
        .record_id
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
    let value = crate::store_error_value(&error);
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
    Reroll(
        crate::domain::EvaluationRerollMismatch,
        EvaluationAdmissionRemedy,
    ),
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
        AcceptanceEvaluationAdmissionCause::Reroll(cause) => {
            Typed::Reroll(cause.mismatch, cause.remedy)
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
