use super::*;

#[test]
fn rejection_correction_closed_root_is_typed_and_atomic() {
    let (_dir, verbs, path, project) = fixture();
    let root = add(&verbs, "Root", None, false, 0);
    let parent = add(&verbs, "Optional parent", Some(&root), true, 1);
    let child = add(&verbs, "Required grandchild", Some(&parent), false, 2);
    verbs
        .claim(
            ClaimInput {
                work_ref: root.clone(),
                ttl_seconds: Some(300),
                recover: None,
            },
            at(3),
        )
        .unwrap();
    let done = verbs
        .done(
            DoneInput {
                work_ref: Some(root),
                summary: Some("Root delivered".into()),
                ..DoneInput::default()
            },
            at(4),
        )
        .unwrap();
    assert!(!done.owed);
    let mut store = SqliteStore::open(&path).unwrap();
    let parent_item = store.resolve_work_ref(&project, &parent).unwrap();
    let child_item = store.resolve_work_ref(&project, &child).unwrap();
    assert_eq!(parent_item.lifecycle, WorkLifecycle::Open);
    assert_eq!(child_item.lifecycle, WorkLifecycle::Open);
    assert!(child_item.active_run_id.is_some());
    assert!(store.verify_all().unwrap().is_healthy());
    let connection = rusqlite::Connection::open(&path).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
    let error = store
        .reject_required_child(
            &crate::RejectRequiredChildRequest {
                work_id: child_item.work_id,
                expected_work_revision: child_item.revision,
                expected_parent_revision: Some(parent_item.revision),
                reason: "Evidence disproves the finding".into(),
                actor: crate::ActorContext {
                    actor_id: "agent".into(),
                    actor_kind: "test_agent".into(),
                    assurance: crate::domain::AssuranceLevel::Asserted,
                    run_id: None,
                    session_id: Some(SessionId("agent".into())),
                    source_tool: None,
                    source_skill: None,
                    provenance_chain: Vec::new(),
                    reason: "test rejection".into(),
                },
                idempotency_key: "closed-root-rejection".into(),
                rejected_at: at(5),
            },
            &crate::memory::DevelopmentNoopRedactor,
        )
        .unwrap_err();
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&connection).unwrap(),
        before
    );
    let payload = crate::mcp::store_error_value(&error);
    assert_eq!(payload["error"]["code"], "work_reject_refused");
    assert_eq!(payload["error"]["details"]["child_ref"], child);
    assert_eq!(payload["error"]["details"]["parent_ref"], parent);
    assert_eq!(
        payload["error"]["details"]["reason"],
        "the root execution is closed and cannot record a child waiver"
    );
    let remedy = payload["error"]["details"]["remedy"].as_str().unwrap();
    assert!(remedy.contains("if cancellation is admitted"));
    assert!(remedy.contains(&format!("{parent} is open and waivable")));
    let error = VerbError::at(error, &child);
    assert_eq!(
        error.guidance().next,
        vec![format!("engram work show {child}")]
    );
    // The actual word must expose the same typed refusal after protocol admission.
    let word_error = verbs
        .update(
            UpdateInput {
                work_ref: Some(child.clone()),
                action: UpdateAction::Reject {
                    reason: "Evidence disproves the finding".into(),
                },
            },
            at(6),
        )
        .unwrap_err();
    assert_eq!(
        crate::mcp::store_error_value(&word_error.error)["error"]["code"],
        "work_reject_refused"
    );
    assert_eq!(store.get_work_item(child_item.work_id).unwrap(), child_item);
    assert_eq!(
        store.get_work_item(parent_item.work_id).unwrap(),
        parent_item
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn rejection_correction_completed_child_keeps_late_finding_guidance() {
    let (_dir, verbs, path, project) = fixture();
    let parent = add(&verbs, "Parent", None, false, 0);
    let child = add(&verbs, "Delivered child", Some(&parent), false, 1);
    verbs
        .claim(
            ClaimInput {
                work_ref: child.clone(),
                ttl_seconds: Some(300),
                recover: None,
            },
            at(2),
        )
        .unwrap();
    let done = verbs
        .done(
            DoneInput {
                work_ref: Some(child.clone()),
                summary: Some("Child delivered".into()),
                ..DoneInput::default()
            },
            at(3),
        )
        .unwrap();
    assert!(!done.owed);
    let store = SqliteStore::open(path).unwrap();
    let before = store.resolve_work_ref(&project, &child).unwrap();
    let error = verbs
        .update(
            UpdateInput {
                work_ref: Some(child.clone()),
                action: UpdateAction::Reject {
                    reason: "Late finding".into(),
                },
            },
            at(4),
        )
        .unwrap_err();
    assert!(
        matches!(&error.error, StoreError::InvalidWork(reason) if reason == crate::work_service::COMPLETED_WORK_LATE_FINDING_REFUSAL)
    );
    assert_eq!(
        error.guidance().next,
        vec![format!("engram work note {child} \"…\"")]
    );
    assert_eq!(
        error.guidance().reminders,
        vec![crate::work_service::COMPLETED_WORK_LATE_FINDING_REFUSAL.to_string()]
    );
    assert_eq!(store.resolve_work_ref(&project, &child).unwrap(), before);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn rejection_explicit_service_retry_recovers_both_effects() {
    let (_dir, verbs, path, _) = fixture();
    let parent = add(&verbs, "Parent", None, false, 0);
    let child = add(&verbs, "Rejected child", Some(&parent), false, 1);
    let input = crate::work_service::WorkUpdateInput::Reject {
        reason: "Evidence rejected".into(),
        idempotency_key: "exact-rejection".into(),
    };
    let first = verbs
        .service
        .work_update_on(Some(&child), input.clone(), at(2))
        .unwrap();
    let second = verbs
        .service
        .work_update_on(Some(&child), input, at(3))
        .unwrap();
    assert_eq!(first.receipt, second.receipt);
    assert_eq!(second.receipt.result["parent_ref"], parent);
    assert_eq!(second.receipt.result["required_child_waived"], true);
    assert!(
        SqliteStore::open(path)
            .unwrap()
            .verify_all()
            .unwrap()
            .is_healthy()
    );
}

#[test]
fn rejection_cancels_required_child_and_waives_parent_without_claiming_it() {
    let (_dir, verbs, path, project) = fixture();
    let parent = add(&verbs, "Accepted parent", None, false, 0);
    let child = add(&verbs, "Disputed finding", Some(&parent), false, 1);
    let peer = AgentVerbs::new(
        path.clone(),
        project,
        "reviewer".into(),
        SessionId("reviewer".into()),
        None,
    );
    note(&peer, &child, "Evidence refutes the proposed finding", 2);
    let rejected = peer
        .update(
            UpdateInput {
                work_ref: Some(child.clone()),
                action: UpdateAction::Reject {
                    reason: "Evidence disproves this finding".into(),
                },
            },
            at(3),
        )
        .unwrap();
    assert_eq!(rejected.value["operation"], "reject");
    assert_eq!(
        rejected.value["receipt"]["result"]["lifecycle"],
        "cancelled"
    );
    assert_eq!(rejected.value["receipt"]["result"]["parent_ref"], parent);
    assert_eq!(
        rejected.value["receipt"]["result"]["required_child_waived"],
        true
    );
    assert!(
        rejected
            .text()
            .contains("cancelled child and recorded required-child waiver")
    );
    assert!(rejected.text().contains(&parent));
    let shown = verbs.show(&child, at(4)).unwrap();
    assert_eq!(shown.value["status"]["work"]["lifecycle"], "cancelled");
    verbs
        .claim(
            ClaimInput {
                work_ref: parent.clone(),
                ttl_seconds: Some(300),
                recover: None,
            },
            at(5),
        )
        .unwrap();
    let done = verbs
        .done(
            DoneInput {
                work_ref: Some(parent),
                summary: Some("Accepted outcome delivered; false finding rejected".into()),
                ..DoneInput::default()
            },
            at(6),
        )
        .unwrap();
    assert!(!done.owed);
    assert_eq!(done.value["acceptance_criteria_asserted"], 1);
    assert_eq!(done.value["acceptance_criteria_changed"], false);
    assert!(
        done.text()
            .contains("asserted 1 acceptance criterion satisfied; completion changed no criterion")
    );
    assert!(
        SqliteStore::open(path)
            .unwrap()
            .verify_all()
            .unwrap()
            .is_healthy()
    );
}

#[test]
fn rejection_unsupported_shapes_offer_conditional_two_word_remedy() {
    let (_dir, verbs, _, _) = fixture();
    let parent = add(&verbs, "Parent", None, false, 0);
    let optional = add(&verbs, "Optional", Some(&parent), true, 1);
    for reference in [&parent, &optional] {
        let error = verbs
            .update(
                UpdateInput {
                    work_ref: Some(reference.clone()),
                    action: UpdateAction::Reject {
                        reason: "Not accepted".into(),
                    },
                },
                at(2),
            )
            .unwrap_err();
        let text = error.to_string();
        assert!(text.contains("reject refused"));
        assert!(text.contains(&format!("engram work update {reference} --cancel")));
        assert_eq!(text.contains("--waive"), reference == &optional);
        assert!(!text.contains("PARENT"));
        assert!(!text.contains("CHILD"));
        let StoreError::WorkRejectRefused { reason, .. } = &error.error else {
            panic!("expected rejection refusal")
        };
        assert_eq!(error.guidance().reminders, vec![(*reason).to_string()]);
        assert_eq!(
            error.guidance().next,
            vec![format!("engram work show {reference}")]
        );
        assert!(text.contains("if"));
        assert_eq!(
            verbs.show(reference, at(3)).unwrap().value["status"]["work"]["lifecycle"],
            "open"
        );
    }
}

#[test]
fn rejection_done_disclosure_uses_unchanged_current_acceptance_and_replays() {
    let (_dir, verbs, _, _) = fixture();
    let created = verbs
        .add(
            AddInput {
                title: "Two assertions".into(),
                acceptance: vec!["one".into(), "two".into()],
                ..AddInput::default()
            },
            at(0),
        )
        .unwrap();
    let reference = created.value["work"]["short_ref"].as_str().unwrap();
    verbs
        .claim(
            ClaimInput {
                work_ref: reference.into(),
                ttl_seconds: Some(300),
                recover: None,
            },
            at(1),
        )
        .unwrap();
    let before = verbs.show(reference, at(2)).unwrap();
    for now in [3, 4] {
        let done = verbs
            .done(
                DoneInput {
                    work_ref: Some(reference.into()),
                    summary: Some("Both delivered".into()),
                    ..DoneInput::default()
                },
                at(now),
            )
            .unwrap();
        assert!(!done.owed);
        assert_eq!(done.value["acceptance_criteria_asserted"], 2);
        assert_eq!(done.value["acceptance_criteria_changed"], false);
        assert!(
            done.text()
                .contains("asserted 2 acceptance criteria satisfied")
        );
    }
    let after = verbs.show(reference, at(5)).unwrap();
    assert_eq!(
        before.value["status"]["work"]["acceptance"],
        after.value["status"]["work"]["acceptance"]
    );
}
