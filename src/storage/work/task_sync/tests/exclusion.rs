use super::*;
use crate::{
    SessionId,
    domain::{EffectClass, NoteRequest, NoteVisibility, Scope, TurnIntent, TurnPurpose},
};

#[test]
fn task_sync_export_excludes_seeded_claim_grant_delivery_and_private_memory() {
    let mut source = SqliteStore::open_in_memory().unwrap();
    let project = ProjectId("project-a".into());
    let work = source
        .create_work(
            &root_request("project-a", "shared-task", 0),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let live_claim = claim(
        &mut source,
        &work,
        "private-session-marker",
        "live-claim",
        1,
        3600,
    );
    source
        .focus_work_session(
            &project,
            &SessionId("private-session-marker".into()),
            work.work_id,
            at(2),
        )
        .unwrap();
    let private = source
        .insert_historical_memory_fixture(
            &NoteRequest {
                project_id: project.clone(),
                task_id: None,
                work_id: Some(work.work_id),
                prose: "private-scratch-marker".into(),
                visibility: NoteVisibility::Private,
                kind: None,
                authority: None,
                sensitivity: None,
                title: None,
                tags: Vec::new(),
                evidence: Vec::new(),
                refs: Vec::new(),
                actor: actor("private-session-marker"),
                idempotency_key: "private-scratch".into(),
                created_at: at(2),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    assert!(matches!(private.scope, Scope::Agent { .. }));
    let binding = crate::storage::test_support::bind_control_for(
        &mut source,
        "control-session-marker",
        "bind-control",
        &[EffectClass::Observe],
        at(3),
    );
    let decision = source
        .evaluate_control_turn(
            &project,
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            &TurnIntent {
                idempotency_key: "private-grant-intent".into(),
                intent_fingerprint: ObjectId::from_canonical_bytes(b"private-grant-intent"),
                purpose: Some(TurnPurpose::Ordinary),
                requested_effects: vec![EffectClass::Observe],
                resource_intents: Vec::new(),
            },
            at(4),
        )
        .unwrap();
    let crate::ControlTurnDecision::Grant { grant } = decision else {
        panic!("fixture grant");
    };
    let payload =
        CanonicalObject::freeze(&serde_json::json!({"private_delivery_marker": true})).unwrap();
    assert!(
        source
            .stage_work_session_delivery(
                &project,
                &SessionId("delivery-session-marker".into()),
                crate::storage::StageWorkSessionDelivery {
                    expected_confirmed_through: 0,
                    expected_focused_work_id: None,
                    expected_bound_task_id: None,
                    delivered_through: 0,
                    delivered_entries: &[],
                    delivery_payload: payload.bytes(),
                    now: at(5)
                }
            )
            .unwrap()
            .is_some()
    );
    let before = state(&source);
    let documents = source
        .export_task_tree(&project)
        .unwrap()
        .documents()
        .unwrap();
    assert_eq!(state(&source), before, "export is read-only");
    let text = documents
        .values()
        .map(|bytes| std::str::from_utf8(bytes).unwrap())
        .collect::<Vec<_>>()
        .join("\n");
    for forbidden in [
        "private-session-marker",
        "control-session-marker",
        "delivery-session-marker",
        "private-scratch-marker",
        "private_delivery_marker",
        "claim_id",
        "claim_fence",
        "grant_id",
        "active_run_id",
        "session_id",
        live_claim.claim_id.0.to_string().as_str(),
        grant.grant_id.as_str(),
        private.version.as_str(),
    ] {
        assert!(
            !text.contains(forbidden),
            "excluded runtime field/value: {forbidden}"
        );
    }
    assert!(text.contains("Ship local work"));
    let mut target = SqliteStore::open_in_memory().unwrap();
    import(&mut target, &TaskTree::from_documents(&documents).unwrap());
    assert!(target.verify_all().unwrap().is_healthy());
}

#[test]
fn task_sync_completed_planning_satisfies_an_ordinary_prerequisite_without_credit() {
    let mut tree = candidate();
    let root = tree
        .tasks
        .iter_mut()
        .find(|task| task.title == "root")
        .unwrap();
    root.deferred_until = None;
    let root_id = root.id;
    let completed = tree
        .tasks
        .iter()
        .find(|task| task.lifecycle == WorkLifecycle::Completed)
        .unwrap()
        .id;
    let mut store = SqliteStore::open_in_memory().unwrap();
    import(&mut store, &tree);
    let ready = store.inspect_work(root_id, at(6)).unwrap();
    assert_eq!(ready.availability, crate::WorkAvailability::Ready);
    let item = query::load_work_item(&store.connection, completed).unwrap();
    assert!(query::work_completed_by_restored_record_on(&store.connection, &item).unwrap());
    assert_eq!(
        store
            .work_run_evidence(crate::WorkRunId(uuid::Uuid::new_v4()))
            .unwrap(),
        []
    );
}
