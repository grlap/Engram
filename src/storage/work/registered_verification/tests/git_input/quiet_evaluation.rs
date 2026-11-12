//! The staged-tree and stored-witness lane keeps its historical report-based
//! interpretation. The committed endpoint lane is exercised in `live_currency`.

use super::*;
use crate::storage::work::test_support::{evidence, host_verification_from_basis};

mod evidence_capture;
mod kept_root;

fn prepared(bound: bool, fresh: bool) -> Fixture {
    prepared_in(SqliteStore::open_in_memory().unwrap(), bound, fresh)
}

fn prepared_in(store: SqliteStore, bound: bool, fresh: bool) -> Fixture {
    let mut fixture = super::prepared_in(store, bound, fresh);
    fixture.request.input = RegisteredVerificationInput::StagedTree {
        tree: GitTreeId::parse(object_name("staged checked input")).unwrap(),
        base_commit: object_name("staged provenance"),
    };
    fixture
}

fn content_revision() -> String {
    format!(
        "content-v1:{}",
        ObjectId::from_canonical_bytes(b"unchanged source content")
    )
}

pub(super) fn quiet_sighting(f: &mut Fixture, key: &str, second: i64) -> ObjectId {
    source_sighting(f, key, second, &content_revision())
}

fn source_sighting(f: &mut Fixture, key: &str, second: i64, revision: &str) -> ObjectId {
    source_sighting_delivered(f, key, second, second, revision)
}

fn source_sighting_delivered(
    f: &mut Fixture,
    key: &str,
    second: i64,
    delivered: i64,
    revision: &str,
) -> ObjectId {
    let basis = ExecutionSourceBasis {
        workspace_id: f.request.registration.root.workspace_id.clone(),
        source_revision: revision.into(),
        source_root_generation: Some(f.request.registration.root.generation),
        source_root_state: Some(SourceRootState::Named),
        git_tree: None,
    }
    .into();
    basis_sighting_delivered(f, key, second, delivered, basis)
}

fn basis_sighting_delivered(
    f: &mut Fixture,
    key: &str,
    second: i64,
    delivered: i64,
    source_basis: crate::domain::VerificationSourceBasis,
) -> ObjectId {
    let binding = f.request.registration.binding.clone();
    let mut observer = actor("runner");
    observer.run_id = Some(binding.run_id.0.to_string());
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
    let receipt = f
        .store
        .seed_historical_execution_observation(
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
                    from: at(second),
                    through: at(second),
                },
                occurrence: ObservedOccurrence::SourceSighting {
                    source_basis,
                    observed_at: at(second),
                },
                causality: ObservationCausality::Unknown {},
                policy_basis: ObservationPolicyBasis::AccountIfEligible {
                    project_policy_epoch: policy.epoch,
                    policy: policy.policy_id,
                    obligation_rule_set: policy.obligation_rule_set,
                },
            },
            at(delivered),
        )
        .unwrap();
    assert!(
        matches!(receipt.accounting, ObservationAccounting::Sighting {}),
        "the fixture must record a quiet measured sighting: {receipt:?}"
    );
    receipt.observation
}

pub(super) fn note(f: &mut Fixture) -> ObjectId {
    let work = f
        .store
        .get_work_item(f.request.registration.binding.work_id)
        .unwrap();
    let held = f.store.claim_fixture(work.work_id);
    evidence(&mut f.store, &work, &held, "runner", "review-note", 10)
}

pub(super) fn judgment(f: &Fixture, note: &ObjectId) -> RecordAcceptanceEvaluationRequest {
    let mut input = evaluation(f, note, &f.request.input.revision());
    input.verdicts[0].basis = AcceptanceBasis::Judgment;
    input.verdicts[0].rationale =
        "the unbound behavior criterion is judged from its review note".into();
    input.recorded_at = at(13);
    input
}

fn quiet_during_evaluation(cited: bool, bound: bool) {
    let mut f = prepared(bound, true);
    let receipt = f.record().unwrap();
    let check = &receipt.verifications[0].record_id;
    let note = note(&mut f);
    quiet_sighting(&mut f, "before-basis", 10);
    let mut input = if cited {
        evaluation(&f, check, &f.request.input.revision())
    } else {
        judgment(&f, &note)
    };
    input.recorded_at = at(13);
    let first = quiet_sighting(&mut f, "during-evaluation-1", 11);
    let last = quiet_sighting(&mut f, "during-evaluation-2", 12);
    assert_ne!(
        first, last,
        "two closes are distinct immutable observations"
    );
    let before = f.snapshot();
    let recorded = record_evaluation(&mut f, &input);
    if let Err(StoreError::AcceptanceEvaluationBasisMoved {
        moved: crate::storage::EvaluationBasisMove::SourceChanged,
        observation: Some(deciding),
        ..
    }) = &recorded
    {
        assert_eq!(deciding.observation, last);
        assert!(!deciding.source_changed);
        assert_eq!(
            deciding.revision.as_deref(),
            Some(content_revision().as_str())
        );
        assert_eq!(f.snapshot(), before, "refused evaluation writes nothing");
    }
    let recorded =
        recorded.expect("unchanged closes must not void the registered input, even when uncited");
    assert_eq!(
        recorded.record.registered_input_witness.as_ref(),
        (!cited).then_some(check)
    );
    let revision = f.request.input.revision();
    let sealed = complete_with_evidence(
        &mut f,
        &[check.clone(), note],
        &revision,
        "after-quiet-closes",
    )
    .unwrap();
    assert!(sealed.acceptance[0].satisfied);
    assert_eq!(
        f.store.completion_seal_evaluation(&sealed).unwrap(),
        Some(recorded.record)
    );
    let report = f.store.verify_all().unwrap();
    assert!(report.is_healthy(), "{report:?}");
}

#[test]
fn registered_staged_input_quiet_evaluation_accepts_an_uncited_check() {
    quiet_during_evaluation(false, false);
}

#[test]
fn registered_staged_input_quiet_evaluation_accepts_a_cited_check() {
    for bound in [false, true] {
        quiet_during_evaluation(true, bound);
    }
}

#[test]
fn registered_staged_input_quiet_git_identity_before_cut_cannot_corroborate() {
    let mut f = prepared(false, true);
    let receipt = f.record().unwrap();
    let check = &receipt.verifications[0].record_id;
    let note = note(&mut f);
    let other = format!(
        "git-commit:{}",
        object_name("quietly observed other commit")
    );
    source_sighting(&mut f, "different-git-before-cut", 11, &other);
    let input = judgment(&f, &note);
    let recorded = record_evaluation(&mut f, &input).unwrap();
    assert!(
        recorded.record.registered_input_witness.is_none(),
        "a quiet sighting of another Git input cannot corroborate the old check"
    );
    assert_unconfirmed_at_done(&mut f, check);
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[test]
fn registered_staged_input_quiet_git_identity_after_cut_refuses_admission() {
    let mut f = prepared(false, true);
    f.record().unwrap();
    let note = note(&mut f);
    let input = judgment(&f, &note);
    let other = format!(
        "git-commit:{}",
        object_name("quietly observed other commit")
    );
    let sighting = source_sighting(&mut f, "different-git-after-cut", 11, &other);
    let before = f.snapshot();
    let error = record_evaluation(&mut f, &input).unwrap_err();
    assert!(
        matches!(
            error,
            StoreError::AcceptanceEvaluationBasisMoved {
                moved: crate::storage::EvaluationBasisMove::SourceChanged,
                observation: Some(ref deciding),
                ..
            } if deciding.observation == sighting && !deciding.source_changed
        ),
        "the contradictory Git sighting must decide the refusal: {error:?}"
    );
    assert_eq!(f.snapshot(), before);
}

#[test]
fn registered_staged_input_quiet_git_identity_after_recording_stales_witness() {
    let mut f = prepared(false, true);
    let receipt = f.record().unwrap();
    let check = &receipt.verifications[0].record_id;
    let note = note(&mut f);
    let input = judgment(&f, &note);
    let recorded = record_evaluation(&mut f, &input).unwrap();
    assert_eq!(
        recorded.record.registered_input_witness.as_ref(),
        Some(check)
    );
    let other = format!(
        "git-commit:{}",
        object_name("quietly observed other commit")
    );
    let sighting = source_sighting(&mut f, "different-git-after-recording", 14, &other);
    let revision = f.request.input.revision();
    let status = f
        .store
        .acceptance_evaluation_status(f.request.registration.binding.work_id, Some(&revision))
        .unwrap()
        .unwrap();
    assert_eq!(status.stale, Some(AcceptanceStaleReason::Mutation));
    assert_eq!(status.stale_observation.unwrap().observation, sighting);
    let error = complete_with_evidence(
        &mut f,
        &[check.clone(), note],
        &revision,
        "quiet-git-changed-after-witness",
    )
    .unwrap_err();
    assert!(
        matches!(
            error,
            StoreError::WorkCompletionRecoveryRequired {
                cause: WorkCompletionRecoveryCause::AcceptanceEvaluationStale {
                    reason: AcceptanceStaleReason::Mutation
                },
                ..
            }
        ),
        "{error:?}"
    );
    assert!(
        f.store
            .get_work_run(f.request.registration.binding.run_id)
            .unwrap()
            .completion_seal
            .is_none()
    );
    let report = f.store.verify_all().unwrap();
    assert!(report.is_healthy(), "{report:?}");
}

fn earlier_quiet_git_sighting(delivery: &str) {
    let mut f = prepared(false, true);
    let earlier = format!(
        "git-commit:{}",
        object_name("earlier commit before the tested input")
    );
    if delivery == "before-check" {
        source_sighting(&mut f, "early-git-sighting", 5, &earlier);
    }
    let receipt = f.record().unwrap();
    let check = &receipt.verifications[0].record_id;
    let note = note(&mut f);
    if delivery == "before-cut" {
        source_sighting_delivered(&mut f, "delayed-early-git", 5, 11, &earlier);
    }
    let input = judgment(&f, &note);
    if delivery == "after-cut" {
        source_sighting_delivered(&mut f, "delayed-early-git", 5, 11, &earlier);
    }
    let recorded = record_evaluation(&mut f, &input)
        .expect("an older sighting cannot contradict the later checked input");
    assert_eq!(
        recorded.record.registered_input_witness.as_ref(),
        Some(check)
    );
    let revision = f.request.input.revision();
    complete_with_evidence(
        &mut f,
        &[check.clone(), note],
        &revision,
        "complete-after-early-sighting",
    )
    .unwrap();
    let report = f.store.verify_all().unwrap();
    assert!(report.is_healthy(), "{report:?}");
}

#[test]
fn registered_staged_input_earlier_quiet_git_before_check_is_accepted() {
    earlier_quiet_git_sighting("before-check");
}

#[test]
fn registered_staged_input_earlier_quiet_git_delivered_before_cut_is_accepted() {
    earlier_quiet_git_sighting("before-cut");
}

#[test]
fn registered_staged_input_earlier_quiet_git_delivered_after_cut_is_accepted() {
    earlier_quiet_git_sighting("after-cut");
}

#[test]
fn registered_staged_input_quiet_evaluation_does_not_invent_a_bound_citation() {
    let mut f = prepared(true, true);
    f.record().unwrap();
    let note = note(&mut f);
    let input = judgment(&f, &note);
    let before = f.snapshot();
    let reason = record_evaluation(&mut f, &input).unwrap_err().to_string();
    assert!(
        reason.contains("a pass needs an observed basis"),
        "{reason}"
    );
    assert_eq!(f.snapshot(), before);
}

#[test]
fn registered_staged_input_quiet_evaluation_requires_registered_provenance() {
    for legacy in [false, true] {
        let mut f = prepared(false, true);
        let declared = f.request.input.revision();
        if legacy {
            let work = f
                .store
                .get_work_item(f.request.registration.binding.work_id)
                .unwrap();
            let held = f.store.claim_fixture(work.work_id);
            let check = host_verification_from_basis(
                &mut f.store,
                &work,
                &held,
                "runner",
                "legacy-check",
                VerificationKind::Test,
                VerificationResult::Passed,
                9,
                ExecutionSourceBasis {
                    workspace_id: f.request.registration.root.workspace_id.clone(),
                    source_revision: declared.clone(),
                    source_root_generation: Some(1),
                    source_root_state: Some(SourceRootState::Named),
                    git_tree: None,
                },
            );
            let evidence = f.store.load_verification_evidence(&check).unwrap();
            let producer =
                crate::storage::work::load_verification_producer_on(&f.store.connection, &evidence)
                    .unwrap();
            assert_eq!(
                evidence.source_basis.legacy_revision(),
                Some(declared.as_str())
            );
            assert!(producer.registered_git_basis(&evidence).is_none());
        }
        let note = note(&mut f);
        let input = judgment(&f, &note);
        let sighting = quiet_sighting(&mut f, "without-registered-provenance", 11);
        let before = f.snapshot();
        let error = record_evaluation(&mut f, &input).unwrap_err();
        assert!(
            matches!(error, StoreError::AcceptanceEvaluationBasisMoved {
            moved: crate::storage::EvaluationBasisMove::SourceChanged,
            observation: Some(ref deciding), ..
        } if deciding.observation == sighting),
            "{error:?}"
        );
        assert_eq!(f.snapshot(), before);
    }
}

#[test]
fn registered_staged_input_quiet_evaluation_rejects_a_different_or_late_input() {
    for late in [false, true] {
        let mut f = prepared(false, true);
        let note = note(&mut f);
        let mut input = judgment(&f, &note);
        let declared = input
            .source_basis
            .as_ref()
            .unwrap()
            .legacy_fingerprint()
            .unwrap()
            .to_owned();
        if !late {
            f.request.input = RegisteredVerificationInput::StagedTree {
                tree: GitTreeId::parse(object_name("another checked tree")).unwrap(),
                base_commit: object_name("staged provenance"),
            };
        }
        f.record().unwrap();
        if !late {
            input.evaluated_through = f
                .store
                .work_feed_head(&FeedId::RunExecution(f.request.registration.binding.run_id))
                .unwrap();
        }
        assert_eq!(
            input
                .source_basis
                .as_ref()
                .unwrap()
                .legacy_fingerprint()
                .unwrap(),
            declared
        );
        // The late case isolates the evidence-cut guard; a later content
        // sighting would instead decide SourceChanged and mask that guard.
        let sighting = (!late).then(|| quiet_sighting(&mut f, "nonmatching-registered-input", 11));
        let before = f.snapshot();
        let error = record_evaluation(&mut f, &input).unwrap_err();
        match &error {
            StoreError::AcceptanceEvaluationBasisMoved {
                moved, observation, ..
            } if late => {
                assert_eq!(*moved, crate::storage::EvaluationBasisMove::CheckRecorded);
                assert!(observation.is_none());
            }
            StoreError::AcceptanceEvaluationBasisMoved {
                moved, observation, ..
            } => {
                assert_eq!(*moved, crate::storage::EvaluationBasisMove::SourceChanged);
                assert_eq!(observation.as_ref().unwrap().observation, sighting.unwrap());
            }
            _ => panic!("the provenance/input guard must decide: {error:?}"),
        }
        assert_eq!(f.snapshot(), before);
    }
}

#[test]
fn registered_staged_input_quiet_evaluation_keeps_real_change_and_revert_barriers() {
    for cited in [false, true] {
        for before_cut in [false, true] {
            for reverted in [false, true] {
                let mut f = prepared(false, true);
                let receipt = f.record().unwrap();
                let check = &receipt.verifications[0].record_id;
                let note = note(&mut f);
                quiet_sighting(&mut f, "content-before-measured-change", 10);
                let mut input = if cited {
                    evaluation(&f, check, &f.request.input.revision())
                } else {
                    judgment(&f, &note)
                };
                actual_change(&mut f, true, 11);
                if reverted {
                    // The source returns to the content seen before the edit,
                    // but the measured change remains in the immutable feed.
                    quiet_sighting(&mut f, "same-content-after-measured-change", 12);
                }
                if before_cut {
                    input.evaluated_through = f
                        .store
                        .work_feed_head(&FeedId::RunExecution(
                            f.request.registration.binding.run_id,
                        ))
                        .unwrap();
                }
                input.recorded_at = at(13);
                let before = f.snapshot();
                let result = record_evaluation(&mut f, &input);
                if !before_cut {
                    let error = result.unwrap_err();
                    assert!(
                        matches!(error, StoreError::AcceptanceEvaluationBasisMoved {
                        moved: crate::storage::EvaluationBasisMove::SourceChanged,
                        observation: Some(ref deciding), ..
                    } if deciding.source_changed && deciding.position > input.evaluated_through),
                        "{error:?}"
                    );
                    assert_eq!(f.snapshot(), before);
                } else if cited {
                    let error = result.unwrap_err();
                    assert!(
                        matches!(error, StoreError::AcceptanceEvaluationAdmissionRefused {
                        ref cause, ..
                    } if matches!(cause.as_ref(), AcceptanceEvaluationAdmissionCause::Citation(citation)
                        if citation.mismatch == EvaluationCitationMismatch::SourceMovedAfterCheck)),
                        "{error:?}"
                    );
                    assert_eq!(f.snapshot(), before);
                } else {
                    // A note-only declaration remains admissible pending
                    // confirmation; its stale check must never seal it.
                    let recorded = result.expect("the pending judgment shape remains admitted");
                    assert!(recorded.record.registered_input_witness.is_none());
                    assert_unconfirmed_at_done(&mut f, check);
                }
            }
        }
    }
}

fn assert_unconfirmed_at_done(f: &mut Fixture, check: &ObjectId) {
    let revision = f.request.input.revision();
    let status = f
        .store
        .acceptance_evaluation_status(f.request.registration.binding.work_id, Some(&revision))
        .unwrap()
        .unwrap();
    assert_eq!(status.stale, Some(AcceptanceStaleReason::Source));
    let refusal = complete(f, check, &revision, "unconfirmed-declaration").unwrap_err();
    assert!(
        matches!(
            refusal,
            StoreError::WorkCompletionRecoveryRequired {
                cause: WorkCompletionRecoveryCause::AcceptanceEvaluationStale {
                    reason: AcceptanceStaleReason::Source
                },
                ..
            }
        ),
        "the pending declared source, not a successful stale check, decides: {refusal:?}"
    );
    assert!(
        f.store
            .get_work_run(f.request.registration.binding.run_id)
            .unwrap()
            .completion_seal
            .is_none()
    );
}

#[test]
fn registered_staged_input_note_only_pre_cut_change_does_not_seal() {
    let mut f = prepared(false, true);
    let receipt = f.record().unwrap();
    let check = &receipt.verifications[0].record_id;
    let note = note(&mut f);
    quiet_sighting(&mut f, "before-the-change", 10);
    actual_change(&mut f, true, 11);
    let input = judgment(&f, &note);
    record_evaluation(&mut f, &input).expect("a declaration may await host confirmation");
    assert_unconfirmed_at_done(&mut f, check);
    let report = f.store.verify_all().unwrap();
    assert!(
        report.is_healthy(),
        "a pending judgment is valid history: {report:?}"
    );
}

#[test]
fn registered_staged_input_note_only_change_needs_a_later_check() {
    let mut f = prepared(false, true);
    let old = f.record().unwrap();
    let note = note(&mut f);
    quiet_sighting(&mut f, "before-the-edit", 10);
    actual_change(&mut f, true, 11);
    let pending = judgment(&f, &note);
    let pending = record_evaluation(&mut f, &pending).unwrap();
    assert!(pending.record.registered_input_witness.is_none());
    assert_unconfirmed_at_done(&mut f, &old.verifications[0].record_id);

    f.request.operation_key = "termal-run:test-after-edit".into();
    f.request.registration.host_run_id = "test-after-edit".into();
    f.request.registration.registered_at = at(14);
    f.request.input = RegisteredVerificationInput::StagedTree {
        tree: GitTreeId::parse(object_name("new tree after the measured edit")).unwrap(),
        base_commit: object_name("staged provenance"),
    };
    f.request.run.started_at = at(14);
    f.request.run.finished_at = at(15);
    f.request.checks[0].started_at = Some(at(14));
    f.request.checks[0].completed_at = Some(at(15));
    let current = f.record_at(at(16)).unwrap();
    let mut input = judgment(&f, &note);
    input.recorded_at = at(17);
    let recorded = record_evaluation(&mut f, &input).unwrap();
    let check = &current.verifications[0].record_id;
    assert_eq!(
        recorded.record.registered_input_witness.as_ref(),
        Some(check)
    );
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
        "current-checkpoint",
        18,
        &[check.clone(), note.clone()],
    );
    let mut request = completion_request(&work, &held, "runner", check, "current-completion", 19);
    request.evidence.push(note);
    request.acceptance.clear();
    request.source_fingerprint = Some(f.request.input.revision());
    let seal = f
        .store
        .complete_work(&request, &DevelopmentNoopRedactor)
        .unwrap();
    assert!(seal.acceptance[0].satisfied);
    assert_eq!(
        seal.acceptance_evaluation.as_ref(),
        Some(&recorded.evaluation)
    );
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[test]
fn registered_staged_input_later_check_recovers_from_a_quiet_git_conflict() {
    let mut f = prepared(false, true);
    let old = f.record().unwrap();
    let note = note(&mut f);
    let other = format!(
        "git-commit:{}",
        object_name("different commit after the old check")
    );
    source_sighting(&mut f, "quiet-git-conflict-before-recheck", 11, &other);
    let pending = judgment(&f, &note);
    let pending = record_evaluation(&mut f, &pending).unwrap();
    assert!(pending.record.registered_input_witness.is_none());
    assert_unconfirmed_at_done(&mut f, &old.verifications[0].record_id);
    f.request.operation_key = "termal-run:test-after-quiet-git".into();
    f.request.registration.host_run_id = "test-after-quiet-git".into();
    f.request.registration.registered_at = at(14);
    f.request.run.started_at = at(14);
    f.request.run.finished_at = at(15);
    f.request.checks[0].started_at = Some(at(14));
    f.request.checks[0].completed_at = Some(at(15));
    let current = f.record_at(at(16)).unwrap();
    let mut input = judgment(&f, &note);
    input.recorded_at = at(17);
    let recorded = record_evaluation(&mut f, &input).unwrap();
    let check = &current.verifications[0].record_id;
    assert_eq!(
        recorded.record.registered_input_witness.as_ref(),
        Some(check)
    );
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
        "quiet-git-rechecked",
        18,
        &[check.clone(), note.clone()],
    );
    let mut request = completion_request(
        &work,
        &held,
        "runner",
        check,
        "quiet-git-recheck-complete",
        19,
    );
    request.evidence.push(note);
    request.acceptance.clear();
    request.source_fingerprint = Some(f.request.input.revision());
    let seal = f
        .store
        .complete_work(&request, &DevelopmentNoopRedactor)
        .unwrap();
    assert_eq!(
        seal.acceptance_evaluation.as_ref(),
        Some(&recorded.evaluation)
    );
    let report = f.store.verify_all().unwrap();
    assert!(report.is_healthy(), "{report:?}");
}

#[test]
fn registered_staged_input_uncited_witness_rejects_other_roots_and_generations() {
    for moved_workspace in [false, true] {
        let mut f = prepared(false, true);
        f.record().unwrap();
        let binding = f.request.registration.binding.clone();
        f.request.registration.root.generation = 2;
        if moved_workspace {
            f.request.registration.root.workspace_id = "workspace-b".into();
        }
        f.store
            .bind_named_root(
                &f.project,
                &f.session,
                &f.connection,
                crate::storage::HostSessionAuthority::Work,
                binding.claim_id,
                binding.claim_fence,
                &f.request.registration.root.workspace_id,
                2,
                at(10),
                NamedRootBindingKind::Bound,
                None,
                &mut actor("runner"),
                "rename-root",
                at(10),
            )
            .unwrap();
        quiet_sighting(&mut f, "new-root-sighting", 10);
        let note = note(&mut f);
        let mut input = judgment(&f, &note);
        input.source_basis.as_mut().unwrap().workspace_id =
            Some(f.request.registration.root.workspace_id.clone());
        let recorded = record_evaluation(&mut f, &input).unwrap();
        assert!(recorded.record.registered_input_witness.is_none());
        assert_unconfirmed_at_done(&mut f, &note);
    }
}

#[test]
fn registered_staged_input_uncited_witness_needs_a_passed_run_on_this_run() {
    for foreign_run in [false, true] {
        let mut f = prepared(false, true);
        if foreign_run {
            f.record().unwrap();
            let work = f
                .store
                .create_work(
                    &root_request("project-a", "other-evaluated-work", 10),
                    &DevelopmentNoopRedactor,
                )
                .unwrap();
            let held = claim(&mut f.store, &work, "runner", "other-claim", 10, 3600);
            let run = f.store.get_work_run(held.run_id).unwrap();
            f.request.registration.binding = ControlWorkBinding {
                root_execution_id: run.root_execution_id,
                work_id: work.work_id,
                run_id: held.run_id,
                work_revision: held.accepted_work_revision,
                claim_id: held.claim_id,
                claim_fence: held.fence,
            };
            f.store
                .bind_named_root(
                    &f.project,
                    &f.session,
                    &f.connection,
                    crate::storage::HostSessionAuthority::Work,
                    held.claim_id,
                    held.fence,
                    &f.request.registration.root.workspace_id,
                    1,
                    at(10),
                    NamedRootBindingKind::Bound,
                    None,
                    &mut actor("runner"),
                    "other-root",
                    at(10),
                )
                .unwrap();
        } else {
            f.request.run.outcome = RegisteredVerificationOutcome::Failed;
            f.record().unwrap();
        }
        quiet_sighting(&mut f, "without-a-qualifying-run", 10);
        let note = note(&mut f);
        let input = judgment(&f, &note);
        let recorded = record_evaluation(&mut f, &input).unwrap();
        assert!(recorded.record.registered_input_witness.is_none());
        assert_unconfirmed_at_done(&mut f, &note);
    }
}

#[test]
fn registered_staged_input_uncited_witness_validates_the_exact_saved_proof() {
    let mut f = prepared(false, true);
    let receipt = f.record().unwrap();
    let check = &receipt.verifications[0].record_id;
    let note = note(&mut f);
    let input = judgment(&f, &note);
    let recorded = record_evaluation(&mut f, &input).unwrap();
    assert_eq!(
        recorded.record.registered_input_witness.as_ref(),
        Some(check)
    );
    for corrupt in ["wrong-kind", "not-in-basis", "wrong-input", "missing-root"] {
        let mut bad = recorded.record.clone();
        match corrupt {
            "wrong-kind" => bad.registered_input_witness = Some(note.clone()),
            "not-in-basis" => bad.evidence_basis.retain(|id| id != check),
            "wrong-input" => {
                bad.source_basis.as_mut().unwrap().identity =
                    crate::domain::VerificationSourceIdentity::Legacy {
                        source_revision: format!(
                            "git-commit:{}",
                            object_name("not-the-checked-input")
                        ),
                        git_tree: None,
                    };
            }
            "missing-root" => bad.named_root_binding = None,
            _ => unreachable!(),
        }
        assert!(
            crate::storage::work::acceptance_evaluation::validate_evaluation_input_witness_on(
                &f.store.connection,
                &bad,
            )
            .is_err(),
            "a corrupt proof must not fall back to other run evidence: {corrupt}"
        );
    }
    // Doctor reaches the same validation for an open stored evaluation.
    let mut bad = recorded.record.clone();
    bad.registered_input_witness = Some(note);
    f.store
        .connection
        .execute(
            "UPDATE objects SET canonical_json = ?2 WHERE object_id = ?1",
            rusqlite::params![
                recorded.evaluation.as_str(),
                crate::canonical::canonical_bytes(&bad).unwrap()
            ],
        )
        .unwrap();
    assert!(
        f.store
            .acceptance_evaluation_status(f.request.registration.binding.work_id, None)
            .is_err()
    );
    let report = f.store.verify_all().unwrap();
    assert!(
        report
            .invalid_work_records
            .iter()
            .any(|finding| finding.contains("unsupported_or_unbound_object")),
        "doctor must reject the invalid witness: {report:?}"
    );
}

#[test]
fn registered_staged_input_uncited_witness_requires_completion_input_with_freshness_off() {
    let mut f = prepared(false, false);
    let receipt = f.record().unwrap();
    let check = receipt.verifications[0].record_id.clone();
    let note = note(&mut f);
    let revision = f.request.input.revision();
    source_sighting(&mut f, "already-confirmed-git-input", 10, &revision);
    let input = judgment(&f, &note);
    let recorded = record_evaluation(&mut f, &input).unwrap();
    assert_eq!(
        recorded.record.registered_input_witness.as_ref(),
        Some(&check)
    );
    let work = f
        .store
        .get_work_item(f.request.registration.binding.work_id)
        .unwrap();
    let held = f.store.claim_fixture(work.work_id);
    let evidence = [check.clone(), note];
    checkpoint(
        &mut f.store,
        &work,
        &held,
        "runner",
        "missing-input-checkpoint",
        13,
        &evidence,
    );
    let mut request = completion_request(
        &work,
        &held,
        "runner",
        &check,
        "missing-completion-input",
        14,
    );
    request.evidence = evidence.to_vec();
    request.acceptance.clear();
    request.source_fingerprint = None;
    let error = f
        .store
        .complete_work(&request, &DevelopmentNoopRedactor)
        .unwrap_err();
    assert!(
        matches!(
            error,
            StoreError::WorkCompletionRecoveryRequired {
                cause: WorkCompletionRecoveryCause::AcceptanceEvaluationStale {
                    reason: AcceptanceStaleReason::Source
                },
                ..
            }
        ),
        "{error:?}"
    );
    let crate::storage::AcceptanceEvaluationReadiness::Blocked(_, context) = f
        .store
        .acceptance_evaluation_readiness(work.work_id, held.run_id, None, None)
        .unwrap()
    else {
        panic!("a witness still requires the completion's exact input");
    };
    assert_eq!(
        context.source.unwrap().mismatch,
        AcceptanceSourceMismatch::CompletionMeasurementMissing
    );
    complete_with_evidence(&mut f, &evidence, &revision, "measured-completion-input").unwrap();
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[test]
fn registered_staged_input_uncited_witness_stales_after_a_recorded_change() {
    for unadmitted in [false, true] {
        let mut f = prepared(false, true);
        let receipt = f.record().unwrap();
        let check = &receipt.verifications[0].record_id;
        let note = note(&mut f);
        let input = judgment(&f, &note);
        let recorded = record_evaluation(&mut f, &input).unwrap();
        assert_eq!(
            recorded.record.registered_input_witness.as_ref(),
            Some(check)
        );
        actual_change(&mut f, unadmitted, 14);
        let revision = f.request.input.revision();
        let reason = if unadmitted {
            AcceptanceStaleReason::UnadmittedChange
        } else {
            AcceptanceStaleReason::Mutation
        };
        let status = f
            .store
            .acceptance_evaluation_status(f.request.registration.binding.work_id, Some(&revision))
            .unwrap()
            .unwrap();
        assert_eq!(status.stale, Some(reason));
        let error = complete_with_evidence(
            &mut f,
            &[check.clone(), note],
            &revision,
            "changed-after-witness",
        )
        .unwrap_err();
        assert!(
            matches!(error, StoreError::WorkCompletionRecoveryRequired {
            cause: WorkCompletionRecoveryCause::AcceptanceEvaluationStale { reason: stale }, ..
        } if stale == reason),
            "{error:?}"
        );
        assert!(
            f.store
                .get_work_run(f.request.registration.binding.run_id)
                .unwrap()
                .completion_seal
                .is_none()
        );
        let report = f.store.verify_all().unwrap();
        assert!(report.is_healthy(), "{report:?}");
    }
}

#[test]
fn registered_staged_input_uncited_witness_keeps_a_sealed_historical_cut() {
    use crate::storage::work::test_support::{
        ExecutionObservationInput, HistoricalCapture, append_historical_capture,
    };
    let mut f = prepared(false, true);
    let receipt = f.record().unwrap();
    let check = &receipt.verifications[0].record_id;
    let note = note(&mut f);
    let input = judgment(&f, &note);
    let recorded = record_evaluation(&mut f, &input).unwrap();
    let revision = f.request.input.revision();
    let sealed = complete_with_evidence(
        &mut f,
        &[check.clone(), note],
        &revision,
        "before-later-change",
    )
    .unwrap();
    let capture = HistoricalCapture {
        project: f.project.clone(),
        session: f.session.clone(),
        binding: f.request.registration.binding.clone(),
        capture_id: "historical-late-observation".into(),
    };
    let late = ExecutionObservationInput {
        observation_id: "late-flagged-change".into(),
        action_fingerprint: ObjectId::from_canonical_bytes(b"late-flagged-change"),
        effect: EffectClass::MutateLocal,
        outcome: ExecutionOutcome::Succeeded,
        source_changed: true,
        reported_source_change: None,
        source_basis: Some(ExecutionSourceBasis {
            workspace_id: f.request.registration.root.workspace_id.clone(),
            source_revision: content_revision(),
            source_root_generation: Some(1),
            source_root_state: Some(SourceRootState::Named),
            git_tree: None,
        }),
        observed_at: Some(at(15)),
    };
    let appended =
        append_historical_capture(&mut f.store, &capture, &[late], &[], &[], at(16)).unwrap();
    assert_eq!(appended.execution_observations.len(), 1);
    let later: ExecutionObservation = f
        .store
        .get(&appended.execution_observations[0])
        .unwrap()
        .unwrap();
    assert!(
        later.source_changed,
        "the historical-cut test needs an actual flagged record"
    );
    assert_eq!(
        f.store.completion_seal_evaluation(&sealed).unwrap(),
        Some(recorded.record)
    );
    let report = f.store.verify_all().unwrap();
    assert!(
        report.is_healthy(),
        "later changes cannot invalidate a historical witness: {report:?}"
    );
}

#[test]
fn registered_staged_input_uncited_witness_is_source_identity_not_check_selection() {
    let mut f = prepared(false, true);
    let passed = f.record().unwrap();
    let check = &passed.verifications[0].record_id;
    let note = note(&mut f);
    f.request.operation_key = "termal-run:test-newer-failed-check".into();
    f.request.registration.host_run_id = "test-newer-failed-check".into();
    f.request.registration.registered_at = at(14);
    f.request.run.started_at = at(14);
    f.request.run.finished_at = at(15);
    f.request.run.outcome = RegisteredVerificationOutcome::Failed;
    f.request.checks[0].started_at = Some(at(14));
    f.request.checks[0].completed_at = Some(at(15));
    let failed = f.record_at(at(16)).unwrap();
    assert_ne!(&failed.verifications[0].record_id, check);
    let mut input = judgment(&f, &note);
    input.recorded_at = at(17);
    let recorded = record_evaluation(&mut f, &input).unwrap();
    assert_eq!(
        recorded.record.registered_input_witness.as_ref(),
        Some(check)
    );
    assert!(
        recorded
            .record
            .evidence_basis
            .contains(&failed.verifications[0].record_id)
    );
    assert_eq!(recorded.record.verdicts[0].evidence, vec![note]);
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[test]
fn registered_staged_input_old_note_only_record_keeps_its_citation_source_lane() {
    let mut f = prepared(false, true);
    f.record().unwrap();
    let note = note(&mut f);
    let revision = f.request.input.revision();
    // This declaration was already confirmed at the cut, so the historical
    // note-only shape is admissible under the original rules too.
    source_sighting(&mut f, "old-confirmed-declaration", 10, &revision);
    let input = judgment(&f, &note);
    let recorded = record_evaluation(&mut f, &input).unwrap();
    assert!(recorded.record.registered_input_witness.is_some());
    // Historical-format fixture: an absent member never authorizes discovery
    // of an uncited witness, even when one is in the old evidence basis.
    let mut historical = recorded.record.clone();
    historical.registered_input_witness = None;
    let bytes = crate::canonical::canonical_bytes(&historical).unwrap();
    f.store
        .connection
        .execute(
            "UPDATE objects SET canonical_json = ?2 WHERE object_id = ?1",
            rusqlite::params![recorded.evaluation.as_str(), &bytes],
        )
        .unwrap();
    quiet_sighting(&mut f, "after-historical-evaluation", 13);
    let status = f
        .store
        .acceptance_evaluation_status(f.request.registration.binding.work_id, None)
        .unwrap()
        .unwrap();
    assert_eq!(status.stale, Some(AcceptanceStaleReason::Mutation));
    assert!(!status.stale_observation.unwrap().source_changed);
    let after: Vec<u8> = f
        .store
        .connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_id = ?1",
            [recorded.evaluation.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(after, bytes, "reading an old record never rewrites it");
}

#[test]
fn registered_staged_input_old_and_witness_records_round_trip_without_rewriting() {
    for cited in [false, true] {
        let directory = crate::test_support::temp_home().unwrap();
        let source = directory.path().join("source.db");
        let target = directory.path().join("imported.db");
        let export = directory.path().join("store.jsonl");
        let mut f = prepared(false, true);
        let schema = crate::storage::running_schema_reference().unwrap();
        let receipt = f.record().unwrap();
        let check = &receipt.verifications[0].record_id;
        let note = note(&mut f);
        let input = if cited {
            evaluation(&f, check, &f.request.input.revision())
        } else {
            judgment(&f, &note)
        };
        let recorded = record_evaluation(&mut f, &input).unwrap();
        assert_eq!(recorded.record.registered_input_witness.is_none(), cited);
        let bytes = crate::canonical::canonical_bytes(&recorded.record).unwrap();
        let decoded: AcceptanceEvaluation = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(crate::canonical::canonical_bytes(&decoded).unwrap(), bytes);
        if cited {
            assert!(
                serde_json::from_slice::<serde_json::Value>(&bytes)
                    .unwrap()
                    .get("registered_input_witness")
                    .is_none(),
                "the existing citation-selected record retains its omitted-field bytes"
            );
        }
        // Exact replay returns the original receipt, not a newly selected
        // witness or a reinterpreted historical source lane.
        let replay = record_evaluation(&mut f, &input).unwrap();
        assert!(replay.replayed);
        assert_eq!(replay.record, recorded.record);
        let revision = f.request.input.revision();
        complete_with_evidence(
            &mut f,
            &[check.clone(), note],
            &revision,
            "round-trip-complete",
        )
        .unwrap();
        let objects = |store: &SqliteStore| -> Vec<(String, Vec<u8>)> {
            store
                .connection
                .prepare("SELECT object_id, canonical_json FROM objects ORDER BY object_id")
                .unwrap()
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
                .unwrap()
                .collect::<Result<_, _>>()
                .unwrap()
        };
        let before = objects(&f.store);
        f.store
            .connection
            .execute("VACUUM INTO ?1", [source.to_str().unwrap()])
            .unwrap();
        SqliteStore::readiness(&source, None).unwrap();
        assert_eq!(
            crate::storage::store_schema_reference(&source).unwrap(),
            schema
        );
        crate::storage::migration::export_json(&source, &export).unwrap();
        crate::storage::migration::import_json(&export, &target).unwrap();
        let imported = SqliteStore::open(&target).unwrap();
        assert_eq!(
            objects(&imported),
            before,
            "all canonical ids and bytes survive"
        );
        assert!(imported.verify_all().unwrap().is_healthy());
        SqliteStore::readiness(&target, None).unwrap();
        assert_eq!(
            crate::storage::store_schema_reference(&target).unwrap(),
            schema
        );
    }
}

#[test]
fn a_shared_quiet_sighting_preserves_the_historical_staged_tree_witness() {
    let mut f = prepared(false, true);
    let check = f.record().unwrap().verifications[0].record_id.clone();
    let note = note(&mut f);
    let shared = |f: &Fixture| crate::domain::VerificationSourceBasis {
        workspace_id: f.request.registration.root.workspace_id.clone(),
        identity: crate::domain::VerificationSourceIdentity::SourceId {
            source_id: crate::source::SourceId {
                commit: "a".repeat(40),
                tree: "b".repeat(40),
                changes: vec![],
                submodules: vec![],
            },
        },
        source_root_generation: Some(f.request.registration.root.generation),
        source_root_state: Some(SourceRootState::Named),
    };
    let basis = shared(&f);
    basis_sighting_delivered(&mut f, "shared-before-cut", 11, 11, basis);
    let input = judgment(&f, &note);
    let recorded = record_evaluation(&mut f, &input).unwrap();
    assert_eq!(
        recorded.record.registered_input_witness.as_ref(),
        Some(&check)
    );
    assert!(f.store.verify_all().unwrap().is_healthy());
    let basis = shared(&f);
    basis_sighting_delivered(&mut f, "shared-after-recording", 14, 14, basis);
    let revision = f.request.input.revision();
    let status = f
        .store
        .acceptance_evaluation_status(f.request.registration.binding.work_id, Some(&revision))
        .unwrap()
        .unwrap();
    assert_eq!(status.stale, None);
    let sealed =
        complete_with_evidence(&mut f, &[check, note], &revision, "shared-quiet-complete").unwrap();
    assert_eq!(
        f.store.completion_seal_evaluation(&sealed).unwrap(),
        Some(recorded.record)
    );
    assert!(f.store.verify_all().unwrap().is_healthy());
}
