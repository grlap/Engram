//! Remote results through live committed-input decisions and the retained
//! test-ref time, mutation and cut guards, with no sighting of their own.

use super::*;

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
            "remote-input-policy",
            None,
            at(5),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    f
}

fn unevaluated(bound: bool) -> Fixture {
    let mut create = root_request("project-a", "remote-input-work", 1);
    if bound {
        create.acceptance_bindings = vec![AcceptanceBinding {
            criterion: 1,
            requirement: VerificationRequirement {
                check_kind: VerificationKind::Test,
                check_fingerprint: None,
            },
        }];
    }
    let mut fixture = fixture_in(SqliteStore::open_in_memory().unwrap(), 0, Some(&create));
    // Report-based ordering remains the contract for the test-ref lane.
    // Committed-input cases below attach an actual named Git root instead.
    fixture.request.association.intended_use = test_ref();
    fixture
}

fn git(root: &std::path::Path, args: &[&str]) -> String {
    let output = std::process::Command::new("git")
        .current_dir(root)
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

fn commit(root: &std::path::Path) -> String {
    git(
        root,
        &[
            "-c",
            "user.name=Test",
            "-c",
            "user.email=test@example.invalid",
            "commit",
            "--no-gpg-sign",
            "-qm",
            "remote fixture",
            "--allow-empty",
        ],
    );
    git(root, &["rev-parse", "HEAD"])
}

fn real_git_root(
    f: &mut Fixture,
    intended: RemoteVerificationUse,
) -> crate::test_support::TempHome {
    let home = crate::test_support::temp_home().unwrap();
    let root = home.path().join("source");
    std::fs::create_dir(&root).unwrap();
    git(&root, &["init", "-q", "--object-format=sha1"]);
    std::fs::write(root.join("tracked"), "checked\n").unwrap();
    git(&root, &["add", "tracked"]);
    let checked = commit(&root);
    let tree = GitTreeId::parse(git(&root, &["rev-parse", "HEAD^{tree}"])).unwrap();
    let workspace = root.canonicalize().unwrap().to_string_lossy().into_owned();
    let binding = &f.request.association.binding;
    f.store
        .bind_named_root(
            &f.project,
            &f.session,
            &f.connection,
            crate::storage::HostSessionAuthority::Work,
            binding.claim_id,
            binding.claim_fence,
            &workspace,
            2,
            at(4),
            NamedRootBindingKind::Bound,
            None,
            &mut actor("runner"),
            "name-real-git",
            at(4),
        )
        .unwrap();
    f.request.association.root.workspace_id = workspace;
    f.request.association.root.generation = 2;
    f.request.completion.input.commit = Some(checked.clone());
    f.request.completion.input.tree = Some(tree.clone());
    f.request.association.intended_use = match intended {
        RemoteVerificationUse::CommittedInput { purpose, .. } => {
            RemoteVerificationUse::CommittedInput {
                expected_commit: checked,
                purpose,
            }
        }
        RemoteVerificationUse::AuthorizedTestRef {
            ref_name, purpose, ..
        } => RemoteVerificationUse::AuthorizedTestRef {
            ref_name,
            expected_tree: tree,
            purpose,
        },
    };
    home
}

fn evaluation(
    f: &Fixture,
    evidence: &ObjectId,
    revision: &str,
) -> RecordAcceptanceEvaluationRequest {
    let binding = &f.request.association.binding;
    let work = f.store.get_work_item(binding.work_id).unwrap();
    RecordAcceptanceEvaluationRequest {
        supersedes: None,
        project_id: f.project.clone(),
        work_id: work.work_id,
        expected_work_revision: work.revision,
        evaluated_through: f
            .store
            .work_feed_head(&FeedId::RunExecution(binding.run_id))
            .unwrap(),
        mode: AcceptanceEvaluationMode::IndependentSession,
        execution_identity: None,
        parent_session: None,
        evaluator_model: None,
        source_basis: Some(AcceptanceSourceBasis {
            workspace_id: Some(f.request.association.root.workspace_id.clone()),
            fingerprint: revision.into(),
        }),
        verdicts: vec![CriterionVerdictInput {
            criterion: 1,
            verdict: AcceptanceVerdict::Pass,
            basis: AcceptanceBasis::Observed,
            rationale: "the remote test passed on the judged Git input".into(),
            evidence: vec![evidence.clone()],
        }],
        evaluator: actor("independent-evaluator"),
        attempt_key: None,
        recorded_at: at(12),
    }
}

fn complete(
    f: &mut Fixture,
    evidence: &ObjectId,
    input: &str,
    key: &str,
) -> Result<CompletionSeal, StoreError> {
    let work = f
        .store
        .get_work_item(f.request.association.binding.work_id)
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
    if f.store
        .acceptance_evaluation_policy()
        .unwrap()
        .is_evaluated()
    {
        request.acceptance.clear();
    }
    request.source_fingerprint = Some(input.into());
    f.store.complete_work(&request, &DevelopmentNoopRedactor)
}

fn change(f: &mut Fixture, key: &str, second: i64) {
    let work = f
        .store
        .get_work_item(f.request.association.binding.work_id)
        .unwrap();
    let held = f.store.claim_fixture(work.work_id);
    crate::storage::work::test_support::source_mutation_from_basis(
        &mut f.store,
        &work,
        &held,
        "runner",
        key,
        second,
        Some(ExecutionSourceBasis {
            git_tree: None,
            workspace_id: "workspace-a".into(),
            source_revision: format!("content-v1:{key}"),
            source_root_generation: Some(1),
            source_root_state: Some(SourceRootState::Named),
        }),
        None,
    );
}

/// Records one host observation of this claim at its run's current head, as
/// live stores receive them, under the project's current policy.
pub(super) fn observe(
    f: &mut Fixture,
    key: &str,
    second: i64,
    occurrence: ObservedOccurrence,
) -> ExecutionObservationReceipt {
    let binding = f.request.association.binding.clone();
    let root = f
        .store
        .read_named_root(
            &f.project,
            &f.session,
            &f.connection,
            crate::storage::HostSessionAuthority::Work,
            binding.run_id,
            binding.claim_id,
        )
        .unwrap();
    let policy = SqliteStore::load_active_control_policy(&f.store.connection).unwrap();
    let mut observer = actor("runner");
    observer.run_id = Some(binding.run_id.0.to_string());
    f.store
        .record_unadmitted_execution_observation(
            &f.project,
            &f.session,
            &f.connection,
            crate::storage::HostSessionAuthority::Work,
            &observer,
            ExecutionObserveInput {
                idempotency_key: key.into(),
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
                occurrence,
                causality: ObservationCausality::Unknown {},
                policy_basis: ObservationPolicyBasis::AccountIfEligible {
                    project_policy_epoch: policy.epoch,
                    policy: policy.policy_id,
                    obligation_rule_set: policy.obligation_rule_set,
                },
            },
            at(second),
        )
        .unwrap()
}

/// The two changes live stores receive between turns.
#[derive(Clone, Copy, Debug)]
enum LiveChange {
    /// Accounted, with a measured revision.
    Unadmitted,
    /// Accounted from file notifications alone: no revision and no located source.
    WatcherOnly,
}

const LIVE_CHANGES: [LiveChange; 2] = [LiveChange::Unadmitted, LiveChange::WatcherOnly];

fn live_change(f: &mut Fixture, kind: LiveChange, key: &str, second: i64) {
    let workspace = f.request.association.root.workspace_id.clone();
    let source_change = match kind {
        LiveChange::Unadmitted => ObservedSourceChange::ContentComparison {
            workspace_id: workspace.clone(),
            baseline: MeasuredBaseline {
                workspace_id: workspace.clone(),
                source_revision: "content-before".into(),
                observed_at: at(second - 2),
            },
            sighting: MeasuredSighting {
                source_basis: ExecutionSourceBasis {
                    git_tree: None,
                    workspace_id: workspace.clone(),
                    source_revision: format!(
                        "content-v1:{}",
                        ObjectId::from_canonical_bytes(key.as_bytes())
                    ),
                    source_root_generation: Some(f.request.association.root.generation),
                    source_root_state: Some(SourceRootState::Named),
                },
                observed_at: at(second - 1),
            },
        },
        LiveChange::WatcherOnly => ObservedSourceChange::WatcherOnly {
            workspace_id: workspace,
            observed_at: at(second - 1),
        },
    };
    let receipt = observe(
        f,
        key,
        second,
        ObservedOccurrence::InterTurnChange { source_change },
    );
    assert!(
        matches!(
            receipt.accounting,
            ObservationAccounting::SourceChange { .. }
        ),
        "{kind:?} must be an accounted change: {receipt:?}"
    );
}

#[test]
fn remote_verification_late_association_cannot_test_a_later_live_change() {
    for kind in LIVE_CHANGES {
        let mut f = unevaluated(true);
        // The run finished at 9, the change came at 11, and only then the result.
        live_change(&mut f, kind, "later-live-change", 11);
        let receipt = f.record_at(12).unwrap();
        assert!(receipt.qualification.credited, "{receipt:?}");
        let id = &receipt.verifications[0].record_id;
        let work_id = f.request.association.binding.work_id;
        let assessment = f
            .store
            .verification_assessment(work_id, id, None, 20)
            .unwrap()
            .unwrap();
        assert!(!assessment.rows.is_empty(), "{kind:?}");
        assert!(
            assessment.rows.iter().all(|row| !matches!(
                row.assessment,
                crate::control::ObligationAssessment::Matches
            )),
            "{kind:?}: {assessment:?}"
        );
        let work = f.store.get_work_item(work_id).unwrap();
        let held = f.store.claim_fixture(work_id);
        checkpoint(
            &mut f.store,
            &work,
            &held,
            "runner",
            "late-live-checkpoint",
            13,
            std::slice::from_ref(id),
        );
        let request = completion_request(&work, &held, "runner", id, "late-live-completion", 14);
        assert!(
            f.store
                .complete_work(&request, &DevelopmentNoopRedactor)
                .is_err(),
            "{kind:?}: a bound criterion cannot use a check preceding the change"
        );
    }
}

#[test]
fn remote_verification_a_later_live_change_moves_the_evaluation_basis() {
    for kind in LIVE_CHANGES {
        let mut f = prepared(true, true);
        let receipt = f.record().unwrap();
        let id = &receipt.verifications[0].record_id;
        let revision = f.request.association.intended_use.revision();
        let input = evaluation(&f, id, &revision);
        live_change(&mut f, kind, "after-the-check", 11);
        let before = f.snapshot();
        assert!(
            matches!(
                f.store
                    .record_acceptance_evaluation(&input, &DevelopmentNoopRedactor),
                Err(StoreError::AcceptanceEvaluationBasisMoved {
                    moved: crate::storage::EvaluationBasisMove::SourceChanged,
                    ..
                })
            ),
            "{kind:?}"
        );
        assert_eq!(f.snapshot(), before, "{kind:?}");
    }
}

#[test]
fn remote_verification_cut_guards_hold_around_a_live_change() {
    for kind in LIVE_CHANGES {
        let mut f = prepared(true, true);
        live_change(&mut f, kind, "before-the-check", 7);
        let before_record = f
            .store
            .work_feed_head(&FeedId::RunExecution(f.request.association.binding.run_id))
            .unwrap();
        let receipt = f.record().unwrap();
        let id = &receipt.verifications[0].record_id;
        let revision = f.request.association.intended_use.revision();
        let mut input = evaluation(&f, id, &revision);
        input.evaluated_through = before_record;
        let before = f.snapshot();
        assert!(
            f.store
                .record_acceptance_evaluation(&input, &DevelopmentNoopRedactor)
                .is_err(),
            "{kind:?}: a citation beyond the cut"
        );
        assert_eq!(f.snapshot(), before, "{kind:?}");
        // The run really finished after the change, so at the head it counts.
        let input = evaluation(&f, id, &revision);
        f.store
            .record_acceptance_evaluation(&input, &DevelopmentNoopRedactor)
            .unwrap();
    }
}

/// The time guards compare a remote finish time with local times, so they read
/// the origin's clock as if it were this one. The limit is stated in the docs,
/// and these are its two sides; no host-attested time exists to replace it.
#[test]
fn remote_verification_cross_host_order_assumes_comparable_clocks() {
    // The run really ended at 9; a local change came at 39; the delivery at 69.
    // With the origin's clock 45 seconds ahead it states 54, and passes as if
    // it had followed the change.
    let ahead = at(9 + 45);
    let mut f = prepared(true, true);
    f.request.completion.finished_at = Some(ahead);
    change(&mut f, "local-change", 39);
    let receipt = f.record_at(69).unwrap();
    assert!(receipt.qualification.credited, "{receipt:?}");
    let id = &receipt.verifications[0].record_id;
    let work_id = f.request.association.binding.work_id;
    let assessment = f
        .store
        .verification_assessment(work_id, id, None, 20)
        .unwrap()
        .unwrap();
    assert!(!assessment.rows.is_empty());
    assert!(
        assessment.rows.iter().all(|row| matches!(
            row.assessment,
            crate::control::ObligationAssessment::Matches
        )),
        "the known limit: {assessment:?}"
    );
    // Whatever the clocks say, the credit still names the exact input tested.
    let revision = f.request.association.intended_use.revision();
    let mut wrong = evaluation(&f, id, &format!("git-commit:{}", object_name("other")));
    wrong.recorded_at = at(70);
    assert!(
        f.store
            .record_acceptance_evaluation(&wrong, &DevelopmentNoopRedactor)
            .is_err()
    );
    let mut right = evaluation(&f, id, &revision);
    right.recorded_at = at(70);
    f.store
        .record_acceptance_evaluation(&right, &DevelopmentNoopRedactor)
        .unwrap();

    // A clock running behind only withholds: this run really followed the change.
    let mut f = unevaluated(true);
    f.request.completion.finished_at = Some(at(30));
    change(&mut f, "local-change", 39);
    let receipt = f.record_at(69).unwrap();
    let id = &receipt.verifications[0].record_id;
    let assessment = f
        .store
        .verification_assessment(f.request.association.binding.work_id, id, None, 20)
        .unwrap()
        .unwrap();
    assert!(!assessment.rows.is_empty());
    assert!(
        assessment.rows.iter().all(|row| matches!(
            row.assessment,
            crate::control::ObligationAssessment::Mismatch(
                VerificationEvidenceMismatch::InvalidTime
            )
        )),
        "{assessment:?}"
    );
}

#[test]
fn remote_verification_admits_evaluation_and_completion_with_no_sighting() {
    for bound in [false, true] {
        for fresh in [false, true] {
            for intended_use in [
                committed(RemoteVerificationPurpose::Verification),
                committed(RemoteVerificationPurpose::LandingGate),
                test_ref(),
            ] {
                let mut f = prepared(bound, fresh);
                let _source = real_git_root(&mut f, intended_use);
                let receipt = f.record().unwrap();
                let id = &receipt.verifications[0].record_id;
                let revision = f.request.association.intended_use.revision();
                let input = evaluation(&f, id, &revision);
                f.store
                    .record_acceptance_evaluation(&input, &DevelopmentNoopRedactor)
                    .unwrap();
                let sealed = complete(&mut f, id, &revision, "complete").unwrap();
                assert!(sealed.acceptance[0].satisfied);
                assert_eq!(f.count("execution_observation"), 0);
                assert_eq!(f.count("turn_grant"), 0);
                f.assert_healthy();
            }
        }
    }
}

#[test]
fn remote_verification_unevaluated_bound_criterion_is_satisfied_by_the_credit() {
    let mut f = unevaluated(true);
    let _source = real_git_root(&mut f, committed(RemoteVerificationPurpose::Verification));
    let receipt = f.record().unwrap();
    let id = &receipt.verifications[0].record_id;
    let work = f
        .store
        .get_work_item(f.request.association.binding.work_id)
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
    let request = completion_request(&work, &held, "runner", id, "unevaluated-done", 14);
    let seal = f
        .store
        .complete_work(&request, &DevelopmentNoopRedactor)
        .unwrap();
    assert!(seal.acceptance[0].satisfied);
}

#[test]
fn remote_verification_a_different_judged_input_refuses_without_effects() {
    for fresh in [false, true] {
        let mut f = prepared(true, fresh);
        let receipt = f.record().unwrap();
        let id = &receipt.verifications[0].record_id;
        let wrong = format!("git-commit:{}", object_name("different input"));
        let input = evaluation(&f, id, &wrong);
        let before = f.snapshot();
        assert!(
            f.store
                .record_acceptance_evaluation(&input, &DevelopmentNoopRedactor)
                .is_err()
        );
        assert_eq!(f.snapshot(), before);
    }
}

#[test]
fn remote_verification_late_association_of_an_old_execution_cannot_test_a_later_change() {
    // The run finished at 9, this change was observed at 11, and only then is
    // the result delivered: the local association time must not hide that.
    let mut f = unevaluated(true);
    change(&mut f, "later-change", 11);
    let receipt = f.record_at(12).unwrap();
    assert!(receipt.qualification.credited, "{receipt:?}");
    let id = &receipt.verifications[0].record_id;
    let work_id = f.request.association.binding.work_id;
    let assessment = f
        .store
        .verification_assessment(work_id, id, None, 20)
        .unwrap()
        .unwrap();
    assert!(!assessment.rows.is_empty());
    assert!(
        assessment.rows.iter().all(|row| matches!(
            row.assessment,
            crate::control::ObligationAssessment::Mismatch(
                VerificationEvidenceMismatch::InvalidTime
            )
        )),
        "{assessment:?}"
    );
    let work = f.store.get_work_item(work_id).unwrap();
    let held = f.store.claim_fixture(work_id);
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

    // A result that really finished after the change does account for it.
    let mut f = unevaluated(true);
    change(&mut f, "earlier-change", 7);
    let receipt = f.record_at(12).unwrap();
    let id = &receipt.verifications[0].record_id;
    let work_id = f.request.association.binding.work_id;
    let assessment = f
        .store
        .verification_assessment(work_id, id, None, 20)
        .unwrap()
        .unwrap();
    assert!(!assessment.rows.is_empty());
    assert!(
        assessment.rows.iter().all(|row| matches!(
            row.assessment,
            crate::control::ObligationAssessment::Matches
        )),
        "{assessment:?}"
    );
}

#[test]
fn remote_verification_cut_and_later_mutation_guards_hold() {
    // A citation beyond the evaluated cut.
    let mut f = prepared(true, true);
    let before_record = f
        .store
        .work_feed_head(&FeedId::RunExecution(f.request.association.binding.run_id))
        .unwrap();
    let receipt = f.record().unwrap();
    let id = &receipt.verifications[0].record_id;
    let revision = f.request.association.intended_use.revision();
    let mut input = evaluation(&f, id, &revision);
    input.evaluated_through = before_record;
    let before = f.snapshot();
    assert!(
        f.store
            .record_acceptance_evaluation(&input, &DevelopmentNoopRedactor)
            .is_err()
    );
    assert_eq!(f.snapshot(), before);

    // A source change after the check moves the evaluation's basis.
    let mut f = prepared(true, true);
    let receipt = f.record().unwrap();
    let id = &receipt.verifications[0].record_id;
    let revision = f.request.association.intended_use.revision();
    let input = evaluation(&f, id, &revision);
    change(&mut f, "after-the-check", 11);
    let before = f.snapshot();
    assert!(matches!(
        f.store
            .record_acceptance_evaluation(&input, &DevelopmentNoopRedactor),
        Err(StoreError::AcceptanceEvaluationBasisMoved {
            moved: crate::storage::EvaluationBasisMove::SourceChanged,
            ..
        })
    ));
    assert_eq!(f.snapshot(), before);
}

#[test]
fn remote_verification_committed_input_accepts_clean_local_root_at_expected_commit() {
    for bound in [false, true] {
        for purpose in [
            RemoteVerificationPurpose::Verification,
            RemoteVerificationPurpose::LandingGate,
        ] {
            let mut f = prepared(bound, true);
            let _source = real_git_root(&mut f, committed(purpose));
            let receipt = f.record().unwrap();
            assert!(receipt.qualification.credited, "{receipt:?}");
            let check = &receipt.verifications[0].record_id;
            let revision = f.request.association.intended_use.revision();
            let input = evaluation(&f, check, &revision);
            let recorded = f
                .store
                .record_acceptance_evaluation(&input, &DevelopmentNoopRedactor)
                .unwrap();
            assert_eq!(
                recorded
                    .record
                    .registered_input_decision
                    .as_ref()
                    .unwrap()
                    .verification,
                *check
            );
            let seal = complete(&mut f, check, &revision, "current-remote-done").unwrap();
            assert!(seal.acceptance[0].satisfied);
            assert_eq!(
                f.store.completion_seal_evaluation(&seal).unwrap(),
                Some(recorded.record)
            );
            f.assert_healthy();
        }
    }
}

#[test]
fn remote_verification_committed_input_uses_git_after_reports_and_real_changes() {
    for evaluated in [false, true] {
        for state in [
            "report-unadmitted",
            "report-watcher",
            "dirty",
            "new-commit",
            "committed-edit",
            "reverted",
            "missing",
        ] {
            let mut f = if evaluated {
                prepared(true, true)
            } else {
                unevaluated(true)
            };
            let _source = real_git_root(&mut f, committed(RemoteVerificationPurpose::Verification));
            let root = std::path::PathBuf::from(&f.request.association.root.workspace_id);
            let receipt = f.record().unwrap();
            assert!(receipt.qualification.credited, "{receipt:?}");
            let original = f.association(&receipt.operation);
            let check = &receipt.verifications[0].record_id;
            let revision = f.request.association.intended_use.revision();
            if evaluated {
                let mut baseline = evaluation(&f, check, &revision);
                baseline.recorded_at = at(10);
                f.store
                    .record_acceptance_evaluation(&baseline, &DevelopmentNoopRedactor)
                    .unwrap();
            }
            match state {
                "report-unadmitted" => {
                    live_change(&mut f, LiveChange::Unadmitted, "report-only", 11);
                }
                "report-watcher" => {
                    live_change(&mut f, LiveChange::WatcherOnly, "watcher-only", 11);
                }
                "dirty" => std::fs::write(root.join("tracked"), "edited\n").unwrap(),
                "new-commit" => {
                    commit(&root);
                }
                "committed-edit" => {
                    std::fs::write(root.join("tracked"), "edited\n").unwrap();
                    git(&root, &["add", "tracked"]);
                    commit(&root);
                }
                "reverted" => {
                    std::fs::write(root.join("tracked"), "edited\n").unwrap();
                    std::fs::write(root.join("tracked"), "checked\n").unwrap();
                }
                "missing" => std::fs::rename(&root, root.with_file_name("gone")).unwrap(),
                _ => unreachable!(),
            }
            let refusal = match state {
                "dirty" => Some("is dirty"),
                "new-commit" | "committed-edit" => Some("named root is at"),
                "missing" => Some("is unavailable"),
                _ => None,
            };
            if evaluated {
                let mut input = evaluation(&f, check, &revision);
                input.attempt_key = Some("after-endpoint-change".into());
                let result = f
                    .store
                    .record_acceptance_evaluation(&input, &DevelopmentNoopRedactor);
                if let Some(reason) = refusal {
                    let error = result.unwrap_err();
                    assert!(
                        matches!(error, StoreError::WorkCompletionRefused { .. }),
                        "{state}: {error}"
                    );
                    assert!(error.to_string().contains(reason), "{state}: {error}");
                } else {
                    result.unwrap_or_else(|error| panic!("{state}: {error}"));
                }
            }
            let result = complete(&mut f, check, &revision, "remote-endpoint-done");
            if let Some(reason) = refusal {
                let error = result.unwrap_err();
                assert!(
                    matches!(error, StoreError::WorkCompletionRefused { .. }),
                    "{evaluated}/{state}: {error}"
                );
                assert!(
                    error.to_string().contains(reason),
                    "{evaluated}/{state}: {error}"
                );
                assert!(
                    f.store
                        .get_work_run(f.request.association.binding.run_id)
                        .unwrap()
                        .completion_seal
                        .is_none()
                );
            } else {
                let seal = result.unwrap_or_else(|error| panic!("{evaluated}/{state}: {error}"));
                assert!(seal.acceptance[0].satisfied);
            }
            if !evaluated {
                assert_eq!(f.count("acceptance_evaluation"), 0);
            }
            assert_eq!(f.association(&receipt.operation), original);
            let report = f.store.verify_all().unwrap();
            assert!(report.is_healthy(), "{evaluated}/{state}: {report:?}");
        }
    }
}

#[test]
fn remote_verification_a_pinned_check_requirement_is_not_met_by_the_derived_fingerprint() {
    let mut create = root_request("project-a", "pinned-remote-work", 1);
    create.acceptance_bindings = vec![AcceptanceBinding {
        criterion: 1,
        requirement: VerificationRequirement {
            check_kind: VerificationKind::Test,
            check_fingerprint: Some(ObjectId::from_canonical_bytes(b"some other check")),
        },
    }];
    let mut f = fixture_in(SqliteStore::open_in_memory().unwrap(), 0, Some(&create));
    let receipt = f.record().unwrap();
    assert!(receipt.qualification.credited);
    let id = &receipt.verifications[0].record_id;
    let work_id = f.request.association.binding.work_id;
    let assessment = f
        .store
        .verification_assessment(work_id, id, None, 20)
        .unwrap()
        .unwrap();
    assert!(!assessment.rows.is_empty());
    assert!(
        assessment.rows.iter().all(|row| matches!(
            row.assessment,
            crate::control::ObligationAssessment::Mismatch(
                VerificationEvidenceMismatch::CheckFingerprintMismatch
            )
        )),
        "{assessment:?}"
    );
}
