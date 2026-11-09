//! Reading the bound records an earlier build's `verification_bind` stored.
//! The operation is gone. `store_bound` writes such a record and its receipt
//! through the production append and receipt paths, so these tests hold the
//! readers, the matcher and doctor to the rows the live stores keep.

use rusqlite::TransactionBehavior;

use super::super::completion::{
    append_control_execution_observation_on, append_control_verification_evidence_on,
    named_root_at_cut_on,
};
use super::super::feeds::{current_run_feed_cut_on, run_feed_position_for_object_on};
use super::super::query::load_work_run;
use super::super::test_support::*;
use crate::canonical::CanonicalObject;
use crate::domain::SCHEMA_VERSION;
use crate::domain::{
    AcceptanceBinding, BindMeasurement, BoundCriterionEligibility, BoundVerificationSource,
    BoundVerificationTarget, ControlWorkBinding, EffectClass, ExecutionObservation,
    ExecutionOutcome, ExecutionSourceBasis, NamedRootBindingKind, SessionId, SourceRootState,
    VerificationBindReceipt, VerificationBindSighting, VerificationBindTarget,
    VerificationEvidence, VerificationKind, VerificationRequirement, VerificationResult, WorkClaim,
    WorkItem, WorkObligationState,
};
use crate::storage::SqliteStore;
use crate::storage::test_support::open_work_host;
use crate::storage::verification_bind::{VERIFICATION_BIND_OPERATION, VerificationBindIntent};
use crate::{DevelopmentNoopRedactor, ObjectId};

const ROOT: &str = "workspace-root";
const HOLDER: &str = "runner";

mod during_check;
mod stored_shapes;

fn basis(revision: &str, generation: i64) -> ExecutionSourceBasis {
    ExecutionSourceBasis {
        git_tree: None,
        workspace_id: ROOT.into(),
        source_revision: revision.into(),
        source_root_generation: Some(generation),
        source_root_state: Some(SourceRootState::Named),
    }
}

/// One held item of the changeset: its claim, its root's generation and its
/// newest change under that root.
struct Held {
    git_tree: Option<crate::domain::GitTreeId>,
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
        git_tree: None,
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
    let host = open_work_host(store, &claim.holder.0, at(second));
    store
        .bind_named_root(
            &work.project_id,
            &host.session_id,
            &host.connection_token,
            crate::storage::HostSessionAuthority::Work,
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
    let binding = binding_of(store, held);
    let mut source_basis = basis(revision, held.generation);
    source_basis.git_tree.clone_from(&held.git_tree);
    let transaction = store
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
                    source_basis: source_basis.clone(),
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
            source_basis: Some(source_basis.clone()),
            observed_at: Some(at(second)),
            actor: run_actor.clone(),
            recorded_at: at(second),
        },
    )
    .expect("producer");
    let verification = append_control_verification_evidence_on(
        &transaction,
        &VerificationEvidence {
            applicability: source_basis
                .git_tree
                .clone()
                .map(|tree| crate::domain::VerificationApplicability::GitTree { tree }),
            schema_version: SCHEMA_VERSION,
            project_id: held.work.project_id.clone(),
            binding,
            session_id: SessionId(HOLDER.into()),
            producer_observation: producer,
            source_basis: source_basis.into(),
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

/// What a stored bind's intent carried.
struct StoredBind {
    idempotency_key: String,
    original: ObjectId,
    measurement: BindMeasurement,
    targets: Vec<VerificationBindTarget>,
}

fn request(
    original: &ObjectId,
    revision: &str,
    targets: Vec<VerificationBindTarget>,
) -> StoredBind {
    StoredBind {
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

/// Stores what an earlier build's bind left: one bound record per target on
/// the target's own run, then its intent and receipt as an ordinary host
/// operation, through the same append and receipt paths production uses.
fn store_bound(
    store: &mut SqliteStore,
    input: &StoredBind,
    second: i64,
) -> VerificationBindReceipt {
    let project = crate::domain::ProjectId("project-a".into());
    let session = SessionId(HOLDER.into());
    let source = stored(store, &input.original);
    let original_position =
        run_feed_position_for_object_on(&store.connection, source.binding.run_id, &input.original)
            .expect("the original's position")
            .position;
    let transaction = store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("transaction");
    let mut bound = Vec::with_capacity(input.targets.len());
    for target in &input.targets {
        let binding = target.binding.clone();
        let cut = current_run_feed_cut_on(&transaction, binding.run_id)
            .expect("cut")
            .position;
        let root = named_root_at_cut_on(&transaction, binding.run_id, binding.claim_id, cut)
            .expect("root")
            .expect("the target's named root");
        let mut binder = actor(HOLDER);
        binder.session_id = Some(session.clone());
        binder.run_id = Some(binding.run_id.0.to_string());
        let evidence = VerificationEvidence {
            applicability: source.applicability.clone(),
            schema_version: SCHEMA_VERSION,
            project_id: source.project_id.clone(),
            binding: binding.clone(),
            session_id: session.clone(),
            producer_observation: source.producer_observation.clone(),
            source_basis: ExecutionSourceBasis {
                git_tree: root
                    .latest_sighting
                    .as_ref()
                    .and_then(|(_, sighting)| sighting.source_basis.as_ref())
                    .and_then(crate::domain::VerificationSourceBasis::legacy_basis)
                    .and_then(|sighting| sighting.git_tree),
                workspace_id: source.source_basis.workspace_id.clone(),
                source_revision: source
                    .source_basis
                    .legacy_revision()
                    .expect("a bound original holds a revision")
                    .to_owned(),
                source_root_generation: Some(root.generation),
                source_root_state: Some(SourceRootState::Named),
            }
            .into(),
            environment: source.environment.clone(),
            check_kind: source.check_kind,
            check_fingerprint: source.check_fingerprint.clone(),
            result: source.result,
            completed_at: source.completed_at,
            summary: source.summary.clone(),
            refs: source.refs.clone(),
            actor: binder,
            recorded_at: at(second),
            bound_from: Some(BoundVerificationSource {
                verification: input.original.clone(),
                work_id: source.binding.work_id,
                run_id: source.binding.run_id,
                original_position,
                sighting: target.sighting.observation.clone(),
                measurement: input.measurement.clone(),
                criteria: target.criteria.clone(),
            }),
        };
        let verification =
            append_control_verification_evidence_on(&transaction, &evidence).expect("bound record");
        let run_position =
            run_feed_position_for_object_on(&transaction, binding.run_id, &verification)
                .expect("position")
                .position;
        let obligations_satisfied = transaction
            .prepare(
                "SELECT rule_id FROM work_run_obligations
                 WHERE run_id = ?1 AND evidence_id = ?2
                 ORDER BY trigger_position, obligation_id",
            )
            .expect("prepare")
            .query_map(
                rusqlite::params![binding.run_id.0.to_string(), verification.as_str()],
                |row| row.get::<_, String>(0),
            )
            .expect("query")
            .collect::<Result<Vec<_>, _>>()
            .expect("rules");
        let item =
            super::super::query::load_work_item(&transaction, binding.work_id).expect("item");
        bound.push(BoundVerificationTarget {
            work_id: binding.work_id,
            work_ref: item.short_ref.clone(),
            run_id: binding.run_id,
            verification,
            run_position,
            obligations_satisfied,
            criteria: target
                .criteria
                .iter()
                .map(|position| stored_eligibility(&item, *position, &evidence))
                .collect(),
        });
    }
    let receipt = VerificationBindReceipt {
        original: input.original.clone(),
        replayed: false,
        bound,
    };
    crate::host::id_contract::assert_host_ids(&receipt);
    let intent = CanonicalObject::freeze(&VerificationBindIntent::historical(
        &project,
        &session,
        &input.idempotency_key,
        &input.original,
        &input.measurement,
        &input.targets,
    ))
    .expect("intent");
    crate::storage::HostSessionAuthority::Work
        .persist(
            &transaction,
            &project,
            &session,
            VERIFICATION_BIND_OPERATION,
            &input.idempotency_key,
            &intent,
            &receipt,
            at(second),
        )
        .expect("the stored receipt");
    transaction.commit().expect("commit");
    receipt
}

/// What a stored receipt says of one intended criterion: whether the check
/// suited its binding, never a verdict.
fn stored_eligibility(
    item: &WorkItem,
    position: u32,
    evidence: &VerificationEvidence,
) -> BoundCriterionEligibility {
    let binding = item
        .acceptance_bindings
        .iter()
        .find(|binding| {
            usize::try_from(position).is_ok_and(|position| binding.criterion == position)
        })
        .map(|binding| binding.requirement.clone());
    let reason = binding.as_ref().and_then(|requirement| {
        if requirement.check_kind != evidence.check_kind {
            Some(format!(
                "the criterion binds {:?} checks; this check is {:?}",
                requirement.check_kind, evidence.check_kind
            ))
        } else if requirement
            .check_fingerprint
            .as_ref()
            .is_some_and(|pin| pin != &evidence.check_fingerprint)
        {
            Some("the criterion pins another check".to_owned())
        } else {
            None
        }
    });
    BoundCriterionEligibility {
        position,
        binding,
        eligible: reason.is_none(),
        reason,
    }
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

/// A bound record of another kind leaves a kind-bound criterion open and
/// still satisfies the source change.
#[test]
fn a_kind_mismatch_leaves_the_criterion_open() {
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
    let input = request(&original, "R1", vec![target(&store, &b, "R1", vec![1])]);
    store_bound(&mut store, &input, 30);
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
    let input = request(&original, "R1", vec![target(&store, &b, "R1", vec![])]);
    store_bound(&mut store, &input, 30);
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
    let input = request(&original, "R1", vec![target(&store, &b, "R1", vec![])]);
    let receipt = store_bound(&mut store, &input, 30);
    let states = obligations(&store, &b);
    let latest = states.last().expect("the latest change's obligation");
    assert_eq!(latest.1, WorkObligationState::Satisfied, "{states:?}");
    assert_eq!(
        latest.2.as_deref(),
        Some(receipt.bound[0].verification.as_str())
    );
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
        source_basis: declared
            .map(|revision| crate::domain::AcceptanceSourceBasis::legacy(None, revision.into())),
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
        let input = request(&original, "R1", vec![target(&store, &b, "R1", vec![1])]);
        let receipt = store_bound(&mut store, &input, 30);
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

fn binding_page(store: &SqliteStore, held: &Held) -> crate::domain::AcceptanceBindingPage {
    let work = store.get_work_item(held.work.work_id).expect("item");
    super::super::binding_read::read_acceptance_bindings_on(
        &store.connection,
        &crate::domain::ProjectId("project-a".into()),
        &super::super::binding_read::BindingReadRequest {
            work_id: work.work_id,
            expected_work_revision: work.revision,
            run_id: held.claim.run_id,
            after: None,
        },
    )
    .expect("binding read")
}

/// The verification that satisfied the page's first criterion.
fn satisfying(
    page: &crate::domain::AcceptanceBindingPage,
) -> crate::domain::AcceptanceBindingVerification {
    page.rows[0]
        .binding
        .as_ref()
        .and_then(|binding| binding.obligation.as_ref())
        .and_then(|obligation| obligation.resolution.as_ref())
        .and_then(|resolution| resolution.satisfaction.as_ref())
        .map(|satisfaction| satisfaction.verification.clone())
        .expect("criterion 1 is satisfied")
}

fn verification_page(
    store: &SqliteStore,
    held: &Held,
) -> crate::domain::AcceptanceVerificationPage {
    let work = store.get_work_item(held.work.work_id).expect("item");
    super::super::verification_read::read_acceptance_verifications_on(
        &store.connection,
        &crate::domain::ProjectId("project-a".into()),
        &super::super::verification_read::VerificationReadRequest {
            work_id: work.work_id,
            expected_work_revision: work.revision,
            run_id: held.claim.run_id,
            run_cut: run_head(store, held),
            criterion: 1,
            after: None,
        },
    )
    .expect("verification read")
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
    let input = request(&original, "R1", vec![target(&store, &b, "R1", vec![1])]);
    let receipt = store_bound(&mut store, &input, 30);
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
    let read = satisfying(&binding_page(&store, &b));
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
    assert_eq!(
        bound.original_basis,
        original_record.source_basis.legacy_basis().unwrap()
    );
    assert_eq!(bound.original_completed_at, at(20));
    assert_eq!(bound.bound_at, at(30));
    assert_eq!(bound.binder.actor_id, actor(HOLDER).actor_id);
    assert_eq!(bound.sighting, b.change);
    assert_eq!(bound.measurement.source_revision, "R1");
    assert_eq!(bound.criteria, vec![1]);

    let rows = verification_page(&store, &b).rows;
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0].record, bound_id);
    assert_eq!(rows[0].producer.position, producer_on_a);
    assert!(rows[0].bound.is_some());

    // The native original keeps its exact shape.
    let native = satisfying(&binding_page(&store, &a));
    assert_eq!(native.record, original);
    assert!(native.bound.is_none());
    let wire = serde_json::to_value(&native).expect("json");
    assert!(wire.get("bound").is_none(), "{wire}");
    let native_rows = verification_page(&store, &a).rows;
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
    let receipt = store_bound(&mut store, &input, 30);
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

/// The stored bind operations of a store, as a retry or doctor reads them.
fn bind_operations(connection: &rusqlite::Connection) -> Vec<(String, String, Vec<u8>)> {
    connection
        .prepare(
            "SELECT idempotency_key, request_hash, result_json FROM work_operation_results
             WHERE operation = 'host_verification_bind' ORDER BY idempotency_key",
        )
        .expect("prepare")
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .expect("query")
        .map(|row| row.expect("operation"))
        .collect()
}

/// Projection repair rebuilds every derived row of `database` exactly, and a
/// migration export then import carries every record's canonical bytes and
/// every stored bind operation unchanged into a clean store, which it returns.
fn repair_and_migrate(directory: &std::path::Path, database: &std::path::Path) -> SqliteStore {
    let before = all_rows(&rusqlite::Connection::open(database).expect("open"));
    let report = SqliteStore::repair_rebuildable_projections(database).expect("repair");
    assert!(report.is_healthy(), "{report:?}");
    let after = all_rows(&rusqlite::Connection::open(database).expect("open"));
    assert_eq!(after, before, "repair rebuilt every row exactly");

    let file = directory.join("export.jsonl");
    crate::storage::migration::export_json(database, &file).expect("export");
    let imported = directory.join("imported.db");
    crate::storage::migration::import_json(&file, &imported).expect("import");
    let store = SqliteStore::open(&imported).expect("imported store");
    let report = store.verify_all().expect("doctor");
    assert!(report.is_healthy(), "{report:?}");
    let source = rusqlite::Connection::open(database).expect("open");
    assert_eq!(
        objects(&store.connection),
        objects(&source),
        "every record's canonical bytes travel unchanged"
    );
    assert_eq!(
        bind_operations(&store.connection),
        bind_operations(&source),
        "every stored bind operation travels unchanged"
    );
    store
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
                let input = request(&original, "R1", vec![target(&store, &b, "R1", vec![1])]);
                let receipt = store_bound(&mut store, &input, 30);
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
        let store = repair_and_migrate(directory.path(), &database);
        assert_eq!(!bind_operations(&store.connection).is_empty(), with_bound);
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

/// A bound record of a criterion pinned to another check leaves that
/// criterion's obligation open and still satisfies the source change.
#[test]
fn a_pin_mismatch_leaves_the_criterion_open() {
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
    let input = request(&original, "R1", vec![target(&store, &b, "R1", vec![1])]);
    store_bound(&mut store, &input, 30);
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

/// Doctor holds a bound record's sighting to what the bind required: a
/// record whose `bound_from.sighting` names another observation, here the
/// original's producer on another run, is reported.
#[test]
fn doctor_reports_a_bound_record_naming_another_sighting() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let a = held(&mut store, "item-a", HOLDER, 9, "R1", 2);
    let b = held(&mut store, "item-b", HOLDER, 10, "R1", 4);
    let original = native_check(&mut store, &a, "R1", 20);
    let input = request(&original, "R1", vec![target(&store, &b, "R1", vec![])]);
    let receipt = store_bound(&mut store, &input, 30);
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
    let receipt = store_bound(&mut store, &input, 30);
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

/// Report another held item's source without rebinding an active host turn.
fn unadmitted_change_through_host(
    store: &mut SqliteStore,
    held: &Held,
    host: &crate::storage::test_support::TestWorkHost,
    from: &str,
    to: &str,
    second: i64,
) -> crate::domain::ExecutionObservationReceipt {
    use crate::domain::{
        ExecutionObserveInput, MeasuredBaseline, MeasuredSighting, NamedRootState,
        ObservationCausality, ObservationPolicyBasis, ObservationRootBasis, ObservedInterval,
        ObservedOccurrence, ObservedSourceChange,
    };
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
                    identity: crate::domain::VerificationSourceIdentity::Legacy {
                        source_revision: from.into(),
                        git_tree: None,
                    },
                    observed_at: at(second - 2),
                },
                sighting: MeasuredSighting {
                    source_basis: (basis(to, held.generation)).into(),
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
    store
        .seed_historical_execution_observation(
            &held.work.project_id,
            &host.session_id,
            &host.connection_token,
            crate::storage::HostSessionAuthority::Work,
            &observer,
            input,
            at(second),
        )
        .expect("accounted unadmitted change")
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
