//! Binding one native passed check to several held items: the record each
//! target gets, how it satisfies that target's obligations, and every
//! refusal writing nothing.

use rusqlite::TransactionBehavior;

use super::super::completion::{
    append_control_execution_observation_on, append_control_verification_evidence_on,
};
use super::super::query::load_work_run;
use super::super::test_support::*;
use super::{VerificationBindOutcome, bind_verification_on};
use crate::domain::SCHEMA_VERSION;
use crate::domain::{
    AcceptanceBinding, BindMeasurement, ControlWorkBinding, EffectClass, ExecutionObservation,
    ExecutionOutcome, ExecutionSourceBasis, NamedRootBindingKind, SessionId, SourceRootState,
    VerificationBindInput, VerificationBindOriginalRefusal, VerificationBindReceipt,
    VerificationBindRefusal, VerificationBindRequestRefusal, VerificationBindSighting,
    VerificationBindTarget, VerificationBindTargetReason as Reason, VerificationEvidence,
    VerificationKind, VerificationRequirement, VerificationResult, WorkClaim, WorkItem,
    WorkObligationState,
};
use crate::storage::test_support::bind_control_for;
use crate::storage::{SqliteStore, StoreError};
use crate::{DevelopmentNoopRedactor, ObjectId};

const ROOT: &str = "workspace-root";
const HOLDER: &str = "runner";

fn basis(revision: &str, generation: i64) -> ExecutionSourceBasis {
    ExecutionSourceBasis {
        workspace_id: ROOT.into(),
        source_revision: revision.into(),
        source_root_generation: Some(generation),
        source_root_state: Some(SourceRootState::Named),
    }
}

/// One held item of the changeset: its claim, its root's generation and its
/// newest change under that root.
struct Held {
    work: WorkItem,
    claim: WorkClaim,
    generation: i64,
    change: ObjectId,
    /// The named-root binding event, named at the held item's second.
    root_event: ObjectId,
    named_second: i64,
}

/// An item bound to a test criterion, claimed by `holder`, its root named at
/// `generation`, and a change under the root at `revision`.
fn held(
    store: &mut SqliteStore,
    key: &str,
    holder: &str,
    generation: i64,
    revision: &str,
    second: i64,
) -> Held {
    held_with(
        store,
        key,
        holder,
        generation,
        revision,
        second,
        VerificationRequirement {
            check_kind: VerificationKind::Test,
            check_fingerprint: None,
        },
    )
}

fn held_with(
    store: &mut SqliteStore,
    key: &str,
    holder: &str,
    generation: i64,
    revision: &str,
    second: i64,
    requirement: VerificationRequirement,
) -> Held {
    let mut request = root_request("project-a", key, second);
    request.acceptance = vec!["the tests pass".into(), "the change is reviewed".into()];
    request.acceptance_bindings = vec![AcceptanceBinding {
        criterion: 1,
        requirement,
    }];
    let work = store
        .create_work(&request, &DevelopmentNoopRedactor)
        .expect("work");
    let claim = claim(
        store,
        &work,
        holder,
        &format!("{key}-claim"),
        second,
        36_000,
    );
    let root = name_root(store, &work, &claim, generation, second);
    let change = change(
        store,
        &work,
        &claim,
        &format!("{key}-edit"),
        revision,
        generation,
        second + 1,
    );
    let work = store.get_work_item(work.work_id).expect("item");
    Held {
        work,
        claim,
        generation,
        change,
        root_event: root.event,
        named_second: second,
    }
}

fn name_root(
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    generation: i64,
    second: i64,
) -> crate::domain::NamedRootBindingReceipt {
    let host = bind_control_for(
        store,
        &claim.holder.0,
        &format!("root-host-{generation}"),
        &[EffectClass::Observe],
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
            ROOT,
            generation,
            at(second),
            NamedRootBindingKind::Bound,
            None,
            &mut actor(&claim.holder.0),
            &format!("name-{generation}"),
            at(second),
        )
        .expect("host names the root")
}

/// A change under the claim's root at `revision`.
fn change(
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    key: &str,
    revision: &str,
    generation: i64,
    second: i64,
) -> ObjectId {
    source_mutation_from_basis(
        store,
        work,
        claim,
        &claim.holder.0,
        key,
        second,
        Some(basis(revision, generation)),
        None,
    )
}

fn binding_of(store: &SqliteStore, held: &Held) -> ControlWorkBinding {
    let work = store.get_work_item(held.work.work_id).expect("item");
    ControlWorkBinding {
        root_execution_id: load_work_run(&store.connection, held.claim.run_id)
            .expect("run")
            .root_execution_id,
        work_id: work.work_id,
        run_id: held.claim.run_id,
        work_revision: work.revision,
        claim_id: held.claim.claim_id,
        claim_fence: held.claim.fence,
    }
}

/// A native passed test of `held`'s root at `revision`, as the host records
/// it: a producer observation and its verification.
fn native_check(store: &mut SqliteStore, held: &Held, revision: &str, second: i64) -> ObjectId {
    native_check_in(store, held, revision, second, false)
}

/// As [`native_check`], with `environment` the check also links an
/// environment record on its run.
fn native_check_in(
    store: &mut SqliteStore,
    held: &Held,
    revision: &str,
    second: i64,
    environment: bool,
) -> ObjectId {
    check_with(store, held, revision, second, environment, false)
}

/// As [`native_check_in`]; with `unknown_change_between`, a change of
/// unknown place is recorded on the run after the producer and before its
/// verification, which a host may record later than its producer.
fn check_with(
    store: &mut SqliteStore,
    held: &Held,
    revision: &str,
    second: i64,
    environment: bool,
    unknown_change_between: bool,
) -> ObjectId {
    let binding = binding_of(store, held);
    let mut transaction = store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("checkpoint");
    let mut run_actor = actor(HOLDER);
    run_actor.run_id = Some(held.claim.run_id.0.to_string());
    let environment = environment
        .then(|| {
            super::super::completion::append_control_environment_evidence_on(
                &transaction,
                &crate::domain::EnvironmentEvidence {
                    schema_version: SCHEMA_VERSION,
                    project_id: held.work.project_id.clone(),
                    binding: binding.clone(),
                    session_id: SessionId(HOLDER.into()),
                    source_basis: basis(revision, held.generation),
                    environment_fingerprint: check_fingerprint("toolchain"),
                    components: None,
                    observed_at: at(second),
                    actor: run_actor.clone(),
                    recorded_at: at(second),
                },
            )
        })
        .transpose()
        .expect("environment");
    let producer = append_control_execution_observation_on(
        &transaction,
        &ExecutionObservation {
            schema_version: SCHEMA_VERSION,
            project_id: held.work.project_id.clone(),
            binding: binding.clone(),
            session_id: SessionId(HOLDER.into()),
            grant_id: format!("grant-{second}"),
            observation_id: format!("check-{second}"),
            action_fingerprint: check_fingerprint("suite"),
            effect: EffectClass::Observe,
            outcome: ExecutionOutcome::Succeeded,
            source_changed: false,
            reported_source_change: None,
            obligation_rule_set: active_rule_set_id(&transaction),
            source_basis: Some(basis(revision, held.generation)),
            observed_at: Some(at(second)),
            actor: run_actor.clone(),
            recorded_at: at(second),
        },
    )
    .expect("producer");
    if unknown_change_between {
        transaction.commit().expect("commit producer");
        source_mutation_from_basis(
            store,
            &held.work,
            &held.claim,
            HOLDER,
            &format!("unlocated-{second}"),
            second,
            None,
            None,
        );
        transaction = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("verification");
    }
    let verification = append_control_verification_evidence_on(
        &transaction,
        &VerificationEvidence {
            schema_version: SCHEMA_VERSION,
            project_id: held.work.project_id.clone(),
            binding,
            session_id: SessionId(HOLDER.into()),
            producer_observation: producer,
            source_basis: basis(revision, held.generation),
            environment,
            check_kind: VerificationKind::Test,
            check_fingerprint: check_fingerprint("suite"),
            result: VerificationResult::Passed,
            completed_at: at(second),
            summary: "host observed the suite".into(),
            refs: vec!["command:suite".into()],
            actor: run_actor,
            recorded_at: at(second),
            bound_from: None,
        },
    )
    .expect("verification");
    transaction.commit().expect("commit check");
    verification
}

fn target(
    store: &SqliteStore,
    held: &Held,
    revision: &str,
    criteria: Vec<u32>,
) -> VerificationBindTarget {
    VerificationBindTarget {
        binding: binding_of(store, held),
        sighting: VerificationBindSighting {
            observation: held.change.clone(),
            source_revision: revision.into(),
        },
        criteria,
    }
}

fn request(
    original: &ObjectId,
    revision: &str,
    targets: Vec<VerificationBindTarget>,
) -> VerificationBindInput {
    VerificationBindInput {
        idempotency_key: "bind-1".into(),
        original: original.clone(),
        measurement: BindMeasurement {
            workspace_id: ROOT.into(),
            source_revision: revision.into(),
            measured_at: at(90),
        },
        targets,
    }
}

/// Runs the bind in one transaction, committing it only when it bound.
fn bind(
    store: &mut SqliteStore,
    input: &VerificationBindInput,
    second: i64,
) -> VerificationBindOutcome {
    let project = crate::domain::ProjectId("project-a".into());
    let transaction = store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("bind transaction");
    let outcome = bind_verification_on(
        &transaction,
        &project,
        &SessionId(HOLDER.into()),
        &actor(HOLDER),
        input,
        at(second),
    )
    .expect("bind");
    if matches!(outcome, VerificationBindOutcome::Bound(_)) {
        transaction.commit().expect("commit bind");
    }
    outcome
}

fn refused(outcome: VerificationBindOutcome) -> VerificationBindRefusal {
    match outcome {
        VerificationBindOutcome::Refused(refusal) => refusal,
        VerificationBindOutcome::Bound(receipt) => panic!("bound unexpectedly: {receipt:?}"),
    }
}

fn evidence_rows(store: &SqliteStore) -> i64 {
    store
        .connection
        .query_row("SELECT COUNT(*) FROM work_run_evidence", [], |row| {
            row.get(0)
        })
        .expect("count")
}

fn stored(store: &SqliteStore, id: &ObjectId) -> VerificationEvidence {
    super::super::feeds::load_typed_work_object(&store.connection, id, "verification_evidence")
        .expect("verification record")
}

/// The open-or-satisfied state of every obligation on `held`'s run, with
/// the evidence that satisfied each.
fn obligations(
    store: &SqliteStore,
    held: &Held,
) -> Vec<(String, WorkObligationState, Option<String>)> {
    store
        .connection
        .prepare(
            "SELECT rule_id, state, evidence_id FROM work_run_obligations
             WHERE run_id = ?1 ORDER BY trigger_position, obligation_id",
        )
        .expect("prepare")
        .query_map([held.claim.run_id.0.to_string()], |row| {
            let state: String = row.get(1)?;
            Ok((row.get(0)?, state, row.get(2)?))
        })
        .expect("query")
        .map(|row| {
            let (rule, state, evidence): (String, String, Option<String>) = row.expect("row");
            let state = match state.as_str() {
                "open" => WorkObligationState::Open,
                "satisfied" => WorkObligationState::Satisfied,
                _ => WorkObligationState::Waived,
            };
            (rule, state, evidence)
        })
        .collect()
}

/// A check run on item A, bound to items B and C on the same root under
/// their own generations: each gets one record whose producer is A's
/// execution, with the original's check facts and its own basis, satisfying
/// its source change and its bound criterion; the store stays consistent.
#[test]
fn a_check_binds_to_two_items_on_one_root_with_distinct_generations() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let a = held(&mut store, "item-a", HOLDER, 9, "R1", 2);
    let b = held(&mut store, "item-b", HOLDER, 10, "R1", 4);
    let c = held(&mut store, "item-c", HOLDER, 11, "R1", 6);
    let original = native_check(&mut store, &a, "R1", 20);
    let original_record = stored(&store, &original);
    let input = request(
        &original,
        "R1",
        vec![
            target(&store, &b, "R1", vec![1]),
            target(&store, &c, "R1", vec![]),
        ],
    );
    let VerificationBindOutcome::Bound(receipt) = bind(&mut store, &input, 30) else {
        panic!("the bind is refused")
    };
    assert_eq!(receipt.original, original);
    assert!(!receipt.replayed);
    assert_eq!(receipt.bound.len(), 2);
    for (held, bound) in [(&b, &receipt.bound[0]), (&c, &receipt.bound[1])] {
        assert_eq!(bound.work_id, held.work.work_id);
        let record = stored(&store, &bound.verification);
        // The check's own facts are the original's, copied unchanged.
        assert_eq!(record.check_kind, original_record.check_kind);
        assert_eq!(record.check_fingerprint, original_record.check_fingerprint);
        assert_eq!(record.result, original_record.result);
        assert_eq!(record.completed_at, original_record.completed_at);
        assert_eq!(
            record.producer_observation,
            original_record.producer_observation
        );
        assert_eq!(record.environment, original_record.environment);
        // Its basis is the original's content under this item's own name.
        assert_eq!(record.source_basis, basis("R1", held.generation));
        assert_eq!(record.binding.run_id, held.claim.run_id);
        assert_eq!(record.recorded_at, at(30));
        let source = record.bound_from.expect("a bound record");
        assert_eq!(source.verification, original);
        assert_eq!(source.work_id, a.work.work_id);
        assert_eq!(source.run_id, a.claim.run_id);
        assert_eq!(source.sighting, held.change);
        // It satisfies this item's source change and its bound criterion.
        let states = obligations(&store, held);
        assert!(
            states.iter().all(
                |(_, state, evidence)| *state == WorkObligationState::Satisfied
                    && evidence.as_deref() == Some(bound.verification.as_str())
            ),
            "{states:?}"
        );
        assert_eq!(bound.obligations_satisfied.len(), states.len());
    }
    assert_eq!(receipt.bound[0].criteria.len(), 1);
    assert!(receipt.bound[0].criteria[0].eligible);
    // The original is untouched and the store verifies.
    assert_eq!(stored(&store, &original), original_record);
    let report = store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
}

/// A batch with one valid and one invalid target is refused whole: the
/// refusal names the invalid one and nothing is written for either.
#[test]
fn a_mixed_batch_writes_nothing_for_any_target() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let a = held(&mut store, "item-a", HOLDER, 9, "R1", 2);
    let b = held(&mut store, "item-b", HOLDER, 10, "R1", 4);
    let c = held(&mut store, "item-c", HOLDER, 11, "R2", 6);
    let original = native_check(&mut store, &a, "R1", 20);
    let before = evidence_rows(&store);
    let input = request(
        &original,
        "R1",
        vec![
            target(&store, &b, "R1", vec![]),
            target(&store, &c, "R2", vec![]),
        ],
    );
    let refusal = refused(bind(&mut store, &input, 30));
    assert_eq!(refusal.original, None);
    assert_eq!(refusal.request, None);
    assert_eq!(refusal.targets.len(), 1);
    assert_eq!(refusal.targets[0].work_id, c.work.work_id);
    assert_eq!(refusal.targets[0].reason, Reason::SightingRevisionDiffers);
    assert!(refusal.targets[0].remedy.is_some());
    assert_eq!(evidence_rows(&store), before, "nothing was written");
    assert!(
        obligations(&store, &b)
            .iter()
            .all(|(_, state, _)| *state == WorkObligationState::Open)
    );
}

/// Each typed refusal names its target and writes nothing.
#[test]
fn every_target_refusal_is_typed_and_writes_nothing() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let a = held(&mut store, "item-a", HOLDER, 9, "R1", 2);
    let b = held(&mut store, "item-b", HOLDER, 10, "R1", 4);
    let other = held(&mut store, "item-other", "someone-else", 12, "R1", 6);
    let original = native_check(&mut store, &a, "R1", 20);
    let before = evidence_rows(&store);
    let mut stale_sighting = target(&store, &b, "R1", vec![]);
    let newest = change(&mut store, &b.work, &b.claim, "item-b-later", "R1", 10, 21);
    stale_sighting.sighting.observation = b.change.clone();
    let cases = [
        (
            "not held",
            vec![target(&store, &other, "R1", vec![])],
            Reason::ClaimNotHeld,
        ),
        (
            "not newest",
            vec![stale_sighting],
            Reason::SightingNotNewest,
        ),
        (
            "original item",
            vec![target(&store, &a, "R1", vec![])],
            Reason::IsOriginalItem,
        ),
        (
            "criterion out of range",
            vec![{
                let mut t = target(&store, &b, "R1", vec![3]);
                t.sighting.observation = newest.clone();
                t
            }],
            Reason::CriterionOutOfRange,
        ),
    ];
    for (case, targets, reason) in cases {
        let refusal = {
            let input = request(&original, "R1", targets);
            refused(bind(&mut store, &input, 30))
        };
        assert_eq!(refusal.targets.len(), 1, "{case}: {refusal:?}");
        assert_eq!(refusal.targets[0].reason, reason, "{case}");
        assert_eq!(evidence_rows(&store), before, "{case}: nothing was written");
    }
    // A duplicate target is refused once, by name.
    let mut fresh = target(&store, &b, "R1", vec![]);
    fresh.sighting.observation = newest;
    let input = request(&original, "R1", vec![fresh.clone(), fresh]);
    let refusal = refused(bind(&mut store, &input, 30));
    assert_eq!(
        refusal
            .targets
            .iter()
            .map(|target| target.reason)
            .collect::<Vec<_>>(),
        vec![Reason::DuplicateTarget]
    );
}

/// A source change on the target after its sighting whose root is unknown
/// blocks the bind: the measurement does not resolve it.
#[test]
fn an_unresolved_change_after_the_sighting_refuses() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let a = held(&mut store, "item-a", HOLDER, 9, "R1", 2);
    let b = held(&mut store, "item-b", HOLDER, 10, "R1", 4);
    let original = native_check(&mut store, &a, "R1", 20);
    source_mutation_from_basis(
        &mut store,
        &b.work,
        &b.claim,
        HOLDER,
        "unlocated",
        21,
        None,
        None,
    );
    let input = request(&original, "R1", vec![target(&store, &b, "R1", vec![])]);
    let refusal = refused(bind(&mut store, &input, 30));
    assert_eq!(
        refusal.targets[0].reason,
        Reason::UnresolvedSourceAfterSighting
    );
}

/// Request-wide refusals: the measurement differing from the original, too
/// many targets, and more targets than the caller holds claims.
#[test]
fn request_refusals_cover_measurement_and_bounds() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let a = held(&mut store, "item-a", HOLDER, 9, "R1", 2);
    let b = held(&mut store, "item-b", HOLDER, 10, "R1", 4);
    let original = native_check(&mut store, &a, "R1", 20);
    let mut wrong_measure = request(&original, "R1", vec![target(&store, &b, "R1", vec![])]);
    wrong_measure.measurement.source_revision = "R2".into();
    assert_eq!(
        refused(bind(&mut store, &wrong_measure, 30)).request,
        Some(VerificationBindRequestRefusal::MeasurementRevisionDiffers)
    );
    let too_many = request(
        &original,
        "R1",
        (0..17).map(|_| target(&store, &b, "R1", vec![])).collect(),
    );
    assert_eq!(
        refused(bind(&mut store, &too_many, 30)).request,
        Some(VerificationBindRequestRefusal::TooManyTargets)
    );
    // The caller holds two claims, one of them the original's: one target.
    let over_held = request(
        &original,
        "R1",
        vec![
            target(&store, &b, "R1", vec![]),
            target(&store, &b, "R1", vec![]),
        ],
    );
    assert_eq!(
        refused(bind(&mut store, &over_held, 30)).request,
        Some(VerificationBindRequestRefusal::MoreTargetsThanHeldClaims)
    );
}

/// A bound record cannot itself be bound: chains stop at depth one.
#[test]
fn a_bound_record_is_refused_as_an_original() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let a = held(&mut store, "item-a", HOLDER, 9, "R1", 2);
    let b = held(&mut store, "item-b", HOLDER, 10, "R1", 4);
    let c = held(&mut store, "item-c", HOLDER, 11, "R1", 6);
    let original = native_check(&mut store, &a, "R1", 20);
    let VerificationBindOutcome::Bound(receipt) = ({
        let input = request(&original, "R1", vec![target(&store, &b, "R1", vec![])]);
        bind(&mut store, &input, 30)
    }) else {
        panic!("the first bind is refused")
    };
    let chained = request(
        &receipt.bound[0].verification,
        "R1",
        vec![target(&store, &c, "R1", vec![])],
    );
    assert_eq!(
        refused(bind(&mut store, &chained, 31)).original,
        Some(VerificationBindOriginalRefusal::IsBound)
    );
}

/// A criterion bound to another kind of check is reported ineligible with a
/// reason; the bind is not refused and that criterion's obligation stays
/// open, while the source change is still satisfied.
#[test]
fn a_kind_mismatch_is_ineligible_without_a_verdict() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let a = held(&mut store, "item-a", HOLDER, 9, "R1", 2);
    let b = held_with(
        &mut store,
        "item-b",
        HOLDER,
        10,
        "R1",
        4,
        VerificationRequirement {
            check_kind: VerificationKind::Build,
            check_fingerprint: None,
        },
    );
    let original = native_check(&mut store, &a, "R1", 20);
    let VerificationBindOutcome::Bound(receipt) = ({
        let input = request(&original, "R1", vec![target(&store, &b, "R1", vec![1])]);
        bind(&mut store, &input, 30)
    }) else {
        panic!("a kind mismatch refused the bind")
    };
    let eligibility = &receipt.bound[0].criteria[0];
    assert!(!eligibility.eligible);
    assert!(eligibility.reason.is_some());
    let states = obligations(&store, &b);
    assert!(
        states
            .iter()
            .any(|(rule, state, _)| rule.starts_with("acceptance_criterion")
                && *state == WorkObligationState::Open),
        "{states:?}"
    );
    assert!(
        states
            .iter()
            .any(|(rule, state, _)| rule == "source_mutation_requires_test"
                && *state == WorkObligationState::Satisfied),
        "{states:?}"
    );
}

/// A later change on the target stales the bound record like any check: the
/// new change's obligation stays open.
#[test]
fn a_later_change_stales_a_bound_record() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let a = held(&mut store, "item-a", HOLDER, 9, "R1", 2);
    let b = held(&mut store, "item-b", HOLDER, 10, "R1", 4);
    let original = native_check(&mut store, &a, "R1", 20);
    let VerificationBindOutcome::Bound(_) = ({
        let input = request(&original, "R1", vec![target(&store, &b, "R1", vec![])]);
        bind(&mut store, &input, 30)
    }) else {
        panic!("the bind is refused")
    };
    let later = change(&mut store, &b.work, &b.claim, "item-b-after", "R2", 10, 40);
    let records = store
        .work_run_obligations(b.claim.run_id)
        .expect("obligations");
    let after = records
        .iter()
        .find(|record| record.obligation.triggering_observation == later)
        .expect("the later change's obligation");
    assert_eq!(after.state, WorkObligationState::Open);
}

/// Positions are compared only within one run. Here the original's producer
/// sits early on its own run while the target's change sits late on the
/// target's run, so comparing those integers would call the check older
/// than the change; the bound record follows the change on the target run
/// and satisfies it.
#[test]
fn cross_run_positions_are_never_compared() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let a = held(&mut store, "item-a", HOLDER, 9, "R1", 2);
    let original = native_check(&mut store, &a, "R1", 3);
    let mut b = held(&mut store, "item-b", HOLDER, 10, "R0", 4);
    for step in 0..12 {
        b.change = change(
            &mut store,
            &b.work,
            &b.claim,
            &format!("item-b-step-{step}"),
            if step == 11 { "R1" } else { "R0" },
            10,
            10 + step,
        );
    }
    let producer_position = super::super::feeds::run_feed_position_for_object_on(
        &store.connection,
        a.claim.run_id,
        &stored(&store, &original).producer_observation,
    )
    .expect("producer position")
    .position;
    let change_position = super::super::feeds::run_feed_position_for_object_on(
        &store.connection,
        b.claim.run_id,
        &b.change,
    )
    .expect("change position")
    .position;
    assert!(
        producer_position < change_position,
        "the fixture puts the producer earlier in number: {producer_position} vs {change_position}"
    );
    let VerificationBindOutcome::Bound(receipt) = ({
        let input = request(&original, "R1", vec![target(&store, &b, "R1", vec![])]);
        bind(&mut store, &input, 30)
    }) else {
        panic!("the bind is refused")
    };
    let states = obligations(&store, &b);
    let latest = states.last().expect("the latest change's obligation");
    assert_eq!(latest.1, WorkObligationState::Satisfied, "{states:?}");
    assert_eq!(
        latest.2.as_deref(),
        Some(receipt.bound[0].verification.as_str())
    );
}

/// The host's entry: the bind through a bound control session's credentials.
fn bind_through_host(
    store: &mut SqliteStore,
    host: &crate::storage::test_support::TestControlBinding,
    input: &VerificationBindInput,
    second: i64,
) -> Result<VerificationBindReceipt, StoreError> {
    store.bind_verification(
        &crate::domain::ProjectId("project-a".into()),
        &host.status.session_id,
        &host.connection_token,
        &host.routing_token,
        &actor(HOLDER),
        input,
        at(second),
    )
}

fn release(store: &mut SqliteStore, held: &Held, second: i64) {
    let current = store.get_work_item(held.work.work_id).expect("item");
    store
        .release_work(
            &crate::domain::ReleaseWorkRequest {
                work_id: current.work_id,
                run_id: held.claim.run_id,
                expected_work_revision: current.revision,
                holder: held.claim.holder.clone(),
                claim_id: held.claim.claim_id,
                claim_fence: held.claim.fence,
                reason: "stepping away".into(),
                waiver_reason: Some("the check is recorded".into()),
                actor: actor(HOLDER),
                idempotency_key: format!("release-{second}"),
                released_at: at(second),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("release");
}

/// A lost reply is recovered by repeating the request: after the store was
/// closed and opened again, the original's claim ended and the host
/// reconnected with new credentials, the same key and intent return the
/// stored receipt, marked replayed, with the same record ids and nothing
/// written again. The routing token is not part of the intent. The stored
/// operation passes doctor.
#[test]
fn a_retry_replays_after_a_restart_and_an_ended_claim() {
    let directory = crate::test_support::temp_home().expect("directory");
    let database = directory.path().join("engram.sqlite3");
    let mut store = SqliteStore::open(&database).expect("store");
    let a = held(&mut store, "item-a", HOLDER, 9, "R1", 2);
    let b = held(&mut store, "item-b", HOLDER, 10, "R1", 4);
    let c = held(&mut store, "item-c", HOLDER, 11, "R1", 6);
    let original = native_check(&mut store, &a, "R1", 20);
    let input = request(
        &original,
        "R1",
        vec![
            target(&store, &b, "R1", vec![1]),
            target(&store, &c, "R1", vec![]),
        ],
    );
    let first_host = bind_control_for(
        &mut store,
        HOLDER,
        "bind-host-1",
        &[EffectClass::Observe],
        at(25),
    );
    let first = bind_through_host(&mut store, &first_host, &input, 30).expect("bound");
    assert!(!first.replayed);
    let rows = evidence_rows(&store);
    let operations = control_operations(&store);
    let report = store.verify_all().expect("doctor");
    assert!(
        report.is_healthy(),
        "the stored bind passes doctor: {report:?}"
    );

    // The host process restarts: the store is closed and opened again.
    drop(store);
    let mut store = SqliteStore::open(&database).expect("reopened store");
    release(&mut store, &a, 40);
    let second_host = bind_control_for(
        &mut store,
        HOLDER,
        "bind-host-2",
        &[EffectClass::Observe],
        at(50),
    );
    assert_ne!(second_host.connection_token, first_host.connection_token);
    let replay = bind_through_host(&mut store, &second_host, &input, 60).expect("replayed");
    assert!(replay.replayed);
    assert_eq!(replay.original, first.original);
    assert_eq!(replay.bound, first.bound, "the same records, ids unchanged");
    assert_eq!(evidence_rows(&store), rows, "nothing written again");
    assert_eq!(control_operations(&store), operations);

    // The same key for another request is a conflict and writes nothing.
    let mut changed = input.clone();
    changed.targets[1].criteria = vec![1];
    assert!(matches!(
        bind_through_host(&mut store, &second_host, &changed, 61),
        Err(StoreError::ControlOperationIdempotencyConflict { .. })
    ));
    // A new bind needs the original's claim live and held.
    changed.idempotency_key = "bind-2".into();
    match bind_through_host(&mut store, &second_host, &changed, 62) {
        Err(StoreError::VerificationBindRefused(refusal)) => {
            assert_eq!(
                refusal.original,
                Some(VerificationBindOriginalRefusal::ClaimNotHeld)
            );
        }
        other => panic!("a new bind after the claim ended: {other:?}"),
    }
    assert_eq!(evidence_rows(&store), rows);
    assert_eq!(control_operations(&store), operations);
    // After the release, the restart and the replay, the store still verifies.
    let report = store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
}

/// An evaluation of `held` read through `through`, declaring `declared` as
/// the source it judged, with one verdict per criterion.
fn evaluation(
    store: &SqliteStore,
    held: &Held,
    through: i64,
    declared: Option<&str>,
    verdicts: Vec<crate::domain::CriterionVerdictInput>,
    key: &str,
    second: i64,
) -> crate::domain::RecordAcceptanceEvaluationRequest {
    let work = store.get_work_item(held.work.work_id).expect("item");
    crate::domain::RecordAcceptanceEvaluationRequest {
        supersedes: None,
        project_id: work.project_id.clone(),
        work_id: work.work_id,
        expected_work_revision: work.revision,
        evaluated_through: through,
        mode: crate::domain::AcceptanceEvaluationMode::SameSession,
        execution_identity: None,
        parent_session: None,
        evaluator_model: None,
        source_basis: declared.map(|revision| crate::domain::AcceptanceSourceBasis {
            workspace_id: None,
            fingerprint: revision.into(),
        }),
        verdicts,
        evaluator: actor(HOLDER),
        attempt_key: Some(key.into()),
        recorded_at: at(second),
    }
}

fn verdict(
    criterion: usize,
    verdict: crate::domain::AcceptanceVerdict,
    basis: crate::domain::AcceptanceBasis,
    evidence: &[ObjectId],
) -> crate::domain::CriterionVerdictInput {
    crate::domain::CriterionVerdictInput {
        criterion,
        verdict,
        basis,
        rationale: format!("criterion {criterion}"),
        evidence: evidence.to_vec(),
    }
}

fn run_head(store: &SqliteStore, held: &Held) -> i64 {
    super::super::feeds::current_run_feed_cut_on(&store.connection, held.claim.run_id)
        .expect("head")
        .position
}

/// Binding keeps the existing evaluation-staleness rules and their
/// same-source exception: a bound passed check on the revision an evaluation
/// declared asks for no resubmission, while after an evaluation that
/// declared nothing the same bind leaves it stale. A later evaluation whose
/// basis includes the bound record may cite it for the bound criterion.
#[test]
fn a_bound_check_keeps_the_same_source_exception_and_supports_a_cited_pass() {
    use crate::domain::{AcceptanceBasis, AcceptanceStaleReason, AcceptanceVerdict};
    for declared in [Some("R1"), None] {
        let mut store = SqliteStore::open_in_memory().expect("store");
        store
            .set_acceptance_evaluation_policy(
                &crate::domain::AcceptanceEvaluationPolicy {
                    allowed_modes: vec![crate::domain::AcceptanceEvaluationMode::SameSession],
                    mechanical_basis: crate::domain::MechanicalBasis::Asserted,
                    require_source_freshness: false,
                },
                &actor("policy-admin"),
                "enable",
                None,
                at(1),
                &DevelopmentNoopRedactor,
            )
            .expect("policy");
        let a = held(&mut store, "item-a", HOLDER, 9, "R1", 2);
        let b = held(&mut store, "item-b", HOLDER, 10, "R1", 4);
        let original = native_check(&mut store, &a, "R1", 20);
        let failing = vec![
            verdict(1, AcceptanceVerdict::Fail, AcceptanceBasis::Judgment, &[]),
            verdict(2, AcceptanceVerdict::Fail, AcceptanceBasis::Judgment, &[]),
        ];
        let before = evaluation(
            &store,
            &b,
            run_head(&store, &b),
            declared,
            failing,
            "before",
            25,
        );
        store
            .record_acceptance_evaluation(&before, &DevelopmentNoopRedactor)
            .expect("the evaluation records");
        let VerificationBindOutcome::Bound(receipt) = ({
            let input = request(&original, "R1", vec![target(&store, &b, "R1", vec![1])]);
            bind(&mut store, &input, 30)
        }) else {
            panic!("the bind is refused")
        };
        let stale = store
            .acceptance_evaluation_status(b.work.work_id, None)
            .expect("status")
            .expect("an evaluation")
            .stale;
        match declared {
            Some(_) => assert_eq!(stale, None, "the same-source exception holds"),
            None => assert_eq!(stale, Some(AcceptanceStaleReason::Mutation)),
        }
        // A later evaluation through the bind cites the bound record for the
        // criterion bound to a test.
        let bound = receipt.bound[0].verification.clone();
        let cited = evaluation(
            &store,
            &b,
            run_head(&store, &b),
            Some("R1"),
            vec![
                verdict(
                    1,
                    AcceptanceVerdict::Pass,
                    AcceptanceBasis::Observed,
                    &[bound],
                ),
                verdict(2, AcceptanceVerdict::Fail, AcceptanceBasis::Judgment, &[]),
            ],
            "cited",
            40,
        );
        store
            .record_acceptance_evaluation(&cited, &DevelopmentNoopRedactor)
            .expect("a pass citing the bound record records");
    }
}

/// The binding read and the verification read show a bound record with its
/// typed bound branch, the producer's record and position on the original's
/// run, and both times; a native record keeps its exact shape, with no
/// `bound` member. The record's assessment carries the same branch.
#[test]
fn readers_expose_the_bound_branch_only_on_bound_records() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let a = held(&mut store, "item-a", HOLDER, 9, "R1", 2);
    let b = held(&mut store, "item-b", HOLDER, 10, "R1", 4);
    let original = native_check(&mut store, &a, "R1", 20);
    let VerificationBindOutcome::Bound(receipt) = ({
        let input = request(&original, "R1", vec![target(&store, &b, "R1", vec![1])]);
        bind(&mut store, &input, 30)
    }) else {
        panic!("the bind is refused")
    };
    let bound_id = receipt.bound[0].verification.clone();
    let original_record = stored(&store, &original);
    let position_on = |held: &Held, id: &ObjectId| {
        super::super::feeds::run_feed_position_for_object_on(
            &store.connection,
            held.claim.run_id,
            id,
        )
        .expect("position")
        .position
    };
    let producer_on_a = position_on(&a, &original_record.producer_observation);
    let original_on_a = position_on(&a, &original);
    let project = crate::domain::ProjectId("project-a".into());
    let binding_page = |held: &Held| {
        let work = store.get_work_item(held.work.work_id).expect("item");
        super::super::binding_read::read_acceptance_bindings_on(
            &store.connection,
            &project,
            &super::super::binding_read::BindingReadRequest {
                work_id: work.work_id,
                expected_work_revision: work.revision,
                run_id: held.claim.run_id,
                after: None,
            },
        )
        .expect("binding read")
    };
    let satisfying = |page: &crate::domain::AcceptanceBindingPage| {
        page.rows[0]
            .binding
            .as_ref()
            .and_then(|binding| binding.obligation.as_ref())
            .and_then(|obligation| obligation.resolution.as_ref())
            .and_then(|resolution| resolution.satisfaction.as_ref())
            .map(|satisfaction| satisfaction.verification.clone())
            .expect("criterion 1 is satisfied")
    };

    let read = satisfying(&binding_page(&b));
    assert_eq!(read.record, bound_id);
    assert_eq!(read.producer.record, original_record.producer_observation);
    assert_eq!(
        read.producer.position, producer_on_a,
        "on the original's run"
    );
    let bound = read.bound.expect("the bound branch");
    assert_eq!(bound.verification, original);
    assert_eq!(bound.work_ref, a.work.short_ref);
    assert_eq!(bound.run, a.claim.run_id);
    assert_eq!(bound.original_position, original_on_a);
    assert_eq!(bound.original_basis, original_record.source_basis);
    assert_eq!(bound.original_completed_at, at(20));
    assert_eq!(bound.bound_at, at(30));
    assert_eq!(bound.binder.actor_id, actor(HOLDER).actor_id);
    assert_eq!(bound.sighting, b.change);
    assert_eq!(bound.measurement.source_revision, "R1");
    assert_eq!(bound.criteria, vec![1]);

    let verification_page = |held: &Held| {
        let work = store.get_work_item(held.work.work_id).expect("item");
        super::super::verification_read::read_acceptance_verifications_on(
            &store.connection,
            &project,
            &super::super::verification_read::VerificationReadRequest {
                work_id: work.work_id,
                expected_work_revision: work.revision,
                run_id: held.claim.run_id,
                run_cut: run_head(&store, held),
                criterion: 1,
                after: None,
            },
        )
        .expect("verification read")
    };
    let rows = verification_page(&b).rows;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].record, bound_id);
    assert_eq!(rows[0].producer.position, producer_on_a);
    assert!(rows[0].bound.is_some());

    // The native original keeps its exact shape.
    let native = satisfying(&binding_page(&a));
    assert_eq!(native.record, original);
    assert!(native.bound.is_none());
    let wire = serde_json::to_value(&native).expect("json");
    assert!(wire.get("bound").is_none(), "{wire}");
    let native_rows = verification_page(&a).rows;
    assert!(native_rows.iter().all(|row| row.bound.is_none()));

    // The record's assessment, which show --note renders, carries the branch.
    let assessment = store
        .verification_assessment(b.work.work_id, &bound_id, None, 8)
        .expect("assessment")
        .expect("a verification record");
    assert_eq!(
        assessment.bound.as_ref().map(|bound| &bound.verification),
        Some(&original)
    );
    let native_assessment = store
        .verification_assessment(a.work.work_id, &original, None, 8)
        .expect("assessment")
        .expect("a verification record");
    assert!(native_assessment.bound.is_none());
}

/// A file store at `database` holding items A and B of project-a, each
/// claimed by the runner on its own generation of one named root, with a
/// passed test of A's root bound to B's first criterion. Returns B's short
/// ref, the bound record's id and the original's id.
pub(crate) fn bound_assessment_fixture(database: &std::path::Path) -> (String, ObjectId, ObjectId) {
    let mut store = SqliteStore::open(database).expect("store");
    let a = held(&mut store, "item-a", HOLDER, 9, "R1", 2);
    let b = held(&mut store, "item-b", HOLDER, 10, "R1", 4);
    let original = native_check(&mut store, &a, "R1", 20);
    let input = request(&original, "R1", vec![target(&store, &b, "R1", vec![1])]);
    let VerificationBindOutcome::Bound(receipt) = bind(&mut store, &input, 30) else {
        panic!("the bind is refused")
    };
    (
        b.work.short_ref,
        receipt.bound[0].verification.clone(),
        original,
    )
}

/// Every row of every ordinary table, in a stable order. Full-text shadow
/// tables are compared through the doctor instead.
fn all_rows(connection: &rusqlite::Connection) -> std::collections::BTreeMap<String, Vec<String>> {
    let tables: Vec<String> = connection
        .prepare(
            "SELECT name FROM sqlite_master
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' AND name NOT LIKE '%fts%'
             ORDER BY name",
        )
        .expect("tables")
        .query_map([], |row| row.get(0))
        .expect("query")
        .map(|name| name.expect("name"))
        .collect();
    tables
        .into_iter()
        .map(|table| {
            let mut statement = connection
                .prepare(&format!("SELECT * FROM \"{table}\""))
                .expect("select");
            let columns = statement.column_count();
            let mut rows: Vec<String> = statement
                .query_map([], |row| {
                    (0..columns)
                        .map(|column| row.get::<_, rusqlite::types::Value>(column))
                        .collect::<Result<Vec<_>, _>>()
                })
                .expect("rows")
                .map(|row| format!("{:?}", row.expect("row")))
                .collect();
            rows.sort();
            (table, rows)
        })
        .collect()
}

fn objects(connection: &rusqlite::Connection) -> Vec<(String, Vec<u8>)> {
    connection
        .prepare("SELECT object_id, canonical_json FROM objects ORDER BY object_id")
        .expect("objects")
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query")
        .map(|row| row.expect("object"))
        .collect()
}

/// No format change, on a store holding only existing records and on one
/// holding bound records: projection repair rebuilds every derived row
/// exactly and the doctor stays clean, and a migration export then import
/// carries every record's canonical bytes unchanged into a clean store.
#[test]
fn repair_and_migration_round_trip_with_and_without_bound_records() {
    for with_bound in [false, true] {
        let directory = crate::test_support::temp_home().expect("directory");
        let database = directory.path().join("source.db");
        let mut bound_id = None;
        {
            let mut store = SqliteStore::open(&database).expect("store");
            let a = held(&mut store, "item-a", HOLDER, 9, "R1", 2);
            let b = held(&mut store, "item-b", HOLDER, 10, "R1", 4);
            // The original links an environment record on its own run.
            let original = native_check_in(&mut store, &a, "R1", 20, true);
            if with_bound {
                // Through the host entry, which also stores the operation.
                let input = request(&original, "R1", vec![target(&store, &b, "R1", vec![1])]);
                let host = bind_control_for(
                    &mut store,
                    HOLDER,
                    "bind-host",
                    &[EffectClass::Observe],
                    at(25),
                );
                let receipt = bind_through_host(&mut store, &host, &input, 30).expect("bound");
                let bound = receipt.bound[0].verification.clone();
                assert!(stored(&store, &bound).environment.is_some());
                // The target's focus evidence reads the bound record although
                // its environment lies on the original's run.
                let focus = super::super::execution::work_run_evidence_projection_on(
                    &store.connection,
                    b.claim.run_id,
                    8,
                )
                .expect("the target's focus evidence");
                assert!(focus.iter().any(|candidate| candidate.hash == bound));
                bound_id = Some(bound);
            }
            let report = store.verify_all().expect("doctor");
            assert!(report.is_healthy(), "{with_bound}: {report:?}");
        }
        let before = all_rows(&rusqlite::Connection::open(&database).expect("open"));
        let report = SqliteStore::repair_rebuildable_projections(&database).expect("repair");
        assert!(report.is_healthy(), "{with_bound}: {report:?}");
        let after = all_rows(&rusqlite::Connection::open(&database).expect("open"));
        assert_eq!(
            after, before,
            "{with_bound}: repair rebuilt every row exactly"
        );

        let file = directory.path().join("export.jsonl");
        crate::storage::migration::export_json(&database, &file).expect("export");
        let imported = directory.path().join("imported.db");
        crate::storage::migration::import_json(&file, &imported).expect("import");
        let store = SqliteStore::open(&imported).expect("imported store");
        let report = store.verify_all().expect("doctor");
        assert!(report.is_healthy(), "{with_bound}: {report:?}");
        assert_eq!(
            objects(&store.connection),
            objects(&rusqlite::Connection::open(&database).expect("open")),
            "{with_bound}: every record's canonical bytes travel unchanged"
        );
        if let Some(bound_id) = bound_id {
            assert!(stored(&store, &bound_id).bound_from.is_some());
            // The doctor verifies a bound record's projection row: drift in
            // it is reported, not passed over.
            store
                .connection
                .execute(
                    "UPDATE work_run_evidence SET source_revision = 'drifted'
                     WHERE evidence_id = ?1",
                    [bound_id.as_str()],
                )
                .expect("drift the bound row");
            let report = store.verify_all().expect("doctor");
            assert!(!report.is_healthy(), "drift in a bound row is reported");
        }
    }
}

/// A criterion pinned to another check is reported ineligible with a reason;
/// the bind is not refused, the pinned criterion's obligation stays open,
/// and the source change is still satisfied.
#[test]
fn a_pin_mismatch_is_ineligible_and_leaves_the_criterion_open() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let a = held(&mut store, "item-a", HOLDER, 9, "R1", 2);
    let b = held_with(
        &mut store,
        "item-b",
        HOLDER,
        10,
        "R1",
        4,
        VerificationRequirement {
            check_kind: VerificationKind::Test,
            check_fingerprint: Some(check_fingerprint("another suite")),
        },
    );
    let original = native_check(&mut store, &a, "R1", 20);
    let VerificationBindOutcome::Bound(receipt) = ({
        let input = request(&original, "R1", vec![target(&store, &b, "R1", vec![1])]);
        bind(&mut store, &input, 30)
    }) else {
        panic!("a pin mismatch refused the bind")
    };
    let eligibility = &receipt.bound[0].criteria[0];
    assert!(!eligibility.eligible);
    assert!(eligibility.reason.is_some());
    let states = obligations(&store, &b);
    assert!(
        states
            .iter()
            .any(|(rule, state, _)| rule.starts_with("acceptance_criterion")
                && *state == WorkObligationState::Open),
        "{states:?}"
    );
    assert!(
        states
            .iter()
            .any(|(rule, state, _)| rule == "source_mutation_requires_test"
                && *state == WorkObligationState::Satisfied),
        "{states:?}"
    );
}

/// A target whose root was named again since its sighting has no sighting
/// under the new generation, and a sighting outside the root's named scope
/// is not the root's: both are refused, writing nothing.
#[test]
fn a_stale_generation_or_an_unscoped_sighting_refuses() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let a = held(&mut store, "item-a", HOLDER, 9, "R1", 2);
    let b = held(&mut store, "item-b", HOLDER, 10, "R1", 4);
    let c = held(&mut store, "item-c", HOLDER, 11, "R1", 6);
    let original = native_check(&mut store, &a, "R1", 20);
    let before = evidence_rows(&store);

    // b's root is named again, at a new generation, after its sighting.
    name_root(&mut store, &b.work, &b.claim, 12, 21);
    let stale = request(&original, "R1", vec![target(&store, &b, "R1", vec![])]);
    let refusal = refused(bind(&mut store, &stale, 30));
    assert_eq!(refusal.targets.len(), 1, "{refusal:?}");
    assert_eq!(refusal.targets[0].reason, Reason::SightingMissing);

    // c's newest record names the root's workspace without its generation.
    let unscoped = source_mutation_from_basis(
        &mut store,
        &c.work,
        &c.claim,
        HOLDER,
        "item-c-unscoped",
        22,
        Some(ExecutionSourceBasis {
            workspace_id: ROOT.into(),
            source_revision: "R1".into(),
            source_root_generation: None,
            source_root_state: None,
        }),
        None,
    );
    let mut scoped = target(&store, &c, "R1", vec![]);
    scoped.sighting.observation = unscoped;
    let refusal = refused(bind(
        &mut store,
        &request(&original, "R1", vec![scoped]),
        31,
    ));
    assert_eq!(refusal.targets.len(), 1, "{refusal:?}");
    assert_eq!(refusal.targets[0].reason, Reason::SightingNotNewest);
    assert_eq!(evidence_rows(&store), before, "nothing was written");
}

/// Doctor holds a bound record's sighting to what the bind required: a
/// record whose `bound_from.sighting` names another observation, here the
/// original's producer on another run, is reported.
#[test]
fn doctor_reports_a_bound_record_naming_another_sighting() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let a = held(&mut store, "item-a", HOLDER, 9, "R1", 2);
    let b = held(&mut store, "item-b", HOLDER, 10, "R1", 4);
    let original = native_check(&mut store, &a, "R1", 20);
    let VerificationBindOutcome::Bound(receipt) = ({
        let input = request(&original, "R1", vec![target(&store, &b, "R1", vec![])]);
        bind(&mut store, &input, 30)
    }) else {
        panic!("the bind is refused")
    };
    assert!(store.verify_all().expect("doctor").is_healthy());
    let producer = stored(&store, &original).producer_observation;
    store
        .connection
        .execute(
            "UPDATE objects
             SET canonical_json = CAST(json_set(CAST(canonical_json AS TEXT),
                 '$.bound_from.sighting', ?2) AS BLOB)
             WHERE object_id = ?1",
            rusqlite::params![receipt.bound[0].verification.as_str(), producer.as_str()],
        )
        .expect("forge the sighting");
    assert_eq!(
        stored(&store, &receipt.bound[0].verification)
            .bound_from
            .expect("bound")
            .sighting,
        producer
    );
    let report = store.verify_all().expect("doctor");
    assert!(!report.is_healthy(), "a forged sighting is reported");
}

/// A check is current at its producer's position, not its record's: a
/// verification recorded after a change of unknown place, naming a producer
/// from before that change, is refused as an original, writing nothing,
/// because the native rules would not let that check stand either.
#[test]
fn an_original_whose_source_moved_after_its_producer_refuses() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let a = held(&mut store, "item-a", HOLDER, 9, "R1", 2);
    let b = held(&mut store, "item-b", HOLDER, 10, "R1", 4);
    let original = check_with(&mut store, &a, "R1", 20, false, true);
    let before = evidence_rows(&store);
    let input = request(&original, "R1", vec![target(&store, &b, "R1", vec![])]);
    assert_eq!(
        refused(bind(&mut store, &input, 30)).original,
        Some(VerificationBindOriginalRefusal::RootNotCurrent)
    );
    assert_eq!(evidence_rows(&store), before, "nothing was written");
}

/// Intended criteria are strictly increasing positions: a repeat or an
/// unordered list is refused by name, so a list never outgrows the item.
#[test]
fn repeated_or_unordered_criteria_refuse() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let a = held(&mut store, "item-a", HOLDER, 9, "R1", 2);
    let b = held(&mut store, "item-b", HOLDER, 10, "R1", 4);
    let original = native_check(&mut store, &a, "R1", 20);
    for criteria in [vec![1, 1], vec![2, 1]] {
        let input = request(
            &original,
            "R1",
            vec![target(&store, &b, "R1", criteria.clone())],
        );
        let refusal = refused(bind(&mut store, &input, 30));
        assert_eq!(
            refusal.targets[0].reason,
            Reason::CriteriaNotIncreasing,
            "{criteria:?}"
        );
    }
}

/// Doctor's check of a stored bind holds the whole requested sighting to
/// the written record: a stored request naming the right sighting at
/// another revision does not match, nor does a different sighting.
#[test]
fn a_stored_bind_must_name_the_written_sighting_and_revision() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let a = held(&mut store, "item-a", HOLDER, 9, "R1", 2);
    let b = held(&mut store, "item-b", HOLDER, 10, "R1", 4);
    let original = native_check(&mut store, &a, "R1", 20);
    let input = request(&original, "R1", vec![target(&store, &b, "R1", vec![1])]);
    let VerificationBindOutcome::Bound(receipt) = bind(&mut store, &input, 30) else {
        panic!("the bind is refused")
    };
    let matches = |targets: &[VerificationBindTarget]| {
        super::bound_receipt_matches_on(
            &store.connection,
            &SessionId(HOLDER.into()),
            &original,
            &input.measurement,
            targets,
            &receipt.bound,
        )
        .expect("check")
    };
    assert!(matches(&input.targets));
    let mut other_revision = input.targets.clone();
    other_revision[0].sighting.source_revision = "R2".into();
    assert!(!matches(&other_revision));
    let mut other_sighting = input.targets.clone();
    other_sighting[0].sighting.observation = a.change.clone();
    assert!(!matches(&other_sighting));
}

/// An accounted inter-turn change the host observed without admission on
/// `held`'s run, inside its named root, from `from` to `to`, recorded at
/// `second`.
fn unadmitted_change(
    store: &mut SqliteStore,
    held: &Held,
    from: &str,
    to: &str,
    second: i64,
) -> (ObjectId, Vec<ObjectId>) {
    use crate::domain::{
        ExecutionObserveInput, MeasuredBaseline, MeasuredSighting, NamedRootState,
        ObservationCausality, ObservationPolicyBasis, ObservationRootBasis, ObservedInterval,
        ObservedOccurrence, ObservedSourceChange,
    };
    let host = bind_control_for(
        store,
        HOLDER,
        &format!("observe-host-{second}"),
        &[EffectClass::Observe],
        at(second - 3),
    );
    let policy = SqliteStore::load_active_control_policy(&store.connection).expect("policy");
    let input = ExecutionObserveInput {
        idempotency_key: format!("observed-{second}"),
        binding: binding_of(store, held),
        root_basis: ObservationRootBasis {
            capture_run_cut: run_head(store, held),
            latest_event: Some(held.root_event.clone()),
            state: NamedRootState::Bound {
                workspace_id: ROOT.into(),
                generation: held.generation,
                named_at: at(held.named_second),
            },
        },
        observed_interval: ObservedInterval {
            from: at(second - 2),
            through: at(second - 1),
        },
        occurrence: ObservedOccurrence::InterTurnChange {
            source_change: ObservedSourceChange::ContentComparison {
                workspace_id: ROOT.into(),
                baseline: MeasuredBaseline {
                    workspace_id: ROOT.into(),
                    source_revision: from.into(),
                    observed_at: at(second - 2),
                },
                sighting: MeasuredSighting {
                    source_basis: basis(to, held.generation),
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
    let mut observer = actor(HOLDER);
    observer.actor_kind = crate::work_service::WORD_ACTOR_KIND.into();
    observer.run_id = Some(held.claim.run_id.0.to_string());
    let receipt = store
        .record_unadmitted_execution_observation(
            &held.work.project_id,
            &host.status.session_id,
            &host.connection_token,
            &host.routing_token,
            &observer,
            input,
            at(second),
        )
        .expect("accounted unadmitted change");
    (receipt.observation, receipt.opened_obligations)
}

/// The original must still stand on its own run as an evaluation citing it
/// would read it. A later change on that run to another revision retires it,
/// and so does an accounted unadmitted change the check does not follow,
/// even when a later sighting puts the source back at the check's revision.
/// A later change back at the same revision alone leaves it standing.
#[test]
fn an_original_the_native_rules_retired_refuses() {
    // A later change at another revision.
    let mut store = SqliteStore::open_in_memory().expect("store");
    let a = held(&mut store, "item-a", HOLDER, 9, "R1", 2);
    let b = held(&mut store, "item-b", HOLDER, 10, "R1", 4);
    let original = native_check(&mut store, &a, "R1", 20);
    change(&mut store, &a.work, &a.claim, "item-a-moved", "R2", 9, 22);
    let before = evidence_rows(&store);
    let input = request(&original, "R1", vec![target(&store, &b, "R1", vec![])]);
    assert_eq!(
        refused(bind(&mut store, &input, 30)).original,
        Some(VerificationBindOriginalRefusal::RootNotCurrent)
    );
    assert_eq!(evidence_rows(&store), before, "nothing was written");

    // An accounted unadmitted change, then a sighting back at R1.
    let mut store = SqliteStore::open_in_memory().expect("store");
    let a = held(&mut store, "item-a", HOLDER, 9, "R1", 2);
    let b = held(&mut store, "item-b", HOLDER, 10, "R1", 4);
    let original = native_check(&mut store, &a, "R1", 20);
    let _ = unadmitted_change(&mut store, &a, "R1", "R2", 25);
    change(&mut store, &a.work, &a.claim, "item-a-back", "R1", 9, 27);
    let input = request(&original, "R1", vec![target(&store, &b, "R1", vec![])]);
    assert_eq!(
        refused(bind(&mut store, &input, 30)).original,
        Some(VerificationBindOriginalRefusal::RootNotCurrent)
    );

    // Only a later change back at the check's own revision: it still binds.
    let mut store = SqliteStore::open_in_memory().expect("store");
    let a = held(&mut store, "item-a", HOLDER, 9, "R1", 2);
    let b = held(&mut store, "item-b", HOLDER, 10, "R1", 4);
    let original = native_check(&mut store, &a, "R1", 20);
    change(&mut store, &a.work, &a.claim, "item-a-same", "R1", 9, 22);
    let input = request(&original, "R1", vec![target(&store, &b, "R1", vec![])]);
    assert!(
        matches!(
            bind(&mut store, &input, 30),
            VerificationBindOutcome::Bound(_)
        ),
        "a same-revision sighting leaves the check standing"
    );
}

/// On the target's run, an accounted unadmitted change's time floor holds
/// the check's own completion, as for a native check. A check that completed
/// before such a change was recorded could satisfy nothing on that run, so
/// the bind refuses that target by name and writes nothing. That holds
/// whether a later sighting put the source back at the check's revision, or
/// the change itself is the newest sighting. It holds also when the target
/// already passed its own check after the change: that check stays the
/// run's newest, so the target's completion is not refused by a dead
/// record. Binding adds no exemption and no new staleness.
#[test]
fn a_check_older_than_a_target_unadmitted_change_refuses() {
    for case in [
        "back_by_sighting",
        "change_is_newest",
        "target_passed_after",
    ] {
        let mut store = SqliteStore::open_in_memory().expect("store");
        let a = held(&mut store, "item-a", HOLDER, 9, "R1", 2);
        // Unless the source comes back by a sighting, the target starts at
        // R0, so the change to R1 is a real change.
        let start = if case == "back_by_sighting" {
            "R1"
        } else {
            "R0"
        };
        let b = held(&mut store, "item-b", HOLDER, 10, start, 4);
        let original = native_check(&mut store, &a, "R1", 20);
        let sighting = match case {
            "back_by_sighting" => {
                let _ = unadmitted_change(&mut store, &b, "R1", "R2", 25);
                change(&mut store, &b.work, &b.claim, "item-b-back", "R1", 10, 27)
            }
            "change_is_newest" => unadmitted_change(&mut store, &b, "R0", "R1", 25).0,
            _ => {
                let _ = unadmitted_change(&mut store, &b, "R0", "R1", 25);
                let own = native_check(&mut store, &b, "R1", 26);
                stored(&store, &own).producer_observation
            }
        };
        let before = evidence_rows(&store);
        let obligations_before = obligations(&store, &b);
        let mut bound_target = target(&store, &b, "R1", vec![1]);
        bound_target.sighting.observation = sighting;
        let input = request(&original, "R1", vec![bound_target]);
        let refusal = refused(bind(&mut store, &input, 30));
        assert_eq!(refusal.targets.len(), 1, "{case}: {refusal:?}");
        assert_eq!(
            refusal.targets[0].reason,
            Reason::CheckPredatesUnadmittedChange,
            "{case}"
        );
        assert!(refusal.targets[0].remedy.is_some(), "{case}");
        assert_eq!(evidence_rows(&store), before, "{case}: nothing was written");
        assert_eq!(obligations(&store, &b), obligations_before, "{case}");
    }
}

/// A target's `claim_not_held` refusal says why the claim is not held.
#[test]
fn a_claim_not_held_refusal_names_its_cause() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let a = held(&mut store, "item-a", HOLDER, 9, "R1", 2);
    let b = held(&mut store, "item-b", HOLDER, 10, "R1", 4);
    let other = held(&mut store, "item-other", "someone-else", 12, "R1", 6);
    let original = native_check(&mut store, &a, "R1", 20);
    let input = request(
        &original,
        "R1",
        vec![
            target(&store, &b, "R1", vec![]),
            target(&store, &other, "R1", vec![]),
        ],
    );
    let refusal = refused(bind(&mut store, &input, 30));
    let not_held = refusal
        .targets
        .iter()
        .find(|target| target.work_id == other.work.work_id)
        .expect("the other session's item");
    assert_eq!(not_held.reason, Reason::ClaimNotHeld);
    assert_eq!(
        not_held.actual.as_deref(),
        Some("another session holds the claim")
    );
}

fn control_operations(store: &SqliteStore) -> i64 {
    store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM control_operation_results",
            [],
            |row| row.get(0),
        )
        .expect("count")
}

/// An existing native record's canonical bytes have no `bound_from` member,
/// so its bytes and fingerprint are what they were before bound records
/// existed.
#[test]
fn a_native_record_keeps_its_canonical_bytes() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let a = held(&mut store, "item-a", HOLDER, 9, "R1", 2);
    let original = native_check(&mut store, &a, "R1", 20);
    let bytes: Vec<u8> = store
        .connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_id = ?1",
            [original.as_str()],
            |row| row.get(0),
        )
        .expect("bytes");
    let text = String::from_utf8(bytes.clone()).expect("utf8");
    assert!(!text.contains("bound_from"), "{text}");
    let record = stored(&store, &original);
    assert_eq!(
        crate::canonical::canonical_bytes(&record).expect("canonical"),
        bytes
    );
}
