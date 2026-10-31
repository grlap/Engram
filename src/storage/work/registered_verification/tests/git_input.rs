//! Real grant-free producer through evaluation and completion, with no sighting.

use super::*;
use crate::ObjectId;
use crate::storage::work::test_support::{checkpoint, completion_request};

fn object_name(label: &str) -> String {
    ObjectId::from_canonical_bytes(label.as_bytes()).as_str()[..40].to_owned()
}

fn unevaluated(bound: bool) -> Fixture {
    let mut create = root_request("project-a", "git-input-work", 1);
    if bound {
        create.acceptance_bindings = vec![AcceptanceBinding {
            criterion: 1,
            requirement: VerificationRequirement {
                check_kind: VerificationKind::Test,
                check_fingerprint: None,
            },
        }];
    }
    let mut f = fixture_with_create(
        SqliteStore::open_in_memory().unwrap(),
        crate::ControlAssurance::Advisory,
        0,
        &create,
    );
    f.request.input = RegisteredVerificationInput::Commit {
        commit: object_name("tested commit"),
    };
    f
}

fn prepared(bound: bool, fresh: bool) -> Fixture {
    let mut f = unevaluated(bound);
    f.store
        .set_acceptance_evaluation_policy(
            &AcceptanceEvaluationPolicy {
                allowed_modes: vec![AcceptanceEvaluationMode::IndependentSession],
                mechanical_basis: MechanicalBasis::Observed,
                require_source_freshness: fresh,
            },
            &actor("policy-admin"),
            "git-input-policy",
            None,
            at(5),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    f
}

fn evaluation(
    f: &Fixture,
    evidence: &ObjectId,
    revision: &str,
) -> RecordAcceptanceEvaluationRequest {
    let work = f
        .store
        .get_work_item(f.request.registration.binding.work_id)
        .unwrap();
    RecordAcceptanceEvaluationRequest {
        supersedes: None,
        project_id: f.project.clone(),
        work_id: work.work_id,
        expected_work_revision: work.revision,
        evaluated_through: f
            .store
            .work_feed_head(&FeedId::RunExecution(f.request.registration.binding.run_id))
            .unwrap(),
        mode: AcceptanceEvaluationMode::IndependentSession,
        execution_identity: None,
        parent_session: None,
        evaluator_model: None,
        source_basis: Some(AcceptanceSourceBasis {
            workspace_id: Some("workspace-a".into()),
            fingerprint: revision.into(),
        }),
        verdicts: vec![CriterionVerdictInput {
            criterion: 1,
            verdict: AcceptanceVerdict::Pass,
            basis: AcceptanceBasis::Observed,
            rationale: "registered test passed on the judged Git input".into(),
            evidence: vec![evidence.clone()],
        }],
        evaluator: actor("independent-evaluator"),
        attempt_key: None,
        recorded_at: at(12),
    }
}

fn record_evaluation(
    f: &mut Fixture,
    input: &RecordAcceptanceEvaluationRequest,
) -> Result<crate::storage::AcceptanceEvaluationReceipt, StoreError> {
    f.store
        .record_acceptance_evaluation(input, &DevelopmentNoopRedactor)
}

fn complete(
    f: &mut Fixture,
    evidence: &ObjectId,
    input: &str,
    key: &str,
) -> Result<CompletionSeal, StoreError> {
    let work = f
        .store
        .get_work_item(f.request.registration.binding.work_id)
        .unwrap();
    let held = f.store.claim_fixture(work.work_id);
    checkpoint(
        &mut f.store,
        &work,
        &held,
        "runner",
        &format!("{key}-checkpoint"),
        13,
        std::slice::from_ref(evidence),
    );
    let mut request = completion_request(&work, &held, "runner", evidence, key, 14);
    request.acceptance.clear();
    request.source_fingerprint = Some(input.into());
    f.store.complete_work(&request, &DevelopmentNoopRedactor)
}

#[test]
fn registered_git_input_off_without_sighting_admits_and_completes() {
    for bound in [false, true] {
        for fresh in [false, true] {
            let mut f = prepared(bound, fresh);
            let receipt = f.record().unwrap();
            let id = &receipt.verifications[0].record_id;
            let revision = f.request.input.revision();
            let input = evaluation(&f, id, &revision);
            record_evaluation(&mut f, &input).unwrap();
            let sealed = complete(&mut f, id, &revision, "complete").unwrap();
            assert!(sealed.acceptance[0].satisfied);
            let observations: i64 = f.store.connection.query_row(
                "SELECT count(*) FROM objects WHERE object_kind IN ('execution_observation','turn_grant')", [], |row| row.get(0),
            ).unwrap();
            assert_eq!(
                observations, 0,
                "neither a grant nor a sighting was fabricated"
            );
        }
    }
}

#[test]
fn registered_git_input_mismatch_refuses_bound_and_unbound_citations_without_effects() {
    for bound in [false, true] {
        for staged in [false, true] {
            let mut f = prepared(bound, true);
            if staged {
                f.request.input = RegisteredVerificationInput::StagedTree {
                    tree: GitTreeId::parse(object_name("tested tree")).unwrap(),
                    base_commit: object_name("base"),
                };
            }
            let receipt = f.record().unwrap();
            let id = &receipt.verifications[0].record_id;
            let wrong = format!(
                "{}:{}",
                if staged { "git-tree" } else { "git-commit" },
                object_name("different input")
            );
            let input = evaluation(&f, id, &wrong);
            let before = f.snapshot();
            let reason = record_evaluation(&mut f, &input).unwrap_err().to_string();
            assert!(
                reason.contains("selected registered verification"),
                "{reason}"
            );
            assert!(
                reason.contains(&format!(
                    "--source-fingerprint {}",
                    f.request.input.revision()
                )),
                "{reason}"
            );
            assert!(!reason.contains("is bound to"), "{reason}");
            assert_eq!(f.snapshot(), before);
            let revision = f.request.input.revision();
            let input = evaluation(&f, id, &revision);
            record_evaluation(&mut f, &input).unwrap();
            assert!(complete(&mut f, id, &wrong, "wrong-completion").is_err());
            complete(&mut f, id, &revision, "right-completion").unwrap();
        }
    }
}

#[test]
fn registered_git_input_staged_identity_ignores_base_provenance() {
    let mut f = prepared(true, true);
    let tree = GitTreeId::parse(object_name("staged input")).unwrap();
    f.request.input = RegisteredVerificationInput::StagedTree {
        tree: tree.clone(),
        base_commit: object_name("first base"),
    };
    let receipt = f.record().unwrap();
    let id = &receipt.verifications[0].record_id;
    let revision = RegisteredVerificationInput::StagedTree {
        tree,
        base_commit: object_name("other base"),
    }
    .revision();
    let input = evaluation(&f, id, &revision);
    record_evaluation(&mut f, &input).unwrap();
    complete(&mut f, id, &revision, "complete-tree").unwrap();
}

#[test]
fn registered_git_input_dirty_before_cannot_be_rehabilitated_by_a_later_commit() {
    let mut f = prepared(true, true);
    f.request.post_run.clean_before = false;
    let before = f.snapshot();
    assert!(f.record().is_err());
    f.request.input = RegisteredVerificationInput::Commit {
        commit: object_name("later clean commit"),
    };
    assert!(f.record().is_err());
    assert_eq!(f.snapshot(), before);
}

#[test]
fn registered_git_input_failed_aggregate_never_admits_a_passing_subcheck() {
    for outcome in [
        RegisteredVerificationOutcome::Failed,
        RegisteredVerificationOutcome::Indeterminate,
    ] {
        let mut f = prepared(true, true);
        f.request.run.outcome = outcome;
        let receipt = f.record().unwrap();
        let revision = f.request.input.revision();
        let input = evaluation(&f, &receipt.verifications[0].record_id, &revision);
        let before = f.snapshot();
        assert!(record_evaluation(&mut f, &input).is_err());
        assert_eq!(f.snapshot(), before);
    }
}

#[test]
fn registered_git_input_requires_the_cited_record_within_cut_and_its_root() {
    for wrong_root in [false, true] {
        let mut f = prepared(true, true);
        let before_record = f
            .store
            .work_feed_head(&FeedId::RunExecution(f.request.registration.binding.run_id))
            .unwrap();
        let receipt = f.record().unwrap();
        let revision = f.request.input.revision();
        let mut input = evaluation(&f, &receipt.verifications[0].record_id, &revision);
        if wrong_root {
            input.source_basis.as_mut().unwrap().workspace_id = Some("other-root".into());
        } else {
            input.evaluated_through = before_record;
        }
        let before = f.snapshot();
        assert!(record_evaluation(&mut f, &input).is_err());
        assert_eq!(f.snapshot(), before);
    }
}

#[test]
fn registered_git_input_open_evaluation_replay_and_completed_seal_remain_frozen() {
    let mut f = prepared(true, true);
    let receipt = f.record().unwrap();
    let id = &receipt.verifications[0].record_id;
    let revision = f.request.input.revision();
    let input = evaluation(&f, id, &revision);
    let original = record_evaluation(&mut f, &input).unwrap();
    let replay = record_evaluation(&mut f, &input).unwrap();
    assert!(replay.replayed);
    assert_eq!(original.evaluation, replay.evaluation);
    let seal = complete(&mut f, id, &revision, "complete-window").unwrap();
    let replay = record_evaluation(&mut f, &input).unwrap();
    assert!(replay.replayed);
    assert_eq!(original.record, replay.record);
    let run = f
        .store
        .get_work_run(f.request.registration.binding.run_id)
        .unwrap();
    assert_eq!(
        f.store
            .completion_seal_at(&run.completion_seal.unwrap())
            .unwrap(),
        seal
    );
}

#[test]
fn registered_git_input_content_observations_do_not_replace_the_judged_commit() {
    let mut f = prepared(true, true);
    let receipt = f.record().unwrap();
    let id = &receipt.verifications[0].record_id;
    let revision = f.request.input.revision();
    let work = f
        .store
        .get_work_item(f.request.registration.binding.work_id)
        .unwrap();
    let held = f.store.claim_fixture(work.work_id);
    for (key, second) in [
        ("content-before-evaluation", 11),
        ("content-after-evaluation", 12),
    ] {
        let mut observation = crate::storage::work::test_support::source_mutation_observation(
            &f.store.connection,
            &work,
            &held,
            "runner",
            key,
            second,
            Some(ExecutionSourceBasis {
                git_tree: None,
                workspace_id: "workspace-a".into(),
                source_revision: format!(
                    "content-v1:{}",
                    ObjectId::from_canonical_bytes(key.as_bytes())
                ),
                source_root_generation: Some(1),
                source_root_state: Some(SourceRootState::Named),
            }),
            None,
        );
        // A later content sighting is not a later source mutation. Actual
        // mutations retain their time floors, tested separately below.
        observation.source_changed = false;
        observation.effect = crate::domain::EffectClass::Observe;
        let tx = f.store.connection.transaction().unwrap();
        crate::storage::work::test_support::append_source_mutation_on(&tx, &observation);
        tx.commit().unwrap();
        if second == 11 {
            let input = evaluation(&f, id, &revision);
            record_evaluation(&mut f, &input).unwrap();
        }
    }
    complete(&mut f, id, &revision, "complete-content-window").unwrap();
}

#[test]
fn registered_git_input_late_registration_cannot_test_a_later_change() {
    for operator_rule in [false, true] {
        let mut f = unevaluated(true);
        if operator_rule {
            f.store
                .set_obligation_rule_set(
                    &crate::domain::ObligationRuleSet {
                        schema_version: crate::domain::OBLIGATION_RULE_SET_SCHEMA_VERSION,
                        rules: vec![crate::domain::ObligationRuleDefinition {
                            rule: crate::domain::BuiltinObligationRuleRef {
                                rule_id: "operator-test-after-change".into(),
                                rule_version: 1,
                            },
                            trigger: crate::domain::BuiltinObligationTrigger::SourceChanged,
                            requirement: VerificationRequirement {
                                check_kind: VerificationKind::Test,
                                check_fingerprint: None,
                            },
                        }],
                    },
                    &actor("rule-admin"),
                    "operator-rule",
                    None,
                    at(5),
                    &DevelopmentNoopRedactor,
                )
                .unwrap();
        }
        let work = f
            .store
            .get_work_item(f.request.registration.binding.work_id)
            .unwrap();
        let held = f.store.claim_fixture(work.work_id);
        // The check completed at 9, this actual change was observed at 11,
        // and only then does the host deliver the run's terminal result.
        crate::storage::work::test_support::source_mutation_from_basis(
            &mut f.store,
            &work,
            &held,
            "runner",
            "later-change",
            11,
            Some(ExecutionSourceBasis {
                git_tree: None,
                workspace_id: "workspace-a".into(),
                source_revision: "content-v1:later-change".into(),
                source_root_generation: Some(1),
                source_root_state: Some(SourceRootState::Named),
            }),
            None,
        );
        let receipt = f
            .store
            .record_registered_verification(
                &f.project,
                &f.session,
                &f.connection,
                &f.routing,
                &actor("runner"),
                &f.request,
                at(12),
            )
            .unwrap();
        let id = &receipt.verifications[0].record_id;
        let assessment = f
            .store
            .verification_assessment(work.work_id, id, None, 20)
            .unwrap()
            .unwrap();
        assert!(!assessment.rows.is_empty());
        assert!(
            assessment.rows.iter().all(|row| matches!(
                row.assessment,
                crate::control::ObligationAssessment::Mismatch(
                    crate::domain::VerificationEvidenceMismatch::InvalidTime
                )
            )),
            "{assessment:?}"
        );
        assert!(assessment.rows.iter().all(
            |row| row.recorded == crate::storage::work::completion::RecordedObligationEnd::Open
        ));
        checkpoint(
            &mut f.store,
            &work,
            &held,
            "runner",
            "late-checkpoint",
            13,
            std::slice::from_ref(id),
        );
        let request = completion_request(&work, &held, "runner", id, "late-completion", 14);
        assert!(
            f.store
                .complete_work(&request, &DevelopmentNoopRedactor)
                .is_err(),
            "a bound criterion cannot use a check preceding the change"
        );
    }
}

#[test]
fn registered_git_input_unevaluated_completion_uses_recorded_input_unless_supplied() {
    let mut f = unevaluated(true);
    let receipt = f.record().unwrap();
    let id = &receipt.verifications[0].record_id;
    let work = f
        .store
        .get_work_item(f.request.registration.binding.work_id)
        .unwrap();
    let held = f.store.claim_fixture(work.work_id);
    checkpoint(
        &mut f.store,
        &work,
        &held,
        "runner",
        "unevaluated-checkpoint",
        13,
        std::slice::from_ref(id),
    );
    let mut request = completion_request(&work, &held, "runner", id, "unevaluated-done", 14);
    request.source_fingerprint = Some(format!("git-commit:{}", object_name("wrong commit")));
    let reason = f
        .store
        .complete_work(&request, &DevelopmentNoopRedactor)
        .unwrap_err()
        .to_string();
    assert!(
        reason.contains(&format!(
            "--source-fingerprint {}",
            f.request.input.revision()
        )),
        "{reason}"
    );
    assert!(
        reason.contains("omit the declaration for unevaluated completion"),
        "{reason}"
    );
    assert!(!reason.contains("latest source change"), "{reason}");
    request.source_fingerprint = None;
    let seal = f
        .store
        .complete_work(&request, &DevelopmentNoopRedactor)
        .unwrap();
    assert!(seal.acceptance[0].satisfied);
}

#[test]
fn registered_git_input_missing_declaration_names_the_exact_remedy_for_unbound_pass() {
    let mut f = prepared(false, false);
    let receipt = f.record().unwrap();
    let id = &receipt.verifications[0].record_id;
    let mut input = evaluation(&f, id, &f.request.input.revision());
    input.source_basis = None;
    let before = f.snapshot();
    let reason = record_evaluation(&mut f, &input).unwrap_err().to_string();
    assert!(
        reason.contains("criterion 1 selected registered verification"),
        "{reason}"
    );
    assert!(
        reason.contains(&format!(
            "--source-fingerprint {}",
            f.request.input.revision()
        )),
        "{reason}"
    );
    assert!(!reason.contains("is bound to"), "{reason}");
    assert_eq!(f.snapshot(), before);
}

fn actual_change(f: &mut Fixture, unadmitted: bool, second: i64) {
    use crate::domain::{
        ExecutionObserveInput, MeasuredBaseline, MeasuredSighting, ObservationCausality,
        ObservationPolicyBasis, ObservationRootBasis, ObservedInterval, ObservedOccurrence,
        ObservedSourceChange,
    };
    let basis = ExecutionSourceBasis {
        git_tree: None,
        workspace_id: "workspace-a".into(),
        source_revision: format!(
            "content-v1:{}",
            ObjectId::from_canonical_bytes(b"changed content")
        ),
        source_root_generation: Some(1),
        source_root_state: Some(SourceRootState::Named),
    };
    let binding = f.request.registration.binding.clone();
    if !unadmitted {
        let work = f.store.get_work_item(binding.work_id).unwrap();
        let held = f.store.claim_fixture(work.work_id);
        crate::storage::work::test_support::source_mutation_from_basis(
            &mut f.store,
            &work,
            &held,
            "runner",
            "actual-change",
            second,
            Some(basis),
            None,
        );
        return;
    }
    let root = f
        .store
        .read_named_root(
            &f.project,
            &f.session,
            &f.connection,
            &f.routing,
            binding.run_id,
            binding.claim_id,
        )
        .unwrap();
    let policy = SqliteStore::load_active_control_policy(&f.store.connection).unwrap();
    let receipt = f
        .store
        .record_unadmitted_execution_observation(
            &f.project,
            &f.session,
            &f.connection,
            &f.routing,
            &actor("runner"),
            ExecutionObserveInput {
                idempotency_key: "actual-unadmitted-change".into(),
                binding,
                root_basis: ObservationRootBasis {
                    capture_run_cut: root.read_cut.position,
                    latest_event: root.latest_event.as_ref().map(|event| event.event.clone()),
                    state: root.named_root,
                },
                observed_interval: ObservedInterval {
                    from: at(second - 2),
                    through: at(second - 1),
                },
                occurrence: ObservedOccurrence::InterTurnChange {
                    source_change: ObservedSourceChange::ContentComparison {
                        workspace_id: "workspace-a".into(),
                        baseline: MeasuredBaseline {
                            workspace_id: "workspace-a".into(),
                            source_revision: "content-before".into(),
                            observed_at: at(second - 2),
                        },
                        sighting: MeasuredSighting {
                            source_basis: basis,
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
            },
            at(second),
        )
        .unwrap();
    assert!(
        matches!(
            receipt.accounting,
            crate::domain::ObservationAccounting::SourceChange { .. }
        ),
        "the test must exercise an accounted change: {receipt:?}"
    );
}

#[test]
fn registered_git_input_citations_retain_change_floors_despite_late_terminal_arrival() {
    for bound in [false, true] {
        for unadmitted in [false, true] {
            for change_before_check in [false, true] {
                let mut f = prepared(bound, true);
                actual_change(&mut f, unadmitted, if change_before_check { 7 } else { 11 });
                let receipt = f
                    .store
                    .record_registered_verification(
                        &f.project,
                        &f.session,
                        &f.connection,
                        &f.routing,
                        &actor("runner"),
                        &f.request,
                        at(12),
                    )
                    .unwrap();
                let id = &receipt.verifications[0].record_id;
                let mut input = evaluation(&f, id, &f.request.input.revision());
                input.recorded_at = at(13);
                let before = f.snapshot();
                let recorded = record_evaluation(&mut f, &input);
                if change_before_check {
                    recorded.unwrap();
                } else {
                    let reason = recorded.unwrap_err().to_string();
                    assert!(
                        reason.contains("which the check did not follow"),
                        "{reason}"
                    );
                    assert_eq!(f.snapshot(), before);
                }
            }
        }
    }
}

#[test]
fn registered_git_input_post_cut_actual_changes_refuse_admission_and_stale_consumption() {
    use crate::domain::AcceptanceStaleReason;
    for bound in [false, true] {
        for unadmitted in [false, true] {
            for fresh in [false, true] {
                for after_recording in [false, true] {
                    let mut f = prepared(bound, fresh);
                    let receipt = f.record().unwrap();
                    let id = &receipt.verifications[0].record_id;
                    let revision = f.request.input.revision();
                    let input = evaluation(&f, id, &revision);
                    if after_recording {
                        record_evaluation(&mut f, &input).unwrap();
                    }
                    actual_change(&mut f, unadmitted, if after_recording { 13 } else { 11 });
                    if after_recording {
                        let status = f
                            .store
                            .acceptance_evaluation_status(
                                f.request.registration.binding.work_id,
                                Some(&revision),
                            )
                            .unwrap()
                            .unwrap();
                        assert_eq!(
                            status.stale,
                            Some(if unadmitted {
                                AcceptanceStaleReason::UnadmittedChange
                            } else {
                                AcceptanceStaleReason::Mutation
                            })
                        );
                        assert!(complete(&mut f, id, &revision, "changed-after-cut").is_err());
                    } else {
                        let before = f.snapshot();
                        assert!(matches!(
                            record_evaluation(&mut f, &input),
                            Err(StoreError::AcceptanceEvaluationBasisMoved {
                                moved: crate::storage::EvaluationBasisMove::SourceChanged,
                                ..
                            })
                        ));
                        assert_eq!(f.snapshot(), before);
                    }
                }
            }
        }
    }
}
