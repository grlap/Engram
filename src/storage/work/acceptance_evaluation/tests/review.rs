//! Regression cases from the first read-only review pair, written before the
//! corrections they discriminate.

use super::*;
use crate::domain::{
    AcceptWorkHandoffRequest, ControlPolicy, EvaluatorModel, MAX_EXECUTION_IDENTITY_BYTES,
    OfferWorkHandoffRequest, ReopenWorkRequest, WorkEvent, WorkTransition,
};

fn pass_judgment(note: &ObjectHash) -> Vec<CriterionVerdictInput> {
    vec![verdict(
        1,
        AcceptanceVerdict::Pass,
        AcceptanceBasis::Judgment,
        std::slice::from_ref(note),
    )]
}

/// Same-run handoff from the live holder to `to`; returns the new claim.
fn handoff(
    store: &mut SqliteStore,
    work: &WorkItem,
    from: &WorkClaim,
    to: &str,
    second: i64,
) -> WorkClaim {
    let offer = store
        .offer_work_handoff(
            &OfferWorkHandoffRequest {
                work_id: work.work_id,
                run_id: from.run_id,
                expected_work_revision: work.revision,
                from: from.holder.clone(),
                to: SessionId(to.into()),
                claim_id: from.claim_id,
                claim_fence: from.fence,
                ttl_seconds: 300,
                checkpoint_summary: format!("handing the run to {to}"),
                actor: actor(&from.holder.0),
                idempotency_key: format!("offer-to-{to}-{second}"),
                offered_at: at(second),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("offer handoff");
    store
        .accept_work_handoff(
            &AcceptWorkHandoffRequest {
                work_id: work.work_id,
                offer_id: offer.offer_id,
                to: SessionId(to.into()),
                actor: actor(to),
                idempotency_key: format!("accept-as-{to}-{second}"),
                accepted_at: at(second + 1),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("accept handoff")
}

/// Re-freezes `seal` as `forged` in place. The seal hash is named by the
/// immutable completion event and the run projection, so both are
/// re-frozen too: every hash is recomputed and only the forged relationship
/// is wrong.
fn refreeze_seal(store: &SqliteStore, seal: &CompletionSeal, forged: &CompletionSeal) {
    let original = CanonicalObject::freeze(seal).expect("freeze seal");
    let forged_object = CanonicalObject::freeze(forged).expect("freeze forged seal");
    let event_hash: String = store
        .connection
        .query_row(
            "SELECT object_hash FROM work_feed_entries
             WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND object_kind = 'work_event'
             ORDER BY position DESC LIMIT 1",
            [seal.run_id.0.to_string()],
            |row| row.get(0),
        )
        .expect("completion event");
    let event_hash = ObjectHash::from_stored(event_hash).expect("stored hash");
    let mut event: WorkEvent = store
        .get(&event_hash)
        .expect("read event")
        .expect("canonical event");
    assert!(
        matches!(&event.transition, WorkTransition::Completed { seal } if seal == original.hash()),
        "{:?}",
        event.transition
    );
    event.transition = WorkTransition::Completed {
        seal: forged_object.hash().clone(),
    };
    if let Some(run) = &mut event.run {
        run.completion_seal = Some(forged_object.hash().clone());
    }
    let forged_event = CanonicalObject::freeze(&event).expect("freeze forged event");
    for (object, source) in [
        (&forged_object, original.hash()),
        (&forged_event, &event_hash),
    ] {
        store
            .connection
            .execute(
                "INSERT INTO objects (object_hash, object_kind, canonical_json, created_at)
                 SELECT ?1, object_kind, ?2, created_at FROM objects WHERE object_hash = ?3",
                params![object.hash().as_str(), object.bytes(), source.as_str()],
            )
            .expect("insert re-frozen object");
    }
    store
        .connection
        .execute(
            "UPDATE work_completion_seals SET seal_hash = ?1, seal_json = ?2 WHERE seal_hash = ?3",
            params![
                forged_object.hash().as_str(),
                forged_object.bytes(),
                original.hash().as_str()
            ],
        )
        .expect("repoint the seal projection");
    for (new, old) in [
        (forged_object.hash(), original.hash()),
        (forged_event.hash(), &event_hash),
    ] {
        store
            .connection
            .execute(
                "UPDATE work_feed_entries SET object_hash = ?1 WHERE object_hash = ?2",
                params![new.as_str(), old.as_str()],
            )
            .expect("repoint the feed entries");
    }
    store
        .connection
        .execute(
            "UPDATE work_runs
             SET completion_seal_hash = ?1,
                 run_json = CAST(replace(CAST(run_json AS TEXT), ?2, ?1) AS BLOB)
             WHERE run_id = ?3",
            params![
                forged_object.hash().as_str(),
                original.hash().as_str(),
                seal.run_id.0.to_string()
            ],
        )
        .expect("repoint the run projection");
    store
        .connection
        .execute(
            "UPDATE work_items SET latest_event_hash = ?1 WHERE latest_event_hash = ?2",
            params![forged_event.hash().as_str(), event_hash.as_str()],
        )
        .expect("repoint the item's latest event");
}

// Review 1 (High): an independent evaluator that later becomes the holder,
// by handoff or by recovery, must not consume its own earlier pass; the
// relationship is rechecked when the evaluation is consumed.
#[test]
fn an_independent_evaluator_who_becomes_the_holder_cannot_consume_its_own_pass() {
    // Handoff variant.
    let mut fx = fixture("project-evaluation-consumption-handoff");
    let store = &mut fx.store;
    enable(
        store,
        &[Mode::SameSession, Mode::IndependentSession],
        MechanicalBasis::Asserted,
        false,
        "enable-consumption",
        5,
    );
    let work = fx.work.clone();
    let runner = fx.claim.clone();
    let note = fx.evidence.clone();
    record(
        store,
        &request(
            &work,
            cut(store, &work),
            "second",
            Mode::IndependentSession,
            pass_judgment(&note),
            6,
        ),
    )
    .expect("second evaluates independently while runner holds the run");
    let second = handoff(store, &work, &runner, "second", 7);
    let work = store
        .get_work_item(work.work_id)
        .expect("item after handoff");
    let cause = recovery_cause(checkpoint_then_complete(
        store,
        &work,
        &second,
        "second",
        std::slice::from_ref(&note),
        false,
        None,
        "complete-as-evaluator",
        9,
    ));
    assert!(
        matches!(
            cause,
            WorkCompletionRecoveryCause::AcceptanceEvaluationStale {
                reason: AcceptanceStaleReason::Identity
            }
        ),
        "{cause:?}"
    );
    // A genuinely independent record then lets the new holder complete.
    let reviewer = record(
        store,
        &request(
            &work,
            cut(store, &work),
            "reviewer",
            Mode::IndependentSession,
            pass_judgment(&note),
            10,
        ),
    )
    .expect("a never-holding reviewer evaluates independently");
    let seal = checkpoint_then_complete(
        store,
        &work,
        &second,
        "second",
        std::slice::from_ref(&note),
        false,
        None,
        "complete-on-reviewer",
        11,
    )
    .expect("the holder completes on an independent pass");
    assert_eq!(seal.acceptance_evaluation, Some(reviewer.evaluation));

    // Recovery variant.
    let mut fx = fixture("project-evaluation-consumption-recovery");
    let store = &mut fx.store;
    enable(
        store,
        &[Mode::SameSession, Mode::IndependentSession],
        MechanicalBasis::Asserted,
        false,
        "enable-consumption",
        5,
    );
    let work = fx.work.clone();
    let note = fx.evidence.clone();
    record(
        store,
        &request(
            &work,
            cut(store, &work),
            "second",
            Mode::IndependentSession,
            pass_judgment(&note),
            6,
        ),
    )
    .expect("second evaluates independently before recovering the run");
    let recovered = claim(store, &work, "second", "recover-as-second", 4_000, 3_600);
    let work = store
        .get_work_item(work.work_id)
        .expect("item after recovery");
    let cause = recovery_cause(checkpoint_then_complete(
        store,
        &work,
        &recovered,
        "second",
        std::slice::from_ref(&note),
        false,
        None,
        "complete-after-recovery",
        4_001,
    ));
    assert!(
        matches!(
            cause,
            WorkCompletionRecoveryCause::AcceptanceEvaluationStale {
                reason: AcceptanceStaleReason::Identity
            }
        ),
        "{cause:?}"
    );

    // Positive controls: the original holder completes on the independent
    // pass, and holding a sibling child run under the same root does not
    // taint independence for the parent's run.
    let mut fx = fixture("project-evaluation-consumption-control");
    let store = &mut fx.store;
    enable(
        store,
        &[Mode::SameSession, Mode::IndependentSession],
        MechanicalBasis::Asserted,
        false,
        "enable-consumption",
        5,
    );
    let work = fx.work.clone();
    let runner = fx.claim.clone();
    let note = fx.evidence.clone();
    // The holder decomposes its own item; the child's run belongs to the
    // same root execution.
    let decomposition = store
        .decompose_work(
            &crate::domain::DecomposeWorkRequest {
                parent_id: work.work_id,
                expected_parent_revision: work.revision,
                children: vec![child(
                    "sibling",
                    crate::domain::ChildRequirement::Optional,
                    "Sibling child run",
                )],
                prerequisites: vec![],
                authority: WorkPlanningAuthority::Claim {
                    run_id: runner.run_id,
                    holder: runner.holder.clone(),
                    claim_id: runner.claim_id,
                    claim_fence: runner.fence,
                },
                actor: actor("runner"),
                idempotency_key: "decompose-sibling".into(),
                created_at: at(6),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("a child under the same root");
    let work = decomposition.parent;
    let sibling = decomposition.children[0].clone();
    assert_eq!(sibling.root_id, work.root_id);
    let sibling_claim = claim(store, &sibling, "second", "claim-sibling", 6, 3_600);
    assert_ne!(sibling_claim.run_id, runner.run_id);
    let independent = record(
        store,
        &request(
            &work,
            cut(store, &work),
            "second",
            Mode::IndependentSession,
            pass_judgment(&note),
            7,
        ),
    )
    .expect("holding a sibling child run keeps second independent for this run");
    assert_eq!(
        store
            .acceptance_evaluation_status(work.work_id, None)
            .expect("status read")
            .expect("newest evaluation")
            .stale,
        None
    );
    // The sibling finishes first (its holder evaluates it in same_session),
    // so the root's completion owes no live descendant claim.
    let sibling_note = evidence(
        store,
        &sibling,
        &sibling_claim,
        "second",
        "sibling-evidence",
        8,
    );
    record(
        store,
        &request(
            &sibling,
            cut(store, &sibling),
            "second",
            Mode::SameSession,
            pass_judgment(&sibling_note),
            9,
        ),
    )
    .expect("second evaluates its own child in same_session");
    checkpoint_then_complete(
        store,
        &sibling,
        &sibling_claim,
        "second",
        std::slice::from_ref(&sibling_note),
        false,
        None,
        "complete-sibling",
        10,
    )
    .expect("the sibling child completes");
    let seal = checkpoint_then_complete(
        store,
        &work,
        &runner,
        "runner",
        std::slice::from_ref(&note),
        false,
        None,
        "complete-as-runner",
        11,
    )
    .expect("the holder completes on the independent pass");
    assert_eq!(seal.acceptance_evaluation, Some(independent.evaluation));
}

// Review 3 (Medium): doctor validates the seal-to-evaluation binding. The
// forged seal is re-frozen so only the relationship is wrong.
#[test]
fn doctor_reports_a_seal_bound_to_the_wrong_evaluation() {
    let mut fx = fixture("project-evaluation-seal-binding");
    let store = &mut fx.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable-seal-binding",
        5,
    );
    let work = fx.work.clone();
    let claim_a = fx.claim.clone();
    let note = fx.evidence.clone();
    record(
        store,
        &request(
            &work,
            cut(store, &work),
            "runner",
            Mode::SameSession,
            pass_judgment(&note),
            6,
        ),
    )
    .expect("record the consumed evaluation");
    let seal = checkpoint_then_complete(
        store,
        &work,
        &claim_a,
        "runner",
        std::slice::from_ref(&note),
        false,
        None,
        "complete-bound",
        7,
    )
    .expect("seal");
    // Positive control: an evaluated completion is healthy end to end.
    let healthy = store.verify_all().expect("scan");
    assert!(healthy.is_healthy(), "{healthy:?}");

    // An evaluation on another item's run to point the forged seal at.
    let other = store
        .create_work(
            &root_request("project-evaluation-seal-binding", "create-other", 10),
            &DevelopmentNoopRedactor,
        )
        .expect("another root");
    let other_claim = claim(store, &other, "runner", "claim-other", 11, 3_600);
    let other_note = evidence(store, &other, &other_claim, "runner", "other-evidence", 12);
    let other_evaluation = record(
        store,
        &request(
            &other,
            cut(store, &other),
            "runner",
            Mode::SameSession,
            pass_judgment(&other_note),
            13,
        ),
    )
    .expect("evaluation on the other run");

    // Forge the seal to bind the other run's evaluation; the re-freeze keeps
    // every hash consistent so only the seal-to-evaluation relationship is
    // wrong.
    let mut forged = seal.clone();
    forged.acceptance_evaluation = Some(other_evaluation.evaluation);
    refreeze_seal(store, &seal, &forged);
    let report = store.verify_all().expect("scan");
    assert!(
        report
            .invalid_work_records
            .iter()
            .any(|record| record.contains("acceptance_evaluation_binding")),
        "{report:?}"
    );
}

// Review 4 (Medium): a policy version must agree with its authority decision
// on the acceptance-evaluation settings.
#[test]
fn policy_history_refuses_an_authority_that_disagrees_on_acceptance_evaluation() {
    let mut fx = fixture("project-evaluation-authority");
    let database = fx.directory.path().join("engram.sqlite3");
    let store = &mut fx.store;
    let receipt = enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable-authority",
        5,
    );
    let original_hash = receipt.active_policy.clone();
    // Positive control: an enabled policy is healthy, including its audited
    // operation receipt.
    let healthy = store.verify_all().expect("scan");
    assert!(healthy.is_healthy(), "{healthy:?}");
    let stored: Vec<u8> = store
        .connection
        .query_row(
            "SELECT policy_json FROM control_policy_versions WHERE policy_hash = ?1",
            [original_hash.as_str()],
            |row| row.get(0),
        )
        .expect("policy bytes");
    let mut policy: ControlPolicy = serde_json::from_slice(&stored).expect("decode policy");
    policy.acceptance_evaluation.allowed_modes = vec![Mode::IndependentSession];
    let forged = CanonicalObject::freeze(&policy).expect("freeze forged policy");
    store
        .connection
        .execute(
            "INSERT INTO objects (object_hash, object_kind, canonical_json, created_at)
             SELECT ?1, object_kind, ?2, created_at FROM objects WHERE object_hash = ?3",
            params![
                forged.hash().as_str(),
                forged.bytes(),
                original_hash.as_str()
            ],
        )
        .expect("insert forged policy object");
    store
        .connection
        .execute(
            "UPDATE control_policy_versions SET policy_hash = ?1, policy_json = ?2 WHERE policy_hash = ?3",
            params![forged.hash().as_str(), forged.bytes(), original_hash.as_str()],
        )
        .expect("repoint the policy version");
    store
        .connection
        .execute(
            "UPDATE control_policy_state SET policy_hash = ?1 WHERE policy_hash = ?2",
            params![forged.hash().as_str(), original_hash.as_str()],
        )
        .expect("repoint the policy head");
    store
        .connection
        .execute(
            "DELETE FROM objects WHERE object_hash = ?1",
            [original_hash.as_str()],
        )
        .expect("remove the original policy object");
    // The version loader names the disagreement; doctor reports the version
    // and the head that rests on it, and nothing else.
    let error = SqliteStore::load_control_policy_version(&store.connection, forged.hash())
        .expect_err("a policy that disagrees with its authority does not load");
    assert!(
        error.to_string().contains("authority is invalid"),
        "{error}"
    );
    let report = store.verify_all().expect("scan");
    assert!(
        report
            .invalid_control_records
            .contains(&format!("control_policy_version:{}", forged.hash())),
        "{report:?}"
    );
    assert!(
        report
            .invalid_control_records
            .iter()
            .all(|record| !record.starts_with("control_policy_operation:")),
        "the audited operation receipt is untouched: {report:?}"
    );
    // Policy-history verification on open refuses the same store.
    let reopened = SqliteStore::open(&database)
        .err()
        .map(|error| error.to_string());
    assert!(
        reopened.is_some(),
        "a store whose policy history disagrees with its authority must not open"
    );
}

// A shared validator, one case per relationship it checks.
#[test]
fn seal_evaluation_binding_is_validated_per_relationship() {
    fn binding(
        store: &SqliteStore,
        seal: &CompletionSeal,
    ) -> Result<Option<crate::domain::AcceptanceEvaluation>, StoreError> {
        crate::storage::work::acceptance_evaluation::validate_completion_seal_acceptance_evaluation_on(
            &store.connection,
            seal,
        )
    }
    fn invalid_reason(
        result: Result<Option<crate::domain::AcceptanceEvaluation>, StoreError>,
    ) -> String {
        match result {
            Err(StoreError::InvalidWorkProjection(reason)) => reason,
            other => panic!("expected an invalid projection, got {other:?}"),
        }
    }
    let mut fx = fixture("project-evaluation-binding-cases");
    let store = &mut fx.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable-binding-cases",
        5,
    );
    let work = fx.work.clone();
    let claim_a = fx.claim.clone();
    let note = fx.evidence.clone();
    let earlier_pass = record(
        store,
        &request(
            &work,
            cut(store, &work),
            "runner",
            Mode::SameSession,
            pass_judgment(&note),
            6,
        ),
    )
    .expect("an earlier passing record on the same run");
    let failing = record(
        store,
        &request(
            &work,
            cut(store, &work),
            "runner",
            Mode::SameSession,
            vec![verdict(
                1,
                AcceptanceVerdict::Fail,
                AcceptanceBasis::Judgment,
                std::slice::from_ref(&note),
            )],
            6,
        ),
    )
    .expect("a failing record on the same run");
    let passing = record(
        store,
        &request(
            &work,
            cut(store, &work),
            "runner",
            Mode::SameSession,
            pass_judgment(&note),
            7,
        ),
    )
    .expect("the passing record");
    let seal = checkpoint_then_complete(
        store,
        &work,
        &claim_a,
        "runner",
        std::slice::from_ref(&note),
        false,
        None,
        "complete-binding-cases",
        8,
    )
    .expect("seal");

    // Valid: the bound evaluation comes back.
    assert_eq!(
        binding(store, &seal)
            .expect("valid binding")
            .map(|evaluation| evaluation.attempt_key),
        Some(passing.record.attempt_key.clone())
    );
    // Legacy: no binding to check.
    let mut legacy = seal.clone();
    legacy.acceptance_evaluation = None;
    assert!(binding(store, &legacy).expect("legacy seal").is_none());
    // Missing object.
    let mut missing = seal.clone();
    missing.acceptance_evaluation = Some(ObjectHash::from_canonical_bytes(b"no such evaluation"));
    assert!(binding(store, &missing).is_err());
    // Another work item or run.
    let other = store
        .create_work(
            &root_request("project-evaluation-binding-cases", "create-other", 10),
            &DevelopmentNoopRedactor,
        )
        .expect("another root");
    let other_claim = claim(store, &other, "runner", "claim-other", 11, 3_600);
    let other_note = evidence(store, &other, &other_claim, "runner", "other-evidence", 12);
    let other_evaluation = record(
        store,
        &request(
            &other,
            cut(store, &other),
            "runner",
            Mode::SameSession,
            pass_judgment(&other_note),
            13,
        ),
    )
    .expect("evaluation on the other run");
    let mut foreign = seal.clone();
    foreign.acceptance_evaluation = Some(other_evaluation.evaluation);
    assert!(
        invalid_reason(binding(store, &foreign)).contains("another work item or run"),
        "{:?}",
        binding(store, &foreign)
    );
    // After the completion cut: the record sits one position past the cut it
    // named, so a seal cut at that position excludes it.
    let mut early = seal.clone();
    early.completion_cut.position = passing.record.evaluated_cut.position;
    assert!(invalid_reason(binding(store, &early)).contains("at or before the completion cut"));
    // A non-passing evaluation on the same run.
    let mut non_pass = seal.clone();
    non_pass.acceptance_evaluation = Some(failing.evaluation);
    assert!(invalid_reason(binding(store, &non_pass)).contains("does not pass every criterion"));
    // An older passing record while a newer one sits before the cut: it
    // passes and derives the same vector, so only newest-at-cut refuses it.
    let mut older = seal.clone();
    older.acceptance_evaluation = Some(earlier_pass.evaluation);
    assert!(invalid_reason(binding(store, &older)).contains("newest evaluation"));
    // A sealed vector that the evaluation does not derive.
    let mut mismatch = seal.clone();
    mismatch.acceptance[0].note.push_str(" (edited)");
    assert!(
        invalid_reason(binding(store, &mismatch))
            .contains("does not derive the sealed acceptance vector")
    );
}

// Host-minted verification evidence is cited by its full hash through the
// word; no locator window prints it.
#[test]
fn the_word_accepts_full_hashes_of_host_minted_verification_evidence() {
    let mut fx = fixture("project-evaluation-typed-citation");
    let database = fx.directory.path().join("engram.sqlite3");
    let store = &mut fx.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Observed,
        false,
        "enable-typed-citation",
        5,
    );
    disable_obligation_rules(store, 6);
    let work = fx.work.clone();
    let claim = fx.claim.clone();
    let mut host = HostSession::bind(store, &work, &claim, 10);
    let passed = host.checkpoint(
        store,
        false,
        Some((VerificationKind::Build, ExecutionOutcome::Succeeded)),
        20,
    );
    let evidence_basis = cut(store, &work);
    let verbs = crate::AgentVerbs::new(
        database,
        work.project_id.clone(),
        "runner".into(),
        SessionId("runner".into()),
        None,
    );
    let recorded = verbs
        .evaluate(
            crate::EvaluateInput {
                work_ref: Some(work.work_id.0.to_string()),
                mode: "same_session".into(),
                acceptance_basis: work.revision,
                evidence_basis,
                verdicts: vec![crate::WorkCriterionVerdictInput {
                    criterion: 1,
                    verdict: "pass".into(),
                    basis: "observed".into(),
                    rationale: "the observed build passed".into(),
                    evidence: vec![passed[0].as_str().to_owned()],
                }],
                attempt: None,
                source_fingerprint: None,
                model: None,
                execution_identity: None,
                parent_session: None,
            },
            at(25),
        )
        .expect("a typed evidence hash resolves through the word");
    assert_eq!(recorded.value["evaluation"]["passed"], 1);
}

// Review 8 (Low): evaluator model bounds hold at the core write boundary.
#[test]
fn oversized_evaluator_model_segments_are_refused_at_the_store() {
    let mut fx = fixture("project-evaluation-model-bounds");
    let store = &mut fx.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable-model-bounds",
        5,
    );
    let work = fx.work.clone();
    let note = fx.evidence.clone();
    let head_before = cut(store, &work);
    for model in [
        EvaluatorModel {
            provider: "p".repeat(129),
            model: "m".into(),
            version: None,
        },
        EvaluatorModel {
            provider: "provider".into(),
            model: String::new(),
            version: None,
        },
        EvaluatorModel {
            provider: "provider".into(),
            model: "model".into(),
            version: Some("v\u{0007}".into()),
        },
    ] {
        let reason = refusal(record(
            store,
            &RecordAcceptanceEvaluationRequest {
                evaluator_model: Some(model),
                ..request(
                    &work,
                    head_before,
                    "runner",
                    Mode::SameSession,
                    pass_judgment(&note),
                    6,
                )
            },
        ));
        assert!(reason.contains("model"), "{reason}");
        assert_eq!(cut(store, &work), head_before);
    }
}

// Round 4 (Low): sub-agent identity fields are bounded where the record is
// admitted; a refusal leaves the run feed untouched and the edges are valid.
#[test]
fn sub_agent_identity_fields_are_bounded_at_the_store() {
    let mut fx = fixture("project-evaluation-identity-bounds");
    let store = &mut fx.store;
    enable(
        store,
        &[Mode::SubAgent],
        MechanicalBasis::Asserted,
        false,
        "enable-identity-bounds",
        5,
    );
    let work = fx.work.clone();
    let note = fx.evidence.clone();
    let head_before = cut(store, &work);
    // The evaluated cut is passed in so the closure borrows nothing mutable.
    let sub_agent =
        |execution_identity: String, parent: &str, evaluated_through: i64, second: i64| {
            RecordAcceptanceEvaluationRequest {
                execution_identity: Some(execution_identity),
                parent_session: Some(SessionId(parent.into())),
                ..request(
                    &work,
                    evaluated_through,
                    "sub-agent",
                    Mode::SubAgent,
                    pass_judgment(&note),
                    second,
                )
            }
        };
    for (label, request, word) in [
        (
            "oversized execution identity",
            sub_agent(
                "x".repeat(MAX_EXECUTION_IDENTITY_BYTES + 1),
                "runner",
                head_before,
                6,
            ),
            "execution identity",
        ),
        (
            "control character in the execution identity",
            sub_agent("worker\u{0007}".into(), "runner", head_before, 6),
            "execution identity",
        ),
        (
            "oversized parent session",
            sub_agent(
                "worker".into(),
                &"p".repeat(crate::MAX_SESSION_ID_BYTES + 1),
                head_before,
                6,
            ),
            "parent session",
        ),
    ] {
        let reason = refusal(record(store, &request));
        assert!(reason.contains(word), "{label}: {reason}");
        assert_eq!(cut(store, &work), head_before, "{label}");
    }
    // Valid edge for the execution identity.
    let admitted = record(
        store,
        &sub_agent(
            "x".repeat(MAX_EXECUTION_IDENTITY_BYTES),
            "runner",
            head_before,
            7,
        ),
    )
    .expect("an execution identity at the bound is admitted");
    assert_eq!(
        admitted.record.execution_identity.as_deref().map(str::len),
        Some(MAX_EXECUTION_IDENTITY_BYTES)
    );
    // Valid edge for the parent session: a holder with a 64-byte session id.
    let long_session = "p".repeat(crate::MAX_SESSION_ID_BYTES);
    let other = store
        .create_work(
            &root_request("project-evaluation-identity-bounds", "create-other", 8),
            &DevelopmentNoopRedactor,
        )
        .expect("another root");
    let other_claim = claim(store, &other, &long_session, "claim-other", 9, 3_600);
    let other_note = evidence(
        store,
        &other,
        &other_claim,
        &long_session,
        "other-evidence",
        10,
    );
    let admitted = record(
        store,
        &RecordAcceptanceEvaluationRequest {
            execution_identity: Some("worker".into()),
            parent_session: Some(SessionId(long_session.clone())),
            ..request(
                &other,
                cut(store, &other),
                "sub-agent",
                Mode::SubAgent,
                pass_judgment(&other_note),
                11,
            )
        },
    )
    .expect("a parent session at the live session bound is admitted");
    assert_eq!(
        admitted.record.parent_session,
        Some(SessionId(long_session))
    );
}

// Review 9 (Low): an empty mode list is the legacy policy whatever the other
// flags say; requested, stored, and read policies agree.
#[test]
fn legacy_policy_normalizes_its_other_fields_and_round_trips() {
    let mut fx = fixture("project-evaluation-legacy-normalization");
    let store = &mut fx.store;
    let flagged_legacy = AcceptanceEvaluationPolicy {
        allowed_modes: Vec::new(),
        mechanical_basis: MechanicalBasis::Observed,
        require_source_freshness: true,
    };
    let unchanged = store
        .set_acceptance_evaluation_policy(
            &flagged_legacy,
            &actor("policy-admin"),
            "legacy with stray flags",
            "legacy-with-flags",
            None,
            at(5),
            &DevelopmentNoopRedactor,
        )
        .expect("an empty mode list is the legacy policy");
    assert!(!unchanged.changed);
    assert_eq!(
        unchanged.acceptance_evaluation,
        AcceptanceEvaluationPolicy::default()
    );
    assert_eq!(
        store.acceptance_evaluation_policy().expect("read"),
        AcceptanceEvaluationPolicy::default()
    );

    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable-first",
        6,
    );
    let back = store
        .set_acceptance_evaluation_policy(
            &flagged_legacy,
            &actor("policy-admin"),
            "back to legacy with stray flags",
            "back-to-legacy",
            None,
            at(7),
            &DevelopmentNoopRedactor,
        )
        .expect("return to legacy");
    assert!(back.changed);
    assert_eq!(
        back.acceptance_evaluation,
        AcceptanceEvaluationPolicy::default()
    );
    assert_eq!(
        store.acceptance_evaluation_policy().expect("read"),
        AcceptanceEvaluationPolicy::default()
    );
    let stored: Vec<u8> = store
        .connection
        .query_row(
            "SELECT policy_json FROM control_policy_versions WHERE policy_hash = ?1",
            [back.active_policy.as_str()],
            |row| row.get(0),
        )
        .expect("policy bytes");
    assert!(
        !String::from_utf8_lossy(&stored).contains("acceptance_evaluation"),
        "legacy bytes must omit the field"
    );
}

// Review 11 (Low): an observed pass is classified from the canonical
// verification object, so a corrupted projection column cannot admit it.
#[test]
fn observed_passes_are_classified_from_canonical_verification_evidence() {
    let mut fx = fixture("project-evaluation-canonical-verification");
    let store = &mut fx.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Observed,
        false,
        "enable-canonical",
        5,
    );
    disable_obligation_rules(store, 6);
    let work = fx.work.clone();
    let claim = fx.claim.clone();
    let mut host = HostSession::bind(store, &work, &claim, 10);
    let failed = host.checkpoint(
        store,
        false,
        Some((VerificationKind::Build, ExecutionOutcome::Failed)),
        20,
    );
    store
        .connection
        .execute(
            "UPDATE work_run_evidence SET verification_result = 'passed' WHERE evidence_hash = ?1",
            [failed[0].as_str()],
        )
        .expect("corrupt the projection column");
    let result = record(
        store,
        &request(
            &work,
            cut(store, &work),
            "runner",
            Mode::SameSession,
            vec![verdict(
                1,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::Observed,
                &failed,
            )],
            25,
        ),
    );
    assert!(
        matches!(result, Err(StoreError::InvalidWorkProjection(_))),
        "{result:?}"
    );
}

// Round 6 (Low): the projection column must equal the canonical result's
// serialized word exactly; any disagreement, including a missing value,
// refuses the record without touching the feed.
#[test]
fn verification_projection_mismatches_refuse_exactly() {
    let mut fx = fixture("project-evaluation-exact-verification");
    let store = &mut fx.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Observed,
        false,
        "enable-exact-verification",
        5,
    );
    disable_obligation_rules(store, 6);
    let work = fx.work.clone();
    let claim = fx.claim.clone();
    let mut host = HostSession::bind(store, &work, &claim, 10);
    let failed = host.checkpoint(
        store,
        false,
        Some((VerificationKind::Build, ExecutionOutcome::Failed)),
        20,
    );
    let indeterminate = host.checkpoint(
        store,
        false,
        Some((VerificationKind::Build, ExecutionOutcome::Unknown)),
        30,
    );
    let fail_citing = |cut: i64, hash: &ObjectHash, second: i64| {
        request(
            &work,
            cut,
            "runner",
            Mode::SameSession,
            vec![verdict(
                1,
                AcceptanceVerdict::Fail,
                AcceptanceBasis::Judgment,
                std::slice::from_ref(hash),
            )],
            second,
        )
    };
    // A consistent record first, so each refusal below is checked against a
    // real newest evaluation. It is read directly: the status read would
    // itself refuse the corrupt citation.
    record(store, &fail_citing(cut(store, &work), &failed[0], 39))
        .expect("a consistent failed check backs a fail before any corruption");
    let newest = |store: &SqliteStore| {
        crate::storage::work::acceptance_evaluation::latest_on(&store.connection, claim.run_id)
            .expect("read the newest evaluation")
            .map(|(hash, _)| hash)
    };
    let head = cut(store, &work);
    let newest_before = newest(store);
    assert!(newest_before.is_some());
    for (label, hash, column) in [
        (
            "canonical failed, column indeterminate",
            &failed[0],
            Some("indeterminate"),
        ),
        ("canonical failed, column missing", &failed[0], None),
        (
            "canonical indeterminate, column failed",
            &indeterminate[0],
            Some("failed"),
        ),
    ] {
        store
            .connection
            .execute(
                "UPDATE work_run_evidence SET verification_result = ?1 WHERE evidence_hash = ?2",
                params![column, hash.as_str()],
            )
            .expect("set the projection column");
        let result = record(store, &fail_citing(head, hash, 40));
        assert!(
            matches!(result, Err(StoreError::InvalidWorkProjection(_))),
            "{label}: {result:?}"
        );
        assert_eq!(cut(store, &work), head, "{label}");
        assert_eq!(newest(store), newest_before, "{label}: newest evaluation");
    }
    // Consistent columns classify: restore both and cite each in a fail.
    for (hash, column) in [(&failed[0], "failed"), (&indeterminate[0], "indeterminate")] {
        store
            .connection
            .execute(
                "UPDATE work_run_evidence SET verification_result = ?1 WHERE evidence_hash = ?2",
                params![column, hash.as_str()],
            )
            .expect("restore the projection column");
    }
    record(store, &fail_citing(cut(store, &work), &failed[0], 41))
        .expect("a consistent failed check backs a fail");
    assert_ne!(
        newest(store),
        newest_before,
        "a consistent column records a newer evaluation"
    );
    record(
        store,
        &fail_citing(cut(store, &work), &indeterminate[0], 42),
    )
    .expect("a consistent indeterminate check backs a fail");
}

// Round 10 (Low): under an evaluated policy the sealed vector's citations
// must still be a subset of the completion evidence the seal names. The
// core refuses a completion whose evidence set omits a cited object and
// seals when the set carries it.
#[test]
fn a_direct_evaluated_seal_with_citations_outside_its_evidence_is_refused() {
    let mut fx = fixture("project-evaluation-citation-subset");
    let store = &mut fx.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable-citation-subset",
        5,
    );
    disable_obligation_rules(store, 6);
    let work = fx.work.clone();
    let claim = fx.claim.clone();
    let cited = fx.evidence.clone();
    let other = evidence(store, &work, &claim, "runner", "uncited-evidence", 7);
    record(
        store,
        &request(
            &work,
            cut(store, &work),
            "runner",
            Mode::SameSession,
            pass_judgment(&cited),
            8,
        ),
    )
    .expect("a pass citing the first note");
    let refused = checkpoint_then_complete(
        store,
        &work,
        &claim,
        "runner",
        std::slice::from_ref(&other),
        false,
        None,
        "complete-without-citation",
        9,
    )
    .expect_err("a completion evidence set that omits the cited note is refused");
    assert!(
        matches!(
            refused,
            StoreError::WorkCompletionRefused { ref reason, .. }
                if reason.contains("outside the completion evidence set")
        ),
        "{refused:?}"
    );
    assert_eq!(
        store.get_work_item(work.work_id).expect("item").lifecycle,
        crate::WorkLifecycle::Open
    );
    let seal = checkpoint_then_complete(
        store,
        &work,
        &claim,
        "runner",
        &[other.clone(), cited.clone()],
        false,
        None,
        "complete-with-citation",
        10,
    )
    .expect("the evidence set carrying the citation seals");
    assert!(seal.evidence.contains(&cited) && seal.evidence.contains(&other));
    assert_eq!(seal.acceptance[0].evidence, vec![cited]);
}

// Round 10 (Low): the service completion unions the fresh evaluation's
// citations, including observed typed evidence, into the completion
// evidence it captures and checkpoints, so the seal names them and the
// checkpoint acknowledges them even when the caller supplied a narrower
// evidence set.
#[test]
fn evaluated_completion_unions_verdict_citations_into_the_seal_evidence_and_checkpoint() {
    let mut fx = fixture("project-evaluation-citation-union");
    let database = fx.directory.path().join("engram.sqlite3");
    let store = &mut fx.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable-citation-union",
        5,
    );
    disable_obligation_rules(store, 6);
    let work = fx.work.clone();
    let claim = fx.claim.clone();
    let note = fx.evidence.clone();
    let uncited = evidence(store, &work, &claim, "runner", "uncited-evidence", 7);
    let mut host = HostSession::bind(store, &work, &claim, 10);
    let passed = host.checkpoint(
        store,
        false,
        Some((VerificationKind::Build, ExecutionOutcome::Succeeded)),
        20,
    );
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
                AcceptanceBasis::Judgment,
                &[note.clone(), passed[0].clone()],
            )],
            25,
        ),
    )
    .expect("a pass citing a note and host-minted verification evidence");
    let service = crate::work_service::LocalWorkService::new(
        database,
        work.project_id.clone(),
        "runner".into(),
        SessionId("runner".into()),
        None,
    );
    let completed = service
        .work_complete_on(
            Some(&work.work_id.0.to_string()),
            crate::work_service::WorkCompleteInput {
                source_fingerprint: None,
                links: Vec::new(),
                link_basis: None,
                capture: Some(crate::work_service::WorkCompletionCaptureInput {
                    summary: "delivered".into(),
                    refs: Vec::new(),
                }),
                evidence: vec![uncited.as_str().to_owned()],
                acceptance: None,
                note: None,
                idempotency_key: "complete-union".into(),
            },
            at(30),
        )
        .expect("service completion");
    let crate::work_service::WorkCompleteResult::Completed(receipt) = completed else {
        panic!("the fresh observed pass completes: {completed:?}");
    };
    let seal: CompletionSeal = store
        .get(&receipt.seal)
        .expect("read seal")
        .expect("canonical seal");
    for cited in [&note, &passed[0]] {
        assert!(
            seal.evidence.contains(cited),
            "the seal evidence must carry the citation {cited}: {:?}",
            seal.evidence
        );
    }
    assert!(seal.evidence.contains(&uncited));
    let mut sealed_citations = seal.acceptance[0].evidence.clone();
    sealed_citations.sort();
    let mut expected_citations = vec![note.clone(), passed[0].clone()];
    expected_citations.sort();
    assert_eq!(sealed_citations, expected_citations);
    let checkpoint_hash = store
        .get_work_run(claim.run_id)
        .expect("run")
        .last_checkpoint
        .expect("completion checkpoint");
    let checkpoint: crate::domain::WorkCheckpoint = store
        .get(&checkpoint_hash)
        .expect("read checkpoint")
        .expect("canonical checkpoint");
    for cited in [&note, &passed[0]] {
        assert!(
            checkpoint.evidence.contains(cited),
            "the completion checkpoint must acknowledge {cited}: {:?}",
            checkpoint.evidence
        );
    }
}

// Round 10 (Low): the shared seal-to-evaluation check selects the newest
// evaluation at the completion cut. A seal re-frozen to bind an older pass
// while a newer blocking evaluation sits before the cut is refused by the
// doctor, the shared check completion applies before writing, and the
// provenance read; the unforged seal stays healthy, and the bound query
// ignores anything after its cut.
#[test]
fn a_seal_binding_an_older_pass_under_a_newer_blocking_evaluation_is_refused() {
    for (blocking, basis) in [
        (AcceptanceVerdict::Fail, AcceptanceBasis::Judgment),
        (
            AcceptanceVerdict::NeedsHuman,
            AcceptanceBasis::HumanRequired,
        ),
    ] {
        let mut fx = fixture("project-evaluation-newest-at-cut");
        let database = fx.directory.path().join("engram.sqlite3");
        let store = &mut fx.store;
        enable(
            store,
            &[Mode::SameSession],
            MechanicalBasis::Asserted,
            false,
            "enable-newest-at-cut",
            5,
        );
        disable_obligation_rules(store, 6);
        let work = fx.work.clone();
        let claim = fx.claim.clone();
        let note = fx.evidence.clone();
        let older = record(
            store,
            &request(
                &work,
                cut(store, &work),
                "runner",
                Mode::SameSession,
                pass_judgment(&note),
                7,
            ),
        )
        .expect("the older pass");
        let after_older = cut(store, &work);
        record(
            store,
            &request(
                &work,
                after_older,
                "runner",
                Mode::SameSession,
                vec![verdict(1, blocking, basis, std::slice::from_ref(&note))],
                8,
            ),
        )
        .expect("the newer blocking evaluation");
        let cause = recovery_cause(checkpoint_then_complete(
            store,
            &work,
            &claim,
            "runner",
            std::slice::from_ref(&note),
            false,
            None,
            "complete-blocked",
            9,
        ));
        match (blocking, &cause) {
            (AcceptanceVerdict::Fail, WorkCompletionRecoveryCause::AcceptanceFailed { .. })
            | (
                AcceptanceVerdict::NeedsHuman,
                WorkCompletionRecoveryCause::AcceptanceNeedsHuman { .. },
            ) => {}
            other => panic!("the blocking verdict must name its own cause: {other:?}"),
        }
        let newest = record(
            store,
            &request(
                &work,
                cut(store, &work),
                "runner",
                Mode::SameSession,
                pass_judgment(&note),
                10,
            ),
        )
        .expect("the newest pass");
        let seal = checkpoint_then_complete(
            store,
            &work,
            &claim,
            "runner",
            std::slice::from_ref(&note),
            false,
            None,
            "complete-newest",
            11,
        )
        .expect("the newest pass completes");
        assert_eq!(seal.acceptance_evaluation, Some(newest.evaluation.clone()));
        let healthy = store.verify_all().expect("scan");
        assert!(
            healthy.is_healthy(),
            "{blocking:?} healthy control: {healthy:?}"
        );
        // The bound query excludes anything after its cut.
        let newest_through = |cut: i64| {
            crate::storage::work::acceptance_evaluation::newest_evaluation_through(
                &store.connection,
                claim.run_id,
                cut,
            )
            .expect("bounded newest read")
        };
        assert_eq!(newest_through(after_older), Some(older.evaluation.clone()));
        assert_eq!(
            newest_through(seal.completion_cut.position),
            Some(newest.evaluation.clone())
        );
        // Positive control through the shared check itself: an evaluation
        // entry appended after the completion cut is outside the seal's
        // selection, so the unforged seal still validates. The run is
        // sealed, so the later record is an isolated canonical object and
        // feed entry appended past the cut.
        let mut later: crate::domain::AcceptanceEvaluation = store
            .get(&newest.evaluation)
            .expect("read the newest evaluation")
            .expect("canonical evaluation");
        later.verdicts[0]
            .rationale
            .push_str(" (appended after the cut)");
        let later_object = CanonicalObject::freeze(&later).expect("freeze the later evaluation");
        assert_ne!(later_object.hash(), &newest.evaluation);
        store
            .connection
            .execute(
                "INSERT INTO objects (object_hash, object_kind, canonical_json, created_at)
                 SELECT ?1, object_kind, ?2, created_at FROM objects WHERE object_hash = ?3",
                params![
                    later_object.hash().as_str(),
                    later_object.bytes(),
                    newest.evaluation.as_str()
                ],
            )
            .expect("insert the later evaluation object");
        let run_feed = claim.run_id.0.to_string();
        let head: i64 = store
            .connection
            .query_row(
                "SELECT position FROM work_feed_heads
                 WHERE feed_kind = 'run_execution' AND feed_id = ?1",
                [&run_feed],
                |row| row.get(0),
            )
            .expect("run feed head");
        assert!(head >= seal.completion_cut.position);
        store
            .connection
            .execute(
                "INSERT INTO work_feed_entries
                     (feed_kind, feed_id, position, object_kind, object_hash, work_id)
                 SELECT feed_kind, feed_id, ?1, object_kind, ?2, work_id FROM work_feed_entries
                 WHERE feed_kind = 'run_execution' AND feed_id = ?3 AND object_hash = ?4",
                params![
                    head + 1,
                    later_object.hash().as_str(),
                    run_feed,
                    newest.evaluation.as_str()
                ],
            )
            .expect("append the later evaluation entry");
        store
            .connection
            .execute(
                "UPDATE work_feed_heads SET position = ?1
                 WHERE feed_kind = 'run_execution' AND feed_id = ?2",
                params![head + 1, run_feed],
            )
            .expect("advance the run feed head");
        assert_eq!(newest_through(head + 1), Some(later_object.hash().clone()));
        let still_bound = store
            .completion_seal_evaluation(&seal)
            .expect("a later entry past the cut leaves the unforged seal valid")
            .expect("the bound evaluation");
        assert_eq!(still_bound.attempt_key, newest.record.attempt_key);

        // Forge the seal to bind the older pass: it sits before the cut and
        // derives the same vector, so only newest-at-cut selection can
        // refuse it.
        let mut forged = seal.clone();
        forged.acceptance_evaluation = Some(older.evaluation.clone());
        refreeze_seal(store, &seal, &forged);
        let report = store.verify_all().expect("scan");
        assert!(
            report
                .invalid_work_records
                .iter()
                .any(|record| record.contains("acceptance_evaluation_binding")),
            "{blocking:?} forged seal: {report:?}"
        );
        let refused = store
            .completion_seal_evaluation(&forged)
            .expect_err("the shared check refuses an older pass under a newer blocking evaluation");
        assert!(
            matches!(refused, StoreError::InvalidWorkProjection(ref message) if message.contains("newest")),
            "{blocking:?}: {refused:?}"
        );
        let verbs = crate::AgentVerbs::new(
            database,
            work.project_id.clone(),
            "runner".into(),
            SessionId("runner".into()),
            None,
        );
        let shown = verbs
            .show(&work.work_id.0.to_string(), at(12))
            .expect("the completed item stays readable");
        assert_eq!(
            shown.value["acceptance"]["provenance"], "unavailable",
            "{blocking:?}: {}",
            shown.value
        );
    }
}

// Round 8 (Low): a completed item whose evaluated seal binding cannot be
// read discloses an unavailable provenance with its error class, distinct
// from a legacy seal's intentional omission; the item stays readable and the
// doctor still reports the corruption.
#[test]
fn a_broken_evaluated_seal_binding_is_disclosed_as_unavailable_provenance() {
    let mut fx = fixture("project-evaluation-unavailable-provenance");
    let database = fx.directory.path().join("engram.sqlite3");
    let store = &mut fx.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable-unavailable-provenance",
        5,
    );
    disable_obligation_rules(store, 6);
    let work = fx.work.clone();
    let claim = fx.claim.clone();
    let note = fx.evidence.clone();
    let recorded = record(
        store,
        &request(
            &work,
            cut(store, &work),
            "runner",
            Mode::SameSession,
            pass_judgment(&note),
            7,
        ),
    )
    .expect("a same-session pass");
    let seal = complete_evaluated(
        store,
        &work,
        &claim,
        "runner",
        &note,
        None,
        "complete-evaluated",
        8,
    )
    .expect("evaluated completion");
    assert_eq!(
        seal.acceptance_evaluation,
        Some(recorded.evaluation.clone())
    );
    let verbs = crate::AgentVerbs::new(
        database,
        work.project_id.clone(),
        "runner".into(),
        SessionId("runner".into()),
        None,
    );
    let work_ref = work.work_id.0.to_string();
    let healthy = verbs
        .show(&work_ref, at(9))
        .expect("show the evaluated completion");
    assert_eq!(healthy.value["acceptance"]["provenance"], "evaluated");
    assert!(
        healthy
            .text()
            .contains("acceptance: evaluated (same_session, asserted) by you"),
        "{}",
        healthy.text()
    );
    // The reachable corruption: the bound evaluation's canonical bytes no
    // longer decode, so the shared binding check fails on every read.
    store
        .connection
        .execute(
            "UPDATE objects SET canonical_json = X'7B7D' WHERE object_hash = ?1",
            [recorded.evaluation.as_str()],
        )
        .expect("corrupt the evaluation object");
    let broken = verbs
        .show(&work_ref, at(10))
        .expect("show keeps the item readable");
    assert_eq!(
        broken.value["acceptance"]["provenance"], "unavailable",
        "{}",
        broken.value
    );
    assert!(
        broken.value["acceptance"]["error_class"].is_string(),
        "{}",
        broken.value
    );
    assert!(
        broken.text().contains("acceptance: provenance unavailable")
            && broken.text().contains("diagnostic class: "),
        "{}",
        broken.text()
    );
    let report = store.verify_all().expect("scan");
    assert!(!report.is_healthy(), "{report:?}");
}

// Round 6 (Low): a verification projection that disagrees with its canonical
// object refuses reads and completion alike. The refusal is the existing
// evidence read, which `show` reaches before and independently of the
// evaluation block, so the evaluation classification adds no softer path;
// restoring the column restores both the read and the observed pass.
#[test]
fn a_corrupted_verification_projection_refuses_reads_and_completion_through_the_existing_check() {
    let mut fx = fixture("project-evaluation-advisory-read");
    let database = fx.directory.path().join("engram.sqlite3");
    let store = &mut fx.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Observed,
        false,
        "enable-advisory-read",
        5,
    );
    disable_obligation_rules(store, 6);
    let work = fx.work.clone();
    let claim = fx.claim.clone();
    let mut host = HostSession::bind(store, &work, &claim, 10);
    let passed = host.checkpoint(
        store,
        false,
        Some((VerificationKind::Build, ExecutionOutcome::Succeeded)),
        20,
    );
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
                AcceptanceBasis::Observed,
                &passed,
            )],
            25,
        ),
    )
    .expect("an observed pass on the consistent projection");
    let verbs = crate::AgentVerbs::new(
        database,
        work.project_id.clone(),
        "runner".into(),
        SessionId("runner".into()),
        None,
    );
    let work_ref = work.work_id.0.to_string();
    // The holder's keyless claim through the word gives this session its
    // focus while the projection is still consistent, so the peek below
    // assembles the focused item.
    verbs
        .claim(
            crate::verbs::ClaimInput {
                work_ref: work_ref.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(26),
        )
        .expect("the holder focuses its item");
    store
        .connection
        .execute(
            "UPDATE work_run_evidence SET verification_result = 'indeterminate' WHERE evidence_hash = ?1",
            [passed[0].as_str()],
        )
        .expect("corrupt the projection column");
    let refused_read = verbs
        .show(&work_ref, at(30))
        .expect_err("show refuses an evidence projection that disagrees with its object");
    assert!(
        matches!(
            refused_read.error,
            StoreError::InvalidWorkProjection(ref message)
                if message.contains("disagrees with its redundant run projection")
        ),
        "{refused_read:?}"
    );
    // The focused `next --peek` assembles the same evidence summaries for
    // the held item and refuses through the same read.
    let refused_peek = verbs
        .next(
            &crate::NextInput {
                limit: None,
                peek: true,
                verbose: false,
                context_generation: None,
            },
            at(31),
        )
        .expect_err("next --peek refuses through the shared focus assembly");
    assert!(
        matches!(
            refused_peek.error,
            StoreError::InvalidWorkProjection(ref message)
                if message.contains("disagrees with its redundant run projection")
        ),
        "{refused_peek:?}"
    );
    let refused_completion = verbs
        .done(
            crate::DoneInput {
                work_ref: Some(work_ref.clone()),
                summary: Some("finished".into()),
                ..crate::DoneInput::default()
            },
            at(32),
        )
        .expect_err("completion stays strict on a corrupted evaluation index");
    assert!(
        matches!(
            refused_completion.error,
            StoreError::InvalidWorkProjection(_)
        ),
        "{refused_completion:?}"
    );
    store
        .connection
        .execute(
            "UPDATE work_run_evidence SET verification_result = 'passed' WHERE evidence_hash = ?1",
            [passed[0].as_str()],
        )
        .expect("restore the projection column");
    let shown = verbs
        .show(&work_ref, at(33))
        .expect("the restored projection reads again");
    assert!(
        shown.value.get("acceptance_evaluation").is_some() && shown.text().contains("evaluation:"),
        "{}",
        shown.text()
    );
    // The restored read is the observed pass recorded before the corruption,
    // fresh and still citing the verification evidence.
    let restored = store
        .acceptance_evaluation_status(work.work_id, None)
        .expect("status read on the restored projection")
        .expect("the observed pass is the newest evaluation");
    assert!(restored.stale.is_none(), "stale: {:?}", restored.stale);
    assert_eq!(restored.record.verdicts.len(), 1);
    assert_eq!(restored.record.verdicts[0].verdict, AcceptanceVerdict::Pass);
    assert_eq!(restored.record.verdicts[0].basis, AcceptanceBasis::Observed);
    assert_eq!(
        restored.record.verdicts[0].evidence,
        vec![passed[0].clone()]
    );
}

// Review 12 (Low) and B23: the supported reopen transition starts a new run
// with a new revision; the old run's evaluation is absent for the new run and
// an identical payload records fresh rather than replaying.
#[test]
fn reopening_starts_a_run_without_an_evaluation_and_the_same_payload_records_fresh() {
    let mut fx = fixture("project-evaluation-reopen");
    let store = &mut fx.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable-reopen",
        5,
    );
    let work = fx.work.clone();
    let claim_a = fx.claim.clone();
    let note = fx.evidence.clone();
    let explicit_first = record(
        store,
        &RecordAcceptanceEvaluationRequest {
            attempt_key: Some("attempt-1".into()),
            ..request(
                &work,
                cut(store, &work),
                "runner",
                Mode::SameSession,
                pass_judgment(&note),
                5,
            )
        },
    )
    .expect("explicit attempt on the first run");
    let first = record(
        store,
        &request(
            &work,
            cut(store, &work),
            "runner",
            Mode::SameSession,
            pass_judgment(&note),
            6,
        ),
    )
    .expect("first evaluation");
    checkpoint_then_complete(
        store,
        &work,
        &claim_a,
        "runner",
        std::slice::from_ref(&note),
        false,
        None,
        "complete-first-run",
        7,
    )
    .expect("seal the first run");
    let completed = store.get_work_item(work.work_id).expect("completed item");
    let run = store
        .reopen_work(
            &ReopenWorkRequest {
                work_id: work.work_id,
                expected_work_revision: completed.revision,
                reason: "a regression was found after completion".into(),
                actor: actor("runner"),
                idempotency_key: "reopen".into(),
                reopened_at: at(30),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("reopen");
    let work = store.get_work_item(work.work_id).expect("reopened item");
    assert!(work.revision > completed.revision);
    assert_eq!(work.active_run_id, Some(run.run_id));
    let claim_b = claim(store, &work, "runner", "claim-reopened", 31, 3_600);
    let note_b = evidence(store, &work, &claim_b, "runner", "evidence-reopened", 32);
    // B23: the new run has no evaluation.
    assert!(matches!(
        recovery_cause(complete_evaluated(
            store,
            &work,
            &claim_b,
            "runner",
            &note_b,
            None,
            "complete-reopened-unevaluated",
            33,
        )),
        WorkCompletionRecoveryCause::MissingAcceptanceEvaluation { .. }
    ));
    // Round 4: a host that restarts its per-task counter on the new run
    // records a fresh attempt under the same explicit key.
    let restarted = record(
        store,
        &RecordAcceptanceEvaluationRequest {
            attempt_key: Some("attempt-1".into()),
            ..request(
                &work,
                cut(store, &work),
                "runner",
                Mode::SameSession,
                pass_judgment(&note_b),
                34,
            )
        },
    )
    .expect("the same explicit key on the new run is a new attempt");
    assert!(!restarted.replayed);
    assert_eq!(restarted.record.run_id, run.run_id);
    assert_ne!(restarted.evaluation, explicit_first.evaluation);
    let again = record(
        store,
        &request(
            &work,
            cut(store, &work),
            "runner",
            Mode::SameSession,
            pass_judgment(&note_b),
            35,
        ),
    )
    .expect("the same shape records fresh on the new run");
    assert!(!again.replayed);
    assert_ne!(again.evaluation, first.evaluation);
    assert_eq!(again.record.run_id, run.run_id);
}

// Claude's attempt-key note: explicit keys are scoped to the work item, so a
// host may reuse per-task counters across items.
#[test]
fn explicit_attempt_keys_are_scoped_to_the_work_item() {
    let mut fx = fixture("project-evaluation-key-scope");
    let store = &mut fx.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable-key-scope",
        5,
    );
    let work = fx.work.clone();
    let note = fx.evidence.clone();
    let keyed = RecordAcceptanceEvaluationRequest {
        attempt_key: Some("attempt-1".into()),
        ..request(
            &work,
            cut(store, &work),
            "runner",
            Mode::SameSession,
            pass_judgment(&note),
            6,
        )
    };
    let first = record(store, &keyed).expect("attempt-1 on the first item");
    // Same item: the exact resend replays; a contradicting payload under the
    // same key refuses.
    let resent = record(
        store,
        &RecordAcceptanceEvaluationRequest {
            recorded_at: at(7),
            ..keyed.clone()
        },
    )
    .expect("exact resend under the same key");
    assert!(resent.replayed);
    assert_eq!(resent.evaluation, first.evaluation);
    let mut contradicting = keyed.clone();
    contradicting.verdicts[0].rationale = "criterion 1: changed my mind".into();
    assert!(matches!(
        record(store, &contradicting),
        Err(StoreError::WorkOperationIdempotencyConflict { .. })
    ));
    let other = store
        .create_work(
            &root_request("project-evaluation-key-scope", "create-other", 7),
            &DevelopmentNoopRedactor,
        )
        .expect("another root");
    let other_claim = claim(store, &other, "runner", "claim-other", 8, 3_600);
    let other_note = evidence(store, &other, &other_claim, "runner", "other-evidence", 9);
    let reused = record(
        store,
        &RecordAcceptanceEvaluationRequest {
            attempt_key: Some("attempt-1".into()),
            ..request(
                &other,
                cut(store, &other),
                "runner",
                Mode::SameSession,
                vec![verdict(
                    1,
                    AcceptanceVerdict::Fail,
                    AcceptanceBasis::Judgment,
                    std::slice::from_ref(&other_note),
                )],
                10,
            )
        },
    )
    .expect("the same key on another item is a new attempt");
    assert!(!reused.replayed);
    assert_eq!(reused.record.work_id, other.work_id);
}
