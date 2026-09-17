//! An admitted evaluation must always return a usable result: many short
//! criteria, one large rationale, a large blocking criterion, maximal
//! metadata, and escaping-heavy text all fit the agent response budget by
//! explicit omission, never by failing after the commit.

use super::*;
use crate::domain::{
    ActorContext, AssuranceLevel, ChildRequirement, ClaimWorkRequest, CreateWorkRequest,
    ProvenanceLink, RecordWorkEvidenceRequest, WorkItemKind, WorkOrigin,
};
use crate::work_service::MAX_AGENT_WORK_RESPONSE_BYTES;

const SESSION: &str = "budget-evaluator-session";

fn session_actor() -> ActorContext {
    ActorContext {
        actor_id: "agent".into(),
        actor_kind: "test_agent".into(),
        assurance: AssuranceLevel::Asserted,
        run_id: None,
        session_id: Some(SessionId(SESSION.into())),
        source_tool: Some("evaluate_budget_test".into()),
        source_skill: None,
        provenance_chain: Vec::<ProvenanceLink>::new(),
        reason: "exercise the evaluate response budget".into(),
    }
}

/// A claimed root with the given criteria and one generic evidence object,
/// created directly in storage so the criteria count is the fixture's choice.
fn claimed_root(
    database: &Path,
    project: &ProjectId,
    criteria: Vec<String>,
) -> (String, i64, ObjectHash) {
    let mut store = SqliteStore::open(database).expect("store");
    let work = store
        .create_work(
            &CreateWorkRequest {
                evaluation_mode: None,
                external_ref: None,
                notes: Vec::new(),
                project_id: project.clone(),
                parent_id: None,
                child_requirement: ChildRequirement::Required,
                title: "Budgeted evaluation".into(),
                outcome: "the result always fits".into(),
                acceptance: criteria,
                kind: WorkItemKind::Task,
                priority: 1,
                labels: Vec::new(),
                assigned_to: None,
                deferred_until: None,
                origin: WorkOrigin::Local,
                source_snapshot_id: None,
                actor: session_actor(),
                idempotency_key: "budget-root".into(),
                created_at: at(0),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("create root");
    let claim = store
        .claim_work(
            &ClaimWorkRequest {
                work_id: work.work_id,
                expected_work_revision: work.revision,
                expected_run_id: Some(work.active_run_id.expect("active run")),
                holder: SessionId(SESSION.into()),
                ttl_seconds: 3_600,
                recovery_reason: None,
                actor: session_actor(),
                idempotency_key: "budget-claim".into(),
                claimed_at: at(1),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("claim");
    let evidence = store
        .record_work_evidence(
            &RecordWorkEvidenceRequest {
                work_id: work.work_id,
                run_id: claim.run_id,
                expected_work_revision: work.revision,
                holder: SessionId(SESSION.into()),
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                summary: "focused validation passed".into(),
                refs: vec!["test:budget".into()],
                actor: session_actor(),
                idempotency_key: "budget-evidence".into(),
                recorded_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("evidence");
    let basis = store
        .work_feed_head(&FeedId::RunExecution(claim.run_id))
        .expect("run feed head");
    (work.short_ref, basis, evidence)
}

fn service(database: &Path, project: &ProjectId) -> LocalWorkService {
    LocalWorkService::new(
        database.to_path_buf(),
        project.clone(),
        "agent".into(),
        SessionId(SESSION.into()),
        Some("evaluate_budget_test".into()),
    )
}

fn result_bytes(result: &WorkEvaluateResult) -> usize {
    serde_json::to_vec(result).expect("serialize result").len()
}

#[test]
fn many_short_criteria_return_a_bounded_result_with_exact_omissions() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("evaluate-budget-many".into());
    let criteria = (0..300)
        .map(|index| format!("criterion {index:03} holds"))
        .collect::<Vec<_>>();
    let (work_ref, basis, evidence) = claimed_root(&database, &project, criteria);
    enable(&database, &[AcceptanceEvaluationMode::SameSession], 3);
    let service = service(&database, &project);
    let citation = vec![evidence.as_str().to_owned()];
    let verdicts = (1..=300)
        .map(|position| verdict(position, "pass", "judgment", &citation))
        .collect::<Vec<_>>();
    let result = service
        .work_evaluate_on(&evaluate_input(&work_ref, 1, basis, verdicts), at(4))
        .expect("an admitted evaluation always returns a usable result");
    assert!(
        result_bytes(&result) <= MAX_AGENT_WORK_RESPONSE_BYTES,
        "{} bytes",
        result_bytes(&result)
    );
    let projection = &result.projection;
    assert_eq!(projection.verdicts_total, 300);
    assert!(projection.verdicts_omitted > 0, "{projection:?}");
    assert_eq!(projection.verdicts.len() + projection.verdicts_omitted, 300);
    assert_eq!(projection.passed, 300);
    assert!(projection.blocking.is_none());
    assert!(
        projection
            .verdicts
            .iter()
            .enumerate()
            .all(|(index, row)| row.position == index + 1 && row.citations == 1)
    );
    assert_eq!(
        projection.full_detail,
        format!("engram work show {work_ref} --full")
    );
    let recorded = SqliteStore::open(&database)
        .expect("store")
        .acceptance_evaluation_status(
            SqliteStore::open(&database)
                .expect("store")
                .resolve_work_ref(&project, &work_ref)
                .expect("resolve")
                .work_id,
            None,
        )
        .expect("status")
        .expect("newest evaluation");
    assert_eq!(recorded.record.verdicts.len(), 300);
}

#[test]
fn a_large_blocking_criterion_and_a_large_rationale_still_fit() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("evaluate-budget-large".into());
    let criteria = vec![
        format!("a: the build passes {}", "x".repeat(8 * 1024)),
        "b: docs updated".into(),
    ];
    let (work_ref, basis, evidence) = claimed_root(&database, &project, criteria);
    enable(&database, &[AcceptanceEvaluationMode::SameSession], 3);
    let service = service(&database, &project);
    let citation = vec![evidence.as_str().to_owned()];
    let result = service
        .work_evaluate_on(
            &WorkEvaluateInput {
                source_fingerprint: Some("f".repeat(256)),
                model: Some(format!(
                    "{}/{}@{}",
                    "p".repeat(128),
                    "m".repeat(128),
                    "v".repeat(128)
                )),
                attempt: Some("a".repeat(256)),
                ..evaluate_input(
                    &work_ref,
                    1,
                    basis,
                    vec![
                        WorkCriterionVerdictInput {
                            rationale: format!(
                                "\"quoted\" \\backslash\\ ünïcödé {}",
                                "r".repeat(60 * 1024)
                            ),
                            ..verdict(1, "fail", "judgment", &[])
                        },
                        verdict(2, "pass", "judgment", &citation),
                    ],
                )
            },
            at(4),
        )
        .expect("an admitted evaluation always returns a usable result");
    assert!(
        result_bytes(&result) <= MAX_AGENT_WORK_RESPONSE_BYTES,
        "{} bytes",
        result_bytes(&result)
    );
    let projection = &result.projection;
    assert_eq!(projection.verdicts_total, 2);
    assert_eq!(projection.verdicts_omitted, 0);
    assert_eq!(projection.passed, 1);
    let blocking = projection
        .blocking
        .as_ref()
        .expect("the failing criterion blocks");
    assert_eq!(blocking.position, 1);
    assert!(
        blocking.criterion.len() < 256,
        "{}",
        blocking.criterion.len()
    );
    assert!(blocking.criterion.starts_with("a: the build passes"));
    assert_eq!(
        projection.source_fingerprint.as_deref().map(str::len),
        Some(256)
    );
    // The projected key is the recorded one: explicit keys are scoped to the
    // item.
    assert!(
        projection.attempt_key.starts_with("explicit:")
            && projection
                .attempt_key
                .ends_with(&format!(":{}", "a".repeat(256))),
        "{}",
        projection.attempt_key
    );
}
