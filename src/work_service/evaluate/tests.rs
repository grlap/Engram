mod budget;
mod review;

use std::path::Path;

use super::super::test_support::*;
use super::super::*;
use crate::domain::{
    AcceptanceEvaluationMode, AcceptanceEvaluationPolicy, ActorContext, AssuranceLevel,
    CompletionSeal, MechanicalBasis, ProvenanceLink,
};
use crate::{DevelopmentNoopRedactor, SqliteStore};

fn policy_admin() -> ActorContext {
    ActorContext {
        actor_id: "policy-admin".into(),
        actor_kind: "host_operator".into(),
        assurance: AssuranceLevel::Asserted,
        run_id: None,
        session_id: None,
        source_tool: Some("evaluate_test".into()),
        source_skill: None,
        provenance_chain: Vec::<ProvenanceLink>::new(),
        reason: "enable acceptance evaluation for the test project".into(),
    }
}

fn enable(database: &Path, modes: &[AcceptanceEvaluationMode], second: i64) {
    SqliteStore::open(database)
        .expect("store")
        .set_acceptance_evaluation_policy(
            &AcceptanceEvaluationPolicy {
                allowed_modes: modes.to_vec(),
                mechanical_basis: MechanicalBasis::Asserted,
                require_source_freshness: false,
            },
            &policy_admin(),
            "enable-evaluated-completion",
            None,
            at(second),
            &DevelopmentNoopRedactor,
        )
        .expect("activate the acceptance evaluation policy");
}

fn verdict(
    position: usize,
    verdict: &str,
    basis: &str,
    evidence: &[String],
) -> WorkCriterionVerdictInput {
    WorkCriterionVerdictInput {
        criterion: position,
        verdict: verdict.into(),
        basis: basis.into(),
        rationale: format!("criterion {position} {verdict}"),
        evidence: evidence.to_vec(),
    }
}

fn evaluate_input(
    work_ref: &str,
    revision: i64,
    evidence_basis: i64,
    verdicts: Vec<WorkCriterionVerdictInput>,
) -> WorkEvaluateInput {
    WorkEvaluateInput {
        work_ref: Some(work_ref.into()),
        mode: "same_session".into(),
        acceptance_basis: revision,
        evidence_basis,
        verdicts,
        attempt: None,
        source_fingerprint: None,
        model: None,
        execution_identity: None,
        parent_session: None,
    }
}

// The service word binds the ambient focus, the printed revision basis, and
// the session's attributed identity; completion then consumes only a fresh
// passing record, and peers see the evaluation as one `next` change.
#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one scenario walks the self-asserted refusal, the record, the completion refusals, and the seal in order"
)]
fn evaluate_records_under_policy_and_completion_consumes_it() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("evaluated-service".into());
    let service = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("evaluated-service-session".into()),
        Some("protocol-test".into()),
    );
    let root = proposed_root(
        service
            .work_propose(root_input("Evaluated root", "evaluated-root"), at(0))
            .expect("root"),
    );
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(3_600),
                recovery_reason: None,
                idempotency_key: "evaluated-claim".into(),
            },
            at(1),
        )
        .expect("claim");
    let evidence: ObjectId = serde_json::from_value(
        service
            .work_update(
                WorkUpdateInput::Evidence {
                    summary: "focused validation passed".into(),
                    refs: vec!["test:evaluated".into()],
                    attach: None,
                    idempotency_key: "evaluated-evidence".into(),
                },
                at(2),
            )
            .expect("evidence")
            .receipt
            .result,
    )
    .expect("evidence id");
    let citation = vec![evidence.as_str().to_owned()];
    let work_ref = root.short_ref.clone();

    let self_asserted = service
        .work_evaluate_on(
            &evaluate_input(
                &work_ref,
                root.revision,
                completion_run_feed_head(&service, root.work_id),
                vec![verdict(1, "pass", "judgment", &citation)],
            ),
            at(3),
        )
        .expect_err("the self-asserted policy refuses evaluations");
    assert!(
        matches!(&self_asserted, StoreError::AcceptanceEvaluationRefused { reason, .. } if reason.contains("does not enable")),
        "{self_asserted:?}"
    );

    enable(&database, &[AcceptanceEvaluationMode::SameSession], 4);
    // B02: a refused evaluated completion leaves no capture behind. The run
    // evidence set, the latest checkpoint, and the run feed head are read
    // before and after each refusal; only the pending protocol attempt may
    // remain, and it lives outside all three.
    let run_id = SqliteStore::open(&database)
        .expect("store")
        .get_work_item(root.work_id)
        .expect("item")
        .active_run_id
        .expect("active run");
    let capture_state = || {
        let store = SqliteStore::open(&database).expect("store");
        (
            store.work_run_evidence(run_id).expect("run evidence"),
            store
                .get_work_run(run_id)
                .expect("run")
                .last_checkpoint
                .clone(),
            completion_run_feed_head(&service, root.work_id),
        )
    };
    let before_unevaluated = capture_state();
    let refused = service
        .work_complete(
            completion_input("done before evaluating", "complete-unevaluated"),
            at(5),
        )
        .expect("completion attempt");
    let WorkCompleteResult::Refused(refusal) = refused else {
        panic!("completion without an evaluation must be refused: {refused:?}");
    };
    assert_eq!(refusal.code, "missing_acceptance_evaluation");
    assert!(refusal.remedy.contains("evaluate"), "{}", refusal.remedy);
    // The recovery command is runnable navigation to the criteria and
    // evidence, not an evaluation template with a pre-filled verdict. The
    // exact string is pinned here; the mcp-dogfood suite parses the emitted
    // command through the real CLI.
    assert_eq!(
        refusal.recovery.command,
        format!(
            "engram work show {} --notes --gates",
            refusal.recovery.item.short_ref
        )
    );
    assert_eq!(
        capture_state(),
        before_unevaluated,
        "a refused unevaluated completion must record no capture"
    );
    let explicit = service
        .work_complete(
            WorkCompleteInput {
                acceptance: Some(vec![WorkAcceptanceInput {
                    criterion: None,
                    satisfied: true,
                    evidence: Vec::new(),
                    note: "self-asserted".into(),
                }]),
                ..completion_input("done with self-assertion", "complete-self-asserted")
            },
            at(5),
        )
        .expect_err("explicit acceptance is refused under an evaluated policy");
    assert!(
        matches!(&explicit, StoreError::WorkCompletionRefused { reason, .. } if reason.contains("evaluate")),
        "{explicit:?}"
    );

    let stale_basis = service
        .work_evaluate_on(
            &evaluate_input(
                &work_ref,
                root.revision + 1,
                completion_run_feed_head(&service, root.work_id),
                vec![verdict(1, "pass", "judgment", &citation)],
            ),
            at(6),
        )
        .expect_err("a different acceptance basis refuses");
    assert!(
        matches!(stale_basis, StoreError::WorkRevisionConflict { .. }),
        "{stale_basis:?}"
    );
    let unknown_mode = service
        .work_evaluate_on(
            &WorkEvaluateInput {
                mode: "telepathy".into(),
                ..evaluate_input(
                    &work_ref,
                    root.revision,
                    completion_run_feed_head(&service, root.work_id),
                    vec![verdict(1, "pass", "judgment", &citation)],
                )
            },
            at(6),
        )
        .expect_err("an unknown mode word refuses");
    assert!(
        matches!(&unknown_mode, StoreError::InvalidWork(reason) if reason.contains("same_session")),
        "{unknown_mode:?}"
    );
    let unknown_verdict = service
        .work_evaluate_on(
            &evaluate_input(
                &work_ref,
                root.revision,
                completion_run_feed_head(&service, root.work_id),
                vec![verdict(1, "maybe", "judgment", &citation)],
            ),
            at(6),
        )
        .expect_err("an unknown verdict word refuses");
    assert!(
        matches!(&unknown_verdict, StoreError::InvalidWork(reason) if reason.contains("insufficient_evidence")),
        "{unknown_verdict:?}"
    );
    let bad_model = service
        .work_evaluate_on(
            &WorkEvaluateInput {
                model: Some("no-slash".into()),
                ..evaluate_input(
                    &work_ref,
                    root.revision,
                    completion_run_feed_head(&service, root.work_id),
                    vec![verdict(1, "pass", "judgment", &citation)],
                )
            },
            at(6),
        )
        .expect_err("a malformed model locator refuses");
    assert!(
        matches!(bad_model, StoreError::InvalidWork(_)),
        "{bad_model:?}"
    );

    let failing = service
        .work_evaluate_on(
            &evaluate_input(
                &work_ref,
                root.revision,
                completion_run_feed_head(&service, root.work_id),
                vec![verdict(1, "fail", "judgment", &[])],
            ),
            at(7),
        )
        .expect("record a failing evaluation");
    assert_eq!(failing.operation, "evaluate");
    assert_eq!(
        failing.projection.mode,
        AcceptanceEvaluationMode::SameSession
    );
    assert_eq!(failing.projection.verdicts_total, 1);
    assert_eq!(failing.projection.verdicts_omitted, 0);
    assert_eq!(
        failing
            .projection
            .blocking
            .as_ref()
            .map(|blocking| blocking.position),
        Some(1)
    );
    assert!(!failing.replayed);
    assert_eq!(failing.receipt.work_ref, work_ref);
    assert_eq!(
        failing.receipt.result["evaluation"],
        failing.evaluation.as_str()
    );
    let before_failed = capture_state();
    let refused = service
        .work_complete(
            completion_input("done after failing", "complete-failed"),
            at(8),
        )
        .expect("completion attempt");
    let WorkCompleteResult::Refused(refusal) = refused else {
        panic!("completion after a failing evaluation must be refused: {refused:?}");
    };
    assert_eq!(refusal.code, "acceptance_failed");
    assert_eq!(
        capture_state(),
        before_failed,
        "a refused failing completion must record no capture"
    );

    let passing = service
        .work_evaluate_on(
            &WorkEvaluateInput {
                model: Some("provider-a/model-b@2026-09".into()),
                ..evaluate_input(
                    &work_ref,
                    root.revision,
                    completion_run_feed_head(&service, root.work_id),
                    vec![verdict(1, "pass", "judgment", &citation)],
                )
            },
            at(9),
        )
        .expect("record a passing evaluation");
    assert_eq!(passing.projection.passed, 1);
    assert!(passing.projection.blocking.is_none());
    let stored_model = SqliteStore::open(&database)
        .expect("store")
        .acceptance_evaluation_status(root.work_id, None)
        .expect("status read")
        .expect("newest evaluation")
        .record
        .evaluator_model;
    assert_eq!(
        stored_model
            .as_ref()
            .and_then(|model| model.version.as_deref()),
        Some("2026-09")
    );
    let evaluation_hash = passing.evaluation.clone();
    let before_passing = capture_state();
    let completed = service
        .work_complete(
            completion_input("done after passing", "complete-passing"),
            at(10),
        )
        .expect("completion attempt");
    let WorkCompleteResult::Completed(receipt) = completed else {
        panic!("a fresh passing evaluation completes: {completed:?}");
    };
    let seal = SqliteStore::open(&database)
        .expect("store")
        .get::<CompletionSeal>(&receipt.seal)
        .expect("read seal")
        .expect("canonical seal");
    assert_eq!(seal.acceptance_evaluation, Some(evaluation_hash));
    assert_eq!(seal.acceptance.len(), 1);
    assert!(seal.acceptance[0].satisfied);
    assert_eq!(seal.acceptance[0].evidence, vec![evidence]);
    // The positive control of B02: unlike the refusals, the fresh pass adds
    // its nonempty completion capture to the run evidence and records the
    // completion checkpoint.
    let (evidence_after, checkpoint_after, head_after) = capture_state();
    assert!(
        evidence_after.len() > before_passing.0.len(),
        "the completion capture must join the run evidence: {evidence_after:?}"
    );
    assert!(
        checkpoint_after.is_some() && checkpoint_after != before_passing.1,
        "the completion checkpoint must be recorded: {checkpoint_after:?}"
    );
    assert_ne!(head_after, before_passing.2);

    let peer = LocalWorkService::new(
        database,
        project,
        "peer".into(),
        SessionId("evaluated-peer-session".into()),
        Some("protocol-test".into()),
    );
    // Change pages are byte-bounded; read densely until the feed is drained.
    let mut described = Vec::new();
    for page_index in 0..16 {
        let page = peer
            .work_next(
                50,
                WorkNextQuery {
                    sections: vec![WorkNextSection::Changes],
                    ..WorkNextQuery::default()
                },
                at(11 + page_index),
            )
            .expect("peer next");
        let changes = page.changes.as_ref().expect("changes section");
        if changes.is_empty() {
            break;
        }
        described.extend(changes.iter().map(|change| match &change.delivery {
            WorkChangeProjection::Visible(summary) => format!(
                "{}:{}:{}",
                change.entry.object_kind, summary.change_kind, summary.summary
            ),
            WorkChangeProjection::Omitted(omission) => {
                format!("{}:omitted:{omission:?}", change.entry.object_kind)
            }
        }));
    }
    let summaries = described
        .iter()
        .filter_map(|line| line.strip_prefix("acceptance_evaluation:evaluated:"))
        .collect::<Vec<_>>();
    assert_eq!(
        summaries,
        vec![
            "acceptance evaluated (same_session): 0 of 1 pass; fail \"Evaluated root accepted\"",
            "acceptance evaluated (same_session): all 1 criteria pass",
        ],
        "delivered changes: {described:#?}"
    );
}
