//! Real foreign proof selected through ordinary native completion/evaluation.
use super::*;
use crate::task_sync::evidence::consumption::ConsumptionRequest;

#[path = "consumption/foreign_decisions.rs"]
mod foreign_decisions;
#[path = "consumption/judgment.rs"]
mod judgment;
#[path = "consumption/retirement.rs"]
mod retirement;
#[path = "consumption/typed_dependencies.rs"]
mod typed_dependencies;
#[path = "consumption/writer_controls.rs"]
mod writer_controls;
#[path = "consumption/writer_selection.rs"]
mod writer_selection;

struct Selected {
    root: crate::test_support::TempHome,
    origin: Fixture,
    target: Fixture,
    verification: ApplicabilityRequest,
    acceptance: AcceptanceUseRequest,
    original_seal: ObjectId,
}

fn selected(reverse: bool) -> Selected {
    selected_bindings(reverse, true)
}

fn selected_bindings(reverse: bool, bound: bool) -> Selected {
    let root = repository();
    let mut origin = Fixture::with_bindings(root.path(), bound);
    let mut target = destination(&mut origin, root.path());
    target.observe(root.path(), 16);
    let (seal_id, seal) = closure::complete_origin(&mut origin, root.path());
    let check = seal.acceptance[0].evidence[0].clone();
    let (pack, origin_id, cuts) = seal_pack(&origin, &seal_id, &seal, reverse);
    retain(&mut target.store, &pack);
    retain(&mut target.store, &pack);
    let mut verification = intent(&target, check, origin_id.clone());
    verification.origin_cut = cuts
        .iter()
        .find(|id| {
            let (_, body) = object_on(&target.store.connection, id).unwrap().unwrap();
            let cut: crate::task_sync::evidence::witnesses::OriginFeedCut =
                serde_json::from_slice(&body).unwrap();
            cut.source.feed == FeedId::RunExecution(seal.run_id)
        })
        .unwrap()
        .clone();
    verification.criterion = bound.then_some(1);
    let actor = target.local_actor();
    target
        .store
        .associate_imported_verification(&project(), &session(), &actor, &verification, at(21))
        .unwrap();
    let acceptance = AcceptanceUseRequest {
        decision: ObjectId::mint(),
        origin: origin_id,
        proof: HistoricalProof::NativeCompletion {
            seal: seal_id.clone(),
            governing_witness: None,
        },
        origin_cuts: cuts,
        binding: target.binding.clone(),
        verification_decisions: vec![verification.decision.clone()],
    };
    target
        .store
        .associate_imported_acceptance(&project(), &session(), &actor, &acceptance, at(22))
        .unwrap();
    Selected {
        root,
        origin,
        target,
        verification,
        acceptance,
        original_seal: seal_id,
    }
}

fn consume(target: &mut Fixture, decision: &ObjectId, acceptance: bool) -> ConsumptionRequest {
    let request = ConsumptionRequest {
        consumption: ObjectId::mint(),
        decision: decision.clone(),
        binding: target.binding.clone(),
    };
    let actor = target.local_actor();
    let receipt = if acceptance {
        target
            .store
            .consume_imported_acceptance(&project(), &session(), &actor, &request, at(23))
    } else {
        target
            .store
            .consume_imported_verification(&project(), &session(), &actor, &request, at(23))
    }
    .unwrap();
    assert!(!receipt.replayed);
    request
}

fn completion(
    target: &mut Fixture,
    root: &std::path::Path,
    evidence: &ObjectId,
    imported_acceptance: bool,
) -> CompleteWorkRequest {
    let item = target.store.get_work_item(target.binding.work_id).unwrap();
    let claim = target.store.claim_fixture(item.work_id);
    checkpoint(
        &mut target.store,
        &item,
        &claim,
        "runner",
        "selected-checkpoint",
        26,
        std::slice::from_ref(evidence),
    );
    let mut request =
        completion_request(&item, &claim, "runner", evidence, "selected-completion", 28);
    request.source_id = Some(source(root));
    if imported_acceptance
        || target
            .store
            .acceptance_evaluation_policy()
            .unwrap()
            .is_evaluated()
    {
        request.acceptance.clear();
    }
    request
}

fn count(store: &SqliteStore, kind: &str) -> i64 {
    store.connection.query_row("SELECT count(*) FROM objects WHERE object_kind=?1 AND NOT EXISTS(SELECT 1 FROM work_task_sync_evidence_imported imported WHERE imported.object_id=objects.object_id)", [kind], |row| row.get(0)).unwrap()
}

#[test]
fn unbound_full_source_use_satisfies_a_later_stock_change_without_retiming_the_original() {
    let mut data = selected_bindings(false, false);
    data.target.report_source(data.root.path(), 22, true);
    let request = consume(&mut data.target, &data.verification.decision, false);
    let obligations = data
        .target
        .store
        .work_run_obligations(data.target.binding.run_id)
        .unwrap();
    assert_ne!(obligations.len(), 0);
    assert!(
        obligations
            .iter()
            .all(|row| row.state == WorkObligationState::Satisfied)
    );
    let input = completion(
        &mut data.target,
        data.root.path(),
        &request.consumption,
        false,
    );
    data.target
        .store
        .complete_work(&input, &DevelopmentNoopRedactor)
        .unwrap();
    healthy(&data.target.store);
}

#[test]
fn selected_foreign_verification_supports_an_observed_local_evaluation_and_completion() {
    let mut data = selected(false);
    assert!(
        data.target
            .store
            .imported_verification_summary_for_project(
                &ProjectId("another-project".into()),
                &data.verification.decision,
            )
            .is_err()
    );
    let use_request = consume(&mut data.target, &data.verification.decision, false);
    assert_eq!(count(&data.target.store, "verification_evidence"), 0);
    let evaluation = evaluate(&mut data.target, data.root.path(), &use_request.consumption);
    let request = completion(
        &mut data.target,
        data.root.path(),
        &use_request.consumption,
        false,
    );
    let seal = data
        .target
        .store
        .complete_work(&request, &DevelopmentNoopRedactor)
        .unwrap();
    assert_eq!(seal.acceptance_evaluation, Some(evaluation.evaluation));
    assert_eq!(
        seal.acceptance[0].evidence,
        vec![use_request.consumption.clone()]
    );
    let consumed: String = data
        .target
        .store
        .connection
        .query_row(
            "SELECT consumed_by FROM work_task_sync_evidence_applicability WHERE decision_id=?1",
            [data.verification.decision.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(consumed, use_request.consumption.as_str());
    // The rebuildable index is derived from native canonical use; no durable
    // evidence row or original producer is rewritten by this repair.
    data.target
        .store
        .connection
        .execute(
            "UPDATE work_task_sync_evidence_applicability SET consumed_by=NULL",
            [],
        )
        .unwrap();
    crate::storage::work::task_sync::evidence::consumption::rebuild(&data.target.store.connection)
        .unwrap();
    healthy(&data.target.store);
}

#[test]
fn selected_original_evaluation_completes_under_observed_independent_policy_without_a_new_verdict()
{
    let root = repository();
    let mut target = Fixture::new(root.path());
    let decision = prepare(&mut target, root.path(), false);
    let actor = target.local_actor();
    target
        .store
        .associate_imported_acceptance(&project(), &session(), &actor, &decision, at(22))
        .unwrap();
    assert!(
        target
            .store
            .imported_acceptance_summary_for_project(
                &ProjectId("another-project".into()),
                &decision.decision,
                at(22),
            )
            .is_err()
    );
    let use_request = consume(&mut target, &decision.decision, true);
    let evaluations = count(&target.store, "acceptance_evaluation");
    let request = completion(&mut target, root.path(), &use_request.consumption, true);
    let seal = target
        .store
        .complete_work(&request, &DevelopmentNoopRedactor)
        .unwrap();
    assert!(seal.acceptance_evaluation.is_none());
    assert_eq!(count(&target.store, "acceptance_evaluation"), evaluations);
    assert_eq!(seal.acceptance[0].evidence, vec![use_request.consumption]);
    healthy(&target.store);
}

#[test]
fn selected_foreign_native_seal_derives_a_distinct_local_seal_without_a_verdict_write() {
    for reverse in [false, true] {
        let mut data = selected(reverse);
        let original = object_on(&data.target.store.connection, &data.original_seal)
            .unwrap()
            .unwrap()
            .1;
        let request = consume(&mut data.target, &data.acceptance.decision, true);
        let use_bytes = object_on(&data.target.store.connection, &request.consumption)
            .unwrap()
            .unwrap()
            .1;
        let before = count(&data.target.store, "acceptance_evaluation");
        let input = completion(
            &mut data.target,
            data.root.path(),
            &request.consumption,
            true,
        );
        let seal = data
            .target
            .store
            .complete_work(&input, &DevelopmentNoopRedactor)
            .unwrap();
        assert_eq!(seal.run_id, data.target.binding.run_id);
        assert_ne!(seal.run_id, data.origin.binding.run_id);
        assert!(seal.acceptance_evaluation.is_none());
        assert_eq!(
            seal.acceptance[0].evidence,
            vec![request.consumption.clone()]
        );
        assert_eq!(count(&data.target.store, "acceptance_evaluation"), before);
        assert_eq!(
            object_on(&data.target.store.connection, &data.original_seal)
                .unwrap()
                .unwrap()
                .1,
            original
        );
        assert_eq!(
            object_on(&data.target.store.connection, &request.consumption)
                .unwrap()
                .unwrap()
                .1,
            use_bytes
        );
        let actor = data.target.local_actor();
        assert!(
            data.target
                .store
                .consume_imported_acceptance(&project(), &session(), &actor, &request, at(31))
                .unwrap()
                .replayed
        );
        healthy(&data.target.store);
        let item = data
            .target
            .store
            .get_work_item(data.target.binding.work_id)
            .unwrap();
        data.target
            .store
            .reopen_work(
                &ReopenWorkRequest {
                    work_id: item.work_id,
                    expected_work_revision: item.revision,
                    reason: "new generation".into(),
                    actor: actor.clone(),
                    idempotency_key: "reopen-selected".into(),
                    reopened_at: at(33),
                },
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        let mut fresh = request.clone();
        fresh.consumption = ObjectId::mint();
        assert!(
            data.target
                .store
                .consume_imported_acceptance(&project(), &session(), &actor, &fresh, at(34))
                .is_err()
        );
        assert_eq!(
            object_on(&data.target.store.connection, &request.consumption)
                .unwrap()
                .unwrap()
                .1,
            use_bytes
        );
        healthy(&data.target.store);
    }
}

#[test]
fn new_completion_rechecks_source_governing_policy_claim_and_late_terminal_conflict() {
    for fault in ["source", "governing", "policy", "claim", "conflict"] {
        let mut data = selected(false);
        let use_request = consume(&mut data.target, &data.acceptance.decision, true);
        let mut input = completion(
            &mut data.target,
            data.root.path(),
            &use_request.consumption,
            true,
        );
        match fault {
            "source" => {
                std::fs::write(data.root.path().join("file"), "changed after consumption\n")
                    .unwrap();
            }
            "governing" => {
                let item = data
                    .target
                    .store
                    .get_work_item(data.target.binding.work_id)
                    .unwrap();
                let actor = data.target.local_actor();
                data.target
                    .store
                    .revise_work(
                        &ReviseWorkRequest {
                            work_id: item.work_id,
                            expected_revision: item.revision,
                            patch: WorkRevisionPatch {
                                acceptance: Some(vec!["different criterion".into()]),
                                ..WorkRevisionPatch::default()
                            },
                            authority: WorkPlanningAuthority::Claim {
                                run_id: data.target.binding.run_id,
                                holder: session(),
                                claim_id: data.target.binding.claim_id,
                                claim_fence: data.target.binding.claim_fence,
                            },
                            actor,
                            idempotency_key: "change-selected-governing".into(),
                            updated_at: at(27),
                        },
                        &DevelopmentNoopRedactor,
                    )
                    .unwrap();
            }
            "policy" => enable(&mut data.target.store),
            "claim" => input.completed_at = at(4_000),
            "conflict" => {
                data.origin.request.operation_key = "later-conflict".into();
                data.origin.request.completion.result.verdict = RemoteRunVerdict::Fail;
                let refused = data.origin.record(24);
                let conflict = pack(
                    &data.acceptance.origin,
                    vec![envelope(
                        &data.origin.store,
                        &data.acceptance.origin,
                        &refused.operation,
                    )],
                );
                retain(&mut data.target.store, &conflict);
            }
            _ => unreachable!(),
        }
        let before =
            crate::storage::test_database_shape_snapshot(&data.target.store.connection).unwrap();
        assert!(
            data.target
                .store
                .complete_work(&input, &DevelopmentNoopRedactor)
                .is_err(),
            "{fault}"
        );
        assert_eq!(
            before,
            crate::storage::test_database_shape_snapshot(&data.target.store.connection).unwrap(),
            "{fault}"
        );
    }
}

#[test]
fn native_consumption_rolls_back_all_appends_and_rejects_missing_original_context() {
    let mut data = selected(false);
    let actor = data.target.local_actor();
    let request = ConsumptionRequest {
        consumption: ObjectId::mint(),
        decision: data.verification.decision.clone(),
        binding: data.target.binding.clone(),
    };
    data.target.store.connection.execute_batch("CREATE TEMP TRIGGER refuse_consumption BEFORE UPDATE OF consumed_by ON work_task_sync_evidence_applicability BEGIN SELECT RAISE(ABORT,'fixture consumption rollback'); END;").unwrap();
    let before =
        crate::storage::test_database_shape_snapshot(&data.target.store.connection).unwrap();
    assert!(
        data.target
            .store
            .consume_imported_verification(&project(), &session(), &actor, &request, at(23))
            .is_err()
    );
    assert_eq!(
        before,
        crate::storage::test_database_shape_snapshot(&data.target.store.connection).unwrap()
    );
    data.target
        .store
        .connection
        .execute_batch("DROP TRIGGER refuse_consumption")
        .unwrap();
    data.target
        .store
        .connection
        .execute(
            "DELETE FROM work_task_sync_evidence_feed_members WHERE object_id=?1",
            [data.verification.original.as_str()],
        )
        .unwrap();
    let before =
        crate::storage::test_database_shape_snapshot(&data.target.store.connection).unwrap();
    assert!(
        data.target
            .store
            .consume_imported_verification(&project(), &session(), &actor, &request, at(24))
            .is_err()
    );
    assert_eq!(
        before,
        crate::storage::test_database_shape_snapshot(&data.target.store.connection).unwrap()
    );
}
