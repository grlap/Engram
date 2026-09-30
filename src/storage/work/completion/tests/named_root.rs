use super::*;
use crate::domain::{
    AcceptanceBinding, ExecutionSourceBasis, NamedRootBindingKind, SourceRootState,
    VerificationKind, VerificationRequirement, VerificationResult,
};
use crate::storage::test_support::bind_control_for;

fn basis(workspace: &str, revision: &str, generation: Option<i64>) -> ExecutionSourceBasis {
    ExecutionSourceBasis {
        workspace_id: workspace.into(),
        source_revision: revision.into(),
        source_root_generation: generation,
        source_root_state: generation.map(|_| SourceRootState::Named),
    }
}

fn named_work() -> (SqliteStore, WorkItem, WorkClaim, ObjectId) {
    named_work_in(SqliteStore::open_in_memory().expect("store"))
}

/// A bound work item claimed by `runner` in `store`, with a source change in
/// workspace A and then the host naming root B at generation 9.
fn named_work_in(mut store: SqliteStore) -> (SqliteStore, WorkItem, WorkClaim, ObjectId) {
    let mut request = root_request("project-a", "named-root-work", 1);
    request.acceptance = vec!["run a test".into()];
    request.acceptance_bindings = vec![AcceptanceBinding {
        criterion: 1,
        requirement: VerificationRequirement {
            check_kind: VerificationKind::Test,
            check_fingerprint: None,
        },
    }];
    let work = store
        .create_work(&request, &DevelopmentNoopRedactor)
        .expect("work");
    let claim = claim(&mut store, &work, "runner", "named-root-claim", 2, 300);
    let earlier = source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "foreign-before-name",
        3,
        Some(basis("workspace-A", "A3", None)),
        None,
    );
    let host = bind_control_for(
        &mut store,
        "runner",
        "named-root-host",
        &[crate::domain::EffectClass::Observe],
        at(4),
    );
    let mut host_actor = actor("runner");
    store
        .bind_named_root(
            &work.project_id,
            &host.status.session_id,
            &host.connection_token,
            &host.routing_token,
            claim.claim_id,
            claim.fence,
            "workspace-B",
            9,
            at(4),
            NamedRootBindingKind::Bound,
            None,
            &mut host_actor,
            "name-B",
            at(4),
        )
        .expect("host names B");
    (store, work, claim, earlier)
}

/// The obligation `pick` selects: the bound criterion's, or the stock one a
/// named change triggered.
enum Pick<'a> {
    Criterion,
    TriggeredBy(&'a ObjectId),
}

/// The state of the one obligation `pick` selects, with the evidence that
/// satisfied it when it is satisfied.
fn obligation(
    store: &SqliteStore,
    claim: &WorkClaim,
    pick: &Pick<'_>,
) -> (WorkObligationState, Option<ObjectId>) {
    let records = store
        .work_run_obligations(claim.run_id)
        .expect("obligations");
    let matching = records
        .iter()
        .filter(|record| {
            let criterion =
                crate::control::acceptance_binding_criterion(&record.obligation.rule).is_some();
            match pick {
                Pick::Criterion => criterion,
                Pick::TriggeredBy(change) => {
                    !criterion && &record.obligation.triggering_observation == *change
                }
            }
        })
        .collect::<Vec<_>>();
    assert_eq!(matching.len(), 1, "exactly one obligation is selected");
    let satisfying = match matching[0]
        .resolution
        .as_ref()
        .map(|event| &event.resolution)
    {
        Some(WorkObligationResolution::Satisfied { evidence, .. }) => Some(evidence.clone()),
        _ => None,
    };
    (matching[0].state, satisfying)
}

fn stock_completion_action(
    store: &SqliteStore,
    claim: &WorkClaim,
    change: &ObjectId,
) -> WorkObligationCompletionAction {
    let records = store
        .work_run_obligations(claim.run_id)
        .expect("obligations");
    let stock = records
        .iter()
        .find(|record| {
            record.state == WorkObligationState::Open
                && &record.obligation.triggering_observation == change
                && crate::control::is_stock_source_change_obligation(
                    &record.obligation.rule,
                    &record.obligation.requirement,
                )
        })
        .expect("open stock source-change obligation");
    store
        .work_obligation_completion_actions(&[&stock.obligation])
        .expect("completion guidance")[0]
}

/// A write transaction standing for one host checkpoint.
fn begin(store: &mut SqliteStore) -> rusqlite::Transaction<'_> {
    store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("checkpoint transaction")
}

/// A check the host verified only after a change whose root is unknown,
/// but that ran before it, cannot account for that change: what counts is
/// when the check ran, its producer's place on the run feed, not when its
/// verification was recorded. This holds whether the verification cites a
/// producer stored in an earlier checkpoint or arrives in the same
/// checkpoint as the change.
#[test]
fn a_check_run_before_an_unknown_root_change_cannot_account_for_it() {
    for same_checkpoint in [false, true] {
        let (mut store, work, claim, _) = named_work();
        let check_basis = || basis("workspace-B", "B5", Some(9));
        let (unknown, check) = if same_checkpoint {
            let transaction = begin(&mut store);
            let producer = host_check_producer(
                &transaction,
                &work,
                &claim,
                "runner",
                "early-check",
                5,
                check_basis(),
            );
            let change = source_mutation_observation(
                &transaction,
                &work,
                &claim,
                "runner",
                "unknown-root",
                6,
                None,
                None,
            );
            let unknown = append_source_mutation_on(&transaction, &change);
            let check = host_verification_of_producer(
                &transaction,
                &work,
                &claim,
                "runner",
                "early-check",
                5,
                7,
                check_basis(),
                producer,
            );
            transaction.commit().expect("commit the checkpoint");
            (unknown, check)
        } else {
            let transaction = begin(&mut store);
            let producer = host_check_producer(
                &transaction,
                &work,
                &claim,
                "runner",
                "early-check",
                5,
                check_basis(),
            );
            transaction.commit().expect("commit the producer");
            let unknown = source_mutation_from_basis(
                &mut store,
                &work,
                &claim,
                "runner",
                "unknown-root",
                6,
                None,
                None,
            );
            let transaction = begin(&mut store);
            let check = host_verification_of_producer(
                &transaction,
                &work,
                &claim,
                "runner",
                "early-check",
                5,
                7,
                check_basis(),
                producer,
            );
            transaction.commit().expect("commit the late verification");
            (unknown, check)
        };
        for pick in [Pick::Criterion, Pick::TriggeredBy(&unknown)] {
            assert_eq!(
                obligation(&store, &claim, &pick),
                (WorkObligationState::Open, None),
                "same checkpoint: {same_checkpoint}"
            );
        }
        checkpoint(
            &mut store,
            &work,
            &claim,
            "runner",
            "checkpoint-early-check",
            8,
            std::slice::from_ref(&check),
        );
        assert!(
            matches!(
                complete(
                    &mut store,
                    &work,
                    &claim,
                    "runner",
                    &check,
                    "complete-early",
                    9
                ),
                Err(StoreError::WorkCompletionRefused { .. })
            ),
            "same checkpoint: {same_checkpoint}"
        );
    }
}

/// A change whose root is unknown, after a known change in the named root,
/// leaves that change's revision behind. A fresh check in the root after
/// both, at the root's newest revision, accounts for the two changes and
/// the bound criterion, and the work completes.
#[test]
fn a_fresh_check_after_an_unknown_root_change_accounts_for_an_older_named_root_change() {
    let (mut store, work, claim, _) = named_work();
    let known = source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "change-in-B",
        5,
        Some(basis("workspace-B", "B5", Some(9))),
        None,
    );
    let unknown = source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "unknown-root-after-B",
        6,
        None,
        None,
    );
    let fresh = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "fresh-test-in-B",
        VerificationKind::Test,
        VerificationResult::Passed,
        7,
        basis("workspace-B", "B7", Some(9)),
    );
    for pick in [
        Pick::Criterion,
        Pick::TriggeredBy(&known),
        Pick::TriggeredBy(&unknown),
    ] {
        assert_eq!(
            obligation(&store, &claim, &pick),
            (WorkObligationState::Satisfied, Some(fresh.clone()))
        );
    }
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-fresh-B",
        8,
        std::slice::from_ref(&fresh),
    );
    complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &fresh,
        "complete-fresh-B",
        9,
    )
    .expect("the fresh check accounts for both changes");
    let report = store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
}

#[test]
fn a_check_in_the_named_root_seals_and_discloses_an_earlier_foreign_change() {
    let (mut store, work, claim, earlier) = named_work();
    let checked = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-in-B",
        VerificationKind::Test,
        VerificationResult::Passed,
        5,
        basis("workspace-B", "B5", Some(9)),
    );
    let obligations = store
        .work_run_obligations(claim.run_id)
        .expect("obligations");
    assert_eq!(
        obligations
            .iter()
            .filter(|record| record.state == WorkObligationState::Satisfied)
            .count(),
        1
    );
    assert_eq!(
        obligations
            .iter()
            .filter(|record| record.state == WorkObligationState::Open)
            .count(),
        1
    );
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-B",
        6,
        std::slice::from_ref(&checked),
    );
    let seal = complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &checked,
        "complete-B",
        7,
    )
    .expect("B check satisfies the bound criterion");
    assert_eq!(seal.foreign_workspace_changes, vec![earlier]);
    let records = store
        .work_run_obligations(claim.run_id)
        .expect("terminal obligations");
    assert!(records.iter().any(|record| matches!(
        record.resolution.as_ref().map(|event| &event.resolution),
        Some(WorkObligationResolution::Displaced { trigger_workspace_id, .. })
            if trigger_workspace_id == "workspace-A"
    )));
    let report = store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
}

#[test]
fn later_named_root_and_foreign_changes_do_not_inherit_an_older_pass() {
    let (mut store, work, claim, _) = named_work();
    let checked = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-in-B-before-change",
        VerificationKind::Test,
        VerificationResult::Passed,
        5,
        basis("workspace-B", "B5", Some(9)),
    );
    source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "foreign-after-name",
        6,
        Some(basis("workspace-A", "A6", Some(9))),
        None,
    );
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-foreign",
        7,
        std::slice::from_ref(&checked),
    );
    assert!(matches!(
        complete(&mut store, &work, &claim, "runner", &checked, "complete-foreign", 8),
        Err(StoreError::WorkCompletionRefused { reason, .. }) if reason.contains("foreign workspace")
    ));
}

#[test]
fn a_named_root_change_after_the_check_needs_a_new_check() {
    let (mut store, work, claim, _) = named_work();
    let checked = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-in-B-before-change",
        VerificationKind::Test,
        VerificationResult::Passed,
        5,
        basis("workspace-B", "B5", Some(9)),
    );
    source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "B-after-check",
        6,
        Some(basis("workspace-B", "B6", Some(9))),
        None,
    );
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-new-B",
        7,
        std::slice::from_ref(&checked),
    );
    let refused = complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &checked,
        "complete-stale-B",
        8,
    );
    assert!(
        matches!(
            &refused,
            Err(StoreError::WorkBoundVerificationRefused { reason, cause, .. })
                if reason.contains("latest source change")
                    && cause.verification == checked
                    && cause.mismatch == crate::domain::VerificationEvidenceMismatch::StaleSourceRevision
                    && cause.remedy == crate::domain::BoundVerificationRemedy::RunCurrentCheck
        ),
        "{refused:?}"
    );
}

#[test]
fn an_unknown_root_change_needs_a_fresh_named_root_check() {
    let (mut store, work, claim, _) = named_work();
    let old_check = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-before-unknown",
        VerificationKind::Test,
        VerificationResult::Passed,
        5,
        basis("workspace-B", "B5", Some(9)),
    );
    source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "unknown-after-name",
        6,
        None,
        None,
    );
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-unknown",
        7,
        std::slice::from_ref(&old_check),
    );
    let refused = complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &old_check,
        "complete-unknown",
        8,
    );
    assert!(
        matches!(
            &refused,
            Err(StoreError::WorkCompletionRefused { reason, .. }) if reason.contains("unknown workspace")
        ),
        "{refused:?}"
    );

    let fresh_check = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-after-unknown",
        VerificationKind::Test,
        VerificationResult::Passed,
        9,
        basis("workspace-B", "B9", Some(9)),
    );
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-fresh",
        10,
        &[old_check, fresh_check.clone()],
    );
    complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &fresh_check,
        "complete-fresh",
        11,
    )
    .expect("fresh named-root check accounts for unknown source change");
}

#[test]
fn a_foreign_check_cannot_satisfy_a_named_root_source_change() {
    let (mut store, work, claim, _) = named_work();
    let change_in_b = source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "change-in-B",
        5,
        Some(basis("workspace-B", "same-revision", Some(9))),
        None,
    );
    let foreign_check = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-in-A",
        VerificationKind::Test,
        VerificationResult::Passed,
        6,
        basis("workspace-A", "same-revision", Some(9)),
    );
    // Each obligation the foreign check might wrongly satisfy stays open;
    // the fixture's earlier foreign change keeps its own obligation open too,
    // so a check over all of them would prove nothing.
    for pick in [Pick::Criterion, Pick::TriggeredBy(&change_in_b)] {
        assert_eq!(
            obligation(&store, &claim, &pick),
            (WorkObligationState::Open, None)
        );
    }
    let correct_check = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-in-B",
        VerificationKind::Test,
        VerificationResult::Passed,
        7,
        basis("workspace-B", "same-revision", Some(9)),
    );
    for pick in [Pick::Criterion, Pick::TriggeredBy(&change_in_b)] {
        assert_eq!(
            obligation(&store, &claim, &pick),
            (WorkObligationState::Satisfied, Some(correct_check.clone()))
        );
    }
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-foreign-check",
        8,
        &[foreign_check, correct_check.clone()],
    );
    complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &correct_check,
        "complete-with-B",
        9,
    )
    .expect("B check satisfies B source change");
}

/// The generation of the claim's active named root, if it has one.
fn active_generation(store: &SqliteStore, claim: &WorkClaim) -> Option<i64> {
    super::super::named_root_context_on(&store.connection, claim.run_id, claim.claim_id, i64::MAX)
        .expect("named-root lookup")
        .map(|root| root.binding.generation)
}

fn name_root(
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    generation: i64,
    second: i64,
) {
    name_root_in(store, work, claim, "workspace-B", generation, second);
}

fn name_root_in(
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    workspace: &str,
    generation: i64,
    second: i64,
) {
    let host = bind_control_for(
        store,
        &claim.holder.0,
        &format!("named-root-host-{generation}"),
        &[crate::domain::EffectClass::Observe],
        at(second),
    );
    store
        .bind_named_root(
            &work.project_id,
            &host.status.session_id,
            &host.connection_token,
            &host.routing_token,
            claim.claim_id,
            claim.fence,
            workspace,
            generation,
            at(second),
            NamedRootBindingKind::Bound,
            None,
            &mut actor(&claim.holder.0),
            &format!("name-B-{generation}"),
            at(second),
        )
        .expect("host names B");
}

fn release(store: &mut SqliteStore, work: &WorkItem, claim: &WorkClaim, key: &str, second: i64) {
    let work = store.get_work_item(work.work_id).expect("item");
    store
        .release_work(
            &ReleaseWorkRequest {
                work_id: work.work_id,
                run_id: claim.run_id,
                expected_work_revision: work.revision,
                holder: claim.holder.clone(),
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                reason: "stepping away from the run".into(),
                waiver_reason: Some("nothing to contribute yet".into()),
                actor: actor(&claim.holder.0),
                idempotency_key: key.into(),
                released_at: at(second),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("release the claim");
}

/// A binding belongs to the claim, not to whoever holds it: renewal, a
/// handoff and recovery by another holder keep it, because the claim goes
/// on. Releasing the claim ends it, although the next claim of the run
/// reuses the claim id.
#[test]
fn a_binding_survives_holder_changes_on_its_claim_but_not_its_release() {
    let (mut store, work, claim, _) = named_work();
    assert_eq!(active_generation(&store, &claim), Some(9));
    let renewed = super::claim(&mut store, &work, "runner", "renew-runner", 5, 300);
    assert_eq!(renewed.claim_id, claim.claim_id);
    assert_eq!(active_generation(&store, &renewed), Some(9), "renewal");
    let work = store.get_work_item(work.work_id).expect("item");
    let offer = store
        .offer_work_handoff(
            &OfferWorkHandoffRequest {
                work_id: work.work_id,
                run_id: renewed.run_id,
                expected_work_revision: work.revision,
                from: renewed.holder.clone(),
                to: SessionId("second".into()),
                claim_id: renewed.claim_id,
                claim_fence: renewed.fence,
                ttl_seconds: 300,
                checkpoint_summary: "handing the run to second".into(),
                actor: actor("runner"),
                idempotency_key: "offer-to-second".into(),
                offered_at: at(6),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("offer handoff");
    let handed = store
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
    assert_eq!(handed.claim_id, claim.claim_id);
    assert_eq!(active_generation(&store, &handed), Some(9), "handoff");
    let work = store.get_work_item(work.work_id).expect("item");
    let recovered = super::claim(&mut store, &work, "third", "recover-as-third", 2_000, 300);
    assert_eq!(recovered.holder, SessionId("third".into()));
    assert_eq!(recovered.claim_id, claim.claim_id);
    assert_eq!(active_generation(&store, &recovered), Some(9), "recovery");
    release(&mut store, &work, &recovered, "release-third", 2_001);
    let work = store.get_work_item(work.work_id).expect("item");
    let reclaimed = super::claim(&mut store, &work, "third", "reclaim-as-third", 2_002, 300);
    assert_eq!(reclaimed.claim_id, claim.claim_id);
    assert_eq!(
        active_generation(&store, &reclaimed),
        None,
        "release ends the binding"
    );
    let report = store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
}

/// After its release a claim is unbound. A host that missed the release
/// may go on stating the old generation; that sighting is accepted as
/// recorded but is unbound: when the claim later names a fresh generation,
/// it is neither judged inside the root nor displaced, and like any unbound
/// change it is satisfied by a later matching check, while a change from
/// before the claim's first name is still displaced.
#[test]
fn a_released_generation_never_displaces_a_later_change() {
    let (mut store, work, claim, earlier) = named_work();
    release(&mut store, &work, &claim, "release-runner", 5);
    let work = store.get_work_item(work.work_id).expect("item");
    let claim = super::claim(&mut store, &work, "runner", "reclaim-runner", 6, 300);
    assert_eq!(active_generation(&store, &claim), None);
    let stale = source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "stale-generation-in-D",
        7,
        Some(basis("workspace-D", "D7", Some(9))),
        None,
    );
    name_root(&mut store, &work, &claim, 10, 8);
    assert_eq!(active_generation(&store, &claim), Some(10));
    let checked = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-in-B-10",
        VerificationKind::Test,
        VerificationResult::Passed,
        9,
        basis("workspace-B", "B9", Some(10)),
    );
    assert_eq!(
        obligation(&store, &claim, &Pick::Criterion),
        (WorkObligationState::Satisfied, Some(checked.clone()))
    );
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-B-10",
        10,
        std::slice::from_ref(&checked),
    );
    let seal = complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &checked,
        "complete-B-10",
        11,
    )
    .expect("the B check completes the work");
    assert_eq!(seal.foreign_workspace_changes, vec![earlier.clone()]);
    let records = store
        .work_run_obligations(claim.run_id)
        .expect("terminal obligations");
    let resolution_of = |change: &ObjectId| {
        records
            .iter()
            .find(|record| &record.obligation.triggering_observation == change)
            .and_then(|record| record.resolution.as_ref())
            .map(|event| event.resolution.clone())
    };
    assert!(matches!(
        resolution_of(&earlier),
        Some(WorkObligationResolution::Displaced { .. })
    ));
    assert!(matches!(
        resolution_of(&stale),
        Some(WorkObligationResolution::Satisfied { evidence, .. }) if evidence == checked
    ));
    let report = store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
}

/// The doctor traces the generation a sighting states to an event recorded
/// before it: a generation the claim never bound, or an end that was never
/// recorded, is reported by the sighting's record id.
#[test]
fn the_doctor_reports_a_stated_generation_it_cannot_trace() {
    let (mut store, work, claim, _) = named_work();
    let report = store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
    let untraced = source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "no-such-generation",
        5,
        Some(basis("workspace-B", "B5", Some(12))),
        None,
    );
    let mut ended = basis("workspace-B", "B6", Some(9));
    ended.source_root_state = Some(SourceRootState::Ended);
    let never_ended = source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "never-ended",
        6,
        Some(ended),
        None,
    );
    let report = store.verify_all().expect("doctor");
    for id in [&untraced, &never_ended] {
        let label = format!(
            "execution_observation:{id}:source root generation without its recorded binding"
        );
        assert!(
            report.invalid_work_records.contains(&label),
            "{label}: {report:?}"
        );
    }
}

/// A named-root bind receipt must name the event it recorded, at that
/// event's run-feed position and with its workspace and generation. A
/// receipt changed in any of them is reported, and the original is healthy.
#[test]
fn the_doctor_ties_a_named_root_receipt_to_its_event() {
    let (store, _, _, _) = named_work();
    let (sequence, stored): (i64, Vec<u8>) = store
        .connection
        .query_row(
            "SELECT sequence, result_json FROM control_operation_results
             WHERE operation = 'named_root_bind'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("the bind receipt");
    let original: serde_json::Value = serde_json::from_slice(&stored).expect("receipt JSON");
    let write = |receipt: &serde_json::Value| {
        store
            .connection
            .execute(
                "UPDATE control_operation_results SET result_json = ?1 WHERE sequence = ?2",
                rusqlite::params![
                    serde_json_canonicalizer::to_vec(receipt).expect("canonical receipt"),
                    sequence
                ],
            )
            .expect("rewrite the receipt");
    };
    for (field, value) in [
        ("workspace_id", serde_json::json!("workspace-C")),
        ("event", serde_json::json!(ObjectId::mint())),
        ("generation", serde_json::json!(10)),
    ] {
        let mut altered = original.clone();
        altered[field] = value;
        write(&altered);
        let report = store.verify_all().expect("doctor");
        assert!(
            report
                .invalid_control_records
                .contains(&format!("control_operation:{sequence}")),
            "{field}: {report:?}"
        );
    }
    write(&original);
    let report = store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
}

/// A displaced change reaches the agent: after `done`, the item's
/// obligation page, which `done` and `show` render, names the change the
/// named root displaced with its workspace and revision, and counts it.
#[test]
fn the_obligation_page_names_a_displaced_change() {
    let directory = crate::test_support::temp_home().expect("directory");
    let database = directory.path().join("named-root.db");
    let (mut store, work, claim, _) = named_work_in(SqliteStore::open(&database).expect("store"));
    let checked = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-in-B",
        VerificationKind::Test,
        VerificationResult::Passed,
        5,
        basis("workspace-B", "B5", Some(9)),
    );
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-B",
        6,
        std::slice::from_ref(&checked),
    );
    complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &checked,
        "complete-B",
        7,
    )
    .expect("the B check completes the work");
    drop(store);
    let service = LocalWorkService::new(
        database,
        work.project_id.clone(),
        "reader".into(),
        SessionId("reader".into()),
        None,
    );
    let focus = service
        .work_focus(&work.short_ref, at(8))
        .expect("focus the completed item");
    let page = &focus.obligation_page;
    assert_eq!(page.displaced_total, 1);
    assert_eq!(
        page.items
            .iter()
            .filter_map(|item| item.displaced_change.clone())
            .collect::<Vec<_>>(),
        vec![crate::DisplacedSourceChange {
            observation_id: "write-foreign-before-name".into(),
            workspace_id: "workspace-A".into(),
            source_revision: "A3".into(),
        }]
    );
    assert_eq!(page.untested_total, 0, "a displaced change is not untested");
}

/// After the claim names a later generation, a check under the earlier one
/// no longer carries the bound criterion: `done` asks for a passing check in
/// the current named root.
#[test]
fn after_a_rename_a_check_under_the_old_generation_carries_no_bound_criterion() {
    let (mut store, work, claim, _) = named_work();
    let checked = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-under-9",
        VerificationKind::Test,
        VerificationResult::Passed,
        5,
        basis("workspace-B", "B5", Some(9)),
    );
    assert_eq!(
        obligation(&store, &claim, &Pick::Criterion),
        (WorkObligationState::Satisfied, Some(checked.clone()))
    );
    name_root(&mut store, &work, &claim, 10, 6);
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-after-rename",
        7,
        std::slice::from_ref(&checked),
    );
    let refused = complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &checked,
        "complete-after-rename",
        8,
    );
    assert!(
        matches!(
            &refused,
            Err(StoreError::WorkCompletionRefused { reason, .. })
                if reason.contains("requires a passing check in the current named source root")
        ),
        "{refused:?}"
    );
}

/// The doctor holds a claim's naming history to what the host writer
/// admits: an end of a generation that a later name already superseded is
/// reported, although its fields repeat the bound event it names.
#[test]
fn the_doctor_reports_an_end_of_a_superseded_generation() {
    let (mut store, work, claim, _) = named_work();
    name_root(&mut store, &work, &claim, 10, 5);
    let report = store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
    let bytes: Vec<u8> = store
        .connection
        .query_row(
            "SELECT canonical_json FROM objects
             WHERE object_kind = 'named_root_binding'
               AND json_extract(canonical_json, '$.generation') = 9",
            [],
            |row| row.get(0),
        )
        .expect("the generation-9 binding");
    let mut ended: crate::domain::NamedRootBindingEvent =
        serde_json::from_slice(&bytes).expect("binding event");
    ended.kind = NamedRootBindingKind::Ended;
    ended.end_reason = Some(crate::domain::NamedRootEndReason::ExplicitClear);
    ended.recorded_at = at(6);
    let transaction = begin(&mut store);
    let (id, _) = crate::storage::work::append_named_root_binding_on(&transaction, &ended)
        .expect("append the end of generation 9");
    transaction.commit().expect("commit the end");
    let report = store.verify_all().expect("doctor");
    assert!(
        report
            .invalid_work_records
            .contains(&format!("named_root_binding:{id}")),
        "{report:?}"
    );
}

/// A later name displaces a change made in an earlier name's own root. The
/// claim moves its root from B at generation 9 to C at generation 10; the
/// change made in B under 9 is displaced and disclosed together with the
/// change from before any name, and a check in C completes the work.
#[test]
fn a_later_name_displaces_a_change_made_in_the_earlier_root() {
    let (mut store, work, claim, earlier) = named_work();
    let in_b = source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "change-in-B-under-9",
        5,
        Some(basis("workspace-B", "B5", Some(9))),
        None,
    );
    name_root_in(&mut store, &work, &claim, "workspace-C", 10, 6);
    assert_eq!(active_generation(&store, &claim), Some(10));
    for change in [&earlier, &in_b] {
        assert_eq!(
            stock_completion_action(&store, &claim, change),
            WorkObligationCompletionAction::DoneDisplaces
        );
    }
    let checked = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-in-C-10",
        VerificationKind::Test,
        VerificationResult::Passed,
        7,
        basis("workspace-C", "C7", Some(10)),
    );
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-C-10",
        8,
        std::slice::from_ref(&checked),
    );
    let seal = complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &checked,
        "complete-C-10",
        9,
    )
    .expect("the check in C completes the work");
    let mut displaced = seal.foreign_workspace_changes.clone();
    displaced.sort();
    let mut expected = vec![earlier, in_b];
    expected.sort();
    assert_eq!(displaced, expected);
    let report = store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
}

/// A later name never discharges a change that was foreign to the root
/// bound when it was recorded. Workspace A reported a change while B was
/// bound at generation 9; after the claim names generation 10 that change
/// is still open, and `done` asks for an explicit human waiver.
#[test]
fn a_later_name_keeps_a_foreign_change_under_the_earlier_root_open() {
    let (mut store, work, claim, _) = named_work();
    let foreign = source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "foreign-under-9",
        5,
        Some(basis("workspace-A", "A5", Some(9))),
        None,
    );
    name_root(&mut store, &work, &claim, 10, 6);
    assert_eq!(
        stock_completion_action(&store, &claim, &foreign),
        WorkObligationCompletionAction::WaiverOnly
    );
    let checked = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-in-B-10",
        VerificationKind::Test,
        VerificationResult::Passed,
        7,
        basis("workspace-B", "B7", Some(10)),
    );
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-B-10",
        8,
        std::slice::from_ref(&checked),
    );
    let refused = complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &checked,
        "complete-B-10",
        9,
    );
    assert!(
        matches!(
            &refused,
            Err(StoreError::WorkCompletionRefused { reason, .. })
                if reason.contains("foreign workspace workspace-A") && reason.contains("human waiver")
        ),
        "{refused:?}"
    );
    assert_eq!(
        obligation(&store, &claim, &Pick::TriggeredBy(&foreign)),
        (WorkObligationState::Open, None)
    );
}

/// A later name never makes a foreign change its own. Workspace A reports a
/// change while B is bound at generation 9; the claim then names A itself at
/// generation 10, directly or after a release and a reclaim. A check in A
/// under 10 does not satisfy that change, it is not waived as untested, and
/// `done` asks for an explicit human waiver.
#[test]
fn naming_the_foreign_workspace_later_does_not_discharge_its_change() {
    for through_release in [false, true] {
        let (mut store, work, claim, _) = named_work();
        let foreign = source_mutation_from_basis(
            &mut store,
            &work,
            &claim,
            "runner",
            "foreign-under-9",
            5,
            Some(basis("workspace-A", "A5", Some(9))),
            None,
        );
        let claim = if through_release {
            release(&mut store, &work, &claim, "release-runner", 6);
            let work = store.get_work_item(work.work_id).expect("item");
            super::claim(&mut store, &work, "runner", "reclaim-runner", 7, 300)
        } else {
            claim
        };
        name_root_in(&mut store, &work, &claim, "workspace-A", 10, 8);
        let checked = host_verification_from_basis(
            &mut store,
            &work,
            &claim,
            "runner",
            "test-in-A-10",
            VerificationKind::Test,
            VerificationResult::Passed,
            9,
            basis("workspace-A", "A9", Some(10)),
        );
        assert_eq!(
            obligation(&store, &claim, &Pick::TriggeredBy(&foreign)),
            (WorkObligationState::Open, None),
            "through release: {through_release}"
        );
        checkpoint(
            &mut store,
            &work,
            &claim,
            "runner",
            "checkpoint-A-10",
            10,
            std::slice::from_ref(&checked),
        );
        let refused = complete(
            &mut store,
            &work,
            &claim,
            "runner",
            &checked,
            "complete-A-10",
            11,
        );
        assert!(
            matches!(
                &refused,
                Err(StoreError::WorkCompletionRefused { reason, .. })
                    if reason.contains("foreign workspace workspace-A")
                        && reason.contains("human waiver")
            ),
            "through release: {through_release}: {refused:?}"
        );
        assert_eq!(
            obligation(&store, &claim, &Pick::TriggeredBy(&foreign)),
            (WorkObligationState::Open, None),
            "through release: {through_release}"
        );
    }
}

/// A change in the root's own workspace from before the binding is part of
/// the source the root now holds: a later check of the root's newest
/// sighting satisfies it, so it is neither untested nor displaced.
#[test]
fn a_change_in_the_root_before_its_binding_is_satisfied_by_a_later_check() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let mut request = root_request("project-a", "named-root-work", 1);
    request.acceptance = vec!["run a test".into()];
    request.acceptance_bindings = vec![AcceptanceBinding {
        criterion: 1,
        requirement: VerificationRequirement {
            check_kind: VerificationKind::Test,
            check_fingerprint: None,
        },
    }];
    let work = store
        .create_work(&request, &DevelopmentNoopRedactor)
        .expect("work");
    let claim = claim(&mut store, &work, "runner", "named-root-claim", 2, 300);
    let before = source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "change-in-B-before-name",
        3,
        Some(basis("workspace-B", "B3", None)),
        None,
    );
    name_root(&mut store, &work, &claim, 9, 4);
    let checked = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-in-B",
        VerificationKind::Test,
        VerificationResult::Passed,
        5,
        basis("workspace-B", "B3", Some(9)),
    );
    for pick in [Pick::Criterion, Pick::TriggeredBy(&before)] {
        assert_eq!(
            obligation(&store, &claim, &pick),
            (WorkObligationState::Satisfied, Some(checked.clone()))
        );
    }
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-B",
        6,
        std::slice::from_ref(&checked),
    );
    let seal = complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &checked,
        "complete-B",
        7,
    )
    .expect("the B check completes the work");
    assert!(seal.foreign_workspace_changes.is_empty());
    let report = store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
}

/// Appends `resolution` for the open obligation `change` opened, as if the
/// store had recorded it.
fn forge_resolution(
    store: &mut SqliteStore,
    claim: &WorkClaim,
    change: &ObjectId,
    resolution: WorkObligationResolution,
) -> String {
    let transaction = begin(store);
    let record = super::super::load_work_obligation_records_on(
        &transaction,
        claim.run_id,
        Some(WorkObligationState::Open),
    )
    .expect("open obligations")
    .into_iter()
    .find(|record| &record.obligation.triggering_observation == change)
    .expect("the change's open obligation");
    let event = WorkObligationResolutionEvent {
        schema_version: SCHEMA_VERSION,
        project_id: record.obligation.project_id.clone(),
        obligation_id: record.obligation.obligation_id,
        definition: record.definition_id.clone(),
        run_id: claim.run_id,
        resolution,
        actor: actor("runner"),
        created_at: at(20),
    };
    super::super::append_obligation_resolution_on(&transaction, &record, &event)
        .expect("append the forged resolution");
    transaction.commit().expect("commit the forgery");
    format!("work_obligation:{}", record.obligation.obligation_id.0)
}

/// The doctor re-derives every displacement and named-root satisfaction: a
/// displaced resolution recorded for a foreign change the root keeps open or
/// for a change in the root itself, and a satisfied resolution of a foreign
/// change by a check in the root, are each reported by their obligation.
#[test]
fn the_doctor_reports_forged_named_root_resolutions() {
    type Forge = fn(&ObjectId, &ObjectId, &FeedPosition) -> WorkObligationResolution;
    let cases: [(&str, &str, Forge); 3] = [
        ("workspace-A", "displaced-open-foreign", |binding, _, _| {
            WorkObligationResolution::Displaced {
                binding: binding.clone(),
                trigger_workspace_id: "workspace-A".into(),
            }
        }),
        ("workspace-B", "displaced-in-root", |binding, _, _| {
            WorkObligationResolution::Displaced {
                binding: binding.clone(),
                trigger_workspace_id: "workspace-B".into(),
            }
        }),
        ("workspace-A", "satisfied-foreign", |_, check, cut| {
            WorkObligationResolution::Satisfied {
                evidence: check.clone(),
                evaluated_cut: cut.clone(),
            }
        }),
    ];
    for (workspace, key, forge) in cases {
        let (mut store, work, claim, _) = named_work();
        let check = host_verification_from_basis(
            &mut store,
            &work,
            &claim,
            "runner",
            &format!("{key}-check"),
            VerificationKind::Test,
            VerificationResult::Passed,
            5,
            basis("workspace-B", "B6", Some(9)),
        );
        // Recorded after the check, the change keeps its obligation open.
        let change = source_mutation_from_basis(
            &mut store,
            &work,
            &claim,
            "runner",
            key,
            6,
            Some(basis(workspace, "R5", Some(9))),
            None,
        );
        let binding = super::super::named_root_context_on(
            &store.connection,
            claim.run_id,
            claim.claim_id,
            i64::MAX,
        )
        .expect("lookup")
        .expect("root B is active")
        .binding_id;
        let cut = super::super::current_run_feed_cut_on(&store.connection, claim.run_id)
            .expect("run cut");
        assert_eq!(
            obligation(&store, &claim, &Pick::TriggeredBy(&change)).0,
            WorkObligationState::Open,
            "{key}: the change's obligation must still be open to forge it"
        );
        let label = forge_resolution(&mut store, &claim, &change, forge(&binding, &check, &cut));
        let report = store.verify_all().expect("doctor");
        assert!(
            report.invalid_work_records.contains(&label),
            "{key}: {label}: {report:?}"
        );
    }
}

/// A seal's disclosed foreign changes must be exactly its displaced
/// resolutions; a seal rewritten to hide one is reported.
#[test]
fn the_doctor_reports_a_seal_that_hides_a_displaced_change() {
    let (mut store, work, claim, _) = named_work();
    let checked = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-in-B",
        VerificationKind::Test,
        VerificationResult::Passed,
        5,
        basis("workspace-B", "B5", Some(9)),
    );
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-B",
        6,
        std::slice::from_ref(&checked),
    );
    complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &checked,
        "complete-B",
        7,
    )
    .expect("the B check completes the work");
    let seal_id = store
        .get_work_run(claim.run_id)
        .expect("run")
        .completion_seal
        .expect("the run is sealed");
    let bytes: Vec<u8> = store
        .connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_id = ?1",
            [seal_id.as_str()],
            |row| row.get(0),
        )
        .expect("the seal");
    let mut seal: serde_json::Value = serde_json::from_slice(&bytes).expect("seal JSON");
    assert!(
        seal.as_object_mut()
            .expect("seal")
            .remove("foreign_workspace_changes")
            .is_some()
    );
    store
        .connection
        .execute(
            "UPDATE objects SET canonical_json = ?1 WHERE object_id = ?2",
            rusqlite::params![
                serde_json_canonicalizer::to_vec(&seal).expect("canonical seal"),
                seal_id.as_str()
            ],
        )
        .expect("rewrite the seal");
    // Its projection is rewritten the same way, so only the disclosure is off.
    store
        .connection
        .execute(
            "UPDATE work_completion_seals SET seal_json = ?1 WHERE seal_id = ?2",
            rusqlite::params![
                serde_json::to_vec(&seal).expect("seal projection"),
                seal_id.as_str()
            ],
        )
        .expect("rewrite the seal projection");
    let report = store.verify_all().expect("doctor");
    let label = format!("completion_seal:{seal_id}:obligation_basis");
    assert!(
        report.invalid_work_records.contains(&label),
        "{label}: {report:?}"
    );
}

/// The host ends the claim's root `workspace` at `generation`, named at
/// `named_second`, as a root that is no longer valid.
fn end_root(
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    workspace: &str,
    generation: i64,
    named_second: i64,
    second: i64,
) {
    let host = bind_control_for(
        store,
        &claim.holder.0,
        &format!("ending-host-{generation}"),
        &[crate::domain::EffectClass::Observe],
        at(second),
    );
    store
        .bind_named_root(
            &work.project_id,
            &host.status.session_id,
            &host.connection_token,
            &host.routing_token,
            claim.claim_id,
            claim.fence,
            workspace,
            generation,
            at(named_second),
            NamedRootBindingKind::Ended,
            Some(crate::domain::NamedRootEndReason::RootInvalid),
            &mut actor(&claim.holder.0),
            &format!("end-{generation}"),
            at(second),
        )
        .expect("host ends the root");
}

/// How the claim loses its root between a foreign change and a check.
#[derive(Clone, Copy, Debug)]
enum Unbind {
    Release,
    End,
}

/// Losing the root does not discharge a change foreign to it either.
/// Workspace A reports a change while B is bound at generation 9; the claim
/// then loses B, by a release and a reclaim or by the host ending B, and a
/// check in A runs while the claim is unbound. That check does not satisfy
/// the change, whether the claim stays unbound or later names A itself and
/// checks it there, and `done` asks for an explicit human waiver.
#[test]
fn an_unbound_interval_does_not_discharge_a_foreign_change() {
    for unbind in [Unbind::Release, Unbind::End] {
        for rename in [false, true] {
            let context = format!("{unbind:?}, later name: {rename}");
            let (mut store, work, claim, _) = named_work();
            let foreign = source_mutation_from_basis(
                &mut store,
                &work,
                &claim,
                "runner",
                "foreign-under-9",
                5,
                Some(basis("workspace-A", "A5", Some(9))),
                None,
            );
            let claim = match unbind {
                Unbind::Release => {
                    release(&mut store, &work, &claim, "release-runner", 6);
                    let work = store.get_work_item(work.work_id).expect("item");
                    super::claim(&mut store, &work, "runner", "reclaim-runner", 7, 300)
                }
                Unbind::End => {
                    end_root(&mut store, &work, &claim, "workspace-B", 9, 4, 6);
                    claim
                }
            };
            assert_eq!(active_generation(&store, &claim), None, "{context}");
            let unbound_check = host_verification_from_basis(
                &mut store,
                &work,
                &claim,
                "runner",
                "test-in-A-unbound",
                VerificationKind::Test,
                VerificationResult::Passed,
                8,
                basis("workspace-A", "A5", None),
            );
            assert_eq!(
                obligation(&store, &claim, &Pick::TriggeredBy(&foreign)),
                (WorkObligationState::Open, None),
                "{context}"
            );
            let checked = if rename {
                name_root_in(&mut store, &work, &claim, "workspace-A", 10, 9);
                host_verification_from_basis(
                    &mut store,
                    &work,
                    &claim,
                    "runner",
                    "test-in-A-10",
                    VerificationKind::Test,
                    VerificationResult::Passed,
                    10,
                    basis("workspace-A", "A5", Some(10)),
                )
            } else {
                unbound_check
            };
            assert_eq!(
                obligation(&store, &claim, &Pick::TriggeredBy(&foreign)),
                (WorkObligationState::Open, None),
                "{context}"
            );
            checkpoint(
                &mut store,
                &work,
                &claim,
                "runner",
                "checkpoint-A",
                11,
                std::slice::from_ref(&checked),
            );
            let refused = complete(
                &mut store,
                &work,
                &claim,
                "runner",
                &checked,
                "complete-A",
                12,
            );
            assert!(
                matches!(
                    &refused,
                    Err(StoreError::WorkCompletionRefused { reason, .. })
                        if reason.contains("foreign workspace workspace-A")
                            && reason.contains("human waiver")
                ),
                "{context}: {refused:?}"
            );
            assert_eq!(
                obligation(&store, &claim, &Pick::TriggeredBy(&foreign)),
                (WorkObligationState::Open, None),
                "{context}"
            );
            let report = store.verify_all().expect("doctor");
            assert!(report.is_healthy(), "{context}: {report:?}");
        }
    }
}

/// The doctor holds a satisfaction recorded while the claim was unbound to
/// the same history: neither a change foreign to the root bound when it was
/// recorded nor a change whose root cannot be established, recorded while
/// that root was bound, can be satisfied by a check after that root ended,
/// even one that accounts for a later change by the rules without a root.
#[test]
fn the_doctor_reports_a_bound_change_satisfied_while_unbound() {
    for (key, change_basis) in [
        ("foreign-under-9", Some(basis("workspace-A", "A5", Some(9)))),
        ("unknown-under-9", None),
    ] {
        let (mut store, work, claim, _) = named_work();
        let change = source_mutation_from_basis(
            &mut store,
            &work,
            &claim,
            "runner",
            key,
            5,
            change_basis,
            None,
        );
        end_root(&mut store, &work, &claim, "workspace-B", 9, 4, 6);
        source_mutation_from_basis(
            &mut store,
            &work,
            &claim,
            "runner",
            "unbound-change",
            7,
            Some(basis("workspace-B", "B7", None)),
            None,
        );
        let check = host_verification_from_basis(
            &mut store,
            &work,
            &claim,
            "runner",
            "test-unbound",
            VerificationKind::Test,
            VerificationResult::Passed,
            8,
            basis("workspace-B", "B7", None),
        );
        assert_eq!(
            obligation(&store, &claim, &Pick::TriggeredBy(&change)).0,
            WorkObligationState::Open,
            "{key}"
        );
        let cut = super::super::current_run_feed_cut_on(&store.connection, claim.run_id)
            .expect("run cut");
        let label = forge_resolution(
            &mut store,
            &claim,
            &change,
            WorkObligationResolution::Satisfied {
                evidence: check,
                evaluated_cut: cut,
            },
        );
        let report = store.verify_all().expect("doctor");
        assert!(
            report.invalid_work_records.contains(&label),
            "{key}: {label}: {report:?}"
        );
    }
}

/// A change whose root cannot be established, recorded while B was bound at
/// generation 9, keeps needing a fresh check in a named root after B ends, is
/// released, or is renamed. A check while the claim is unbound, even one that
/// accounts for a later change, does not satisfy it; `done` neither waives it
/// as untested nor completes; and a fresh check in a later named root
/// accounts for it.
#[test]
fn an_unknown_root_change_outlives_its_binding() {
    for unbind in [Some(Unbind::Release), Some(Unbind::End), None] {
        let context = format!("{unbind:?}");
        let (mut store, work, claim, _) = named_work();
        let before = host_verification_from_basis(
            &mut store,
            &work,
            &claim,
            "runner",
            "test-before-unknown",
            VerificationKind::Test,
            VerificationResult::Passed,
            5,
            basis("workspace-B", "B5", Some(9)),
        );
        let unknown = source_mutation_from_basis(
            &mut store,
            &work,
            &claim,
            "runner",
            "unknown-under-9",
            6,
            None,
            None,
        );
        let (claim, stale) = if let Some(unbind) = unbind {
            let claim = match unbind {
                Unbind::Release => {
                    release(&mut store, &work, &claim, "release-runner", 7);
                    let work = store.get_work_item(work.work_id).expect("item");
                    super::claim(&mut store, &work, "runner", "reclaim-runner", 8, 300)
                }
                Unbind::End => {
                    end_root(&mut store, &work, &claim, "workspace-B", 9, 4, 7);
                    claim
                }
            };
            assert_eq!(active_generation(&store, &claim), None, "{context}");
            // Unbound, the host reports a change it can place, and a
            // check that accounts for it by the rules without a root.
            source_mutation_from_basis(
                &mut store,
                &work,
                &claim,
                "runner",
                "unbound-change",
                9,
                Some(basis("workspace-B", "B9", None)),
                None,
            );
            let unbound_check = host_verification_from_basis(
                &mut store,
                &work,
                &claim,
                "runner",
                "test-unbound",
                VerificationKind::Test,
                VerificationResult::Passed,
                10,
                basis("workspace-B", "B9", None),
            );
            (claim, unbound_check)
        } else {
            name_root(&mut store, &work, &claim, 10, 7);
            (claim, before)
        };
        assert_eq!(
            obligation(&store, &claim, &Pick::TriggeredBy(&unknown)),
            (WorkObligationState::Open, None),
            "{context}"
        );
        assert_eq!(
            stock_completion_action(&store, &claim, &unknown),
            if unbind.is_some() {
                WorkObligationCompletionAction::NameRootCheckOrWaiver
            } else {
                WorkObligationCompletionAction::CheckOrWaiver
            },
            "{context}"
        );
        checkpoint(
            &mut store,
            &work,
            &claim,
            "runner",
            "checkpoint-stale",
            11,
            std::slice::from_ref(&stale),
        );
        let refused = complete(
            &mut store,
            &work,
            &claim,
            "runner",
            &stale,
            "complete-stale",
            12,
        );
        let remedy = if unbind.is_some() {
            "name a root and run a fresh check in it"
        } else {
            "run a fresh check in the named root"
        };
        assert!(
            matches!(
                &refused,
                Err(StoreError::WorkCompletionRefused { reason, .. })
                    if reason.contains("unknown workspace") && reason.contains(remedy)
            ),
            "{context}: {refused:?}"
        );
        assert_eq!(
            obligation(&store, &claim, &Pick::TriggeredBy(&unknown)),
            (WorkObligationState::Open, None),
            "{context}"
        );
        if unbind.is_some() {
            name_root(&mut store, &work, &claim, 10, 13);
        }
        let fresh = host_verification_from_basis(
            &mut store,
            &work,
            &claim,
            "runner",
            "test-in-B-10",
            VerificationKind::Test,
            VerificationResult::Passed,
            14,
            basis("workspace-B", "B14", Some(10)),
        );
        assert_eq!(
            obligation(&store, &claim, &Pick::TriggeredBy(&unknown)),
            (WorkObligationState::Satisfied, Some(fresh.clone())),
            "{context}"
        );
        checkpoint(
            &mut store,
            &work,
            &claim,
            "runner",
            "checkpoint-fresh",
            15,
            &[stale, fresh.clone()],
        );
        complete(
            &mut store,
            &work,
            &claim,
            "runner",
            &fresh,
            "complete-fresh",
            16,
        )
        .unwrap_or_else(|error| panic!("{context}: {error:?}"));
        let report = store.verify_all().expect("doctor");
        assert!(report.is_healthy(), "{context}: {report:?}");
    }
}

/// An operator rule: every source change owes a passing test with the pinned
/// check of `suite`.
fn pinned_rule(suite: &str) -> crate::domain::ObligationRuleDefinition {
    crate::domain::ObligationRuleDefinition {
        rule: crate::domain::BuiltinObligationRuleRef {
            rule_id: "operator-pinned-suite".into(),
            rule_version: 1,
        },
        trigger: crate::domain::BuiltinObligationTrigger::SourceChanged,
        requirement: VerificationRequirement {
            check_kind: VerificationKind::Test,
            check_fingerprint: Some(check_fingerprint(suite)),
        },
    }
}

/// Selects `rules` as `store`'s active rule set.
fn select_rules(store: &mut SqliteStore, rules: Vec<crate::domain::ObligationRuleDefinition>) {
    store
        .set_obligation_rule_set(
            &crate::domain::ObligationRuleSet {
                schema_version: crate::domain::OBLIGATION_RULE_SET_SCHEMA_VERSION,
                rules,
            },
            &actor("obligation-rule-admin"),
            "select-rules",
            None,
            at(1),
            &DevelopmentNoopRedactor,
        )
        .expect("select the rule set");
}

/// A store whose active rule set is the operator rule pinning `suite`
/// alone, without the stock rule.
fn pinned_store(suite: &str) -> SqliteStore {
    let mut store = SqliteStore::open_in_memory().expect("store");
    select_rules(&mut store, vec![pinned_rule(suite)]);
    store
}

mod assessment;
mod operator_guidance;
mod stock_reminder;

/// When the stock rule and an operator rule both open an obligation for one
/// change, both are displaced, and the seal and the obligation page count
/// the change once.
#[test]
fn a_change_two_rules_displace_is_disclosed_once() {
    let directory = crate::test_support::temp_home().expect("directory");
    let database = directory.path().join("named-root.db");
    let mut store = SqliteStore::open(&database).expect("store");
    let mut rules = crate::control::builtin_obligation_rule_set().rules;
    rules.push(pinned_rule("pinned-suite"));
    select_rules(&mut store, rules);
    let (mut store, work, claim, earlier) = named_work_in(store);
    let checked = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-in-B",
        VerificationKind::Test,
        VerificationResult::Passed,
        5,
        basis("workspace-B", "B5", Some(9)),
    );
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-B",
        6,
        std::slice::from_ref(&checked),
    );
    let seal = complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &checked,
        "complete-B",
        7,
    )
    .expect("the B check completes the work");
    assert_eq!(seal.foreign_workspace_changes, vec![earlier.clone()]);
    let displaced = store
        .work_run_obligations(claim.run_id)
        .expect("obligations")
        .into_iter()
        .filter(|record| record.obligation.triggering_observation == earlier)
        .map(|record| record.state)
        .collect::<Vec<_>>();
    assert_eq!(displaced, vec![WorkObligationState::Displaced; 2]);
    let report = store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
    drop(store);
    let service = LocalWorkService::new(
        database,
        work.project_id.clone(),
        "reader".into(),
        SessionId("reader".into()),
        None,
    );
    let focus = service
        .work_focus(&work.short_ref, at(8))
        .expect("focus the completed item");
    assert_eq!(focus.obligation_page.displaced_total, 1);
}

/// The one obligation the operator rule opened for `change`.
fn operator_obligation(
    store: &SqliteStore,
    claim: &WorkClaim,
    change: &ObjectId,
) -> crate::storage::WorkObligationRecord {
    let records = store
        .work_run_obligations(claim.run_id)
        .expect("obligations")
        .into_iter()
        .filter(|record| &record.obligation.triggering_observation == change)
        .collect::<Vec<_>>();
    assert_eq!(records.len(), 1, "one obligation per change");
    assert!(!crate::control::is_stock_source_change_obligation(
        &records[0].obligation.rule,
        &records[0].obligation.requirement
    ));
    records.into_iter().next().expect("the operator obligation")
}

/// Whether `done` refused on the open obligation `record`.
fn refused_on(
    refused: &Result<CompletionSeal, StoreError>,
    record: &crate::storage::WorkObligationRecord,
) -> bool {
    matches!(
        refused,
        Err(StoreError::OpenWorkObligations { obligations, .. })
            if obligations
                .iter()
                .any(|open| open.obligation_id == record.obligation.obligation_id)
    )
}

/// An operator rule that pins its check is held to the same principle as the
/// stock rule. A change from workspace A before the claim named any root is
/// displaced and disclosed, and the doctor accepts that displacement. A
/// change recorded after its generation was released is unbound: a matching
/// check in the current root after it satisfies it, but without one it stays
/// open for an operator waiver instead of being waived as untested.
#[test]
fn an_operator_rule_is_displaced_and_satisfied_like_the_stock_rule() {
    let (mut store, work, claim, earlier) = named_work_in(pinned_store("pinned-suite"));
    release(&mut store, &work, &claim, "release-runner", 5);
    let work = store.get_work_item(work.work_id).expect("item");
    let claim = super::claim(&mut store, &work, "runner", "reclaim-runner", 6, 300);
    name_root(&mut store, &work, &claim, 10, 7);
    let stale = source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "stale-generation-in-D",
        8,
        Some(basis("workspace-D", "D8", Some(9))),
        None,
    );
    let unpinned = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "unpinned-check",
        VerificationKind::Test,
        VerificationResult::Passed,
        9,
        basis("workspace-B", "B9", Some(10)),
    );
    assert_eq!(
        obligation(&store, &claim, &Pick::Criterion),
        (WorkObligationState::Satisfied, Some(unpinned.clone()))
    );
    let open_stale = operator_obligation(&store, &claim, &stale);
    assert_eq!(open_stale.state, WorkObligationState::Open);
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-unpinned",
        10,
        std::slice::from_ref(&unpinned),
    );
    let refused = complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &unpinned,
        "complete-unpinned",
        11,
    );
    assert!(refused_on(&refused, &open_stale), "{refused:?}");
    assert_eq!(
        operator_obligation(&store, &claim, &stale).state,
        WorkObligationState::Open,
        "an operator obligation is never waived as untested"
    );
    let pinned = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "pinned-suite",
        VerificationKind::Test,
        VerificationResult::Passed,
        12,
        basis("workspace-B", "B9", Some(10)),
    );
    let satisfied_stale = operator_obligation(&store, &claim, &stale);
    assert_eq!(satisfied_stale.state, WorkObligationState::Satisfied);
    assert!(matches!(
        satisfied_stale.resolution.map(|event| event.resolution),
        Some(WorkObligationResolution::Satisfied { evidence, .. }) if evidence == pinned
    ));
    assert_eq!(
        operator_obligation(&store, &claim, &earlier).state,
        WorkObligationState::Open,
        "no check in the root is credited for a change in workspace A"
    );
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-pinned",
        13,
        &[unpinned, pinned.clone()],
    );
    let seal = complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &pinned,
        "complete-pinned",
        14,
    )
    .expect("the pinned check completes the work");
    assert_eq!(seal.foreign_workspace_changes, vec![earlier.clone()]);
    assert!(matches!(
        operator_obligation(&store, &claim, &earlier)
            .resolution
            .map(|event| event.resolution),
        Some(WorkObligationResolution::Displaced { .. })
    ));
    let report = store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
}

/// A change workspace A reports while B is bound stays foreign under an
/// operator rule too: the pinned check in B does not satisfy it, and `done`
/// neither displaces nor waives it.
#[test]
fn an_operator_rule_keeps_a_foreign_change_under_a_bound_name_open() {
    let (mut store, work, claim, _) = named_work_in(pinned_store("pinned-suite"));
    let foreign = source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "foreign-under-9",
        5,
        Some(basis("workspace-A", "A5", Some(9))),
        None,
    );
    let pinned = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "pinned-suite",
        VerificationKind::Test,
        VerificationResult::Passed,
        6,
        basis("workspace-B", "B6", Some(9)),
    );
    let open = operator_obligation(&store, &claim, &foreign);
    assert_eq!(open.state, WorkObligationState::Open);
    assert_eq!(
        store
            .work_obligation_completion_actions(&[&open.obligation])
            .expect("operator guidance")[0],
        WorkObligationCompletionAction::WaiverOnly
    );
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-pinned",
        7,
        std::slice::from_ref(&pinned),
    );
    let refused = complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &pinned,
        "complete-pinned",
        8,
    );
    assert!(refused_on(&refused, &open), "{refused:?}");
    assert_eq!(
        operator_obligation(&store, &claim, &foreign).state,
        WorkObligationState::Open
    );
    let report = store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
}

/// A check accounts only for a change it ran after. A host that states a
/// released generation for a change in workspace D records an unbound
/// change; a check in the current root that ran before that change, though
/// its verification was recorded after it, does not satisfy it, and a later
/// check does.
#[test]
fn a_check_run_before_an_unbound_change_cannot_account_for_it() {
    let (mut store, work, claim, _) = named_work();
    release(&mut store, &work, &claim, "release-runner", 5);
    let work = store.get_work_item(work.work_id).expect("item");
    let claim = super::claim(&mut store, &work, "runner", "reclaim-runner", 6, 300);
    name_root(&mut store, &work, &claim, 10, 7);
    let early_basis = || basis("workspace-B", "B8", Some(10));
    let transaction = begin(&mut store);
    let producer = host_check_producer(
        &transaction,
        &work,
        &claim,
        "runner",
        "early-check",
        8,
        early_basis(),
    );
    transaction.commit().expect("commit the producer");
    let stale = source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "stale-generation-in-D",
        9,
        Some(basis("workspace-D", "D9", Some(9))),
        None,
    );
    let transaction = begin(&mut store);
    host_verification_of_producer(
        &transaction,
        &work,
        &claim,
        "runner",
        "early-check",
        8,
        10,
        early_basis(),
        producer,
    );
    transaction.commit().expect("commit the late verification");
    assert_eq!(
        obligation(&store, &claim, &Pick::TriggeredBy(&stale)),
        (WorkObligationState::Open, None)
    );
    let late = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "late-check",
        VerificationKind::Test,
        VerificationResult::Passed,
        11,
        basis("workspace-B", "B8", Some(10)),
    );
    assert_eq!(
        obligation(&store, &claim, &Pick::TriggeredBy(&stale)),
        (WorkObligationState::Satisfied, Some(late))
    );
}

/// Under an active named root, `done` still waives the stock rule's untested
/// changes that the root gives no other disposition: a change in the root's
/// own workspace and an unbound change, each with no later check, are
/// recorded as untested, and work with no bound criterion completes.
#[test]
fn done_waives_untested_changes_the_root_gives_no_other_disposition() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let mut request = root_request("project-a", "named-root-work", 1);
    request.acceptance = vec!["describe the change".into()];
    let work = store
        .create_work(&request, &DevelopmentNoopRedactor)
        .expect("work");
    let claim = claim(&mut store, &work, "runner", "named-root-claim", 2, 300);
    name_root(&mut store, &work, &claim, 9, 3);
    release(&mut store, &work, &claim, "release-runner", 4);
    let work = store.get_work_item(work.work_id).expect("item");
    let claim = super::claim(&mut store, &work, "runner", "reclaim-runner", 5, 300);
    name_root(&mut store, &work, &claim, 10, 6);
    let checked = host_verification_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "test-in-B-10",
        VerificationKind::Test,
        VerificationResult::Passed,
        7,
        basis("workspace-B", "B7", Some(10)),
    );
    let in_root = source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "change-in-B",
        8,
        Some(basis("workspace-B", "B8", Some(10))),
        None,
    );
    let unbound = source_mutation_from_basis(
        &mut store,
        &work,
        &claim,
        "runner",
        "stale-generation-in-D",
        9,
        Some(basis("workspace-D", "D9", Some(9))),
        None,
    );
    for change in [&in_root, &unbound] {
        assert_eq!(
            obligation(&store, &claim, &Pick::TriggeredBy(change)),
            (WorkObligationState::Open, None)
        );
        assert_eq!(
            stock_completion_action(&store, &claim, change),
            WorkObligationCompletionAction::DoneWaives
        );
    }
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "checkpoint-B-10",
        10,
        std::slice::from_ref(&checked),
    );
    let seal = complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &checked,
        "complete-B-10",
        11,
    )
    .expect("the untested changes are waived and the work completes");
    assert!(seal.foreign_workspace_changes.is_empty());
    let records = store
        .work_run_obligations(claim.run_id)
        .expect("terminal obligations");
    for change in [&in_root, &unbound] {
        let resolution = records
            .iter()
            .find(|record| &record.obligation.triggering_observation == change)
            .and_then(|record| record.resolution.as_ref())
            .map(|event| event.resolution.clone());
        assert!(
            matches!(resolution, Some(WorkObligationResolution::Waived { .. })),
            "{change}: {resolution:?}"
        );
    }
    let report = store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
}
