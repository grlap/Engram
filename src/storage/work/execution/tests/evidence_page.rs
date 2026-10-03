use super::*;
use crate::verbs::AgentVerbs;
use crate::work_service::{LocalWorkService, MAX_FOCUS_RELATIONS, WorkNextQuery};
use std::sync::Arc;

/// Binds the holder's host session to its claim and records, through real
/// control turns, one verification together with the environment it ran in.
/// Returns the environment and the verification.
fn record_checked_environment(
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
) -> (ObjectId, ObjectId) {
    let run = load_work_run(&store.connection, claim.run_id).expect("claimed run");
    let binding = ControlWorkBinding {
        root_execution_id: run.root_execution_id,
        work_id: work.work_id,
        run_id: run.run_id,
        work_revision: claim.accepted_work_revision,
        claim_id: claim.claim_id,
        claim_fence: claim.fence,
    };
    let session = claim.holder.clone();
    let connection = store
        .resume_control_connection(&session, at(10))
        .expect("resume connection");
    let mut host = actor(&session.0);
    host.run_id = Some(run.run_id.0.to_string());
    let bound = store
        .bind_control_session_with_work(
            &work.project_id,
            "evidence-page",
            "Record checked evidence",
            &session,
            &connection,
            &host,
            Some(&binding),
            ControlAssurance::TurnGated,
            &[EffectClass::Observe, EffectClass::MutateLocal],
            1,
            "bind-evidence-page",
            at(10),
        )
        .expect("bind host session");
    let turn = |store: &mut SqliteStore, key: &str, effects: Vec<EffectClass>, second: i64| {
        let resource_intents = if effects.contains(&EffectClass::MutateLocal) {
            vec![crate::domain::ResourceSubject::Path {
                project_id: work.project_id.clone(),
                segments: vec!["src".into()],
                coverage: crate::domain::ResourceCoverage::Tree,
            }]
        } else {
            Vec::new()
        };
        let decision = store
            .evaluate_control_turn(
                &work.project_id,
                &session,
                &connection,
                &bound.routing_token,
                &TurnIntent {
                    idempotency_key: format!("evaluate-{key}"),
                    intent_fingerprint: ObjectId::from_canonical_bytes(key.as_bytes()),
                    purpose: Some(TurnPurpose::Ordinary),
                    requested_effects: effects,
                    resource_intents,
                },
                at(second),
            )
            .expect("evaluate turn");
        let ControlTurnDecision::Grant { grant } = decision else {
            panic!("the turn must grant: {decision:?}");
        };
        let tokens = grant
            .delivery
            .as_ref()
            .map(|delivery| vec![delivery.page.delivery_token.clone()])
            .unwrap_or_default();
        assert!(matches!(
            store
                .begin_control_turn(
                    &work.project_id,
                    &session,
                    &connection,
                    &bound.routing_token,
                    &grant.grant_id,
                    &tokens,
                    &format!("begin-{key}"),
                    at(second + 1),
                )
                .expect("begin turn"),
            ControlTurnBeginDecision::Begin { .. }
        ));
        grant
    };
    let synchronize = turn(store, "synchronize", vec![EffectClass::Observe], 11);
    assert!(matches!(
        store
            .checkpoint_control_turn(
                &work.project_id,
                &session,
                &connection,
                &bound.routing_token,
                &synchronize.grant_id,
                TurnNextIntent::Continue,
                "checkpoint-synchronize",
                at(13),
            )
            .expect("checkpoint synchronization"),
        ControlTurnCheckpointDecision::Checkpointed { .. }
    ));
    let check = turn(store, "check", vec![EffectClass::MutateLocal], 14);
    let basis = ExecutionSourceBasis {
        workspace_id: "workspace-page".into(),
        source_revision: "content-revision-1".into(),
        source_root_generation: None,
        source_root_state: None,
    };
    let components = EnvironmentComponents {
        toolchain: "rustc-test".into(),
        sandbox: None,
        workspace_id: basis.workspace_id.clone(),
        capability_map_revision: 1,
    };
    let checkpointed = store
        .checkpoint_control_turn_with_evidence(
            &work.project_id,
            &session,
            &connection,
            &bound.routing_token,
            &check.grant_id,
            TurnNextIntent::Continue,
            &[ExecutionObservationInput {
                observation_id: "test-command".into(),
                action_fingerprint: ObjectId::from_canonical_bytes(b"cargo test"),
                effect: EffectClass::MutateLocal,
                outcome: ExecutionOutcome::Succeeded,
                source_changed: false,
                reported_source_change: None,
                source_basis: Some(basis.clone()),
                observed_at: Some(at(15)),
            }],
            &[VerificationEvidenceInput {
                producer_observation: ExecutionObservationReference::ObservationId {
                    observation_id: "test-command".into(),
                },
                check_kind: VerificationKind::Test,
                environment: Some(EnvironmentEvidenceReference::Index { index: 0 }),
                summary: Some("host observed the tests".into()),
                refs: vec!["command:cargo-test".into()],
            }],
            &[EnvironmentEvidenceInput {
                source_basis: basis,
                environment_fingerprint: CanonicalObject::freeze(&components)
                    .expect("freeze components")
                    .key()
                    .clone(),
                components: Some(components),
                observed_at: at(15),
            }],
            "checkpoint-check",
            at(16),
        )
        .expect("checkpoint the check");
    let ControlTurnCheckpointDecision::Checkpointed { receipt } = checkpointed else {
        panic!("the check must checkpoint: {checkpointed:?}");
    };
    (
        receipt.environment_evidence[0].clone(),
        receipt.verification_evidence[0].clone(),
    )
}

// More records than the page holds: the page keeps the newest by run-feed
// position, so the oldest generic records drop out. The verification at the
// boundary keeps its environment, which lies beyond it, and both come first;
// show and rich next select the same rows, and the latest record is reported
// on its own.
#[test]
fn the_evidence_page_keeps_the_newest_records_and_each_verifications_environment() {
    let directory = crate::test_support::temp_home().expect("temporary home");
    let database = directory.path().join("engram.sqlite3");
    let mut store = SqliteStore::open(&database).expect("store");
    let work = store
        .create_work(
            &root_request("project-evidence-page", "create", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("create work");
    let claim = claim(&mut store, &work, "runner", "claim", 1, 3_600);
    let oldest = evidence(&mut store, &work, &claim, "runner", "oldest", 2);
    let (environment, verification) = record_checked_environment(&mut store, &work, &claim);
    // Seven newer generic records leave the verification as the eighth
    // newest and its environment, recorded just before it, as the ninth.
    let newer = (0..7)
        .map(|index| {
            let current = store.get_work_item(work.work_id).expect("current item");
            evidence(
                &mut store,
                &current,
                &claim,
                "runner",
                &format!("newer-{index}"),
                20 + index,
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        newer.len() + 1,
        MAX_FOCUS_RELATIONS,
        "the verification sits at the boundary"
    );
    let mut expected = vec![environment, verification];
    expected.extend(newer.iter().rev().take(MAX_FOCUS_RELATIONS - 2).cloned());
    let omitted = [oldest, newer[0].clone()];
    assert!(expected.iter().all(|kept| !omitted.contains(kept)));
    drop(store);

    let service = Arc::new(LocalWorkService::new(
        database,
        work.project_id.clone(),
        "runner".into(),
        claim.holder.clone(),
        None,
    ));
    let ids = |items: &[crate::work_service::WorkEvidenceSummary]| {
        items
            .iter()
            .map(|item| item.evidence.clone())
            .collect::<Vec<_>>()
    };

    // The default show reads this view.
    let shown = service
        .work_focus_for_agent(&work.short_ref, at(40))
        .expect("show view");
    assert_eq!(ids(&shown.evidence_items), expected);
    assert_eq!(shown.evidence_count, MAX_FOCUS_RELATIONS + omitted.len());
    assert_eq!(
        shown
            .latest_evidence_item
            .as_ref()
            .map(|item| &item.evidence),
        newer.last(),
        "the latest record is reported on its own"
    );

    // Rich next selects the same rows for the focused item.
    service
        .work_focus(&work.short_ref, at(41))
        .expect("focus the item");
    let next = service
        .work_next(5, WorkNextQuery::default(), at(42))
        .expect("rich next");
    let focus = next.focus.expect("next carries the focus");
    assert_eq!(ids(&focus.evidence_items), expected);
    assert_eq!(focus.evidence_count, MAX_FOCUS_RELATIONS + omitted.len());

    // The show word reports the two omitted records.
    let words = AgentVerbs::with_shared_service(service, "runner".into(), claim.holder.clone());
    let rendered = words.show(&work.short_ref, at(43)).expect("show word");
    assert_eq!(
        rendered.value["notes"].as_array().expect("notes").len(),
        MAX_FOCUS_RELATIONS
    );
    assert_eq!(rendered.value["notes_omitted"], omitted.len());
}

// A projected evidence row with no run-feed entry has no position to order
// by. It is refused, not silently left off the page, even when it is the
// oldest row and newer ones fill the page.
#[test]
fn run_evidence_without_a_run_feed_position_is_refused() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let work = store
        .create_work(
            &root_request("project-evidence-position", "create", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("create work");
    let claim = claim(&mut store, &work, "runner", "claim", 1, 3_600);
    let recorded = (0..=MAX_FOCUS_RELATIONS)
        .map(|index| {
            let current = store.get_work_item(work.work_id).expect("current item");
            evidence(
                &mut store,
                &current,
                &claim,
                "runner",
                &format!("evidence-{index}"),
                2 + i64::try_from(index).expect("small index"),
            )
        })
        .collect::<Vec<_>>();
    store
        .work_run_evidence_projection(claim.run_id, MAX_FOCUS_RELATIONS)
        .expect("an intact projection reads");
    store
        .connection
        .execute(
            "DELETE FROM work_feed_entries
             WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND object_id = ?2",
            params![claim.run_id.0.to_string(), recorded[0].as_str()],
        )
        .expect("drop the oldest row's run-feed entry");
    assert!(matches!(
        store.work_run_evidence_projection(claim.run_id, MAX_FOCUS_RELATIONS),
        Err(StoreError::InvalidWorkProjection(message)) if message.contains("run-execution feed")
    ));
}
