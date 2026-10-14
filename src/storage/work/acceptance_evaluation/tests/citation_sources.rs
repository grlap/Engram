//! A pass on a criterion bound to a typed check rests only on checks of the
//! source the evaluation judged: record admission refuses a citation of a
//! check that ran on another source, and completion treats a record holding
//! one as stale, whichever way the binding's obligation was resolved.

use super::*;
use crate::domain::{
    AcceptanceBinding, VerificationRequirement, WaiveWorkObligationRequest, WorkObligationState,
};

mod citable {
    //! Wrong-source suggestions describe admitted alternatives, never rescue a submission.

    use super::*;
    use serde_json::{Value, json};

    fn suggestions(cause: &crate::domain::CitationAdmissionCause) -> Value {
        serde_json::to_value(cause)
            .expect("serialize transient citation context")
            .get("citable_records")
            .cloned()
            .expect("wrong-source refusal includes citable-record discovery")
    }

    fn wrong_source(
        store: &mut SqliteStore,
        request: &RecordAcceptanceEvaluationRequest,
    ) -> (String, Box<crate::domain::CitationAdmissionCause>) {
        let before = test_database_shape_snapshot(&store.connection).expect("before refusal");
        let (reason, cause) = admission::typed_refusal(record(store, request));
        assert_eq!(
            test_database_shape_snapshot(&store.connection).expect("after refusal"),
            before,
            "candidate discovery and rejected submissions write nothing"
        );
        let AcceptanceEvaluationAdmissionCause::Citation(cause) = cause else {
            panic!("citation refusal: {cause:?}");
        };
        assert_eq!(cause.mismatch, EvaluationCitationMismatch::WrongSource);
        assert_eq!(cause.remedy, Remedy::RunCurrentCheckAndEvaluate);
        (reason, cause)
    }

    fn source_matrix(workspace: bool, candidate_present: bool) {
        let (mut fixture, work, _claim, mut host) = bound_fixture("citable-source-matrix", false);
        let note = fixture.evidence.clone();
        let store = &mut fixture.store;
        let bad = one_check(host.checkpoint(
            store,
            true,
            Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
            20,
        ));
        if workspace {
            host.basis.workspace_id = "other-workspace".into();
        } else {
            host.basis.source_revision = "revision-e".into();
        }
        host.checkpoint(store, true, None, 30);
        let candidate = candidate_present.then(|| {
            one_check(host.checkpoint(
                store,
                false,
                Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
                40,
            ))
        });
        let source = declared(&host.basis.source_revision, Some(&host.basis.workspace_id));
        let input = evaluation(
            store,
            &work,
            std::slice::from_ref(&bad),
            &note,
            Some(source),
            50,
        );
        let (reason, cause) = wrong_source(store, &input);
        assert_eq!(cause.citation, bad.as_str());
        assert!(
            reason.contains(bad.as_str()),
            "the original offender remains named"
        );
        assert!(
            reason.contains("run the check on the current source"),
            "{reason}"
        );
        let records = candidate.iter().map(ObjectId::as_str).collect::<Vec<_>>();
        assert_eq!(
            suggestions(&cause),
            json!({"records": records, "more": false})
        );
        if let Some(candidate) = candidate {
            assert!(
                reason.starts_with("Citable verification records"),
                "{reason}"
            );
            assert!(
                reason[..reason.len().min(512)].contains(candidate.as_str()),
                "{reason}"
            );
            // Discovery includes a record the evaluator did not submit. Adding it
            // cannot make the rejected citation disappear or admit the submission.
            let mut mixed = input.clone();
            mixed.verdicts[0].evidence.push(candidate.clone());
            let (_, mixed_cause) = wrong_source(store, &mixed);
            assert_eq!(mixed_cause.citation, bad.as_str());
            let mut valid = input;
            valid.verdicts[0].evidence = vec![candidate];
            record(store, &valid).expect("the suggested record alone admits the bound pass");
        } else {
            assert!(
                reason.starts_with("Citable verification records: none"),
                "{reason}"
            );
        }
    }

    #[test]
    fn citable_records_wrong_revision_with_matching_record() {
        source_matrix(false, true);
    }

    #[test]
    fn citable_records_wrong_revision_without_matching_record() {
        source_matrix(false, false);
    }

    #[test]
    fn citable_records_wrong_workspace_with_matching_record() {
        source_matrix(true, true);
    }

    #[test]
    fn citable_records_wrong_workspace_without_matching_record() {
        source_matrix(true, false);
    }

    #[test]
    fn citable_records_newest_four_are_visible_before_long_rejection_text() {
        let (mut fixture, work, _claim, mut host) = bound_fixture("citable-limit", false);
        let note = fixture.evidence.clone();
        let store = &mut fixture.store;
        host.basis.workspace_id = format!("old-{}", "x".repeat(240));
        let bad = one_check(host.checkpoint(
            store,
            true,
            Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
            20,
        ));
        host.basis.workspace_id = format!("new-{}", "y".repeat(240));
        host.checkpoint(store, true, None, 30);
        let mut candidates = Vec::new();
        for second in [40, 50, 60, 70, 80, 90] {
            candidates.push(one_check(host.checkpoint(
                store,
                false,
                Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
                second,
            )));
        }
        // A newer failed check or a check of another kind must not consume the cap.
        host.checkpoint(
            store,
            false,
            Some((VerificationKind::Test, ExecutionOutcome::Failed)),
            100,
        );
        host.checkpoint(
            store,
            false,
            Some((VerificationKind::Build, ExecutionOutcome::Succeeded)),
            110,
        );
        host.checkpoint(
            store,
            false,
            Some((VerificationKind::Test, ExecutionOutcome::Unknown)),
            115,
        );
        let source = declared(&host.basis.source_revision, Some(&host.basis.workspace_id));
        let input = evaluation(
            store,
            &work,
            std::slice::from_ref(&bad),
            &note,
            Some(source),
            120,
        );
        let (reason, cause) = wrong_source(store, &input);
        let expected = candidates
            .iter()
            .rev()
            .take(4)
            .map(ObjectId::as_str)
            .collect::<Vec<_>>();
        assert_eq!(
            suggestions(&cause),
            json!({"records": expected, "more": true})
        );
        for candidate in candidates.iter().rev().take(4) {
            assert!(reason[..512].contains(candidate.as_str()), "{reason}");
        }
        assert!(!reason[..512].contains(candidates[0].as_str()), "{reason}");
        assert!(reason[..512].contains("more=true"), "{reason}");
        let (again, _) = wrong_source(store, &input);
        assert_eq!(again, reason, "same snapshot gives deterministic ordering");
    }

    /// A host may keep only the first 512 bytes of a refusal. The ids of
    /// records stored before ids became random UUIDs are 64 hex digits, the
    /// widest an id can be. The refusal the store returns, and the message an
    /// agent receives for it by CLI or MCP, must still show all four of them
    /// whole.
    #[test]
    fn full_historical_id_widths_fit_the_host_refusal_excerpt() {
        const EXCERPT: usize = 512;
        // Derive the ids at runtime from the same code that made them, never
        // a pinned digest.
        let records = crate::domain::CitableVerificationRecords {
            records: (0_u8..4)
                .map(|seed| ObjectId::from_canonical_bytes(&[seed]))
                .collect(),
            more: true,
        };
        for record in &records.records {
            let id = record.as_str();
            assert_eq!(id.len(), 64, "{id}");
            assert!(
                id.bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
                "{id}"
            );
        }
        let distinct = records
            .records
            .iter()
            .map(ObjectId::as_str)
            .collect::<std::collections::HashSet<_>>();
        assert_eq!(distinct.len(), 4, "{:?}", records.records);

        // Built as the store builds a wrong-source refusal: the discovery
        // prefix ahead of a rejection long enough to be cut short.
        let original = format!(
            "the cited check ran on another source; {}run the check on the current source",
            "the cited check's workspace was a long path. ".repeat(20)
        );
        let reason = format!(
            "{}{original}",
            super::super::super::citable::prefix(&records)
        );
        let cause = crate::domain::CitationAdmissionCause {
            mismatch: EvaluationCitationMismatch::WrongSource,
            criterion: 1,
            citation: "0".repeat(32),
            run_id: crate::domain::WorkRunId::new(),
            evaluated_cut: 7,
            citation_position: None,
            requirement: None,
            checked_revision: None,
            judged_revision: None,
            producer_observation: None,
            citable_records: Some(records.clone()),
            remedy: Remedy::RunCurrentCheckAndEvaluate,
        };
        let work = crate::domain::WorkId::new();
        let error = super::super::super::admission::refusal(
            work,
            reason,
            AcceptanceEvaluationAdmissionCause::Citation(Box::new(cause)),
        );
        assert!(matches!(
            error,
            StoreError::AcceptanceEvaluationAdmissionRefused { .. }
        ));

        // The core envelope host core consumers keep carries the Display.
        let display = error.to_string();
        let shared = crate::verbs::store_error_value(&error);
        assert_eq!(shared["error"]["message"].as_str(), Some(display.as_str()));
        // An agent, by CLI or MCP, receives the agent projection instead: the
        // item named by short reference and arguments spelled for the caller.
        // Rendering it opens no store; the path lies in this test's own
        // fixture, so a service that ever opened it eagerly would only create
        // a throwaway file there.
        let error = crate::verbs::VerbError::from(error);
        let home = crate::test_support::temp_home().expect("test fixture directory");
        let agent = || {
            crate::verbs::AgentVerbs::new(
                home.path().join("never-opened.db"),
                crate::domain::ProjectId("historical-id-excerpt".into()),
                "agent".into(),
                crate::domain::SessionId("historical-id-excerpt".into()),
                None,
            )
        };
        let mut surfaces = vec![("Display", display)];
        for (surface, verbs) in [
            ("CLI agent message", agent()),
            ("MCP agent message", agent().with_mcp_argument_names()),
        ] {
            let message = verbs.error_message(&error);
            let projected = verbs.project_error(&error, shared.clone());
            assert_eq!(
                projected["error"]["message"].as_str(),
                Some(message.as_str()),
                "{surface}"
            );
            assert!(
                message.starts_with("acceptance evaluation for w-")
                    && !message.contains(&format!("{work:?}")),
                "{surface} names the item by short reference: {message}"
            );
            surfaces.push((surface, message));
        }
        for (surface, text) in &surfaces {
            assert!(text.is_ascii(), "{surface}: {text}");
            assert!(text.len() > EXCERPT, "{surface} must be cut short: {text}");
            let excerpt = &text[..EXCERPT];
            for record in &records.records {
                assert!(excerpt.contains(record.as_str()), "{surface}: {text}");
            }
            assert!(excerpt.contains("more=true"), "{surface}: {text}");
            let discovery = text
                .find("Citable verification records")
                .unwrap_or_else(|| panic!("{surface} carries the discovery: {text}"));
            let rejection = text
                .find("the cited check ran on another source")
                .unwrap_or_else(|| panic!("{surface} carries the rejection: {text}"));
            assert!(
                discovery < rejection,
                "{surface}: the discovery precedes the rejection: {text}"
            );
        }
    }

    #[test]
    fn citable_records_do_not_use_a_check_beyond_the_evaluated_cut() {
        let (mut fixture, work, _claim, mut host) = bound_fixture("citable-cut", false);
        let note = fixture.evidence.clone();
        let store = &mut fixture.store;
        let bad = one_check(host.checkpoint(
            store,
            true,
            Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
            20,
        ));
        host.basis.source_revision = "revision-e".into();
        host.checkpoint(store, true, None, 30);
        let input = evaluation(
            store,
            &work,
            std::slice::from_ref(&bad),
            &note,
            Some(declared("revision-e", None)),
            35,
        );
        let later = one_check(host.checkpoint(
            store,
            false,
            Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
            40,
        ));
        // A later passing check on the same judged source does not stale this
        // basis. Discovery still stops at the evaluated cut, so it cannot offer it.
        let (reason, cause) = wrong_source(store, &input);
        assert_eq!(suggestions(&cause), json!({"records": [], "more": false}));
        assert!(
            !reason.contains(later.as_str()),
            "a future record is not suggested"
        );
    }

    #[test]
    fn citable_records_honor_the_pinned_command_without_requiring_a_new_work_revision() {
        let (mut fixture, work, claim, mut host) = bound_fixture("citable-command", false);
        let note = fixture.evidence.clone();
        let store = &mut fixture.store;
        let command = ObjectId::from_canonical_bytes(b"cargo test --lib selected");
        let checks = [(VerificationKind::Test, ExecutionOutcome::Succeeded)];
        let bad = one_check(host.checkpoint_commands(
            store,
            true,
            &checks,
            20,
            Some(std::slice::from_ref(&command)),
        ));
        host.basis.source_revision = "revision-e".into();
        host.checkpoint(store, true, None, 30);
        let good = one_check(host.checkpoint_commands(
            store,
            false,
            &checks,
            40,
            Some(std::slice::from_ref(&command)),
        ));
        let other_command = ObjectId::from_canonical_bytes(b"cargo test --lib another");
        let other = one_check(host.checkpoint_commands(
            store,
            false,
            &checks,
            50,
            Some(std::slice::from_ref(&other_command)),
        ));
        let work = revise(
            store,
            &work,
            &claim,
            WorkRevisionPatch {
                acceptance_bindings: Some(vec![AcceptanceBinding {
                    criterion: 1,
                    requirement: VerificationRequirement {
                        check_kind: VerificationKind::Test,
                        check_fingerprint: Some(command),
                    },
                }]),
                ..empty_patch()
            },
            "pin-command",
            60,
        )
        .expect("pin the command after its checks");
        let input = evaluation(
            store,
            &work,
            std::slice::from_ref(&bad),
            &note,
            Some(declared("revision-e", None)),
            70,
        );
        let (reason, cause) = wrong_source(store, &input);
        assert_eq!(
            suggestions(&cause),
            json!({"records": [good], "more": false})
        );
        assert!(
            !reason.starts_with(&format!("Citable verification records: {other}")),
            "{reason}"
        );
        let mut valid = input;
        valid.verdicts[0].evidence = vec![good];
        record(store, &valid).expect("a valid older-work-revision record remains admissible");
    }

    fn damaged_candidate_is_not_an_empty_list(damage: &str) {
        let (mut fixture, work, _claim, mut host) = bound_fixture("citable-damage", false);
        let note = fixture.evidence.clone();
        let store = &mut fixture.store;
        let bad = one_check(host.checkpoint(
            store,
            true,
            Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
            20,
        ));
        host.basis.source_revision = "revision-e".into();
        host.checkpoint(store, true, None, 30);
        let good = one_check(host.checkpoint(
            store,
            false,
            Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
            40,
        ));
        assert_eq!(
            store
                .connection
                .execute(damage, [good.as_str()])
                .expect("damage the candidate projection"),
            1
        );
        let input = evaluation(
            store,
            &work,
            std::slice::from_ref(&bad),
            &note,
            Some(declared("revision-e", None)),
            50,
        );
        let before = test_database_shape_snapshot(&store.connection).expect("before damaged read");
        let error = record(store, &input).expect_err("candidate corruption must be visible");
        assert!(
            matches!(error, StoreError::InvalidWorkProjection(_)),
            "{error:?}"
        );
        assert_eq!(
            test_database_shape_snapshot(&store.connection).expect("after damaged read"),
            before
        );
    }

    #[test]
    fn citable_records_do_not_turn_projection_damage_into_no_matching_records() {
        damaged_candidate_is_not_an_empty_list(
            "UPDATE work_run_evidence SET source_revision = 'damaged' WHERE evidence_id = ?1",
        );
    }

    #[test]
    fn citable_records_refuse_a_missing_candidate_projection() {
        damaged_candidate_is_not_an_empty_list(
            "DELETE FROM work_run_evidence WHERE evidence_id = ?1",
        );
    }

    #[test]
    fn citable_records_refuse_a_retyped_candidate_projection() {
        damaged_candidate_is_not_an_empty_list(
            "UPDATE work_run_evidence SET evidence_kind = 'environment' WHERE evidence_id = ?1",
        );
    }

    #[test]
    fn citable_records_include_an_admissible_bound_verification() {
        let directory = crate::test_support::temp_home().expect("fixture directory");
        let database = directory.path().join("bound-candidate.db");
        let (_, bound_id, original_id) = crate::storage::work::bound_assessment_fixture(&database);
        let mut store = SqliteStore::open(&database).expect("reopen bound fixture");
        let bound: VerificationEvidence =
            load_typed_work_object(&store.connection, &bound_id, "verification_evidence")
                .expect("bound canonical evidence");
        assert_eq!(
            bound
                .bound_from
                .as_ref()
                .expect("real bound provenance")
                .verification,
            original_id
        );
        let work = store
            .get_work_item(bound.binding.work_id)
            .expect("target item");
        let claim = crate::storage::work::query::load_work_claim_optional(
            &store.connection,
            work.active_run_id.expect("target run"),
        )
        .expect("target claim query")
        .expect("target claim");
        enable(
            &mut store,
            &[Mode::SameSession],
            MechanicalBasis::Observed,
            false,
            "bound-candidate-policy",
            40,
        );
        let mut foreign_basis = bound.source_basis.clone();
        foreign_basis.workspace_id = "foreign-root".into();
        foreign_basis.source_revision = "R0".into();
        let bad = host_verification_from_basis(
            &mut store,
            &work,
            &claim,
            "runner",
            "foreign-bound-candidate-check",
            VerificationKind::Test,
            VerificationResult::Passed,
            50,
            foreign_basis,
        );
        let mut input = request(
            &work,
            cut(&store, &work),
            "runner",
            Mode::SameSession,
            vec![
                verdict(
                    1,
                    AcceptanceVerdict::Pass,
                    AcceptanceBasis::Observed,
                    &[bad],
                ),
                verdict(
                    2,
                    AcceptanceVerdict::Pass,
                    AcceptanceBasis::Judgment,
                    std::slice::from_ref(&bound_id),
                ),
            ],
            60,
        );
        input.source_basis = Some(declared("R1", Some("workspace-root")));
        let (reason, cause) = wrong_source(&mut store, &input);
        assert_eq!(
            suggestions(&cause),
            json!({"records": [bound_id], "more": false})
        );
        assert!(reason.starts_with(&format!("Citable verification records: {bound_id};")));
        input.verdicts[0].evidence = vec![bound_id];
        record(&mut store, &input).expect("the suggested bound check alone is admitted");
    }

    #[test]
    fn citable_records_exclude_a_matching_check_from_another_run() {
        let (mut fixture, work, _claim, mut host) = bound_fixture("citable-other-run", false);
        let note = fixture.evidence.clone();
        let store = &mut fixture.store;
        let bad = one_check(host.checkpoint(
            store,
            true,
            Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
            20,
        ));
        host.basis.source_revision = "revision-e".into();
        host.checkpoint(store, true, None, 30);
        let foreign = store
            .create_work(
                &root_request(&work.project_id.0, "another-run", 40),
                &DevelopmentNoopRedactor,
            )
            .expect("another item");
        let foreign_claim =
            super::super::claim(store, &foreign, "peer", "foreign-claim", 41, 3_600);
        let foreign_check = host_verification_from_basis(
            store,
            &foreign,
            &foreign_claim,
            "peer",
            "same-source-foreign-check",
            VerificationKind::Test,
            VerificationResult::Passed,
            42,
            host.basis.clone(),
        );
        let input = evaluation(
            store,
            &work,
            std::slice::from_ref(&bad),
            &note,
            Some(declared("revision-e", None)),
            50,
        );
        let (reason, cause) = wrong_source(store, &input);
        assert_eq!(suggestions(&cause), json!({"records": [], "more": false}));
        assert!(!reason.contains(foreign_check.as_str()), "{reason}");
    }

    #[test]
    fn citable_records_exclude_checks_outside_the_current_named_generation() {
        let (mut fixture, work, claim, mut host) = bound_fixture("citable-naming", false);
        let note = fixture.evidence.clone();
        let store = &mut fixture.store;
        let bad = one_check(host.checkpoint(
            store,
            true,
            Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
            20,
        ));
        let target = ExecutionSourceBasis {
            workspace_id: "workspace-B".into(),
            source_revision: "revision-d".into(),
            source_root_generation: None,
            source_root_state: None,
        };
        let unnamed = host_verification_from_basis(
            store,
            &work,
            &claim,
            "runner",
            "before-naming",
            VerificationKind::Test,
            VerificationResult::Passed,
            25,
            target.clone(),
        );
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
                at(30),
                crate::domain::NamedRootBindingKind::Bound,
                None,
                &mut actor("runner"),
                "bind-citable-root",
                at(30),
            )
            .expect("name the current root");
        let old = host_verification_from_basis(
            store,
            &work,
            &claim,
            "runner",
            "old-naming",
            VerificationKind::Test,
            VerificationResult::Passed,
            35,
            ExecutionSourceBasis {
                source_root_generation: Some(8),
                source_root_state: Some(SourceRootState::Named),
                ..target.clone()
            },
        );
        let current = host_verification_from_basis(
            store,
            &work,
            &claim,
            "runner",
            "current-naming",
            VerificationKind::Test,
            VerificationResult::Passed,
            40,
            ExecutionSourceBasis {
                source_root_generation: Some(9),
                source_root_state: Some(SourceRootState::Named),
                ..target
            },
        );
        let input = evaluation(
            store,
            &work,
            std::slice::from_ref(&bad),
            &note,
            Some(declared("revision-d", Some("workspace-B"))),
            50,
        );
        let (reason, cause) = wrong_source(store, &input);
        assert_eq!(
            suggestions(&cause),
            json!({"records": [current], "more": false})
        );
        assert!(!reason.contains(unnamed.as_str()), "{reason}");
        assert!(!reason.contains(old.as_str()), "{reason}");
    }
}

/// The fixture's item with its first criterion bound to a test, under an
/// evaluated policy that requires observed mechanical passes, with a host
/// session on its live claim reporting the source at `revision-d`.
fn bound_fixture(project: &str, fresh: bool) -> (Fixture, WorkItem, WorkClaim, HostSession) {
    bound_fixture_of(project, fresh, VerificationKind::Test)
}

/// `bound_fixture` with the first criterion bound to a check of `kind`.
fn bound_fixture_of(
    project: &str,
    fresh: bool,
    kind: VerificationKind,
) -> (Fixture, WorkItem, WorkClaim, HostSession) {
    let mut fixture = fixture(project);
    let work = revise(
        &mut fixture.store,
        &fixture.work,
        &fixture.claim,
        WorkRevisionPatch {
            // Criteria keep the order typed, so the bound one stays first.
            acceptance: Some(vec!["run the tests".into(), "write the docs".into()]),
            acceptance_bindings: Some(vec![AcceptanceBinding {
                criterion: 1,
                requirement: VerificationRequirement {
                    check_kind: kind,
                    check_fingerprint: None,
                },
            }]),
            ..empty_patch()
        },
        "bind-the-check",
        5,
    )
    .expect("bind the first criterion to a check");
    enable(
        &mut fixture.store,
        &[Mode::SameSession],
        MechanicalBasis::Observed,
        fresh,
        "enable-observed",
        6,
    );
    let claim = fixture
        .store
        .current_work_claim(work.work_id)
        .expect("claim read")
        .expect("live claim");
    let mut host = HostSession::bind(&mut fixture.store, &work, &claim, 10);
    host.basis.source_revision = "revision-d".into();
    (fixture, work, claim, host)
}

/// A passing evaluation at the current cut: the bound criterion on the
/// `checks` it cites, the free-text one on `note`, declaring `declared` as
/// the source it judged.
fn evaluation(
    store: &SqliteStore,
    work: &WorkItem,
    checks: &[ObjectId],
    note: &ObjectId,
    declared: Option<AcceptanceSourceBasis>,
    second: i64,
) -> RecordAcceptanceEvaluationRequest {
    let mut request = request(
        work,
        cut(store, work),
        "runner",
        Mode::SameSession,
        vec![
            verdict(
                1,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::Observed,
                checks,
            ),
            verdict(
                2,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::Judgment,
                std::slice::from_ref(note),
            ),
        ],
        second,
    );
    request.source_basis = declared;
    request
}

fn declared(fingerprint: &str, workspace: Option<&str>) -> AcceptanceSourceBasis {
    AcceptanceSourceBasis {
        workspace_id: workspace.map(Into::into),
        fingerprint: fingerprint.into(),
    }
}

/// The one check a host turn recorded.
fn one_check(checks: Vec<ObjectId>) -> ObjectId {
    let [check] = <[ObjectId; 1]>::try_from(checks).expect("the turn recorded one check");
    check
}

/// Records `request` as an earlier build admitted it: the record path
/// without the rule that a bound pass's checks ran on the judged source,
/// with the receipt an exact resend replays.
fn admitted_by_an_earlier_build(
    store: &mut SqliteStore,
    request: &RecordAcceptanceEvaluationRequest,
) -> AcceptanceEvaluationReceipt {
    let transaction = store
        .connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .expect("evaluation transaction");
    let item = load_work_item(&transaction, request.work_id).expect("evaluated item");
    let run_id = item.active_run_id.expect("active run");
    let attempt = attempt_identity(request, run_id).expect("attempt identity");
    let policy = SqliteStore::load_acceptance_evaluation_policy_on(&transaction).expect("policy");
    let verdicts = bind_verdicts(
        &transaction,
        &item,
        run_id,
        &policy,
        request.evaluated_through,
        &request.verdicts,
    )
    .expect("the verdicts an earlier build admitted");
    let receipt = append_evaluation(&transaction, &item, run_id, request, &attempt, verdicts)
        .expect("append the evaluation");
    transaction.commit().expect("commit the evaluation");
    receipt
}

/// A completion attempt after a fresh checkpoint naming `evidence`.
fn done(
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    evidence: &[ObjectId],
    fingerprint: Option<&str>,
    key: &str,
    second: i64,
) -> Result<CompletionSeal, StoreError> {
    checkpoint_then_complete(
        store,
        work,
        claim,
        "runner",
        evidence,
        false,
        fingerprint,
        key,
        second,
    )
}

fn stale_verification_source(result: Result<CompletionSeal, StoreError>) {
    let cause = recovery_cause(result);
    assert!(
        matches!(
            cause,
            WorkCompletionRecoveryCause::AcceptanceEvaluationStale {
                reason: AcceptanceStaleReason::VerificationSource
            }
        ),
        "{cause:?}"
    );
}

// B50, case D with source freshness off: a check at R_d, an edit to R_e with no
// check after it, and an evaluation after the edit. A pass citing the R_d
// check is refused by name and nothing is appended; the check rerun at R_e
// carries the criterion and seals. B51: the refusal of this undeclared
// evaluation names no declaration remedy.
#[test]
fn a_pass_citing_a_check_of_an_older_revision_is_refused_and_a_rerun_seals() {
    let (mut fixture, work, claim, mut host) = bound_fixture("project-citation-case-d", false);
    let store = &mut fixture.store;
    let note = fixture.evidence.clone();
    let at_d = one_check(host.checkpoint(
        store,
        true,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        20,
    ));
    host.basis.source_revision = "revision-e".into();
    let checkpointed = host.checkpoint(store, true, None, 30);
    assert!(checkpointed.is_empty(), "{checkpointed:?}");

    let before = cut(store, &work);
    let before_database = test_database_shape_snapshot(&store.connection).expect("snapshot");
    let (refused, cause) = admission::typed_refusal(record(
        store,
        &evaluation(store, &work, std::slice::from_ref(&at_d), &note, None, 35),
    ));
    assert_eq!(
        test_database_shape_snapshot(&store.connection).expect("snapshot"),
        before_database
    );
    let AcceptanceEvaluationAdmissionCause::Citation(cause) = cause else {
        panic!("citation family");
    };
    assert_eq!(cause.mismatch, EvaluationCitationMismatch::WrongSource);
    assert_eq!(cause.criterion, 1);
    assert_eq!(cause.citation, at_d.as_str());
    assert_eq!(cause.run_id, claim.run_id);
    assert_eq!(cause.evaluated_cut, before);
    assert_eq!(cause.checked_revision.as_deref(), Some("revision-d"));
    assert_eq!(cause.judged_revision.as_deref(), Some("revision-e"));
    assert!(cause.producer_observation.is_some());
    assert_eq!(
        cause.requirement.as_ref().unwrap().check_kind,
        VerificationKind::Test
    );
    assert_eq!(
        cause.remedy,
        crate::domain::EvaluationAdmissionRemedy::RunCurrentCheckAndEvaluate
    );
    assert!(
        refused.contains(&format!(
            "criterion 1 is bound to test verification, and {at_d} ran on source revision revision-d, not the revision revision-e this evaluation judged; run the check on the current source, then evaluate again citing it"
        )),
        "{refused}"
    );
    // Nothing was declared, so the remedy names no declaration.
    assert!(!refused.contains("declare"), "{refused}");
    assert_eq!(cut(store, &work), before, "a refusal appends nothing");
    assert_eq!(
        store
            .acceptance_evaluation_status(work.work_id, None)
            .expect("status read"),
        None
    );

    let at_e = one_check(host.checkpoint(
        store,
        false,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        40,
    ));
    let admitted = record(
        store,
        &evaluation(store, &work, std::slice::from_ref(&at_e), &note, None, 45),
    )
    .expect("a pass on the rerun check records");
    let seal = done(
        store,
        &work,
        &claim,
        &[note, at_e.clone()],
        None,
        "complete-after-rerun",
        50,
    )
    .expect("the rerun check seals");
    assert_eq!(seal.acceptance_evaluation, Some(admitted.evaluation));
    assert_eq!(seal.acceptance[0].evidence, vec![at_e]);
}

// B50: source freshness (F4) ties the evaluated content to the content at
// completion; it says nothing about the check a pass cites. A host that
// measures R_e at evaluation and again at completion still gets a pass on
// an R_d check refused, and a record an earlier build admitted is stale.
#[test]
fn the_completion_fingerprint_does_not_stand_in_for_the_citation_rule() {
    let (mut fixture, work, claim, mut host) = bound_fixture("project-citation-freshness", true);
    let store = &mut fixture.store;
    let note = fixture.evidence.clone();
    let at_d = one_check(host.checkpoint(
        store,
        true,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        20,
    ));
    host.basis.source_revision = "revision-e".into();
    host.checkpoint(store, true, None, 30);

    let stale = evaluation(
        store,
        &work,
        std::slice::from_ref(&at_d),
        &note,
        Some(declared("revision-e", None)),
        35,
    );
    let refused = refusal(record(store, &stale));
    assert!(
        refused.contains("ran on source revision revision-d, not the revision revision-e"),
        "{refused}"
    );
    let earlier = admitted_by_an_earlier_build(store, &stale);
    assert_eq!(
        store
            .acceptance_evaluation_status(work.work_id, Some("revision-e"))
            .expect("status read")
            .map(|status| (status.evaluation, status.stale)),
        Some((
            earlier.evaluation,
            Some(AcceptanceStaleReason::VerificationSource)
        ))
    );
    stale_verification_source(done(
        store,
        &work,
        &claim,
        &[note.clone(), at_d],
        Some("revision-e"),
        "complete-on-the-old-check",
        40,
    ));

    let at_e = one_check(host.checkpoint(
        store,
        false,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        45,
    ));
    record(
        store,
        &evaluation(
            store,
            &work,
            std::slice::from_ref(&at_e),
            &note,
            Some(declared("revision-e", None)),
            50,
        ),
    )
    .expect("a pass on the rerun check records");
    done(
        store,
        &work,
        &claim,
        &[note, at_e],
        Some("revision-e"),
        "complete-after-rerun",
        55,
    )
    .expect("the rerun check seals with the measured fingerprint");
}

// B50: a record an earlier build admitted is judged again when completion
// consumes it. On the satisfied path the binding's obligation was met by the
// R_d check; the evaluation is refused as stale before any obligation rule
// runs. An exact resend still recovers that record, since replay comes
// before admission, and any other resend is admitted fresh and refused.
#[test]
fn an_earlier_build_record_is_stale_at_done_and_replays_before_the_refusal() {
    let (mut fixture, work, claim, mut host) = bound_fixture("project-citation-satisfied", false);
    let store = &mut fixture.store;
    let note = fixture.evidence.clone();
    let at_d = one_check(host.checkpoint(
        store,
        true,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        20,
    ));
    let run_id = claim.run_id;
    assert!(
        store
            .work_run_obligations(run_id)
            .expect("obligations")
            .iter()
            .all(|record| record.state == WorkObligationState::Satisfied),
        "the R_d check satisfies the binding"
    );
    host.basis.source_revision = "revision-e".into();
    host.checkpoint(store, true, None, 30);

    let stale = evaluation(store, &work, std::slice::from_ref(&at_d), &note, None, 35);
    let earlier = admitted_by_an_earlier_build(store, &stale);
    let replayed = record(store, &stale).expect("an exact resend replays");
    assert!(replayed.replayed);
    assert_eq!(replayed.evaluation, earlier.evaluation);

    let mut changed = stale.clone();
    changed.verdicts[1].rationale = "the docs were read again".into();
    let refused = refusal(record(store, &changed));
    assert!(refused.contains(at_d.as_str()), "{refused}");

    stale_verification_source(done(
        store,
        &work,
        &claim,
        &[note, at_d],
        None,
        "complete-on-the-old-check",
        40,
    ));
}

// B50: on the waived path the binding's obligation was waived before any check,
// so no obligation rule ever matches the cited check to the run's latest
// change: only the evaluation's own citation stands between an R_d check
// and a seal over the R_e edit, whether the evaluation declared nothing or
// declared the older revision the check ran on.
#[test]
fn a_stale_citation_is_refused_at_done_when_the_binding_obligation_was_waived() {
    let (mut fixture, work, claim, mut host) = bound_fixture("project-citation-waived", false);
    let store = &mut fixture.store;
    let note = fixture.evidence.clone();
    let run_id = claim.run_id;
    let binding = store
        .work_run_obligations(run_id)
        .expect("obligations")
        .into_iter()
        .find(|record| record.state == WorkObligationState::Open)
        .expect("the binding's open obligation");
    store
        .waive_work_obligation(
            &WaiveWorkObligationRequest {
                obligation_id: binding.obligation.obligation_id,
                expected_definition: binding.definition_id.clone(),
                waived_by: "operator".into(),
                reason: "the tests are checked by hand".into(),
                actor: actor("operator"),
                idempotency_key: "waive-the-binding".into(),
                waived_at: at(15),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("waive the binding's obligation");
    let at_d = one_check(host.checkpoint(
        store,
        true,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        20,
    ));
    host.basis.source_revision = "revision-e".into();
    host.checkpoint(store, true, None, 30);

    admitted_by_an_earlier_build(
        store,
        &evaluation(store, &work, std::slice::from_ref(&at_d), &note, None, 35),
    );
    stale_verification_source(done(
        store,
        &work,
        &claim,
        &[note.clone(), at_d.clone()],
        None,
        "complete-on-the-old-check",
        40,
    ));
    // Declaring the revision the old check ran on, behind the R_e sighting
    // before the cut, does not bring it back.
    admitted_by_an_earlier_build(
        store,
        &evaluation(
            store,
            &work,
            std::slice::from_ref(&at_d),
            &note,
            Some(declared("revision-d", None)),
            41,
        ),
    );
    stale_verification_source(done(
        store,
        &work,
        &claim,
        &[note.clone(), at_d],
        None,
        "complete-on-a-declared-old-check",
        42,
    ));

    let at_e = one_check(host.checkpoint(
        store,
        false,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        45,
    ));
    record(
        store,
        &evaluation(store, &work, std::slice::from_ref(&at_e), &note, None, 50),
    )
    .expect("a pass on the rerun check records");
    let seal = done(
        store,
        &work,
        &claim,
        &[note, at_e.clone()],
        None,
        "complete-after-rerun",
        55,
    )
    .expect("the rerun check seals");
    assert_eq!(seal.acceptance[0].evidence, vec![at_e]);
}

// B51: the judged source is the declared one whenever a declaration exists,
// workspace included when it names one, and the newest sighting is only the
// fallback: an older sighting never stands in for a declared tree. Nor does
// a declaration hide a sighting: an evaluation that declares the revision
// the check ran on, behind a later sighting at another one, is refused.
#[test]
fn a_declared_source_is_matched_and_never_hides_a_later_sighting() {
    let (mut fixture, work, _claim, mut host) = bound_fixture("project-citation-declared", false);
    let store = &mut fixture.store;
    let note = fixture.evidence.clone();
    let at_d = one_check(host.checkpoint(
        store,
        true,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        20,
    ));

    // The run was last seen at R_d, but the evaluator declares it judged
    // R_e: the older sighting does not stand in for the declared tree.
    let ahead = refusal(record(
        store,
        &evaluation(
            store,
            &work,
            std::slice::from_ref(&at_d),
            &note,
            Some(declared("revision-e", None)),
            25,
        ),
    ));
    assert!(
        ahead.contains("ran on source revision revision-d, not the revision revision-e"),
        "{ahead}"
    );
    // A declaration may be in a form the host never reports, so the remedy
    // names correcting it too.
    assert!(
        ahead.ends_with(
            ", or, when the declared fingerprint is not the source revision the host reports, declare that revision"
        ),
        "{ahead}"
    );
    // A declared workspace must be the one the check ran in.
    let elsewhere = refusal(record(
        store,
        &evaluation(
            store,
            &work,
            std::slice::from_ref(&at_d),
            &note,
            Some(declared("revision-d", Some("another-workspace"))),
            26,
        ),
    ));
    assert!(
        elsewhere.contains(&format!(
            "{at_d} ran in workspace workspace-evaluated, not the workspace another-workspace this evaluation judged"
        )),
        "{elsewhere}"
    );

    // Declared exactly as the check ran, it records.
    record(
        store,
        &evaluation(
            store,
            &work,
            std::slice::from_ref(&at_d),
            &note,
            Some(declared("revision-d", Some("workspace-evaluated"))),
            27,
        ),
    )
    .expect("a pass on a check of the declared source records");

    // The run is then seen at R_e. Declaring R_d, the revision the check ran
    // on, matches the check but not the run after it.
    host.basis.source_revision = "revision-e".into();
    host.checkpoint(store, true, None, 30);
    let behind = typed_reason(
        store,
        Typed::Citation(
            EvaluationCitationMismatch::SourceMovedAfterCheck,
            Remedy::RunCurrentCheckAndEvaluate,
        ),
        |store| {
            record(
                store,
                &evaluation(
                    store,
                    &work,
                    std::slice::from_ref(&at_d),
                    &note,
                    Some(declared("revision-d", Some("workspace-evaluated"))),
                    35,
                ),
            )
        },
    );
    assert!(
        behind.contains(&format!(
            "{at_d} ran on source revision revision-d, but the run was last seen at revision revision-e after it"
        )),
        "{behind}"
    );
}

// B51: a change the host reported without a revision after the check may
// have moved the source anywhere, so the check no longer carries a bound
// pass though every revision the run shows is the one it ran on.
#[test]
fn a_change_without_a_revision_after_the_check_retires_it() {
    let (mut fixture, work, _claim, mut host) = bound_fixture("project-citation-unrevised", false);
    let store = &mut fixture.store;
    let note = fixture.evidence.clone();
    let at_d = one_check(host.checkpoint(
        store,
        true,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        20,
    ));
    host.report(store, &[(true, None)], 30);
    let refused = typed_reason(
        store,
        Typed::Citation(
            EvaluationCitationMismatch::SourceMovedAfterCheck,
            Remedy::RunCurrentCheckAndEvaluate,
        ),
        |store| {
            record(
                store,
                &evaluation(store, &work, std::slice::from_ref(&at_d), &note, None, 35),
            )
        },
    );
    assert!(
        refused.contains(&format!(
            "{at_d} ran on source revision revision-d, but the run reported a source change without a revision after it"
        )),
        "{refused}"
    );
}

// B51: with no declaration, a quiet sighting at R_e before the cut moves the
// judged source even though no change was reported. Seen back at R_d, the
// newest sighting decides, as in F3, and the check stands again. A check of
// R_d stays stale beside a fresh check of R_e, cited or not; the fresh one
// alone carries the criterion.
#[test]
fn a_quiet_move_or_a_fresh_check_does_not_rescue_a_stale_citation() {
    let (mut fixture, work, _claim, mut host) = bound_fixture("project-citation-quiet", false);
    let store = &mut fixture.store;
    let note = fixture.evidence.clone();
    let at_d = one_check(host.checkpoint(
        store,
        true,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        20,
    ));
    host.report(store, &[(false, Some("revision-e"))], 30);
    let quiet = refusal(record(
        store,
        &evaluation(store, &work, std::slice::from_ref(&at_d), &note, None, 35),
    ));
    assert!(
        quiet.contains("ran on source revision revision-d, not the revision revision-e"),
        "{quiet}"
    );
    // Seen back at R_d, the source holds the content the check ran on.
    host.report(store, &[(false, Some("revision-d"))], 36);
    record(
        store,
        &evaluation(store, &work, std::slice::from_ref(&at_d), &note, None, 39),
    )
    .expect("a pass on the check records once the source is back where it ran");

    host.basis.source_revision = "revision-e".into();
    let at_e = one_check(host.checkpoint(
        store,
        false,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        40,
    ));
    for (checks, second) in [
        (vec![at_d.clone()], 45),
        (vec![at_e.clone(), at_d.clone()], 46),
    ] {
        let refused = refusal(record(
            store,
            &evaluation(store, &work, &checks, &note, None, second),
        ));
        assert!(
            refused.contains(&format!("{at_d} ran on source revision revision-d")),
            "{refused}"
        );
    }
    record(
        store,
        &evaluation(store, &work, std::slice::from_ref(&at_e), &note, None, 47),
    )
    .expect("a pass on the fresh check alone records");
}

/// The newest verification recorded on the item's run.
fn newest_verification(store: &SqliteStore, work: &WorkItem) -> ObjectId {
    let stored: String = store
        .connection
        .query_row(
            "SELECT object_id FROM work_feed_entries
             WHERE feed_kind = 'run_execution' AND feed_id = ?1
               AND object_kind = 'verification_evidence'
             ORDER BY position DESC LIMIT 1",
            [work.active_run_id.expect("active run").0.to_string()],
            |row| row.get(0),
        )
        .expect("a verification on the run");
    ObjectId::from_stored(stored).expect("stored id")
}

// B51: later sightings of the check's own revision leave it standing: the
// turn's own closing sighting and one from another workspace. The pass
// records and seals.
#[test]
fn a_check_stands_through_later_sightings_of_its_own_revision() {
    let (mut fixture, work, claim, mut host) =
        bound_fixture("project-citation-same-revision", false);
    let store = &mut fixture.store;
    let note = fixture.evidence.clone();
    let at_d = one_check(host.checkpoint(
        store,
        true,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        20,
    ));
    host.report(store, &[(false, Some("revision-d"))], 25);
    host.basis.workspace_id = "another-workspace".into();
    host.report(store, &[(false, Some("revision-d"))], 30);
    record(
        store,
        &evaluation(store, &work, std::slice::from_ref(&at_d), &note, None, 40),
    )
    .expect("a pass on the check records");
    let seal = done(
        store,
        &work,
        &claim,
        &[note, at_d.clone()],
        None,
        "complete-on-the-check",
        45,
    )
    .expect("the check seals");
    assert_eq!(seal.acceptance[0].evidence, vec![at_d]);
}

/// Each execution observation on the item's run after `position`, oldest
/// first: whether it records a source change, and its revision.
fn observations_after(
    store: &SqliteStore,
    work: &WorkItem,
    position: i64,
) -> Vec<(bool, Option<String>)> {
    let mut statement = store
        .connection
        .prepare(
            "SELECT json_extract(object.canonical_json, '$.source_changed'),
                    json_extract(object.canonical_json, '$.source_basis.source_revision')
             FROM work_feed_entries entry
             JOIN objects object ON object.object_id = entry.object_id
             WHERE entry.feed_kind = 'run_execution' AND entry.feed_id = ?1
               AND entry.position > ?2 AND entry.object_kind = 'execution_observation'
             ORDER BY entry.position",
        )
        .expect("observation query");
    statement
        .query_map(
            rusqlite::params![
                work.active_run_id.expect("active run").0.to_string(),
                position
            ],
            |row| Ok((row.get::<_, bool>(0)?, row.get::<_, Option<String>>(1)?)),
        )
        .expect("observation rows")
        .map(|row| row.expect("observation row"))
        .collect()
}

// B51: reported changes that move the source to R_e and back to R_d after the
// check, both kept as changes, leave the source holding the content the
// check ran on, so the evaluation records and stays fresh. Completion keeps
// its own rule for a satisfied binding: a check must follow the run's latest
// source change, so `done` asks for one.
#[test]
fn a_reported_move_and_revert_leave_the_check_standing() {
    let (mut fixture, work, claim, mut host) =
        bound_fixture("project-citation-reported-revert", false);
    let store = &mut fixture.store;
    let note = fixture.evidence.clone();
    let at_d = one_check(host.checkpoint(
        store,
        true,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        20,
    ));
    let after_check = cut(store, &work);
    host.report(store, &[(true, Some("revision-e"))], 25);
    host.report(store, &[(true, Some("revision-d"))], 30);
    assert_eq!(
        observations_after(store, &work, after_check),
        vec![
            (true, Some("revision-e".to_owned())),
            (true, Some("revision-d".to_owned())),
        ],
        "both reported changes are kept as changes"
    );
    let admitted = record(
        store,
        &evaluation(store, &work, std::slice::from_ref(&at_d), &note, None, 35),
    )
    .expect("a pass on the check records once the source is back where it ran");
    assert_eq!(
        store
            .acceptance_evaluation_status(work.work_id, None)
            .expect("status read")
            .map(|status| (status.evaluation, status.stale)),
        Some((admitted.evaluation, None))
    );
    let Err(StoreError::WorkBoundVerificationRefused { reason, cause, .. }) = done(
        store,
        &work,
        &claim,
        &[note, at_d.clone()],
        None,
        "complete-after-revert",
        40,
    ) else {
        panic!("the satisfied binding needs a check after the latest change");
    };
    assert!(
        reason.contains("does not verify the run's latest source change"),
        "{reason}"
    );
    assert_eq!(cause.verification, at_d);
    // The revision returned to the checked source, but both reported changes
    // remain ordered after this check.
    assert_eq!(
        cause.mismatch,
        crate::domain::VerificationEvidenceMismatch::NotAfterMutation
    );
    assert_eq!(
        cause.remedy,
        crate::domain::BoundVerificationRemedy::RunCurrentCheck
    );
}

// B51: a check the host records in a later turn, citing the observation that
// ran it, is judged from that observation, and sightings of its revision in
// between leave it standing.
#[test]
fn a_late_record_of_a_check_stands_through_sightings_of_its_revision() {
    let (mut fixture, work, _claim, mut host) = bound_fixture_of(
        "project-citation-late-record",
        false,
        VerificationKind::Build,
    );
    let store = &mut fixture.store;
    let note = fixture.evidence.clone();
    let first = one_check(host.checkpoint(
        store,
        true,
        Some((VerificationKind::Build, ExecutionOutcome::Succeeded)),
        20,
    ));
    let producer = store
        .load_verification_evidence(&first)
        .expect("load the first build")
        .producer_observation;
    host.report(store, &[(false, Some("revision-d"))], 25);
    host.cite_earlier_producer(store, &producer, Some("revision-d"), 30);
    let late = newest_verification(store, &work);
    assert_ne!(late, first, "the later turn recorded its own verification");
    record(
        store,
        &evaluation(store, &work, std::slice::from_ref(&late), &note, None, 35),
    )
    .expect("a pass on the late record of the build records");
}

/// An accounted inter-turn change the host observed without admission, in
/// the evaluated workspace, to `revision`, recorded at `second` by the
/// holder's host session.
pub(super) fn unadmitted_change(
    store: &mut SqliteStore,
    host: &HostSession,
    claim: &WorkClaim,
    revision: &str,
    key: &str,
    second: i64,
) -> crate::domain::ExecutionObservationReceipt {
    use crate::domain::{
        ExecutionObserveInput, MeasuredBaseline, MeasuredSighting, NamedRootState,
        ObservationCausality, ObservationPolicyBasis, ObservationRootBasis, ObservedInterval,
        ObservedOccurrence, ObservedSourceChange,
    };
    let run = load_work_run(&store.connection, claim.run_id).expect("run");
    let policy = SqliteStore::load_active_control_policy(&store.connection).expect("policy");
    let workspace = host.basis.workspace_id.clone();
    let input = ExecutionObserveInput {
        idempotency_key: key.into(),
        binding: crate::domain::ControlWorkBinding {
            root_execution_id: run.root_execution_id,
            work_id: claim.work_id,
            run_id: claim.run_id,
            work_revision: claim.accepted_work_revision,
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
        },
        root_basis: ObservationRootBasis {
            capture_run_cut: store
                .work_feed_head(&FeedId::RunExecution(claim.run_id))
                .expect("run head"),
            latest_event: None,
            state: NamedRootState::NoRoot,
        },
        observed_interval: ObservedInterval {
            from: at(second - 2),
            through: at(second - 1),
        },
        occurrence: ObservedOccurrence::InterTurnChange {
            source_change: ObservedSourceChange::ContentComparison {
                workspace_id: workspace.clone(),
                baseline: MeasuredBaseline {
                    workspace_id: workspace.clone(),
                    source_revision: "revision-before".into(),
                    observed_at: at(second - 2),
                },
                sighting: MeasuredSighting {
                    source_basis: ExecutionSourceBasis {
                        workspace_id: workspace,
                        source_revision: revision.into(),
                        source_root_generation: None,
                        source_root_state: None,
                    },
                    observed_at: at(second - 1),
                },
            },
        },
        causality: ObservationCausality::Unknown {},
        policy_basis: ObservationPolicyBasis::AccountIfEligible {
            project_policy_epoch: policy.epoch,
            policy: policy.policy_id,
            obligation_rule_set: policy.obligation_rule_set,
        },
    };
    store
        .record_unadmitted_execution_observation(
            &host.project_id,
            &host.session_id,
            &host.connection_token,
            &host.routing_token,
            &actor(&host.session_id.0),
            input,
            at(second),
        )
        .expect("accounted unadmitted change")
}

// A check recorded before an accounted unadmitted change does not follow it,
// even when the change reports the revision the check ran on: the report
// may describe the source from before the check. A check after it carries
// the criterion.
#[test]
fn an_unadmitted_change_after_a_check_retires_it_whatever_revision_it_reports() {
    let (mut fixture, work, claim, mut host) = bound_fixture("project-citation-unadmitted", false);
    let store = &mut fixture.store;
    let note = fixture.evidence.clone();
    let at_d = one_check(host.checkpoint(
        store,
        false,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        20,
    ));
    let change = unadmitted_change(store, &host, &claim, "revision-d", "late-report", 30);
    assert!(matches!(
        change.accounting,
        crate::domain::ObservationAccounting::SourceChange { .. }
    ));
    let refused = refusal(record(
        store,
        &evaluation(store, &work, std::slice::from_ref(&at_d), &note, None, 35),
    ));
    assert!(
        refused.contains(&format!(
            "{at_d} ran on source revision revision-d, but the run accounted unadmitted source change unadmitted:{}",
            change.observation
        )),
        "{refused}"
    );
    let fresh = one_check(host.checkpoint(
        store,
        false,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        40,
    ));
    record(
        store,
        &evaluation(store, &work, std::slice::from_ref(&fresh), &note, None, 45),
    )
    .expect("a pass on a check after the change records");
}

// A check whose records arrive after an accounted unadmitted change but that
// completed before the change was recorded may be an old result: it does
// not carry the criterion.
#[test]
fn a_check_completed_before_an_unadmitted_change_was_recorded_does_not_follow_it() {
    let (mut fixture, work, claim, mut host) = bound_fixture("project-citation-cached", false);
    let store = &mut fixture.store;
    let note = fixture.evidence.clone();
    let change = unadmitted_change(store, &host, &claim, "revision-d", "late-report", 40);
    let cached = one_check(host.checkpoint(
        store,
        false,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        20,
    ));
    let refused = refusal(record(
        store,
        &evaluation(store, &work, std::slice::from_ref(&cached), &note, None, 45),
    ));
    assert!(
        refused.contains(&format!(
            "but the run accounted unadmitted source change unadmitted:{}",
            change.observation
        )),
        "{refused}"
    );
}

/// Environment evidence alone, of the evaluated workspace at `revision`, as
/// a host records the environment a check ran in: a measured sighting with
/// no producer observation beside it.
fn environment_sighting(
    store: &mut SqliteStore,
    host: &HostSession,
    claim: &WorkClaim,
    revision: &str,
    second: i64,
) {
    use crate::domain::{EnvironmentComponents, EnvironmentEvidence};
    let run = load_work_run(&store.connection, claim.run_id).expect("run");
    let components = EnvironmentComponents {
        toolchain: "rustc-test".into(),
        sandbox: None,
        workspace_id: host.basis.workspace_id.clone(),
        capability_map_revision: 1,
    };
    let mut run_actor = actor(&claim.holder.0);
    run_actor.run_id = Some(run.run_id.0.to_string());
    let transaction = store
        .connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .expect("transaction");
    crate::storage::work::completion::append_control_environment_evidence_on(
        &transaction,
        &EnvironmentEvidence {
            schema_version: crate::domain::SCHEMA_VERSION,
            project_id: host.project_id.clone(),
            binding: crate::domain::ControlWorkBinding {
                root_execution_id: run.root_execution_id,
                work_id: claim.work_id,
                run_id: claim.run_id,
                work_revision: claim.accepted_work_revision,
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
            },
            session_id: claim.holder.clone(),
            source_basis: ExecutionSourceBasis {
                source_revision: revision.into(),
                ..host.basis.clone()
            },
            environment_fingerprint: CanonicalObject::freeze(&components)
                .expect("freeze")
                .key()
                .clone(),
            components: Some(components),
            observed_at: at(second),
            actor: run_actor.clone(),
            recorded_at: at(second),
        },
    )
    .expect("environment evidence");
    transaction.commit().expect("commit");
}

// After an unadmitted change, the newest measured sighting decides where the
// source is, environment evidence included, as it does for obligations: a
// check that followed the change is stale once the environment is seen at
// another revision.
#[test]
fn after_an_unadmitted_change_a_later_environment_sighting_retires_a_check() {
    let (mut fixture, work, claim, mut host) = bound_fixture("project-citation-environment", false);
    let store = &mut fixture.store;
    let note = fixture.evidence.clone();
    unadmitted_change(store, &host, &claim, "revision-u", "late-report", 30);
    let fresh = one_check(host.checkpoint(
        store,
        false,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        40,
    ));
    environment_sighting(store, &host, &claim, "revision-e", 45);
    let refused = refusal(record(
        store,
        &evaluation(store, &work, std::slice::from_ref(&fresh), &note, None, 50),
    ));
    assert!(refused.contains("revision revision-e"), "{refused}");
}

// An accounted unadmitted change after an evaluation's cut is a barrier the
// evaluation's cited checks did not follow, even when it reports the revision
// the evaluation declared: the recorded evaluation goes stale, a fresh check
// does not revive it, and a new submission that keeps the pre-change cut is
// refused.
#[test]
fn an_unadmitted_change_after_the_cut_retires_an_evaluation_that_declared_its_revision() {
    let (mut fixture, work, claim, mut host) = bound_fixture("project-citation-post-cut", false);
    let store = &mut fixture.store;
    let note = fixture.evidence.clone();
    let at_d = one_check(host.checkpoint(
        store,
        false,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        20,
    ));
    let recorded = evaluation(
        store,
        &work,
        std::slice::from_ref(&at_d),
        &note,
        Some(declared("revision-d", None)),
        25,
    );
    let mut resubmitted = recorded.clone();
    resubmitted.attempt_key = Some("resubmitted-at-the-old-cut".into());
    record(store, &recorded).expect("a pass on the check records");
    let change = unadmitted_change(store, &host, &claim, "revision-d", "late-report", 30);
    assert!(matches!(
        change.accounting,
        crate::domain::ObservationAccounting::SourceChange { .. }
    ));
    one_check(host.checkpoint(
        store,
        false,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        40,
    ));
    let status = store
        .acceptance_evaluation_status(work.work_id, None)
        .expect("status read")
        .expect("an evaluation");
    // The barrier decides before the bound citation is judged, and it is
    // named as the barrier it is, not as a content mutation.
    assert_eq!(
        status.stale,
        Some(AcceptanceStaleReason::UnadmittedChange),
        "{status:?}"
    );
    let named = status
        .stale_observation
        .as_ref()
        .expect("the deciding observation");
    assert_eq!(named.observation, change.observation);
    assert!(!named.admitted);
    assert_eq!(named.revisions_compared(), "the same revision");
    refusal(record(store, &resubmitted));
}

// A pass whose citations mix an admissible record with an inadmissible one is
// refused naming the inadmissible one and the every-citation rule, whatever
// order the citations were given in; a list of admissible records alone is
// admitted.
#[test]
fn a_mixed_citation_list_names_the_offending_record_and_the_every_citation_rule() {
    use crate::domain::{
        AcceptanceEvaluationAdmissionCause, EvaluationAdmissionRemedy, EvaluationCitationMismatch,
    };
    let mut fixture = fixture("project-evaluation-mixed-citations");
    let store = &mut fixture.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable-mixed",
        5,
    );
    let work = fixture.work.clone();
    let claim = fixture.claim.clone();
    let note = fixture.evidence.clone();
    let gate_pass = gate(store, &work, &claim, "runner", "cargo-test", &[], 6);
    let passed = host_verification(
        store,
        &work,
        &claim,
        "runner",
        "passed-test",
        VerificationKind::Test,
        VerificationResult::Passed,
        7,
    );
    let cases = [
        (
            AcceptanceBasis::Asserted,
            &gate_pass,
            EvaluationCitationMismatch::PassingGateRequired,
            "an asserted pass requires every citation to be a gate record with no failure labels",
        ),
        (
            AcceptanceBasis::Observed,
            &passed,
            EvaluationCitationMismatch::PassedVerificationRequired,
            "an observed pass requires every citation to be host-minted verification evidence with a passed result",
        ),
    ];
    for (basis, admissible, mismatch, rule) in cases {
        for citations in [
            vec![admissible.clone(), note.clone()],
            vec![note.clone(), admissible.clone()],
        ] {
            let snapshot = test_database_shape_snapshot(&store.connection).unwrap();
            let (reason, cause) = super::admission::typed_refusal(record(
                store,
                &request(
                    &work,
                    cut(store, &work),
                    "runner",
                    Mode::SameSession,
                    vec![verdict(1, AcceptanceVerdict::Pass, basis, &citations)],
                    8,
                ),
            ));
            assert!(
                reason.contains(rule) && reason.contains(&format!("{} is not", note.as_str())),
                "{reason}"
            );
            let AcceptanceEvaluationAdmissionCause::Citation(cause) = cause else {
                panic!("a citation refusal");
            };
            assert_eq!(cause.mismatch, mismatch);
            assert_eq!(cause.citation, note.as_str());
            assert_eq!(cause.remedy, EvaluationAdmissionRemedy::ReadRunEvidence);
            assert_eq!(
                test_database_shape_snapshot(&store.connection).unwrap(),
                snapshot
            );
        }
    }
    // Admissible records alone are admitted, for each basis.
    for (second, basis, admissible) in [
        (9, AcceptanceBasis::Asserted, &gate_pass),
        (10, AcceptanceBasis::Observed, &passed),
    ] {
        record(
            store,
            &request(
                &work,
                cut(store, &work),
                "runner",
                Mode::SameSession,
                vec![verdict(
                    1,
                    AcceptanceVerdict::Pass,
                    basis,
                    std::slice::from_ref(admissible),
                )],
                second,
            ),
        )
        .unwrap_or_else(|error| panic!("{basis:?} with only admissible citations: {error:?}"));
    }
}
