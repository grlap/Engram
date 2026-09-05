use super::*;

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one end-to-end round trip pins multiline work, completion history, and memory bytes"
)]
fn multiline_work_and_memory_save_and_reload_verbatim() {
    let directory = tempdir().expect("temp directory");
    let project = ProjectId("multiline-snapshot".into());
    let source = LocalWorkService::new(
        directory.path().join("source.db"),
        project.clone(),
        "source-agent".into(),
        SessionId("source-session".into()),
        None,
    );
    let title = "First line\n\tSecond line";
    let note = "Observed one thing\n\tAnd another";
    let completion = "Delivered one thing\n\tAnd another";
    let memory = "Remember one thing\n\tAnd another";
    let root = proposed_root(
        source
            .work_propose(root_input(title, "multiline-root"), at(0))
            .expect("multiline work"),
    );
    source
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "claim-multiline".into(),
            },
            at(1),
        )
        .expect("claim");
    source
        .work_note_on(Some(&root.short_ref), note, &[], at(2))
        .expect("multiline note");
    source
        .remember_project_memory(memory.into(), Some("multiline-memory".into()), at(3))
        .expect("multiline memory");
    assert!(matches!(
        source
            .work_complete(completion_input(completion, "complete-multiline"), at(4))
            .expect("multiline completion"),
        WorkCompleteResult::Completed(_)
    ));
    let saved = source
        .save_work_graph_snapshot(None, WorkGraphSnapshotDestinationKind::Stdout, at(5))
        .expect("save multiline store");
    assert_eq!(saved.document.body.items[0].title, title);
    let WorkGraphSnapshotRecordPayload::Native { history } =
        &saved.document.body.records[0].payload
    else {
        panic!("native history");
    };
    assert!(history.notes.iter().any(|entry| entry.summary == note));
    assert_eq!(
        history
            .completion
            .as_ref()
            .expect("completion proof")
            .summary,
        completion
    );
    let destination = LocalWorkService::new(
        directory.path().join("destination.db"),
        project,
        "destination-agent".into(),
        SessionId("destination-session".into()),
        None,
    );
    destination
        .load_work_graph_snapshot(
            &serde_json::to_vec(&saved.document).expect("bytes"),
            false,
            at(6),
        )
        .expect("load multiline store");
    let saved_again = destination
        .save_work_graph_snapshot(None, WorkGraphSnapshotDestinationKind::Stdout, at(7))
        .expect("save loaded store");
    assert_eq!(saved_again.document.body.items[0].title, title);
    let WorkGraphSnapshotRecordPayload::Restored { canonical_json, .. } =
        &saved_again.document.body.records[0].payload
    else {
        panic!("restored history");
    };
    let record: crate::RestoredRecord =
        serde_json::from_value(canonical_json.clone()).expect("record");
    assert!(
        record
            .history
            .notes
            .iter()
            .any(|entry| entry.summary == note)
    );
    assert_eq!(
        record
            .history
            .completion
            .as_ref()
            .expect("completion proof")
            .summary,
        completion
    );
    let crate::WorkGraphSnapshotMemoryState::Active {
        body: crate::WorkGraphSnapshotText::Present { value },
        ..
    } = &saved_again.document.body.memories[0].state
    else {
        panic!("active memory");
    };
    assert_eq!(value, memory);
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one scenario proves a restored child completion satisfies the native parent seal"
)]
fn restored_completed_child_is_bound_into_a_new_parent_seal() {
    let directory = tempdir().expect("temp directory");
    let source_database = directory.path().join("source.sqlite3");
    let destination_database = directory.path().join("destination.sqlite3");
    let project = ProjectId("restored-child-completion".into());
    let source = LocalWorkService::new(
        source_database,
        project.clone(),
        "source-agent".into(),
        SessionId("source-session".into()),
        Some("protocol-test".into()),
    );
    let root = proposed_root(
        source
            .work_propose(root_input("Restored parent", "restored-parent"), at(0))
            .expect("root"),
    );
    let children = source
        .work_propose(
            WorkProposeInput::Decompose {
                children: vec![WorkChildInput {
                    notes: Vec::new(),
                    key: "required-child".into(),
                    title: "Restored required child".into(),
                    outcome: "child outcome".into(),
                    acceptance: vec!["child accepted".into()],
                    requirement: Some(ChildRequirement::Required),
                    kind: Some(WorkItemKind::Task),
                    priority: Some(1),
                    labels: Vec::new(),
                    assigned_to: None,
                    deferred_until: None,
                }],
                prerequisites: Vec::new(),
                idempotency_key: "decompose-restored-parent".into(),
            },
            at(1),
        )
        .expect("decompose root");
    let WorkProposeResult::Decomposition(children) = children else {
        panic!("expected child decomposition");
    };
    let child = &children.children[0];
    source
        .work_focus(&child.short_ref, at(2))
        .expect("focus child");
    source
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "claim-child-before-save".into(),
            },
            at(3),
        )
        .expect("claim child");
    assert!(matches!(
        source
            .work_complete(completion_input("child complete", "complete-child"), at(4))
            .expect("complete child"),
        WorkCompleteResult::Completed(_)
    ));
    let snapshot = source
        .save_work_graph_snapshot(None, WorkGraphSnapshotDestinationKind::Stdout, at(5))
        .expect("save source graph");
    let bytes = serde_json::to_vec_pretty(&snapshot.document).expect("serialize snapshot");

    let destination = LocalWorkService::new(
        destination_database.clone(),
        project.clone(),
        "destination-agent".into(),
        SessionId("destination-session".into()),
        Some("protocol-test".into()),
    );
    destination
        .load_work_graph_snapshot(&bytes, false, at(6))
        .expect("load graph");
    destination
        .work_focus(&root.short_ref, at(7))
        .expect("focus restored parent");
    destination
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "claim-restored-parent".into(),
            },
            at(8),
        )
        .expect("claim restored parent");
    let completed = destination
        .work_complete(
            completion_input("parent complete", "complete-restored-parent"),
            at(9),
        )
        .expect("complete restored parent");
    let WorkCompleteResult::Completed(completed) = completed else {
        panic!("restored child must satisfy the required-child barrier");
    };
    let late_note = destination
        .work_note_on(
            Some(&root.short_ref),
            "native completion late note",
            &["review:native-note".into()],
            at(10),
        )
        .expect("record native late note");
    let late_note_hash: ObjectHash =
        serde_json::from_value(late_note.evidence.result).expect("late note hash");
    let late_gate = destination
        .work_gate_on(
            Some(&root.short_ref),
            "cargo-test",
            &[],
            Some("review:native-gate"),
            at(11),
        )
        .expect("record native late gate");
    let late_gate_hash: ObjectHash =
        serde_json::from_value(late_gate.receipt.result).expect("late gate hash");
    let store = SqliteStore::open(destination_database).expect("destination store");
    let seal: CompletionSeal = store
        .get(&completed.seal)
        .expect("read parent seal")
        .expect("parent seal");
    assert!(seal.restored);
    assert_eq!(seal.restored_child_completions.len(), 1);
    let record: crate::RestoredRecord = store
        .get(&seal.restored_child_completions[0])
        .expect("read restored child completion")
        .expect("restored child completion");
    assert_eq!(record.work_id, child.work_id);
    assert!(record.history.completion.is_some());
    assert!(
        store
            .restored_work_evidence(root.work_id)
            .expect("restored evidence")
            .is_empty()
    );
    let native_evidence = store
        .work_run_evidence(seal.run_id)
        .expect("native run evidence");
    assert!(native_evidence.contains(&late_note_hash));
    assert!(native_evidence.contains(&late_gate_hash));
    assert!(store.verify_all().expect("verify destination").is_healthy());
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the complete restore/reopen/reseal lifecycle proves late evidence uses fresh authority"
)]
fn restored_completed_item_reopens_into_a_fresh_native_run() {
    let directory = tempdir().expect("temp directory");
    let source_database = directory.path().join("source-reopen.sqlite3");
    let destination_database = directory.path().join("destination-reopen.sqlite3");
    let project = ProjectId("restored-reopen".into());
    let source = LocalWorkService::new(
        source_database,
        project.clone(),
        "source-agent".into(),
        SessionId("source-session".into()),
        Some("protocol-test".into()),
    );
    let root = proposed_root(
        source
            .work_propose(root_input("Completed before save", "completed-root"), at(0))
            .expect("root"),
    );
    source
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "claim-completed-root".into(),
            },
            at(1),
        )
        .expect("claim root");
    assert!(matches!(
        source
            .work_complete(completion_input("complete", "complete-root"), at(2))
            .expect("complete root"),
        WorkCompleteResult::Completed(_)
    ));
    let snapshot = source
        .save_work_graph_snapshot(None, WorkGraphSnapshotDestinationKind::Stdout, at(3))
        .expect("save completed root");
    let bytes = serde_json::to_vec_pretty(&snapshot.document).expect("serialize snapshot");
    let destination = LocalWorkService::new(
        destination_database.clone(),
        project.clone(),
        "destination-agent".into(),
        SessionId("destination-session".into()),
        Some("protocol-test".into()),
    );
    destination
        .load_work_graph_snapshot(&bytes, false, at(4))
        .expect("load completed root");
    destination
        .work_focus(&root.short_ref, at(5))
        .expect("focus completed root");
    let reopened = destination
        .work_update(
            WorkUpdateInput::Reopen {
                reason: "new native generation".into(),
                idempotency_key: "reopen-restored-root".into(),
            },
            at(6),
        )
        .expect("reopen restored root");
    destination
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "claim-reopened-root".into(),
            },
            at(7),
        )
        .expect("claim reopened root");
    let completed = destination
        .work_complete(
            completion_input("fresh native completion", "complete-reopened-root"),
            at(8),
        )
        .expect("complete reopened root");
    let WorkCompleteResult::Completed(completed) = completed else {
        panic!("reopened root must complete");
    };
    let late_note = destination
        .work_note_on(
            Some(&root.short_ref),
            "late finding on the fresh seal",
            &["review:fresh-seal".into()],
            at(9),
        )
        .expect("record late finding on fresh seal");
    let late_note_hash: ObjectHash =
        serde_json::from_value(late_note.evidence.result).expect("late note hash");
    let focus = destination
        .work_focus(&root.short_ref, at(10))
        .expect("focus fresh native completion");
    assert!(!focus.completed_by_record);
    assert_eq!(
        focus
            .latest_evidence_item
            .as_ref()
            .map(|item| &item.evidence),
        Some(&late_note_hash)
    );
    let store = SqliteStore::open(destination_database).expect("destination store");
    let run_id: WorkRunId =
        serde_json::from_value(reopened.receipt.result["run_id"].clone()).expect("reopened run id");
    let run = store.get_work_run(run_id).expect("reopened run");
    assert_eq!(run.work_id, root.work_id);
    assert_eq!(run.state, WorkRunState::Completed);
    let item = store.get_work_item(root.work_id).expect("reopened item");
    assert_eq!(item.lifecycle, WorkLifecycle::Completed);
    assert_eq!(item.active_run_id, None);
    assert!(item.restored);
    assert_eq!(completed.run_id, run.run_id);
    assert!(
        store
            .work_run_evidence(run.run_id)
            .expect("fresh run evidence")
            .contains(&late_note_hash)
    );
    assert!(
        store
            .restored_work_evidence(root.work_id)
            .expect("restored evidence")
            .is_empty()
    );
    assert!(
        store
            .verify_all()
            .expect("verify reopened graph")
            .is_healthy()
    );
    let second_reopen = destination
        .work_update(
            WorkUpdateInput::Reopen {
                reason: "another native generation".into(),
                idempotency_key: "reopen-restored-root-again".into(),
            },
            at(11),
        )
        .expect("restored-origin root reopens again after native completion");
    let second_run_id: WorkRunId =
        serde_json::from_value(second_reopen.receipt.result["run_id"].clone())
            .expect("second reopened run id");
    let second_run = store.get_work_run(second_run_id).expect("second run");
    assert_eq!(second_run.generation, run.generation + 1);
    assert_ne!(second_run.root_execution_id, run.root_execution_id);
    assert!(
        store
            .verify_all()
            .expect("verify second reopen")
            .is_healthy()
    );
}

#[allow(
    clippy::too_many_lines,
    reason = "shared fixture creates both restored-origin completion histories through the service"
)]
fn restored_native_completed_child(
    directory: &std::path::Path,
    completed_before_save: bool,
) -> (
    LocalWorkService,
    WorkItemSummary,
    WorkDecompositionChildSummary,
) {
    let project = ProjectId("restored-current-authority".into());
    let source = LocalWorkService::new(
        directory.join("source.sqlite3"),
        project.clone(),
        "source-agent".into(),
        SessionId("source-session".into()),
        Some("protocol-test".into()),
    );
    let parent = proposed_root(
        source
            .work_propose(root_input("Parent", "parent"), at(0))
            .expect("parent"),
    );
    let WorkProposeResult::Decomposition(children) = source
        .work_propose(
            WorkProposeInput::Decompose {
                children: vec![WorkChildInput {
                    notes: Vec::new(),
                    key: "child".into(),
                    title: "Required child".into(),
                    outcome: "Child outcome".into(),
                    acceptance: vec!["Child accepted".into()],
                    requirement: Some(ChildRequirement::Required),
                    kind: None,
                    priority: None,
                    labels: Vec::new(),
                    assigned_to: None,
                    deferred_until: None,
                }],
                prerequisites: Vec::new(),
                idempotency_key: "decompose".into(),
            },
            at(1),
        )
        .expect("decompose")
    else {
        panic!("expected children")
    };
    let child = children.children[0].clone();
    if completed_before_save {
        source
            .work_focus(&child.short_ref, at(2))
            .expect("focus source child");
        source
            .work_update(
                WorkUpdateInput::Claim {
                    ttl_seconds: Some(300),
                    recovery_reason: None,
                    idempotency_key: "source-claim".into(),
                },
                at(3),
            )
            .expect("claim source child");
        assert!(matches!(
            source
                .work_complete(
                    completion_input("source child complete", "source-complete"),
                    at(4)
                )
                .expect("complete source child"),
            WorkCompleteResult::Completed(_)
        ));
    }
    let saved = source
        .save_work_graph_snapshot(None, WorkGraphSnapshotDestinationKind::Stdout, at(5))
        .expect("save");
    let destination = LocalWorkService::new(
        directory.join("destination.sqlite3"),
        project,
        "destination-agent".into(),
        SessionId("destination-session".into()),
        Some("protocol-test".into()),
    );
    destination
        .load_work_graph_snapshot(
            &serde_json::to_vec(&saved.document).expect("snapshot bytes"),
            false,
            at(6),
        )
        .expect("load");
    destination
        .work_focus(&child.short_ref, at(7))
        .expect("focus child");
    if completed_before_save {
        destination
            .work_update(
                WorkUpdateInput::Reopen {
                    reason: "native completion required".into(),
                    idempotency_key: "reopen-child".into(),
                },
                at(8),
            )
            .expect("reopen completed-by-record child");
    }
    destination
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "destination-child-claim".into(),
            },
            at(9),
        )
        .expect("claim restored child");
    assert!(matches!(
        destination
            .work_complete(
                completion_input("native child complete", "native-child-complete"),
                at(10)
            )
            .expect("native completion"),
        WorkCompleteResult::Completed(_)
    ));
    destination
        .work_focus(&parent.short_ref, at(11))
        .expect("focus parent");
    destination
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "destination-parent-claim".into(),
            },
            at(12),
        )
        .expect("claim parent");
    (destination, parent, child)
}

#[test]
fn restored_origin_required_child_reopen_removes_the_native_parent_barrier_credit() {
    let directory = tempdir().expect("directory");
    let (service, parent, child) = restored_native_completed_child(directory.path(), false);
    service
        .work_focus(&child.short_ref, at(13))
        .expect("focus completed child");
    service
        .work_update(
            WorkUpdateInput::Reopen {
                reason: "child needs another pass".into(),
                idempotency_key: "reopen-native-child".into(),
            },
            at(14),
        )
        .expect("reopen native child");
    service
        .work_focus(&parent.short_ref, at(15))
        .expect("focus parent");
    assert!(matches!(
        service.work_complete(completion_input("parent too early", "parent-too-early"), at(16)).expect("typed barrier refusal"),
        WorkCompleteResult::Refused(WorkCompleteRefusal {
            recovery: WorkCompletionRecovery { cause: WorkCompletionRecoveryCause::RequiredChildUnsealed { child: missing }, .. }, ..
        }) if missing == child.work_id
    ));
    let store = SqliteStore::open(directory.path().join("destination.sqlite3")).expect("store");
    let connection = rusqlite::Connection::open(directory.path().join("destination.sqlite3"))
        .expect("connection");
    let root_execution_bytes: Vec<u8> = connection
        .query_row(
            "SELECT execution_json FROM work_root_executions WHERE root_id = ?1",
            [parent.work_id.0.to_string()],
            |row| row.get(0),
        )
        .expect("root execution projection");
    let root_execution: crate::RootExecution =
        serde_json::from_slice(&root_execution_bytes).expect("root execution");
    assert!(root_execution.required_child_seals.is_empty());
    assert!(
        store
            .verify_all()
            .expect("verify reopened child")
            .is_healthy()
    );
}

#[test]
fn restored_native_completion_missing_seal_projection_refuses_evidence_and_parent_seal() {
    let directory = tempdir().expect("directory");
    let (service, parent, child) = restored_native_completed_child(directory.path(), true);
    let connection = rusqlite::Connection::open(directory.path().join("destination.sqlite3"))
        .expect("connection");
    assert_eq!(
        connection
            .execute(
                "DELETE FROM work_completion_seals WHERE work_id = ?1",
                [child.work_id.0.to_string()]
            )
            .expect("remove seal projection"),
        1
    );
    let objects_before: i64 = connection
        .query_row("SELECT COUNT(*) FROM objects", [], |row| row.get(0))
        .expect("object count");
    assert!(matches!(
        service.work_note_on(
            Some(&child.short_ref),
            "must not bind stale history",
            &[],
            at(13)
        ),
        Err(StoreError::InvalidWorkProjection(_))
    ));
    assert!(matches!(
        service.work_gate_on(Some(&child.short_ref), "cargo-test", &[], None, at(14)),
        Err(StoreError::InvalidWorkProjection(_))
    ));
    let objects_after: i64 = connection
        .query_row("SELECT COUNT(*) FROM objects", [], |row| row.get(0))
        .expect("object count after refusals");
    assert_eq!(
        objects_after, objects_before,
        "refused evidence must not mint canonical objects"
    );
    service
        .work_focus(&parent.short_ref, at(15))
        .expect("focus parent");
    assert!(matches!(
        service.work_complete(
            completion_input("must not use stale completion", "missing-seal-parent"),
            at(16)
        ),
        Err(StoreError::InvalidWorkProjection(_))
    ));
    let seals: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM work_completion_seals WHERE work_id = ?1",
            [parent.work_id.0.to_string()],
            |row| row.get(0),
        )
        .expect("parent seal count");
    assert_eq!(
        seals, 0,
        "no parent seal may consume the stale restored completion"
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one scenario proves late note and gate transitions over restored completion authority"
)]
fn restored_completion_accepts_late_notes_and_gate_transitions() {
    let directory = tempdir().expect("temp directory");
    let source_database = directory.path().join("source-late-findings.sqlite3");
    let destination_database = directory.path().join("destination-late-findings.sqlite3");
    let project = ProjectId("restored-late-findings".into());
    let source = LocalWorkService::new(
        source_database,
        project.clone(),
        "source-agent".into(),
        SessionId("source-session".into()),
        Some("protocol-test".into()),
    );
    let root = proposed_root(
        source
            .work_propose(
                root_input("Completed before review", "reviewed-root"),
                at(0),
            )
            .expect("root"),
    );
    source
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "claim-reviewed-root".into(),
            },
            at(1),
        )
        .expect("claim root");
    assert!(matches!(
        source
            .work_complete(
                completion_input("complete", "complete-reviewed-root"),
                at(2)
            )
            .expect("complete root"),
        WorkCompleteResult::Completed(_)
    ));
    let snapshot = source
        .save_work_graph_snapshot(None, WorkGraphSnapshotDestinationKind::Stdout, at(3))
        .expect("save completed root");
    let bytes = serde_json::to_vec_pretty(&snapshot.document).expect("serialize snapshot");
    let destination = LocalWorkService::new(
        destination_database.clone(),
        project.clone(),
        "review-agent".into(),
        SessionId("review-session".into()),
        Some("protocol-test".into()),
    );
    destination
        .load_work_graph_snapshot(&bytes, false, at(4))
        .expect("load completed root");
    destination
        .work_focus(&root.short_ref, at(5))
        .expect("focus completed root");

    let note = destination
        .work_note_on(
            Some(&root.short_ref),
            "late review found one important detail",
            &["review:late-note".into()],
            at(6),
        )
        .expect("append late note");
    let note_hash: ObjectHash =
        serde_json::from_value(note.evidence.result).expect("late note hash");
    let failed_gate = destination
        .work_gate_on(
            Some(&root.short_ref),
            "cargo-test",
            &["late::regression".into()],
            Some("review:failed-gate"),
            at(7),
        )
        .expect("append failed late gate");
    let failed_gate_hash: ObjectHash =
        serde_json::from_value(failed_gate.receipt.result).expect("failed gate hash");
    let passed_gate = destination
        .work_gate_on(
            Some(&root.short_ref),
            "cargo-test",
            &[],
            Some("review:passed-gate"),
            at(7),
        )
        .expect("append passing late gate at the same timestamp");
    let passed_gate_hash: ObjectHash =
        serde_json::from_value(passed_gate.receipt.result).expect("passed gate hash");

    let store = SqliteStore::open(&destination_database).expect("destination store");
    let evidence = store
        .restored_work_evidence(root.work_id)
        .expect("restored evidence");
    assert_eq!(evidence.len(), 3);
    let failed = evidence
        .iter()
        .find(|(hash, _)| hash == &failed_gate_hash)
        .and_then(|(_, evidence)| evidence.gate.as_ref())
        .expect("failed gate evidence");
    assert!(!failed.passed);
    assert!(failed.previous.is_none());
    let passed = evidence
        .iter()
        .find(|(hash, _)| hash == &passed_gate_hash)
        .and_then(|(_, evidence)| evidence.gate.as_ref())
        .expect("passed gate evidence");
    assert!(passed.passed);
    assert_eq!(passed.previous.as_ref(), Some(&failed_gate_hash));
    let focus = destination
        .work_focus(&root.short_ref, at(8))
        .expect("focus with late findings");
    assert_eq!(focus.evidence_count, 3);
    assert!(
        focus
            .evidence_items
            .iter()
            .any(|item| item.evidence == note_hash)
    );
    assert_eq!(
        focus
            .latest_evidence_item
            .as_ref()
            .and_then(|item| item.gate.as_ref())
            .map(|gate| (gate.name.as_str(), gate.passed)),
        Some(("cargo-test", true))
    );
    assert!(
        !focus
            .omissions
            .iter()
            .any(|omission| { omission.reason == WorkSectionOmissionReason::EvidenceCountLimit })
    );

    let saved_again = destination
        .save_work_graph_snapshot(None, WorkGraphSnapshotDestinationKind::Stdout, at(9))
        .expect("save restored graph with late findings");
    let native_history = saved_again
        .document
        .body
        .records
        .iter()
        .find_map(|record| match &record.payload {
            WorkGraphSnapshotRecordPayload::Native { history }
                if record.work_id == root.work_id =>
            {
                Some(history)
            }
            _ => None,
        })
        .expect("late findings become a native history layer");
    assert_eq!(native_history.notes.len(), 3);
    assert!(native_history.notes.iter().any(|note| {
        note.gate
            .as_ref()
            .is_some_and(|gate| gate.name == "cargo-test" && gate.passed)
    }));
    assert!(native_history.completion.is_some());
    let projection_connection = rusqlite::Connection::open(&destination_database)
        .expect("open restored projection fixture");
    projection_connection
        .execute("DELETE FROM work_restored_evidence", [])
        .expect("remove restored evidence projection");
    projection_connection
        .execute("DELETE FROM work_restored_records", [])
        .expect("remove restored record projection");
    drop(projection_connection);
    drop(store);
    let repair = SqliteStore::repair_rebuildable_projections(&destination_database)
        .expect("repair restored projections from canonical objects");
    assert!(repair.is_healthy(), "{repair:?}");
    let repaired = SqliteStore::open(&destination_database).expect("open repaired store");
    assert_eq!(
        repaired
            .restored_work_evidence(root.work_id)
            .expect("repaired restored evidence")
            .len(),
        3
    );
    assert_eq!(
        repaired
            .work_restored_records(root.work_id)
            .expect("repaired restored records")
            .len(),
        1
    );
    assert!(
        repaired
            .verify_all()
            .expect("verify repaired store")
            .is_healthy()
    );
    let bytes_again =
        serde_json::to_vec_pretty(&saved_again.document).expect("serialize late snapshot");
    let replayed = LocalWorkService::new(
        directory.path().join("replayed-late-findings.sqlite3"),
        project.clone(),
        "replay-agent".into(),
        SessionId("replay-session".into()),
        Some("protocol-test".into()),
    );
    replayed
        .load_work_graph_snapshot(&bytes_again, false, at(10))
        .expect("load graph after late findings were saved");
    let replayed_focus = replayed
        .work_focus(&root.short_ref, at(11))
        .expect("focus replayed completed root");
    assert_eq!(
        replayed_focus.status.work.lifecycle,
        WorkLifecycle::Completed
    );
    assert_eq!(replayed_focus.evidence_count, 0);
    assert!(replayed_focus.restored_history.total >= 4);
    assert!(
        replayed_focus
            .restored_history
            .items
            .iter()
            .any(|entry| entry.kind == "completed")
    );
    assert!(
        repaired
            .verify_all()
            .expect("verify restored late findings")
            .is_healthy()
    );

    // Keep the original bounded-history and repair checks above unchanged;
    // the additional observation is asserted through its own live receipt.
    let operational = rusqlite::Connection::open(&destination_database).expect("operational rows");
    let operation_counts = || {
        ["work_operation_results", "work_protocol_attempts"].map(|table| {
            operational
                .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("operation count")
        })
    };
    let before_append = operation_counts();
    let repeated_gate = destination
        .work_gate_on(
            Some(&root.short_ref),
            "cargo-test",
            &[],
            Some("review:passed-gate"),
            at(7),
        )
        .expect("identical late gate appends even at the same timestamp");
    let repeated_gate_hash: ObjectHash =
        serde_json::from_value(repeated_gate.receipt.result).expect("repeated gate hash");
    assert_ne!(repeated_gate_hash, passed_gate_hash);
    assert_eq!(
        operation_counts(),
        before_append,
        "append-only gates create no unreachable retry rows"
    );
    let evidence = repaired
        .restored_work_evidence(root.work_id)
        .expect("repeated evidence");
    assert_eq!(evidence.len(), 4);
    let repeated = evidence
        .iter()
        .find(|(hash, _)| hash == &repeated_gate_hash)
        .and_then(|(_, evidence)| evidence.gate.as_ref())
        .expect("repeated gate evidence");
    assert!(repeated.passed);
    assert_eq!(repeated.previous.as_ref(), Some(&passed_gate_hash));
    let words = crate::AgentVerbs::new(
        destination_database,
        project,
        "review-agent".into(),
        SessionId("review-session".into()),
        Some("protocol-test".into()),
    );
    let shown = words
        .show(&root.short_ref, at(12))
        .expect("show repeated gates");
    assert_eq!(
        shown.value["notes"]
            .as_array()
            .expect("show notes")
            .iter()
            .filter(|note| note["summary"]
                .as_str()
                .is_some_and(|summary| summary.starts_with("gate cargo-test passed")))
            .count(),
        2,
        "show retains both identical late passing observations"
    );
    assert!(
        repaired
            .verify_all()
            .expect("verify repeated late gate")
            .is_healthy()
    );
}
