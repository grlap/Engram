use super::*;

#[test]
fn rejection_storage_bootstraps_restored_child_without_changing_parent_acceptance() {
    let (mut source, parent, child) = fixture();
    let saved = source
        .save_work_graph_snapshot(
            &parent.project_id,
            &actor("planner"),
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(2),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let mut store = SqliteStore::open_in_memory().unwrap();
    store
        .load_work_graph_snapshot(
            &parent.project_id,
            &actor("planner"),
            &serde_json::to_vec(&saved.document).unwrap(),
            false,
            at(3),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let parent = store
        .resolve_work_ref(&parent.project_id, &parent.short_ref)
        .unwrap();
    let child = store
        .resolve_work_ref(&child.project_id, &child.short_ref)
        .unwrap();
    assert!(child.restored && child.active_run_id.is_none());
    let mut request = request(&parent, &child);
    request.rejected_at = at(4);
    let result = store
        .reject_required_child(&request, &DevelopmentNoopRedactor)
        .unwrap();
    assert_eq!(result.child.lifecycle, WorkLifecycle::Cancelled);
    assert_eq!(result.waiver.reason, request.reason);
    assert_eq!(store.get_work_item(parent.work_id).unwrap(), parent);
    assert!(store.verify_all().unwrap().is_healthy());
}

fn fixture() -> (SqliteStore, WorkItem, WorkItem) {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let root = store
        .create_work(
            &root_request("reject-project", "reject-root", 0),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let decomposition = store
        .decompose_work(
            &DecomposeWorkRequest {
                parent_id: root.work_id,
                expected_parent_revision: root.revision,
                children: vec![child("required", ChildRequirement::Required, "Finding")],
                prerequisites: vec![],
                authority: delegated("reject-project", "planner"),
                actor: actor("planner"),
                idempotency_key: "children".into(),
                created_at: at(1),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    (
        store,
        decomposition.parent,
        decomposition.children[0].clone(),
    )
}

fn request(parent: &WorkItem, child: &WorkItem) -> RejectRequiredChildRequest {
    RejectRequiredChildRequest {
        work_id: child.work_id,
        expected_work_revision: child.revision,
        expected_parent_revision: Some(parent.revision),
        reason: "Evidence disproves the finding".into(),
        actor: actor("reviewer"),
        idempotency_key: "reject-once".into(),
        rejected_at: at(3),
    }
}

#[test]
fn rejection_storage_records_both_audits_and_replays_without_new_writes() {
    let (mut store, parent, child) = fixture();
    let parent_claim = claim(&mut store, &parent, "parent-holder", "parent-claim", 2, 100);
    let request = request(&parent, &child);
    let receipt = store
        .reject_required_child(&request, &DevelopmentNoopRedactor)
        .unwrap();
    assert_eq!(receipt.child.lifecycle, WorkLifecycle::Cancelled);
    assert_eq!(receipt.child.acceptance, child.acceptance);
    assert_eq!(receipt.waiver.work_revision, receipt.child.revision);
    assert_eq!(receipt.waiver.reason, request.reason);
    assert_eq!(receipt.waiver.waived_by, "reviewer");
    assert_eq!(receipt.parent_ref, parent.short_ref);
    assert_eq!(store.get_work_item(parent.work_id).unwrap(), parent);
    assert_eq!(
        load_work_claim_optional(&store.connection, parent_claim.run_id)
            .unwrap()
            .unwrap(),
        parent_claim
    );
    let reasons: Vec<(String, String)> = {
        let mut statement = store.connection.prepare("SELECT json_extract(canonical_json, '$.transition.kind'), json_extract(canonical_json, '$.transition.reason') FROM objects WHERE object_kind = 'work_event' AND json_extract(canonical_json, '$.transition.kind') IN ('disposed', 'required_child_waived') ORDER BY json_extract(canonical_json, '$.transition.kind')").unwrap();
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .map(Result::unwrap)
            .collect()
    };
    assert_eq!(
        reasons,
        vec![
            ("disposed".into(), request.reason.clone()),
            ("required_child_waived".into(), request.reason.clone())
        ]
    );
    let before = test_database_shape_snapshot(&store.connection).unwrap();
    assert_eq!(
        store
            .reject_required_child(&request, &DevelopmentNoopRedactor)
            .unwrap(),
        receipt
    );
    assert_eq!(
        test_database_shape_snapshot(&store.connection).unwrap(),
        before
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn rejection_storage_rolls_back_cancel_when_waiver_write_fails() {
    let (mut store, parent, child) = fixture();
    // Fail the real second transition after disposal and its operation result
    // have been written inside the transaction; no production hook is needed.
    store.connection.execute_batch("CREATE TEMP TRIGGER reject_waiver_failure BEFORE INSERT ON objects WHEN NEW.object_kind = 'work_event' AND json_extract(NEW.canonical_json, '$.transition.kind') = 'required_child_waived' BEGIN SELECT RAISE(ABORT, 'test waiver failure'); END;").unwrap();
    let before = test_database_shape_snapshot(&store.connection).unwrap();
    let error = store
        .reject_required_child(&request(&parent, &child), &DevelopmentNoopRedactor)
        .unwrap_err();
    assert!(error.to_string().contains("test waiver failure"));
    assert_eq!(
        test_database_shape_snapshot(&store.connection).unwrap(),
        before
    );
    assert_eq!(store.get_work_item(child.work_id).unwrap(), child);
    store
        .connection
        .execute_batch("DROP TRIGGER reject_waiver_failure")
        .unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn rejection_storage_retains_revision_and_live_child_authority_checks() {
    let (mut store, parent, child) = fixture();
    for stale_parent in [false, true] {
        let mut request = request(&parent, &child);
        if stale_parent {
            request.expected_parent_revision = Some(parent.revision - 1);
        } else {
            request.expected_work_revision -= 1;
        }
        let before = test_database_shape_snapshot(&store.connection).unwrap();
        assert!(matches!(
            store
                .reject_required_child(&request, &DevelopmentNoopRedactor)
                .unwrap_err(),
            StoreError::WorkRevisionConflict { work, .. } if work == if stale_parent { parent.work_id } else { child.work_id }
        ));
        assert_eq!(
            test_database_shape_snapshot(&store.connection).unwrap(),
            before
        );
    }
    claim(&mut store, &child, "child-holder", "child-claim", 2, 100);
    let before = test_database_shape_snapshot(&store.connection).unwrap();
    assert!(matches!(
        store
            .reject_required_child(&request(&parent, &child), &DevelopmentNoopRedactor)
            .unwrap_err(),
        StoreError::InvalidWork(reason) if reason == "actor session Some(\"reviewer\") does not match lifecycle holder \"child-holder\""
    ));
    assert_eq!(
        test_database_shape_snapshot(&store.connection).unwrap(),
        before
    );
    let mut holder_request = request(&parent, &child);
    holder_request.actor = actor("child-holder");
    let receipt = store
        .reject_required_child(&holder_request, &DevelopmentNoopRedactor)
        .unwrap();
    assert_eq!(receipt.child.lifecycle, WorkLifecycle::Cancelled);
    assert!(store.verify_all().unwrap().is_healthy());
}
