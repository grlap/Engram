use super::*;
use crate::domain::{
    AcceptanceBinding, ExecutionSourceBasis, NamedRootBindingKind, SourceRootState,
    VerificationRequirement, VerificationResult, WorkObligationResolution,
};
use crate::storage::test_support::bind_control_for;

mod state;

fn source(workspace: &str, generation: i64) -> ExecutionSourceBasis {
    ExecutionSourceBasis {
        workspace_id: workspace.into(),
        source_revision: "revision-B".into(),
        source_root_generation: Some(generation),
        source_root_state: Some(SourceRootState::Named),
    }
}

/// A check's source before the claim named any root: no generation.
fn unnamed(workspace: &str) -> ExecutionSourceBasis {
    ExecutionSourceBasis {
        workspace_id: workspace.into(),
        source_revision: "revision-B".into(),
        source_root_generation: None,
        source_root_state: None,
    }
}

fn name_root(
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    host: &crate::storage::test_support::TestControlBinding,
    generation: i64,
    second: i64,
) {
    store
        .bind_named_root(
            &work.project_id,
            &host.status.session_id,
            &host.connection_token,
            &host.routing_token,
            claim.claim_id,
            claim.fence,
            "workspace-B",
            generation,
            at(second),
            NamedRootBindingKind::Bound,
            None,
            &mut actor("runner"),
            &format!("name-B-{generation}"),
            at(second),
        )
        .expect("host names B");
}

fn evaluate_check(
    store: &mut SqliteStore,
    work: &WorkItem,
    evidence: &ObjectId,
    key: &str,
    second: i64,
) -> Result<AcceptanceEvaluationReceipt, StoreError> {
    let mut input = request(
        work,
        cut(store, work),
        "runner",
        Mode::SameSession,
        vec![verdict(
            1,
            AcceptanceVerdict::Pass,
            AcceptanceBasis::Observed,
            std::slice::from_ref(evidence),
        )],
        second,
    );
    input.source_basis = Some(AcceptanceSourceBasis {
        workspace_id: Some("workspace-B".into()),
        fingerprint: "revision-B".into(),
    });
    input.attempt_key = Some(key.into());
    record(store, &input)
}

#[test]
fn a_bound_evaluation_accepts_only_a_check_after_the_current_named_binding() {
    let mut fixture = fixture("project-a");
    let claim = fixture.claim.clone();
    let work = revise(
        &mut fixture.store,
        &fixture.work,
        &claim,
        WorkRevisionPatch {
            acceptance: Some(vec!["run tests".into()]),
            acceptance_bindings: Some(vec![AcceptanceBinding {
                criterion: 1,
                requirement: VerificationRequirement {
                    check_kind: VerificationKind::Test,
                    check_fingerprint: None,
                },
            }]),
            ..empty_patch()
        },
        "bind-test",
        5,
    )
    .expect("bind criterion");
    enable(
        &mut fixture.store,
        &[Mode::SameSession],
        MechanicalBasis::Observed,
        false,
        "enable-evaluation",
        6,
    );
    // The same workspace and revision before the claim named any root, and
    // under an earlier name of it, are both older than the current name.
    let before = host_verification_from_basis(
        &mut fixture.store,
        &work,
        &claim,
        "runner",
        "test-before-binding",
        VerificationKind::Test,
        VerificationResult::Passed,
        7,
        unnamed("workspace-B"),
    );
    let host = bind_control_for(
        &mut fixture.store,
        "runner",
        "named-evaluation-host",
        &[crate::domain::EffectClass::Observe],
        at(8),
    );
    name_root(&mut fixture.store, &work, &claim, &host, 8, 8);
    let old_generation = host_verification_from_basis(
        &mut fixture.store,
        &work,
        &claim,
        "runner",
        "test-old-generation",
        VerificationKind::Test,
        VerificationResult::Passed,
        9,
        source("workspace-B", 8),
    );
    name_root(&mut fixture.store, &work, &claim, &host, 9, 10);
    // The current name's own check is the root's newest sighting, at the
    // same revision the older checks carry.
    let current = host_verification_from_basis(
        &mut fixture.store,
        &work,
        &claim,
        "runner",
        "test-current",
        VerificationKind::Test,
        VerificationResult::Passed,
        11,
        source("workspace-B", 9),
    );
    let foreign = host_verification_from_basis(
        &mut fixture.store,
        &work,
        &claim,
        "runner",
        "test-foreign",
        VerificationKind::Test,
        VerificationResult::Passed,
        12,
        source("workspace-A", 9),
    );
    for (check, key) in [
        (&before, "before-binding"),
        (&old_generation, "old-generation"),
        (&foreign, "foreign"),
    ] {
        let refused = refusal(evaluate_check(&mut fixture.store, &work, check, key, 13));
        assert!(refused.contains("criterion 1"), "{key}: {refused}");
    }
    evaluate_check(&mut fixture.store, &work, &current, "current", 14)
        .expect("current B check records");
    let report = fixture.store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
}

/// A source basis stating `generation` and `state` in workspace B.
fn stated(generation: Option<i64>, state: Option<SourceRootState>) -> ExecutionSourceBasis {
    ExecutionSourceBasis {
        workspace_id: "workspace-B".into(),
        source_revision: "revision-B".into(),
        source_root_generation: generation,
        source_root_state: state,
    }
}

/// The host's session names or ends a root at `generation` for `claim`.
#[allow(
    clippy::too_many_arguments,
    reason = "the test mirrors the host binding request"
)]
fn host_binds(
    store: &mut SqliteStore,
    host: &HostSession,
    claim: &WorkClaim,
    workspace: &str,
    generation: i64,
    kind: NamedRootBindingKind,
    named_second: i64,
    key: &str,
    second: i64,
) -> Result<crate::domain::NamedRootBindingReceipt, StoreError> {
    store.bind_named_root(
        &host.project_id,
        &host.session_id,
        &host.connection_token,
        &host.routing_token,
        claim.claim_id,
        claim.fence,
        workspace,
        generation,
        at(named_second),
        kind,
        (kind == NamedRootBindingKind::Ended)
            .then_some(crate::domain::NamedRootEndReason::ExplicitClear),
        &mut actor(&host.session_id.0),
        key,
        at(second),
    )
}

/// One host turn whose only observation states `basis`, a source change or
/// a quiet sighting, with the checkpoint's own answer.
fn checkpoint_basis(
    host: &mut HostSession,
    store: &mut SqliteStore,
    basis: ExecutionSourceBasis,
    source_changed: bool,
    second: i64,
) -> Result<ControlTurnCheckpointDecision, StoreError> {
    let grant = host.grant(store, &[EffectClass::MutateLocal], true, second);
    host.begin(store, &grant, second + 1);
    store.checkpoint_control_turn_with_evidence(
        &host.project_id,
        &host.session_id,
        &host.connection_token,
        &host.routing_token,
        &grant.grant_id,
        TurnNextIntent::Continue,
        &[ExecutionObservationInput {
            observation_id: host.key("stated-sighting"),
            action_fingerprint: ObjectId::from_canonical_bytes(host.key("write").as_bytes()),
            effect: EffectClass::MutateLocal,
            outcome: ExecutionOutcome::Succeeded,
            source_changed,
            reported_source_change: None,
            source_basis: Some(basis),
            observed_at: Some(at(second + 1)),
        }],
        &[],
        &[],
        &host.key("checkpoint"),
        at(second + 2),
    )
}

fn release_runner(fixture: &mut Fixture, work: &WorkItem, claim: &WorkClaim, second: i64) {
    let current = fixture.store.get_work_item(work.work_id).expect("item");
    fixture
        .store
        .release_work(
            &crate::domain::ReleaseWorkRequest {
                work_id: current.work_id,
                run_id: claim.run_id,
                expected_work_revision: current.revision,
                holder: claim.holder.clone(),
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                reason: "stepping away".into(),
                waiver_reason: None,
                actor: actor("runner"),
                idempotency_key: "release-runner".into(),
                released_at: at(second),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("release");
}

/// The host checkpoint admits a sighting's generation and state only as a
/// pair naming an event the claim recorded. The bound generation as `named`
/// is admitted. A generation without its state, a state without its
/// generation and a zero generation are malformed; a generation never bound
/// and an end never recorded name no event. Each is refused before anything
/// is stored.
#[test]
fn a_checkpoint_may_state_only_a_generation_the_claim_recorded() {
    #[derive(Debug, PartialEq)]
    enum Answer {
        Admitted,
        Malformed,
        Unrecorded,
    }
    for (case, basis, expected) in [
        (
            "the bound generation",
            stated(Some(9), Some(SourceRootState::Named)),
            Answer::Admitted,
        ),
        (
            "a generation never bound",
            stated(Some(12), Some(SourceRootState::Named)),
            Answer::Unrecorded,
        ),
        (
            "a generation without its state",
            stated(Some(9), None),
            Answer::Malformed,
        ),
        (
            "a state without its generation",
            stated(None, Some(SourceRootState::Named)),
            Answer::Malformed,
        ),
        (
            "a zero generation",
            stated(Some(0), Some(SourceRootState::Named)),
            Answer::Malformed,
        ),
        (
            "an end never recorded",
            stated(Some(9), Some(SourceRootState::Ended)),
            Answer::Unrecorded,
        ),
    ] {
        let mut fixture = fixture("project-a");
        let (work, claim) = (fixture.work.clone(), fixture.claim.clone());
        let mut host = HostSession::bind(&mut fixture.store, &work, &claim, 5);
        host_binds(
            &mut fixture.store,
            &host,
            &claim,
            "workspace-B",
            9,
            NamedRootBindingKind::Bound,
            9,
            "name-B",
            9,
        )
        .expect("host names B");
        let answer = checkpoint_basis(&mut host, &mut fixture.store, basis, true, 10);
        let got = match &answer {
            Ok(ControlTurnCheckpointDecision::Checkpointed { .. }) => Answer::Admitted,
            Err(StoreError::InvalidControlSession(reason))
                if reason.contains("generation and state must occur together") =>
            {
                Answer::Malformed
            }
            Err(StoreError::NamedRootBindingRefused(_)) => Answer::Unrecorded,
            other => panic!("{case}: {other:?}"),
        };
        assert_eq!(got, expected, "{case}: {answer:?}");
        let report = fixture.store.verify_all().expect("doctor");
        assert!(report.is_healthy(), "{case}: {report:?}");
    }
}

/// A host that missed a release goes on stating the old generation after
/// the claim is taken again. The checkpoint admits it, because its bound
/// event is recorded, and the doctor finds the store healthy.
#[test]
fn after_a_release_a_checkpoint_may_still_state_the_old_generation() {
    let mut fixture = fixture("project-a");
    let (work, claim) = (fixture.work.clone(), fixture.claim.clone());
    let host = HostSession::bind(&mut fixture.store, &work, &claim, 5);
    host_binds(
        &mut fixture.store,
        &host,
        &claim,
        "workspace-B",
        9,
        NamedRootBindingKind::Bound,
        9,
        "name-B",
        9,
    )
    .expect("host names B");
    release_runner(&mut fixture, &work, &claim, 10);
    let current = fixture.store.get_work_item(work.work_id).expect("item");
    let reclaimed = super::claim(&mut fixture.store, &current, "second", "reclaim", 11, 3_600);
    assert_eq!(reclaimed.claim_id, claim.claim_id);
    let mut second = HostSession::bind(&mut fixture.store, &current, &reclaimed, 12);
    let answer = checkpoint_basis(
        &mut second,
        &mut fixture.store,
        stated(Some(9), Some(SourceRootState::Named)),
        true,
        20,
    );
    assert!(
        matches!(
            answer,
            Ok(ControlTurnCheckpointDecision::Checkpointed { .. })
        ),
        "{answer:?}"
    );
    let report = fixture.store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
}

/// The host binding refuses what does not fit the claim's history: a key
/// reused for other content, an end that does not repeat the bound event's
/// `named_at`, and a name for a claim that is no longer active.
#[test]
fn a_named_root_binding_refuses_what_does_not_fit_its_claim() {
    let mut fixture = fixture("project-a");
    let (work, claim) = (fixture.work.clone(), fixture.claim.clone());
    let host = HostSession::bind(&mut fixture.store, &work, &claim, 5);
    host_binds(
        &mut fixture.store,
        &host,
        &claim,
        "workspace-B",
        9,
        NamedRootBindingKind::Bound,
        9,
        "name-B",
        9,
    )
    .expect("host names B");
    let conflict = host_binds(
        &mut fixture.store,
        &host,
        &claim,
        "workspace-C",
        9,
        NamedRootBindingKind::Bound,
        9,
        "name-B",
        10,
    );
    assert!(
        matches!(
            conflict,
            Err(StoreError::ControlOperationIdempotencyConflict { .. })
        ),
        "{conflict:?}"
    );
    let moved = host_binds(
        &mut fixture.store,
        &host,
        &claim,
        "workspace-B",
        9,
        NamedRootBindingKind::Ended,
        10,
        "end-B-at-another-time",
        11,
    );
    assert!(
        matches!(&moved, Err(StoreError::NamedRootBindingRefused(reason)) if reason.contains("named_at")),
        "{moved:?}"
    );
    host_binds(
        &mut fixture.store,
        &host,
        &claim,
        "workspace-B",
        9,
        NamedRootBindingKind::Ended,
        9,
        "end-B",
        12,
    )
    .expect("an end that repeats the naming time");
    release_runner(&mut fixture, &work, &claim, 13);
    let inactive = host_binds(
        &mut fixture.store,
        &host,
        &claim,
        "workspace-B",
        10,
        NamedRootBindingKind::Bound,
        14,
        "name-B-after-release",
        14,
    );
    assert!(
        matches!(&inactive, Err(StoreError::NamedRootBindingRefused(reason)) if reason.contains("not active")),
        "{inactive:?}"
    );
    let report = fixture.store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
}

fn workspace(workspace: &str, revision: &str, generation: Option<i64>) -> ExecutionSourceBasis {
    ExecutionSourceBasis {
        workspace_id: workspace.into(),
        source_revision: revision.into(),
        source_root_generation: generation,
        source_root_state: generation.map(|_| SourceRootState::Named),
    }
}

/// The observed sequence, through the host checkpoint. Workspace A, the
/// shared main checkout, records a source change at `R_a` and then quiet
/// sightings of `R_a`. The named root B is bound and sighted at `R_b`; a delayed
/// checkpoint then reports another change in A, captured before the name.
/// A passing test runs in B and satisfies the bound criterion, and `done`
/// seals, displacing and disclosing both A changes. A later source change
/// in B instead leaves that check stale, and `done` refuses.
#[test]
fn the_observed_sequence_through_the_host_checkpoint() {
    for later_change_in_b in [false, true] {
        let mut fixture = fixture("project-a");
        let claim = fixture.claim.clone();
        let work = revise(
            &mut fixture.store,
            &fixture.work,
            &claim,
            WorkRevisionPatch {
                acceptance: Some(vec!["run tests".into()]),
                acceptance_bindings: Some(vec![AcceptanceBinding {
                    criterion: 1,
                    requirement: VerificationRequirement {
                        check_kind: VerificationKind::Test,
                        check_fingerprint: None,
                    },
                }]),
                ..empty_patch()
            },
            "bind-test",
            5,
        )
        .expect("bind criterion");
        let store = &mut fixture.store;
        // The holder's revision renewed the claim at the new work revision.
        let claim =
            crate::storage::work::query::load_work_claim_optional(&store.connection, claim.run_id)
                .expect("claim read")
                .expect("the claim is held");
        let mut host = HostSession::bind(store, &work, &claim, 6);
        host.basis = workspace("workspace-A", "R_a", None);
        host.checkpoint(store, true, None, 10);
        host.report(store, &[(false, Some("R_a")), (false, Some("R_a"))], 20);
        host_binds(
            store,
            &host,
            &claim,
            "workspace-B",
            9,
            NamedRootBindingKind::Bound,
            30,
            "name-B",
            30,
        )
        .expect("host names B");
        assert!(matches!(
            checkpoint_basis(
                &mut host,
                store,
                workspace("workspace-B", "R_b", Some(9)),
                false,
                40,
            ),
            Ok(ControlTurnCheckpointDecision::Checkpointed { .. })
        ));
        host.basis = workspace("workspace-A", "R_a2", None);
        host.checkpoint(store, true, None, 50);
        host.basis = workspace("workspace-B", "R_b", Some(9));
        let check = host.checkpoint(
            store,
            false,
            Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
            60,
        )[0]
        .clone();
        let obligations = store
            .work_run_obligations(claim.run_id)
            .expect("obligations");
        let criterion = obligations
            .iter()
            .find(|record| {
                crate::control::acceptance_binding_criterion(&record.obligation.rule).is_some()
            })
            .expect("the bound criterion's obligation");
        assert!(matches!(
            criterion.resolution.as_ref().map(|event| &event.resolution),
            Some(WorkObligationResolution::Satisfied { evidence, .. }) if evidence == &check
        ));
        if later_change_in_b {
            host.basis = workspace("workspace-B", "R_b2", Some(9));
            host.checkpoint(store, true, None, 70);
        }
        let completed = checkpoint_then_complete(
            store,
            &work,
            &claim,
            "runner",
            std::slice::from_ref(&check),
            true,
            None,
            "complete",
            80,
        );
        if later_change_in_b {
            assert!(
                matches!(
                    &completed,
                    Err(StoreError::WorkCompletionRefused { reason, .. })
                        if reason.contains("latest source change")
                ),
                "{completed:?}"
            );
            continue;
        }
        let seal = completed.expect("the B check completes the work");
        let records = store
            .work_run_obligations(claim.run_id)
            .expect("obligations");
        let displaced = records
            .iter()
            .filter(|record| {
                matches!(
                    record.resolution.as_ref().map(|event| &event.resolution),
                    Some(WorkObligationResolution::Displaced { trigger_workspace_id, .. })
                        if trigger_workspace_id == "workspace-A"
                )
            })
            .map(|record| record.obligation.triggering_observation.clone())
            .collect::<Vec<_>>();
        assert_eq!(displaced.len(), 2, "both A changes are displaced");
        assert_eq!(seal.foreign_workspace_changes, displaced);
        let report = store.verify_all().expect("doctor");
        assert!(report.is_healthy(), "{report:?}");
    }
}

/// A bound, evaluated work item whose claim named root B at generation 9,
/// with a passing check in B at `revision-B` and the naming host.
struct NamedEvaluation {
    fixture: Fixture,
    work: WorkItem,
    claim: WorkClaim,
    host: crate::storage::test_support::TestControlBinding,
    current: ObjectId,
}

fn named_evaluation() -> NamedEvaluation {
    let mut fixture = fixture("project-a");
    let claim = fixture.claim.clone();
    let work = revise(
        &mut fixture.store,
        &fixture.work,
        &claim,
        WorkRevisionPatch {
            acceptance: Some(vec!["run tests".into()]),
            acceptance_bindings: Some(vec![AcceptanceBinding {
                criterion: 1,
                requirement: VerificationRequirement {
                    check_kind: VerificationKind::Test,
                    check_fingerprint: None,
                },
            }]),
            ..empty_patch()
        },
        "bind-test",
        5,
    )
    .expect("bind criterion");
    enable(
        &mut fixture.store,
        &[Mode::SameSession],
        MechanicalBasis::Observed,
        false,
        "enable-evaluation",
        6,
    );
    let host = bind_control_for(
        &mut fixture.store,
        "runner",
        "named-evaluation-host",
        &[crate::domain::EffectClass::Observe],
        at(8),
    );
    name_root(&mut fixture.store, &work, &claim, &host, 9, 8);
    let current = host_verification_from_basis(
        &mut fixture.store,
        &work,
        &claim,
        "runner",
        "test-current",
        VerificationKind::Test,
        VerificationResult::Passed,
        9,
        source("workspace-B", 9),
    );
    NamedEvaluation {
        fixture,
        work,
        claim,
        host,
        current,
    }
}

/// An evaluation of `evidence` that declares `workspace` and `revision` as
/// the source it judged, cut at `cut`.
fn evaluate_declared(
    named: &mut NamedEvaluation,
    cut: i64,
    workspace: &str,
    revision: &str,
    key: &str,
    second: i64,
) -> Result<AcceptanceEvaluationReceipt, StoreError> {
    let mut input = request(
        &named.work,
        cut,
        "runner",
        Mode::SameSession,
        vec![verdict(
            1,
            AcceptanceVerdict::Pass,
            AcceptanceBasis::Observed,
            std::slice::from_ref(&named.current),
        )],
        second,
    );
    input.source_basis = Some(AcceptanceSourceBasis {
        workspace_id: Some(workspace.into()),
        fingerprint: revision.into(),
    });
    input.attempt_key = Some(key.into());
    record(&mut named.fixture.store, &input)
}

/// An evaluation records the named root active at its cut, and it goes
/// stale when that root ends, when the claim is released, or when the claim
/// names a later generation.
#[test]
fn an_evaluation_goes_stale_when_its_named_root_ends_is_released_or_renamed() {
    for change in ["end", "release", "rename"] {
        let mut named = named_evaluation();
        let current = named.current.clone();
        evaluate_check(
            &mut named.fixture.store,
            &named.work,
            &current,
            "current",
            10,
        )
        .expect("the current B check records");
        let status = |named: &NamedEvaluation| {
            named
                .fixture
                .store
                .acceptance_evaluation_status(named.work.work_id, None)
                .expect("status read")
                .expect("the evaluation is visible")
                .stale
        };
        assert_eq!(status(&named), None, "{change}: fresh before the change");
        match change {
            "end" => {
                named
                    .fixture
                    .store
                    .bind_named_root(
                        &named.work.project_id,
                        &named.host.status.session_id,
                        &named.host.connection_token,
                        &named.host.routing_token,
                        named.claim.claim_id,
                        named.claim.fence,
                        "workspace-B",
                        9,
                        at(8),
                        NamedRootBindingKind::Ended,
                        Some(crate::domain::NamedRootEndReason::ExplicitClear),
                        &mut actor("runner"),
                        "end-B-9",
                        at(11),
                    )
                    .expect("the naming session ends B");
            }
            "release" => {
                let item = named
                    .fixture
                    .store
                    .get_work_item(named.work.work_id)
                    .expect("item");
                named
                    .fixture
                    .store
                    .release_work(
                        &crate::domain::ReleaseWorkRequest {
                            work_id: item.work_id,
                            run_id: named.claim.run_id,
                            expected_work_revision: item.revision,
                            holder: named.claim.holder.clone(),
                            claim_id: named.claim.claim_id,
                            claim_fence: named.claim.fence,
                            reason: "stepping away".into(),
                            waiver_reason: None,
                            actor: actor("runner"),
                            idempotency_key: "release-runner".into(),
                            released_at: at(11),
                        },
                        &DevelopmentNoopRedactor,
                    )
                    .expect("release");
            }
            _ => {
                let (work, claim) = (named.work.clone(), named.claim.clone());
                name_root(&mut named.fixture.store, &work, &claim, &named.host, 10, 11);
            }
        }
        assert_eq!(
            status(&named),
            Some(AcceptanceStaleReason::Mutation),
            "{change}"
        );
    }
}

/// Under a named root an evaluation cannot judge another workspace, and it
/// cannot judge a revision other than the root's newest sighting.
#[test]
fn an_evaluation_under_a_named_root_judges_only_that_root_as_it_stands() {
    let mut named = named_evaluation();
    let cut_now = |named: &NamedEvaluation| cut(&named.fixture.store, &named.work);
    let cut_a = cut_now(&named);
    let foreign = refusal(evaluate_declared(
        &mut named,
        cut_a,
        "workspace-A",
        "revision-B",
        "declares-A",
        10,
    ));
    assert!(
        foreign.contains("declares a workspace other than the claim's named source root"),
        "{foreign}"
    );
    let cut_b = cut_now(&named);
    let behind = refusal(evaluate_declared(
        &mut named,
        cut_b,
        "workspace-B",
        "revision-older",
        "older-revision",
        11,
    ));
    assert!(
        behind.contains("does not match the named root's newest sighting"),
        "{behind}"
    );
}

/// An evaluation cut under one named root cannot be recorded once the claim
/// has named another: its basis moved.
#[test]
fn an_evaluation_cut_before_a_rename_is_refused_as_moved() {
    let mut named = named_evaluation();
    let cut_before = cut(&named.fixture.store, &named.work);
    let (work, claim) = (named.work.clone(), named.claim.clone());
    name_root(&mut named.fixture.store, &work, &claim, &named.host, 10, 10);
    let moved = evaluate_declared(
        &mut named,
        cut_before,
        "workspace-B",
        "revision-B",
        "cut-before-rename",
        11,
    );
    assert!(
        matches!(
            &moved,
            Err(StoreError::AcceptanceEvaluationBasisMoved { reason, .. })
                if reason.contains("named source root changed after the evaluated cut")
        ),
        "{moved:?}"
    );
}

/// A work item with criterion 1 bound to a test, its claim as the binding
/// left it, and a host session bound to that claim.
fn bound_host() -> (Fixture, WorkItem, WorkClaim, HostSession) {
    let mut fixture = fixture("project-a");
    let claim = fixture.claim.clone();
    let work = revise(
        &mut fixture.store,
        &fixture.work,
        &claim,
        WorkRevisionPatch {
            acceptance: Some(vec!["run tests".into()]),
            acceptance_bindings: Some(vec![AcceptanceBinding {
                criterion: 1,
                requirement: VerificationRequirement {
                    check_kind: VerificationKind::Test,
                    check_fingerprint: None,
                },
            }]),
            ..empty_patch()
        },
        "bind-test",
        5,
    )
    .expect("bind criterion");
    let claim = crate::storage::work::query::load_work_claim_optional(
        &fixture.store.connection,
        claim.run_id,
    )
    .expect("claim read")
    .expect("the claim is held");
    let host = HostSession::bind(&mut fixture.store, &work, &claim, 6);
    (fixture, work, claim, host)
}

/// A quiet move inside the root after its newest change: the host reports a
/// change in B at R1, then a quiet sighting of B at R2. The root's newest
/// sighting decides what a check must carry, so a check at R2 satisfies the
/// change's obligation and the bound criterion, and the work completes.
#[test]
fn a_check_after_a_quiet_move_inside_the_root_satisfies_and_completes() {
    let (mut fixture, work, claim, mut host) = bound_host();
    let store = &mut fixture.store;
    host_binds(
        store,
        &host,
        &claim,
        "workspace-B",
        9,
        NamedRootBindingKind::Bound,
        30,
        "name-B",
        30,
    )
    .expect("host names B");
    host.basis = workspace("workspace-B", "R1", Some(9));
    host.checkpoint(store, true, None, 40);
    assert!(matches!(
        checkpoint_basis(
            &mut host,
            store,
            workspace("workspace-B", "R2", Some(9)),
            false,
            50,
        ),
        Ok(ControlTurnCheckpointDecision::Checkpointed { .. })
    ));
    host.basis = workspace("workspace-B", "R2", Some(9));
    let check = host.checkpoint(
        store,
        false,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        60,
    )[0]
    .clone();
    let records = store
        .work_run_obligations(claim.run_id)
        .expect("obligations");
    assert_eq!(records.len(), 2, "the bound criterion and the R1 change");
    for record in &records {
        assert!(
            matches!(
                record.resolution.as_ref().map(|event| &event.resolution),
                Some(WorkObligationResolution::Satisfied { evidence, .. }) if evidence == &check
            ),
            "{record:?}"
        );
    }
    checkpoint_then_complete(
        store,
        &work,
        &claim,
        "runner",
        std::slice::from_ref(&check),
        true,
        None,
        "complete",
        70,
    )
    .expect("the R2 check completes the work");
    let report = store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
}

/// Under a named root a report is compared within its own workspace. A host
/// that re-reports workspace A's unchanged revision as a change after the
/// binding, as a restarted host with no baseline would, is read as a repeat:
/// it opens no post-binding obligation, and the work still completes with
/// the earlier A change displaced.
#[test]
fn a_repeated_foreign_report_under_a_named_root_is_no_change() {
    let (mut fixture, work, claim, mut host) = bound_host();
    let store = &mut fixture.store;
    host.basis = workspace("workspace-A", "A1", None);
    host.checkpoint(store, true, None, 10);
    host_binds(
        store,
        &host,
        &claim,
        "workspace-B",
        9,
        NamedRootBindingKind::Bound,
        20,
        "name-B",
        20,
    )
    .expect("host names B");
    assert!(matches!(
        checkpoint_basis(
            &mut host,
            store,
            workspace("workspace-A", "A1", Some(9)),
            true,
            30,
        ),
        Ok(ControlTurnCheckpointDecision::Checkpointed { .. })
    ));
    assert_eq!(
        store
            .work_run_obligations(claim.run_id)
            .expect("obligations")
            .len(),
        2,
        "the bound criterion and the first A change only"
    );
    host.basis = workspace("workspace-B", "R_b", Some(9));
    let check = host.checkpoint(
        store,
        false,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        40,
    )[0]
    .clone();
    let seal = checkpoint_then_complete(
        store,
        &work,
        &claim,
        "runner",
        std::slice::from_ref(&check),
        true,
        None,
        "complete",
        50,
    )
    .expect("the repeat does not block completion");
    assert_eq!(seal.foreign_workspace_changes.len(), 1);
}

/// A root the host has named but not yet sighted anchors no evaluation: its
/// first sighting could show any source. Once the host has sighted the root,
/// an evaluation that declares no source judges that sighting.
#[test]
fn an_evaluation_waits_for_the_named_root_first_sighting() {
    let mut fixture = fixture("project-a");
    enable(
        &mut fixture.store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable-evaluation",
        5,
    );
    let (work, claim, note) = (
        fixture.work.clone(),
        fixture.claim.clone(),
        fixture.evidence.clone(),
    );
    let host = bind_control_for(
        &mut fixture.store,
        "runner",
        "named-evaluation-host",
        &[crate::domain::EffectClass::Observe],
        at(8),
    );
    name_root(&mut fixture.store, &work, &claim, &host, 9, 8);
    let judge = |store: &SqliteStore, key: &str, second: i64| {
        let mut input = request(
            &work,
            cut(store, &work),
            "runner",
            Mode::SameSession,
            vec![verdict(
                1,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::Judgment,
                std::slice::from_ref(&note),
            )],
            second,
        );
        input.attempt_key = Some(key.into());
        input
    };
    let before = judge(&fixture.store, "before-sighting", 10);
    let unsighted = refusal(record(&mut fixture.store, &before));
    assert!(
        unsighted.contains("the named root has no sighting yet"),
        "{unsighted}"
    );
    host_verification_from_basis(
        &mut fixture.store,
        &work,
        &claim,
        "runner",
        "sighting-B",
        VerificationKind::Test,
        VerificationResult::Passed,
        11,
        source("workspace-B", 9),
    );
    let after = judge(&fixture.store, "after-sighting", 12);
    record(&mut fixture.store, &after).expect("the sighted root anchors the evaluation");
    let report = fixture.store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
}

/// Under a named root a cited check must have been produced in the root too:
/// a verification recorded in B whose producing observation ran in workspace
/// A does not carry a bound criterion, though a consistent check in B does.
#[test]
fn a_citation_whose_producer_ran_outside_the_root_is_refused() {
    let mut named = named_evaluation();
    let (work, claim) = (named.work.clone(), named.claim.clone());
    let transaction = named
        .fixture
        .store
        .connection
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .expect("checkpoint transaction");
    let producer = host_check_producer(
        &transaction,
        &work,
        &claim,
        "runner",
        "producer-in-A",
        10,
        source("workspace-A", 9),
    );
    let contradicted = host_verification_of_producer(
        &transaction,
        &work,
        &claim,
        "runner",
        "producer-in-A",
        10,
        10,
        source("workspace-B", 9),
        producer,
    );
    transaction.commit().expect("commit the contradicted check");
    let refused = refusal(evaluate_check(
        &mut named.fixture.store,
        &work,
        &contradicted,
        "contradicted",
        11,
    ));
    assert!(refused.contains("criterion 1"), "{refused}");
    let current = named.current.clone();
    evaluate_check(&mut named.fixture.store, &work, &current, "consistent", 12)
        .expect("the consistent B check records");
}
