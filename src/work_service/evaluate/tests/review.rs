//! Service-level regression cases from the first review pair.

use super::*;

// B25: under an evaluated policy, `done` with positional links is refused
// before any capture side effect; the run feed head does not move.
#[test]
fn done_with_links_is_refused_under_an_evaluated_policy_without_capture_effects() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("evaluated-links".into());
    let service = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("evaluated-links-session".into()),
        Some("protocol-test".into()),
    );
    let root = proposed_root(
        service
            .work_propose(root_input("Linked root", "linked-root"), at(0))
            .expect("root"),
    );
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(3_600),
                recovery_reason: None,
                idempotency_key: "linked-claim".into(),
            },
            at(1),
        )
        .expect("claim");
    let evidence: ObjectHash = serde_json::from_value(
        service
            .work_update(
                WorkUpdateInput::Evidence {
                    summary: "focused validation passed".into(),
                    refs: vec!["test:linked".into()],
                    attach: None,
                    idempotency_key: "linked-evidence".into(),
                },
                at(2),
            )
            .expect("evidence")
            .receipt
            .result,
    )
    .expect("evidence hash");
    enable(&database, &[AcceptanceEvaluationMode::SameSession], 3);
    let head = completion_run_feed_head(&service, root.work_id);
    let refused = service
        .work_complete(
            WorkCompleteInput {
                links: vec![WorkCriterionLinkInput {
                    criterion: 1,
                    locator: evidence.as_str().to_owned(),
                }],
                link_basis: Some(root.revision),
                ..completion_input("done with links", "complete-with-links")
            },
            at(4),
        )
        .expect_err("links are refused under an evaluated policy");
    assert!(
        matches!(&refused, StoreError::WorkCriterionLinkInvalid { reason, .. } if reason.contains("evaluate")),
        "{refused:?}"
    );
    assert_eq!(completion_run_feed_head(&service, root.work_id), head);
}

// Round 4 (Medium): an exact resend after the item revision changed, or after
// the item completed, recovers the committed attempt; a fresh submission with
// the old basis still refuses without effects.
#[test]
fn exact_retries_replay_after_a_revision_and_after_completion() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("evaluated-retries".into());
    let session = SessionId("evaluated-retries-session".into());
    let service = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        session.clone(),
        Some("protocol-test".into()),
    );
    let root = proposed_root(
        service
            .work_propose(root_input("Retried root", "retried-root"), at(0))
            .expect("root"),
    );
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(3_600),
                recovery_reason: None,
                idempotency_key: "retried-claim".into(),
            },
            at(1),
        )
        .expect("claim");
    let evidence: ObjectHash = serde_json::from_value(
        service
            .work_update(
                WorkUpdateInput::Evidence {
                    summary: "focused validation passed".into(),
                    refs: vec!["test:retried".into()],
                    attach: None,
                    idempotency_key: "retried-evidence".into(),
                },
                at(2),
            )
            .expect("evidence")
            .receipt
            .result,
    )
    .expect("evidence hash");
    enable(&database, &[AcceptanceEvaluationMode::SameSession], 3);
    let citation = vec![evidence.as_str().to_owned()];
    let work_ref = root.short_ref.clone();
    let submission = |basis: i64, attempt: Option<&str>, rationale: &str| WorkEvaluateInput {
        attempt: attempt.map(str::to_owned),
        ..evaluate_input(
            &work_ref,
            basis,
            completion_run_feed_head(&service, root.work_id),
            vec![WorkCriterionVerdictInput {
                rationale: rationale.into(),
                ..verdict(1, "pass", "judgment", &citation)
            }],
        )
    };
    let explicit = submission(root.revision, Some("attempt-1"), "explicit attempt");
    let keyless = submission(root.revision, None, "keyless attempt");
    let explicit_first = service
        .work_evaluate_on(&explicit, at(4))
        .expect("explicit record");
    let keyless_first = service
        .work_evaluate_on(&keyless, at(5))
        .expect("keyless record");
    assert!(!explicit_first.replayed && !keyless_first.replayed);

    // The holder revises the item (pins the mode): the revision moves on.
    // Verbs and service share the database; the pin is an ordinary revision.
    let verbs = crate::AgentVerbs::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        session.clone(),
        None,
    );
    verbs
        .update(
            crate::UpdateInput {
                work_ref: Some(work_ref.clone()),
                action: crate::UpdateAction::EvaluationMode {
                    mode: Some("same_session".into()),
                },
            },
            at(6),
        )
        .expect("pin the mode");
    let revised = SqliteStore::open(&database)
        .expect("store")
        .get_work_item(root.work_id)
        .expect("item");
    assert!(revised.revision > root.revision);
    // A replay or a refusal changes neither the run feed head nor the
    // newest evaluation; the snapshot is taken before each group.
    let snapshot = || {
        (
            completion_run_feed_head(&service, root.work_id),
            SqliteStore::open(&database)
                .expect("store")
                .acceptance_evaluation_status(root.work_id, None)
                .expect("status")
                .map(|status| status.evaluation),
        )
    };
    let before_pin_replays = snapshot();
    for (label, resend, first) in [
        ("explicit", &explicit, &explicit_first),
        ("keyless", &keyless, &keyless_first),
    ] {
        let replayed = service
            .work_evaluate_on(resend, at(7))
            .unwrap_or_else(|error| {
                panic!("{label} resend after the revision must replay: {error}")
            });
        assert!(replayed.replayed, "{label}");
        assert_eq!(replayed.evaluation, first.evaluation, "{label}");
        assert_eq!(
            replayed.projection.attempt_key, first.projection.attempt_key,
            "{label}"
        );
    }
    assert_eq!(
        snapshot(),
        before_pin_replays,
        "replays after the revision have no effect"
    );
    let stale = service
        .work_evaluate_on(
            &submission(root.revision, None, "a new attempt on the old basis"),
            at(8),
        )
        .expect_err("a fresh submission on the old basis refuses");
    assert!(
        matches!(stale, StoreError::WorkRevisionConflict { .. }),
        "{stale:?}"
    );
    assert_eq!(
        snapshot(),
        before_pin_replays,
        "a refused fresh submission has no effect"
    );

    // A fresh pass on the current revision seals the item; afterwards every
    // exact resend, old or new, still replays and nothing new is admitted.
    let fresh = submission(revised.revision, None, "fresh pass after the pin");
    let fresh_first = service
        .work_evaluate_on(&fresh, at(9))
        .expect("fresh record on the current revision");
    let completed = service
        .work_complete(
            completion_input("done after the fresh pass", "complete-retried"),
            at(10),
        )
        .expect("completion attempt");
    assert!(
        matches!(completed, WorkCompleteResult::Completed(_)),
        "{completed:?}"
    );
    let before_completion_replays = snapshot();
    for (label, resend, first) in [
        ("fresh", &fresh, &fresh_first),
        ("explicit", &explicit, &explicit_first),
    ] {
        let replayed = service
            .work_evaluate_on(resend, at(11))
            .unwrap_or_else(|error| panic!("{label} resend after completion must replay: {error}"));
        assert!(replayed.replayed, "{label}");
        assert_eq!(replayed.evaluation, first.evaluation, "{label}");
    }
    assert_eq!(
        snapshot(),
        before_completion_replays,
        "replays after completion have no effect"
    );
    // Completion bumped the revision once more; a new payload at the current
    // revision is refused for the lack of an active run, not for staleness.
    let completed_revision = SqliteStore::open(&database)
        .expect("store")
        .get_work_item(root.work_id)
        .expect("item")
        .revision;
    let refused = service
        .work_evaluate_on(
            &submission(
                completed_revision,
                None,
                "a new attempt on a completed item",
            ),
            at(12),
        )
        .expect_err("a new attempt on a completed item refuses");
    assert!(
        matches!(&refused, StoreError::AcceptanceEvaluationRefused { reason, .. } if reason.contains("no active run")),
        "{refused:?}"
    );
    assert_eq!(
        snapshot(),
        before_completion_replays,
        "a refused new attempt on a completed item has no effect"
    );
}

// The preflight projection carries the attempt key the core records, in the
// explicit and the content-derived form.
#[test]
fn the_preflight_projection_carries_the_recorded_attempt_key() {
    use crate::domain::{
        AcceptanceBasis, AcceptanceVerdict, ActorContext, AssuranceLevel, CriterionVerdictInput,
        ProvenanceLink, RecordAcceptanceEvaluationRequest,
    };
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("evaluated-keys".into());
    let session = SessionId("evaluated-keys-session".into());
    let service = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        session.clone(),
        Some("protocol-test".into()),
    );
    let root = proposed_root(
        service
            .work_propose(root_input("Keyed root", "keyed-root"), at(0))
            .expect("root"),
    );
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(3_600),
                recovery_reason: None,
                idempotency_key: "keyed-claim".into(),
            },
            at(1),
        )
        .expect("claim");
    let evidence: ObjectHash = serde_json::from_value(
        service
            .work_update(
                WorkUpdateInput::Evidence {
                    summary: "focused validation passed".into(),
                    refs: vec!["test:keyed".into()],
                    attach: None,
                    idempotency_key: "keyed-evidence".into(),
                },
                at(2),
            )
            .expect("evidence")
            .receipt
            .result,
    )
    .expect("evidence hash");
    enable(&database, &[AcceptanceEvaluationMode::SameSession], 3);
    let mut store = SqliteStore::open(&database).expect("store");
    let work = store.get_work_item(root.work_id).expect("item");
    let run_id = work.active_run_id.expect("active run");
    for (label, attempt_key) in [
        ("explicit", Some("attempt-7".to_owned())),
        ("content", None),
    ] {
        let request = RecordAcceptanceEvaluationRequest {
            project_id: project.clone(),
            work_id: work.work_id,
            expected_work_revision: work.revision,
            evaluated_through: completion_run_feed_head(&service, root.work_id),
            mode: AcceptanceEvaluationMode::SameSession,
            execution_identity: None,
            parent_session: None,
            evaluator_model: None,
            source_basis: None,
            verdicts: vec![CriterionVerdictInput {
                criterion: 1,
                verdict: AcceptanceVerdict::Pass,
                basis: AcceptanceBasis::Judgment,
                rationale: format!("{label} attempt"),
                evidence: vec![evidence.clone()],
            }],
            evaluator: ActorContext {
                actor_id: "agent".into(),
                actor_kind: "agent".into(),
                assurance: AssuranceLevel::Asserted,
                run_id: None,
                session_id: Some(session.clone()),
                source_tool: Some("evaluate_test".into()),
                source_skill: None,
                provenance_chain: Vec::<ProvenanceLink>::new(),
                reason: "record an acceptance evaluation".into(),
            },
            attempt_key,
            recorded_at: at(4),
        };
        let identity = crate::storage::acceptance_attempt_identity(&request, run_id)
            .expect("attempt identity");
        let preview = super::super::preview_projection(
            &request,
            &work,
            run_id,
            "engram work show keyed --full".into(),
            identity.key.clone(),
        );
        let recorded = store
            .record_acceptance_evaluation(&request, &DevelopmentNoopRedactor)
            .expect("record");
        assert!(!recorded.replayed, "{label}");
        assert_eq!(preview.attempt_key, recorded.record.attempt_key, "{label}");
        assert_eq!(preview.attempt_key, identity.key, "{label}");
    }
}
