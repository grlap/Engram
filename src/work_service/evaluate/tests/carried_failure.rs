//! A failing evaluation whose criteria are revised afterwards is carried to
//! the next evaluation: shown to it, and after the executor's revision named
//! by it with `supersedes`.

use super::*;
use crate::domain::{
    AcceptanceBinding, CarriedFailureReviser, VerificationKind, VerificationRequirement,
    WorkBlockerKind, WorkRevisionPatch,
};
use crate::{CarriedFailureRefusal, WorkRunId};

/// Fields drop in order, so the fixture directory goes last, after the
/// service that holds its database open.
struct Scenario {
    database: std::path::PathBuf,
    project: ProjectId,
    service: LocalWorkService,
    root: WorkItemSummary,
    citation: Vec<String>,
    _directory: crate::test_support::TempHome,
}

/// An evaluated project with one claimed root, one piece of evidence, and
/// the given criteria.
fn scenario(name: &str) -> Scenario {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId(format!("carried-{name}"));
    let service = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "executor".into(),
        SessionId(format!("{name}-executor")),
        Some("protocol-test".into()),
    );
    let root = proposed_root(
        service
            .work_propose(root_input("Carried root", &format!("{name}-root")), at(0))
            .expect("root"),
    );
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(3_600),
                recovery_reason: None,
                idempotency_key: format!("{name}-claim"),
            },
            at(1),
        )
        .expect("claim");
    let evidence: ObjectId = serde_json::from_value(
        service
            .work_update(
                WorkUpdateInput::Evidence {
                    summary: "focused validation passed".into(),
                    refs: vec![format!("test:{name}")],
                    attach: None,
                    idempotency_key: format!("{name}-evidence"),
                },
                at(2),
            )
            .expect("evidence")
            .receipt
            .result,
    )
    .expect("evidence id");
    enable(
        &database,
        &[
            AcceptanceEvaluationMode::SameSession,
            AcceptanceEvaluationMode::SubAgent,
            AcceptanceEvaluationMode::IndependentSession,
        ],
        3,
    );
    Scenario {
        database,
        project,
        service,
        root,
        citation: vec![evidence.as_str().to_owned()],
        _directory: directory,
    }
}

impl Scenario {
    fn revision(&self) -> i64 {
        self.store()
            .get_work_item(self.root.work_id)
            .expect("item")
            .revision
    }

    fn store(&self) -> SqliteStore {
        SqliteStore::open(&self.database).expect("store")
    }

    /// The executor records a correction: new evidence after a blocking
    /// evaluation, which a later one needs before it replaces that one.
    fn correct(&self, second: i64) {
        self.service
            .work_update(
                WorkUpdateInput::Evidence {
                    summary: "correction for the failed criterion".into(),
                    refs: Vec::new(),
                    attach: None,
                    idempotency_key: format!("correction-{second}"),
                },
                at(second),
            )
            .expect("correction");
    }

    fn run(&self) -> WorkRunId {
        self.store()
            .get_work_item(self.root.work_id)
            .expect("item")
            .active_run_id
            .expect("active run")
    }

    fn evaluate(
        &self,
        verdict_word: &str,
        supersedes: Option<&ObjectId>,
        second: i64,
    ) -> Result<WorkEvaluateResult, StoreError> {
        let evidence = if verdict_word == "pass" {
            self.citation.clone()
        } else {
            Vec::new()
        };
        // The executor's evaluation: a sub-agent recorded from its own child
        // session under the executor, the executor-affiliated mode an
        // unmarked task still admits.
        self.executor_agent().work_evaluate_on(
            &WorkEvaluateInput {
                mode: "sub_agent".into(),
                execution_identity: Some("executor-agent".into()),
                parent_session: Some(self.service.session_id.0.clone()),
                supersedes: supersedes.map(|id| id.as_str().to_owned()),
                ..evaluate_input(
                    &self.root.short_ref,
                    self.revision(),
                    completion_run_feed_head(&self.service, self.root.work_id),
                    vec![verdict(1, verdict_word, "judgment", &evidence)],
                )
            },
            at(second),
        )
    }

    /// A session that never held the run: the evaluator that may name a
    /// failure the executor's revision carried.
    fn reviewer(&self) -> LocalWorkService {
        LocalWorkService::new(
            self.database.clone(),
            self.project.clone(),
            "reviewer".into(),
            SessionId(format!("{}-reviewer", self.project.0)),
            Some("protocol-test".into()),
        )
    }

    /// The reviewer's `independent_session` evaluation.
    fn review(
        &self,
        verdict_word: &str,
        supersedes: Option<&ObjectId>,
        second: i64,
    ) -> Result<WorkEvaluateResult, StoreError> {
        let reviewer = self.reviewer();
        let evidence = if verdict_word == "pass" {
            self.citation.clone()
        } else {
            Vec::new()
        };
        reviewer.work_evaluate_on(
            &WorkEvaluateInput {
                mode: "independent_session".into(),
                supersedes: supersedes.map(|id| id.as_str().to_owned()),
                ..evaluate_input(
                    &self.root.short_ref,
                    self.revision(),
                    completion_run_feed_head(&reviewer, self.root.work_id),
                    vec![verdict(1, verdict_word, "judgment", &evidence)],
                )
            },
            at(second),
        )
    }

    /// The executor's own same-session attempt, from its own session: what
    /// the carried-failure rule refuses as self-acknowledgement.
    fn own_evaluation(
        &self,
        verdict_word: &str,
        supersedes: Option<&ObjectId>,
        second: i64,
    ) -> Result<WorkEvaluateResult, StoreError> {
        self.service.work_evaluate_on(
            &WorkEvaluateInput {
                supersedes: supersedes.map(|id| id.as_str().to_owned()),
                ..evaluate_input(
                    &self.root.short_ref,
                    self.revision(),
                    completion_run_feed_head(&self.service, self.root.work_id),
                    vec![verdict(1, verdict_word, "judgment", &self.citation)],
                )
            },
            at(second),
        )
    }

    /// The executor's sub-agent: a child session of its own, never a holder.
    fn executor_agent(&self) -> LocalWorkService {
        LocalWorkService::new(
            self.database.clone(),
            self.project.clone(),
            "executor-agent".into(),
            SessionId(format!("{}-executor-agent", self.project.0)),
            Some("protocol-test".into()),
        )
    }

    /// A `sub_agent` evaluation submitted from `service`'s session, under the
    /// attested parent session `parent`, naming `supersedes`.
    fn sub_agent_review(
        &self,
        service: &LocalWorkService,
        parent: &str,
        supersedes: &ObjectId,
        second: i64,
    ) -> Result<WorkEvaluateResult, StoreError> {
        service.work_evaluate_on(
            &WorkEvaluateInput {
                mode: "sub_agent".into(),
                execution_identity: Some("evaluator-agent".into()),
                parent_session: Some(parent.into()),
                supersedes: Some(supersedes.as_str().to_owned()),
                ..evaluate_input(
                    &self.root.short_ref,
                    self.revision(),
                    completion_run_feed_head(service, self.root.work_id),
                    vec![verdict(1, "pass", "judgment", &self.citation)],
                )
            },
            at(second),
        )
    }

    /// Another session of the same project, which never held the run.
    fn session(&self, name: &str) -> LocalWorkService {
        LocalWorkService::new(
            self.database.clone(),
            self.project.clone(),
            name.into(),
            SessionId(format!("{}-{name}", self.project.0)),
            Some("protocol-test".into()),
        )
    }

    /// The criteria revised by `service`, which must focus the item first.
    fn revise_by(&self, service: &LocalWorkService, criterion: &str, second: i64) {
        service
            .work_focus(&self.root.short_ref, at(second))
            .expect("focus");
        service
            .work_update(
                WorkUpdateInput::Revise {
                    patch: WorkRevisionPatch {
                        acceptance: Some(vec![criterion.into()]),
                        ..WorkRevisionPatch::default()
                    },
                    idempotency_key: format!("revise-{second}"),
                },
                at(second),
            )
            .expect("revise the criteria");
    }

    fn planner(&self) -> LocalWorkService {
        LocalWorkService::new(
            self.database.clone(),
            self.project.clone(),
            "planner".into(),
            SessionId(format!("{}-planner", self.project.0)),
            Some("protocol-test".into()),
        )
    }

    /// The executor lets the claim go so a planner may revise, then takes
    /// the same run back.
    fn release(&self, second: i64) {
        self.service
            .work_update(
                WorkUpdateInput::Release {
                    reason: "a planner revises the criteria".into(),
                    waiver_reason: Some("no contribution yet".into()),
                    idempotency_key: format!("release-{second}"),
                },
                at(second),
            )
            .expect("release");
    }

    fn reclaim(&self, second: i64) {
        self.service
            .work_focus(&self.root.short_ref, at(second))
            .expect("focus");
        self.service
            .work_update(
                WorkUpdateInput::Claim {
                    ttl_seconds: Some(3_600),
                    recovery_reason: None,
                    idempotency_key: format!("reclaim-{second}"),
                },
                at(second),
            )
            .expect("reclaim");
    }

    fn carried(&self) -> Option<crate::domain::CarriedFailure> {
        self.store()
            .acceptance_evaluation_status(self.root.work_id, None)
            .expect("status")
            .and_then(|status| status.carried_failure)
    }
}

/// Naming the failed record when nothing is carried is refused.
fn assert_nothing_to_supersede(scenario: &Scenario, failed: &ObjectId, second: i64, case: &str) {
    let refused = scenario
        .evaluate("pass", Some(failed), second)
        .expect_err("nothing is carried, so nothing may be superseded");
    assert_eq!(
        refusal(&refused),
        Some((CarriedFailureRefusal::NothingToSupersede, None)),
        "{case}"
    );
}

/// A re-roll on the same evidence: the blocking evaluation stands.
fn assert_reroll_refused(result: Result<WorkEvaluateResult, StoreError>, case: &str) {
    match result {
        Err(StoreError::AcceptanceEvaluationRefused { reason, .. }) => assert!(
            reason.contains("nothing that could change it was recorded"),
            "{case}: {reason}"
        ),
        other => panic!("{case}: a re-roll must be refused, got {other:?}"),
    }
}

fn refusal(error: &StoreError) -> Option<(CarriedFailureRefusal, Option<ObjectId>)> {
    match error {
        StoreError::AcceptanceEvaluationCarriedFailure {
            refusal, failed, ..
        } => Some((*refusal, failed.clone())),
        _ => None,
    }
}

// B53: after the executor rewrites the criteria a fresh evaluation failed,
// the task completes only through an evaluation that names that failure,
// and the seal's evaluation names it for good.
#[test]
fn an_executor_revision_after_a_failure_is_completable_only_by_naming_it() {
    let scenario = scenario("executor");
    let failed = scenario
        .evaluate("fail", None, 4)
        .expect("failing evaluation");
    let failed_id = failed.evaluation.clone();
    assert!(
        scenario.carried().is_none(),
        "nothing is carried before a revision"
    );

    scenario.revise_by(&scenario.service, "An easier criterion", 5);
    let carried = scenario.carried().expect("the failure is carried");
    assert_eq!(carried.evaluation, failed_id);
    assert_eq!(carried.revised_by, CarriedFailureReviser::Executor);
    assert_eq!(carried.blocking.len(), 1);

    let head = completion_run_feed_head(&scenario.service, scenario.root.work_id);
    let unacknowledged = scenario
        .evaluate("pass", None, 6)
        .expect_err("a pass that ignores the failure is refused");
    assert_eq!(
        refusal(&unacknowledged),
        Some((
            CarriedFailureRefusal::Unacknowledged,
            Some(failed_id.clone())
        )),
        "{unacknowledged:?}"
    );
    assert!(
        unacknowledged.to_string().contains("--supersedes"),
        "{unacknowledged}"
    );
    let wrong = ObjectId::mint();
    let misnamed = scenario
        .evaluate("pass", Some(&wrong), 6)
        .expect_err("naming another record is refused");
    assert_eq!(
        refusal(&misnamed),
        Some((
            CarriedFailureRefusal::Unacknowledged,
            Some(failed_id.clone())
        ))
    );
    assert_eq!(
        completion_run_feed_head(&scenario.service, scenario.root.work_id),
        head,
        "a refused evaluation records nothing"
    );
    let refused = scenario
        .service
        .work_complete(
            completion_input("done after rewording", "complete-reworded"),
            at(7),
        )
        .expect("completion attempt");
    assert!(
        matches!(refused, WorkCompleteResult::Refused(_)),
        "{refused:?}"
    );

    // The executor naming its own failure is not someone else accepting the
    // revision.
    let self_named = scenario
        .own_evaluation("pass", Some(&failed_id), 8)
        .expect_err("the executor may not acknowledge its own failure");
    assert_eq!(
        refusal(&self_named),
        Some((
            CarriedFailureRefusal::SelfAcknowledged,
            Some(failed_id.clone())
        )),
        "{self_named:?}"
    );
    assert!(
        self_named.to_string().contains("independent_session")
            && self_named.to_string().contains("sub_agent under its own")
            && self_named.to_string().contains(
                "while no later failing evaluation has named the failure, revise the criteria and their bindings back"
            ),
        "the remedy names every way out: {self_named}"
    );
    assert_eq!(
        completion_run_feed_head(&scenario.service, scenario.root.work_id),
        head,
        "a self-acknowledgment records nothing"
    );

    let acknowledged = scenario
        .review("pass", Some(&failed_id), 8)
        .expect("a reviewer's pass that names the failure is recorded");
    assert_eq!(
        acknowledged.projection.supersedes.as_ref(),
        Some(&failed_id)
    );
    assert!(
        scenario.carried().is_none(),
        "the newer evaluation ends the carry"
    );
    let completed = scenario
        .service
        .work_complete(
            completion_input("done after superseding", "complete-superseded"),
            at(9),
        )
        .expect("completion attempt");
    let WorkCompleteResult::Completed(receipt) = completed else {
        panic!("the acknowledged pass completes: {completed:?}");
    };
    let store = scenario.store();
    let seal = store
        .get::<CompletionSeal>(&receipt.seal)
        .expect("read seal")
        .expect("seal");
    let bound: crate::domain::AcceptanceEvaluation = store
        .get(
            seal.acceptance_evaluation
                .as_ref()
                .expect("bound evaluation"),
        )
        .expect("read evaluation")
        .expect("evaluation");
    assert_eq!(bound.supersedes, Some(failed_id));
}

// B53: a host's resend of the evaluation that named the failure replays it,
// although that evaluation ended the carry. Under one attempt key, adding or
// removing what it names is a conflict, never a new evaluation.
#[test]
fn a_resent_superseding_evaluation_replays_and_its_attempt_binds_the_name() {
    let scenario = scenario("replay");
    let agent = scenario.executor_agent();
    let failing = WorkEvaluateInput {
        attempt: Some("the-failure".into()),
        mode: "sub_agent".into(),
        execution_identity: Some("executor-agent".into()),
        parent_session: Some(scenario.service.session_id.0.clone()),
        ..evaluate_input(
            &scenario.root.short_ref,
            scenario.revision(),
            completion_run_feed_head(&scenario.service, scenario.root.work_id),
            vec![verdict(1, "fail", "judgment", &[])],
        )
    };
    let failed = agent
        .work_evaluate_on(&failing, at(4))
        .expect("failing evaluation");
    scenario.revise_by(&scenario.service, "An easier criterion", 5);
    let added = agent
        .work_evaluate_on(
            &WorkEvaluateInput {
                supersedes: Some(failed.evaluation.as_str().to_owned()),
                ..failing.clone()
            },
            at(6),
        )
        .expect_err("adding a name under the failure's attempt key conflicts");
    assert!(
        matches!(added, StoreError::WorkOperationIdempotencyConflict { .. }),
        "{added:?}"
    );

    let reviewer = scenario.reviewer();
    let named = WorkEvaluateInput {
        mode: "independent_session".into(),
        attempt: Some("acknowledge-the-failure".into()),
        supersedes: Some(failed.evaluation.as_str().to_owned()),
        ..evaluate_input(
            &scenario.root.short_ref,
            scenario.revision(),
            completion_run_feed_head(&reviewer, scenario.root.work_id),
            vec![verdict(1, "pass", "judgment", &scenario.citation)],
        )
    };
    let recorded = reviewer
        .work_evaluate_on(&named, at(7))
        .expect("the evaluation naming the failure is recorded");
    assert!(!recorded.replayed);
    assert!(scenario.carried().is_none(), "it ends the carry");
    let resent = reviewer
        .work_evaluate_on(&named, at(8))
        .expect("an exact resend replays although nothing is carried now");
    assert!(resent.replayed);
    assert_eq!(resent.evaluation, recorded.evaluation);
    let removed = reviewer
        .work_evaluate_on(
            &WorkEvaluateInput {
                supersedes: None,
                ..named.clone()
            },
            at(9),
        )
        .expect_err("removing the name under the same attempt key conflicts");
    assert!(
        matches!(removed, StoreError::WorkOperationIdempotencyConflict { .. }),
        "{removed:?}"
    );
}

// R11: the carry belongs to the run, so disposing of the item ends it.
#[test]
fn cancelling_the_item_ends_the_carry() {
    let scenario = scenario("cancelled");
    scenario
        .evaluate("fail", None, 4)
        .expect("failing evaluation");
    scenario.revise_by(&scenario.service, "An easier criterion", 5);
    assert!(scenario.carried().is_some(), "the failure is carried");
    scenario
        .service
        .work_update(
            WorkUpdateInput::Cancel {
                reason: "the outcome is no longer wanted".into(),
                idempotency_key: "cancel".into(),
            },
            at(6),
        )
        .expect("cancel");
    let status = scenario
        .store()
        .acceptance_evaluation_status(scenario.root.work_id, None)
        .expect("status")
        .expect("the ended run's newest evaluation is still read");
    assert!(
        status.carried_failure.is_none(),
        "a cancelled item carries nothing"
    );
    // Nor can a new run shed the failure: only completed work reopens, and
    // completing needed an evaluation that named it.
    let reopened = scenario
        .service
        .work_update(
            WorkUpdateInput::Reopen {
                reason: "try the reworded criteria on a fresh run".into(),
                idempotency_key: "reopen".into(),
            },
            at(7),
        )
        .expect_err("a cancelled item cannot be reopened");
    assert!(
        matches!(&reopened, StoreError::InvalidWork(reason) if reason.contains("only completed work")),
        "{reopened:?}"
    );
}

// B54: a planner's revision after a failure is shown, but the next
// evaluation records without naming it. The executor only retitled the
// item, which changes no criterion, so the planner alone revised them.
#[test]
fn a_planner_revision_after_a_failure_is_shown_but_need_not_be_named() {
    let scenario = scenario("planner");
    let failed = scenario
        .evaluate("fail", None, 4)
        .expect("failing evaluation");
    let run = scenario.run();
    scenario
        .service
        .work_update(
            WorkUpdateInput::Revise {
                patch: WorkRevisionPatch {
                    title: Some("A clearer title".into()),
                    ..WorkRevisionPatch::default()
                },
                idempotency_key: "executor-retitle".into(),
            },
            at(5),
        )
        .expect("the executor retitles");
    scenario.release(5);
    let planner = scenario.planner();
    scenario.revise_by(&planner, "A planner's criterion", 6);
    scenario.reclaim(7);
    assert_eq!(scenario.run(), run, "the executor took the same run back");
    let carried = scenario.carried().expect("the failure is carried");
    assert_eq!(carried.evaluation, failed.evaluation);
    assert_eq!(carried.revised_by, CarriedFailureReviser::Planner);
    let recorded = scenario
        .evaluate("pass", None, 8)
        .expect("a planner's revision needs no acknowledgment");
    assert!(recorded.projection.supersedes.is_none());
}

// B54: a planner's carry may still be named, and the record keeps the name.
#[test]
fn a_planner_revision_after_a_failure_may_be_named() {
    let scenario = scenario("planner-named");
    let failed = scenario
        .evaluate("fail", None, 4)
        .expect("failing evaluation");
    scenario.release(5);
    scenario.revise_by(&scenario.planner(), "A planner's criterion", 6);
    scenario.reclaim(7);
    let carried = scenario.carried().expect("the failure is carried");
    assert_eq!(carried.revised_by, CarriedFailureReviser::Planner);
    let recorded = scenario
        .evaluate("pass", Some(&failed.evaluation), 8)
        .expect("naming a planner's carry is admitted");
    assert_eq!(
        recorded.projection.supersedes.as_ref(),
        Some(&failed.evaluation)
    );
    let stored: crate::domain::AcceptanceEvaluation = scenario
        .store()
        .get(&recorded.evaluation)
        .expect("read evaluation")
        .expect("evaluation");
    assert_eq!(stored.supersedes, Some(failed.evaluation));
}

// B53: an executor that releases the claim and rewords from its own session,
// under project authority, is still the executor: it held the run. Another
// session then takes the run, so only the run's history still names it.
#[test]
fn a_former_holder_rewording_without_the_claim_is_still_the_executor() {
    let scenario = scenario("former-holder");
    let failed = scenario
        .evaluate("fail", None, 4)
        .expect("failing evaluation");
    scenario.release(5);
    scenario.revise_by(&scenario.service, "The former holder's criterion", 6);
    let successor = scenario.planner();
    successor
        .work_focus(&scenario.root.short_ref, at(7))
        .expect("focus");
    successor
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(3_600),
                recovery_reason: None,
                idempotency_key: "successor-claim".into(),
            },
            at(7),
        )
        .expect("the successor claims the run");
    let carried = scenario.carried().expect("the failure is carried");
    assert_eq!(carried.revised_by, CarriedFailureReviser::Executor);
    let refused = successor
        .work_evaluate_on(
            &evaluate_input(
                &scenario.root.short_ref,
                scenario.revision(),
                completion_run_feed_head(&successor, scenario.root.work_id),
                vec![verdict(1, "pass", "judgment", &scenario.citation)],
            ),
            at(8),
        )
        .expect_err("the former holder's revision must be named");
    assert_eq!(
        refusal(&refused),
        Some((
            CarriedFailureRefusal::Unacknowledged,
            Some(failed.evaluation.clone())
        ))
    );
    // B57: nor may the former holder name it, even as a sub_agent under the
    // successor: its session held the run.
    let former = scenario
        .sub_agent_review(
            &scenario.service,
            "carried-former-holder-planner",
            &failed.evaluation,
            9,
        )
        .expect_err("a former holder is an executor of the run");
    assert_eq!(
        refusal(&former),
        Some((
            CarriedFailureRefusal::SelfAcknowledged,
            Some(failed.evaluation.clone())
        )),
        "{former:?}"
    );
}

// B57: after the executor's revision the session decides who may name the
// failure, not the mode label: a sub_agent sharing the executor's session is
// the executor, and one under its own session is someone else.
#[test]
fn only_an_evaluator_that_never_held_the_run_may_name_an_executor_revised_failure() {
    let scenario = scenario("distinct-evaluator");
    let failed = scenario
        .evaluate("fail", None, 4)
        .expect("failing evaluation");
    scenario.revise_by(&scenario.service, "An easier criterion", 5);
    let executor = "distinct-evaluator-executor";
    let shared = scenario
        .sub_agent_review(&scenario.service, executor, &failed.evaluation, 6)
        .expect_err("a sub_agent in the executor's own session is the executor");
    assert_eq!(
        refusal(&shared),
        Some((
            CarriedFailureRefusal::SelfAcknowledged,
            Some(failed.evaluation.clone())
        )),
        "{shared:?}"
    );
    let recorded = scenario
        .sub_agent_review(&scenario.session("helper"), executor, &failed.evaluation, 7)
        .expect("a sub_agent under its own session names it");
    assert_eq!(
        recorded.projection.supersedes.as_ref(),
        Some(&failed.evaluation)
    );
}

// B58: rewording back to the original criteria does not undo a revision a
// later failing evaluation judged: a planner's in between stays carried, so
// the executor cannot revert it and pass itself.
#[test]
fn rewording_back_does_not_undo_a_revision_a_later_failure_judged() {
    // A planner-only carry the executor names and fails, then reverts.
    let planner_first = scenario("planner-then-revert");
    let original = planner_first
        .store()
        .get_work_item(planner_first.root.work_id)
        .expect("item")
        .acceptance;
    let failed = planner_first
        .evaluate("fail", None, 4)
        .expect("failing evaluation")
        .evaluation;
    planner_first.release(5);
    planner_first.revise_by(&planner_first.planner(), "A stricter criterion", 6);
    planner_first.reclaim(7);
    planner_first
        .evaluate("fail", Some(&failed), 8)
        .expect("the executor may name a planner-only carry");
    planner_first.revise_by(&planner_first.service, &original[0], 9);
    let carried = planner_first
        .carried()
        .expect("the revert past the planner's criteria is carried");
    assert_eq!(carried.evaluation, failed);
    assert_eq!(carried.revised_by, CarriedFailureReviser::Executor);
    let refused = planner_first
        .evaluate("pass", None, 10)
        .expect_err("the executor cannot revert the planner and pass itself");
    assert_eq!(
        refusal(&refused),
        Some((CarriedFailureRefusal::Unacknowledged, Some(failed.clone())))
    );

    // An executor carry a reviewer fails, a planner revises, and the
    // executor reverts to the original criteria.
    let reviewed_first = scenario("review-planner-revert");
    let failed = reviewed_first
        .evaluate("fail", None, 4)
        .expect("failing evaluation")
        .evaluation;
    reviewed_first.revise_by(&reviewed_first.service, "An easier criterion", 5);
    reviewed_first
        .review("fail", Some(&failed), 6)
        .expect("a reviewer names and fails it");
    reviewed_first.release(7);
    reviewed_first.revise_by(&reviewed_first.planner(), "A planner's criterion", 8);
    reviewed_first.reclaim(9);
    reviewed_first.revise_by(&reviewed_first.service, &original[0], 10);
    let carried = reviewed_first
        .carried()
        .expect("the revert is still carried");
    assert_eq!(carried.evaluation, failed);
    assert_eq!(carried.revised_by, CarriedFailureReviser::Executor);
    let refused = reviewed_first
        .evaluate("pass", None, 11)
        .expect_err("the executor cannot pass itself");
    assert_eq!(
        refusal(&refused),
        Some((CarriedFailureRefusal::Unacknowledged, Some(failed.clone())))
    );

    // Once a later failing evaluation has named it, the failure stays carried
    // whatever the criteria are, so no revert ends it: only a passing
    // evaluation from someone other than the executor that names it does.
    let reverted = scenario("review-then-revert");
    let failed = reverted
        .evaluate("fail", None, 4)
        .expect("failing evaluation")
        .evaluation;
    reverted.revise_by(&reverted.service, "An easier criterion", 5);
    reverted
        .review("fail", Some(&failed), 6)
        .expect("a reviewer names and fails it");
    reverted.revise_by(&reverted.service, &original[0], 7);
    let carried = reverted
        .carried()
        .expect("the revert to the original criteria is still carried");
    assert_eq!(carried.evaluation, failed);
    assert_eq!(carried.revised_by, CarriedFailureReviser::Executor);
    // Nor does another failing review of the reverted criteria end it, though
    // it and the original judged the same contract as the item's now.
    reverted
        .review("fail", Some(&failed), 8)
        .expect("a second reviewer names and fails it");
    let carried = reverted.carried().expect("still carried");
    assert_eq!(carried.evaluation, failed);
    assert_eq!(carried.revised_by, CarriedFailureReviser::Executor);
    let unnamed = reverted
        .evaluate("pass", None, 9)
        .expect_err("the executor cannot pass itself");
    assert_eq!(
        refusal(&unnamed),
        Some((CarriedFailureRefusal::Unacknowledged, Some(failed.clone())))
    );
    let self_named = reverted
        .own_evaluation("pass", Some(&failed), 9)
        .expect_err("nor acknowledge its own failure");
    assert_eq!(
        refusal(&self_named),
        Some((
            CarriedFailureRefusal::SelfAcknowledged,
            Some(failed.clone())
        ))
    );
}

// B58: after a later failing evaluation named the failure, `show --full`
// needs the bindings that evaluation judged too: a binding-only revision
// after it leaves the original and current contracts alike.
#[test]
fn a_binding_only_revision_after_a_named_failure_shows_the_bindings_it_undid() {
    let scenario = scenario("newest-bindings");
    let failed = scenario
        .evaluate("fail", None, 4)
        .expect("failing evaluation")
        .evaluation;
    let bound = AcceptanceBinding {
        criterion: 1,
        requirement: VerificationRequirement {
            check_kind: VerificationKind::Test,
            check_fingerprint: None,
        },
    };
    let rebind =
        |service: &LocalWorkService, bindings: Vec<AcceptanceBinding>, key: &str, second| {
            service
                .work_focus(&scenario.root.short_ref, at(second))
                .expect("focus");
            service
                .work_update(
                    WorkUpdateInput::Revise {
                        patch: WorkRevisionPatch {
                            acceptance_bindings: Some(bindings),
                            ..WorkRevisionPatch::default()
                        },
                        idempotency_key: key.into(),
                    },
                    at(second),
                )
                .expect("rebind the criterion");
        };
    scenario.release(5);
    rebind(&scenario.planner(), vec![bound.clone()], "planner-binds", 6);
    scenario.reclaim(7);
    scenario
        .evaluate("fail", Some(&failed), 8)
        .expect("the executor may name a planner-only carry");
    rebind(&scenario.service, Vec::new(), "executor-unbinds", 9);
    let carried = scenario
        .carried()
        .expect("dropping the binding the newest evaluation judged is carried");
    assert_eq!(carried.evaluation, failed);
    assert_eq!(carried.revised_by, CarriedFailureReviser::Executor);
    assert!(
        carried.judged_bindings.is_empty(),
        "the original was unbound"
    );
    assert_eq!(
        carried.newest_judged_bindings,
        Some(vec![bound]),
        "the newest evaluation judged the bound criterion"
    );
}

// B58: naming the failure is not accepting the revision. A reviewer that
// names it but fails the revised criteria keeps it carried, anchored at the
// original record, through a later revision and a second failing review;
// only a reviewer's pass that names it ends the carry.
#[test]
fn a_failing_evaluation_that_names_the_failure_keeps_it_carried() {
    let scenario = scenario("named-but-failed");
    let failed = scenario
        .evaluate("fail", None, 4)
        .expect("failing evaluation")
        .evaluation;
    scenario.revise_by(&scenario.service, "An easier criterion", 5);
    let rejected = scenario
        .review("fail", Some(&failed), 6)
        .expect("a reviewer names the failure and fails the revised criteria");
    assert_eq!(rejected.projection.supersedes.as_ref(), Some(&failed));
    let carried = scenario.carried().expect("the failure is still carried");
    assert_eq!(
        carried.evaluation, failed,
        "anchored at the original record"
    );
    assert_eq!(carried.revised_by, CarriedFailureReviser::Executor);
    assert_eq!(
        carried.judged_criteria,
        vec!["Carried root accepted".to_owned()],
        "the before side is the original criteria"
    );

    // The executor still cannot pass itself.
    let unnamed = scenario
        .evaluate("pass", None, 7)
        .expect_err("the carry is still in force");
    assert_eq!(
        refusal(&unnamed),
        Some((CarriedFailureRefusal::Unacknowledged, Some(failed.clone())))
    );
    let self_named = scenario
        .own_evaluation("pass", Some(&failed), 7)
        .expect_err("the executor may not acknowledge its own failure");
    assert_eq!(
        refusal(&self_named),
        Some((
            CarriedFailureRefusal::SelfAcknowledged,
            Some(failed.clone())
        ))
    );

    // A second revision after the review still anchors at the original.
    scenario.revise_by(&scenario.service, "An even easier criterion", 8);
    assert_eq!(
        scenario.carried().expect("still carried").evaluation,
        failed
    );
    // So does a second non-passing review that names it.
    scenario
        .review("needs_human", Some(&failed), 9)
        .expect("a second, needs_human review names it");
    let carried = scenario.carried().expect("still carried");
    assert_eq!(carried.evaluation, failed);
    assert_eq!(carried.revised_by, CarriedFailureReviser::Executor);

    // Only a reviewer's pass that names it ends the carry, and done seals;
    // the needs_human review stands until a correction follows it.
    scenario.correct(10);
    scenario
        .review("pass", Some(&failed), 10)
        .expect("a reviewer's pass names it");
    assert!(scenario.carried().is_none(), "the passing review ends it");
    let completed = scenario
        .service
        .work_complete(
            completion_input("done after the review accepted it", "complete-accepted"),
            at(11),
        )
        .expect("completion attempt");
    assert!(
        matches!(completed, WorkCompleteResult::Completed(_)),
        "{completed:?}"
    );
}

// B54: once the executor has revised, a later planner revision does not
// lift the requirement.
#[test]
fn an_executor_then_planner_revision_still_needs_the_failure_named() {
    let scenario = scenario("mixed");
    let failed = scenario
        .evaluate("fail", None, 4)
        .expect("failing evaluation");
    scenario.revise_by(&scenario.service, "The executor's criterion", 5);
    scenario.release(6);
    scenario.revise_by(&scenario.planner(), "The planner's criterion", 7);
    scenario.reclaim(8);
    let carried = scenario.carried().expect("the failure is carried");
    assert_eq!(carried.revised_by, CarriedFailureReviser::Executor);
    let refused = scenario
        .evaluate("pass", None, 9)
        .expect_err("the executor's revision still needs the failure named");
    assert_eq!(
        refusal(&refused),
        Some((
            CarriedFailureRefusal::Unacknowledged,
            Some(failed.evaluation.clone())
        ))
    );
    scenario
        .review("pass", Some(&failed.evaluation), 10)
        .expect("a reviewer naming it is recorded");
}

// B54: the classification is read at each evaluation over every revision
// since the failure, so it only tightens. An executor's rewording still
// counts after a planner rewords back to the judged criteria and revises
// again, and a planner that revised and then takes the run is its executor.
#[test]
fn the_classification_only_tightens_with_the_revisions_since_the_failure() {
    let reworded = scenario("reworded-back");
    let original = reworded
        .store()
        .get_work_item(reworded.root.work_id)
        .expect("item")
        .acceptance;
    reworded
        .evaluate("fail", None, 4)
        .expect("failing evaluation");
    reworded.revise_by(&reworded.service, "The executor's criterion", 5);
    reworded.release(6);
    let planner = reworded.planner();
    reworded.revise_by(&planner, &original[0], 7);
    assert!(
        reworded.carried().is_none(),
        "rewording back to the judged criteria ends the carry"
    );
    reworded.revise_by(&planner, "The planner's criterion", 8);
    assert_eq!(
        reworded
            .carried()
            .expect("the failure is carried again")
            .revised_by,
        CarriedFailureReviser::Executor,
        "the executor's earlier rewording still counts"
    );

    let taken = scenario("planner-takes-the-run");
    taken.evaluate("fail", None, 4).expect("failing evaluation");
    taken.release(5);
    let planner = taken.planner();
    taken.revise_by(&planner, "The planner's criterion", 6);
    assert_eq!(
        taken.carried().expect("the failure is carried").revised_by,
        CarriedFailureReviser::Planner
    );
    planner
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(3_600),
                recovery_reason: None,
                idempotency_key: "planner-claim".into(),
            },
            at(7),
        )
        .expect("the planner takes the run");
    assert_eq!(
        taken.carried().expect("the failure is carried").revised_by,
        CarriedFailureReviser::Executor,
        "a reviser that holds the run is its executor"
    );
}

// B55: with no failing evaluation, a revision behaves as before, and a
// supersedes naming a passing record, or anything when nothing was
// evaluated, is refused.
#[test]
fn without_a_carried_failure_revisions_behave_as_before_and_supersedes_is_refused() {
    let scenario = scenario("clean");
    let nothing = ObjectId::mint();
    let unevaluated = scenario
        .evaluate("pass", Some(&nothing), 4)
        .expect_err("nothing to supersede before any evaluation");
    assert_eq!(
        refusal(&unevaluated),
        Some((CarriedFailureRefusal::NothingToSupersede, None))
    );
    let passed = scenario
        .evaluate("pass", None, 5)
        .expect("passing evaluation");
    scenario.revise_by(&scenario.service, "A reworded criterion", 6);
    assert!(
        scenario.carried().is_none(),
        "a passing evaluation carries nothing"
    );
    let non_blocking = scenario
        .evaluate("pass", Some(&passed.evaluation), 7)
        .expect_err("a passing record is nothing to supersede");
    assert_eq!(
        refusal(&non_blocking),
        Some((CarriedFailureRefusal::NothingToSupersede, None))
    );
    scenario
        .evaluate("pass", None, 8)
        .expect("the reworded criteria evaluate as before");
}

// B55: a revision that leaves the judged criteria as they were carries
// nothing, and neither does rewording that returns to them.
#[test]
fn only_a_revision_of_the_judged_criteria_carries_the_failure() {
    let scenario = scenario("title");
    let original = scenario
        .store()
        .get_work_item(scenario.root.work_id)
        .expect("item")
        .acceptance;
    let failed = scenario
        .evaluate("fail", None, 4)
        .expect("failing evaluation")
        .evaluation;
    scenario
        .service
        .work_update(
            WorkUpdateInput::Revise {
                patch: WorkRevisionPatch {
                    title: Some("A clearer title".into()),
                    ..WorkRevisionPatch::default()
                },
                idempotency_key: "retitle".into(),
            },
            at(5),
        )
        .expect("retitle");
    assert!(
        scenario.carried().is_none(),
        "a title change carries nothing"
    );
    assert_nothing_to_supersede(&scenario, &failed, 5, "retitled");
    scenario.revise_by(&scenario.service, "A reworded criterion", 6);
    assert!(scenario.carried().is_some());
    scenario.revise_by(&scenario.service, &original[0], 7);
    assert!(
        scenario.carried().is_none(),
        "rewording back to the judged criteria ends the carry"
    );
    assert_nothing_to_supersede(&scenario, &failed, 7, "reworded back");
    // Neither the title edit nor the rewording is new evidence, so the
    // failure stands until a correction follows it.
    assert_reroll_refused(
        scenario.evaluate("pass", None, 8),
        "retitled and reworded back",
    );
    scenario.correct(9);
    scenario
        .evaluate("pass", None, 9)
        .expect("the original criteria evaluate as before");
}

// B55: a blocker added and cleared after a failure bumps the revision, which
// retires the evaluation, but changes no criterion, so nothing is carried.
#[test]
fn a_blocker_after_a_failure_carries_nothing() {
    let scenario = scenario("blocker");
    let failed = scenario
        .evaluate("fail", None, 4)
        .expect("failing evaluation")
        .evaluation;
    let judged = scenario.revision();
    scenario
        .service
        .work_update(
            WorkUpdateInput::Block {
                blocker_kind: WorkBlockerKind::Manual,
                detail: "waiting on a review".into(),
                idempotency_key: "block".into(),
            },
            at(5),
        )
        .expect("block");
    scenario
        .service
        .work_update(
            WorkUpdateInput::Unblock {
                blocker_id: None,
                idempotency_key: "unblock".into(),
            },
            at(6),
        )
        .expect("unblock");
    assert!(
        scenario.revision() > judged,
        "the blocker bumped the revision"
    );
    assert!(scenario.carried().is_none(), "a blocker carries nothing");
    assert_nothing_to_supersede(&scenario, &failed, 6, "blocked and unblocked");
    // The blocker is bookkeeping, not evidence: the failure stands until a
    // correction follows it.
    assert_reroll_refused(scenario.evaluate("pass", None, 7), "blocked and unblocked");
    scenario.correct(8);
    scenario
        .evaluate("pass", None, 8)
        .expect("the unchanged criteria evaluate as before");
}

// B56: dropping or changing a criterion's verification binding after a
// failure weakens it as surely as rewording it, so the failure is carried
// even when the criterion's text is unchanged.
#[test]
fn a_binding_change_after_a_failure_carries_it() {
    let bound = |kind| AcceptanceBinding {
        criterion: 1,
        requirement: VerificationRequirement {
            check_kind: kind,
            check_fingerprint: None,
        },
    };
    let bindings = |after| WorkRevisionPatch {
        acceptance_bindings: Some(after),
        ..WorkRevisionPatch::default()
    };
    for (name, rebind) in [
        ("dropped", bindings(Vec::new())),
        ("changed", bindings(vec![bound(VerificationKind::Review)])),
        // `update --accept` repeating the same wording without `--bind`:
        // replacing the list clears its bindings.
        (
            "reaccepted",
            WorkRevisionPatch {
                acceptance: Some(vec!["Carried root accepted".into()]),
                ..WorkRevisionPatch::default()
            },
        ),
    ] {
        let scenario = scenario(&format!("binding-{name}"));
        scenario
            .service
            .work_update(
                WorkUpdateInput::Revise {
                    patch: WorkRevisionPatch {
                        acceptance_bindings: Some(vec![bound(VerificationKind::Test)]),
                        ..WorkRevisionPatch::default()
                    },
                    idempotency_key: "bind-a-test".into(),
                },
                at(4),
            )
            .expect("bind the criterion to a test");
        let failed = scenario
            .evaluate("fail", None, 5)
            .expect("failing evaluation");
        assert!(
            scenario.carried().is_none(),
            "{name}: nothing before a revision"
        );
        scenario
            .service
            .work_update(
                WorkUpdateInput::Revise {
                    patch: rebind,
                    idempotency_key: "rebind".into(),
                },
                at(6),
            )
            .expect("the executor changes the binding");
        let carried = scenario
            .carried()
            .unwrap_or_else(|| panic!("{name}: the failure is carried"));
        assert_eq!(carried.evaluation, failed.evaluation, "{name}");
        assert_eq!(
            carried.revised_by,
            CarriedFailureReviser::Executor,
            "{name}"
        );
        assert_eq!(
            carried.judged_bindings,
            vec![bound(VerificationKind::Test)],
            "{name}: the carry shows the binding the failure was judged under"
        );
        let refused = scenario
            .evaluate("fail", None, 7)
            .expect_err("an evaluation that ignores the failure is refused");
        assert_eq!(
            refusal(&refused),
            Some((
                CarriedFailureRefusal::Unacknowledged,
                Some(failed.evaluation.clone())
            )),
            "{name}"
        );
    }
}
