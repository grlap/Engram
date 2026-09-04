use super::super::test_support::*;
use super::super::*;
use crate::WorkGraphSnapshotRecordPayload;
use crate::verbs::{AgentVerbs, DoneInput, UpdateAction, UpdateInput};
use chrono::Duration;
use tempfile::tempdir;

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
        project,
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
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one scenario shows the omitted, explicit-empty, and synthesized defaults side by side"
)]
fn omitted_checkpoint_evidence_and_acceptance_take_safe_defaults() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("safe-defaults".into());
    let service = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("safe-defaults-session".into()),
        Some("protocol-test".into()),
    );
    service
        .work_propose(
            WorkProposeInput::Root {
                title: "Safe defaults".into(),
                outcome: "omitted fields do the safe thing".into(),
                acceptance: vec!["first criterion".into(), "second criterion".into()],
                work_kind: None,
                priority: None,
                labels: Vec::new(),
                assigned_to: None,
                deferred_until: None,
                idempotency_key: "defaults-root".into(),
            },
            at(0),
        )
        .expect("root");
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "defaults-claim".into(),
            },
            at(1),
        )
        .expect("claim");
    let evidence = |summary: &str, key: &str| WorkUpdateInput::Evidence {
        summary: summary.into(),
        refs: vec![format!("test:{key}")],
        attach: None,
        idempotency_key: key.into(),
    };
    let first_evidence: ObjectHash = serde_json::from_value(
        service
            .work_update(evidence("first finding", "defaults-evidence-1"), at(2))
            .expect("first evidence")
            .receipt
            .result,
    )
    .expect("evidence hash");
    let second_evidence: ObjectHash = serde_json::from_value(
        service
            .work_update(evidence("second finding", "defaults-evidence-2"), at(3))
            .expect("second evidence")
            .receipt
            .result,
    )
    .expect("evidence hash");

    // Omitted evidence snapshots everything already on the run.
    let checkpoint: ObjectHash = serde_json::from_value(
        service
            .work_update(
                WorkUpdateInput::Checkpoint {
                    summary: "progress".into(),
                    evidence: None,
                    idempotency_key: "defaults-checkpoint".into(),
                },
                at(4),
            )
            .expect("checkpoint")
            .receipt
            .result,
    )
    .expect("checkpoint hash");
    let stored_checkpoint = SqliteStore::open(&database)
        .expect("store")
        .get::<WorkCheckpoint>(&checkpoint)
        .expect("read checkpoint")
        .expect("canonical checkpoint");
    let mut expected = vec![first_evidence.clone(), second_evidence.clone()];
    expected.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    assert_eq!(stored_checkpoint.evidence, expected);

    // Explicit empty still acknowledges none.
    let empty: ObjectHash = serde_json::from_value(
        service
            .work_update(
                WorkUpdateInput::Checkpoint {
                    summary: "explicitly none".into(),
                    evidence: Some(Vec::new()),
                    idempotency_key: "defaults-checkpoint-empty".into(),
                },
                at(5),
            )
            .expect("empty checkpoint")
            .receipt
            .result,
    )
    .expect("checkpoint hash");
    assert!(
        SqliteStore::open(&database)
            .expect("store")
            .get::<WorkCheckpoint>(&empty)
            .expect("read checkpoint")
            .expect("canonical checkpoint")
            .evidence
            .is_empty()
    );

    // Omitted acceptance asserts every criterion with the server note.
    let completed = service
        .work_complete(
            WorkCompleteInput {
                capture: Some(WorkCompletionCaptureInput {
                    summary: "delivered".into(),
                    refs: Vec::new(),
                }),
                evidence: Vec::new(),
                acceptance: None,
                note: None,
                idempotency_key: "defaults-complete".into(),
            },
            at(6),
        )
        .expect("complete");
    let WorkCompleteResult::Completed(receipt) = completed else {
        panic!("completion must seal");
    };
    let seal = SqliteStore::open(&database)
        .expect("store")
        .get::<CompletionSeal>(&receipt.seal)
        .expect("read seal")
        .expect("canonical seal");
    assert_eq!(
        seal.acceptance
            .iter()
            .map(|result| (
                result.criterion.as_str(),
                result.satisfied,
                result.note.as_str()
            ))
            .collect::<Vec<_>>(),
        vec![
            ("first criterion", true, "accepted by agent via work done"),
            ("second criterion", true, "accepted by agent via work done"),
        ]
    );
}

#[test]
fn explicit_empty_acceptance_still_fails_and_note_needs_omitted_acceptance() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("strict-acceptance".into());
    let service = LocalWorkService::new(
        database,
        project,
        "agent".into(),
        SessionId("strict-acceptance-session".into()),
        Some("protocol-test".into()),
    );
    service
        .work_propose(root_input("Strict acceptance", "strict-root"), at(0))
        .expect("root");
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "strict-claim".into(),
            },
            at(1),
        )
        .expect("claim");
    let complete = |acceptance: Option<Vec<WorkAcceptanceInput>>, note: Option<&str>, key: &str| {
        WorkCompleteInput {
            capture: Some(WorkCompletionCaptureInput {
                summary: "delivered".into(),
                refs: Vec::new(),
            }),
            evidence: Vec::new(),
            acceptance,
            note: note.map(str::to_owned),
            idempotency_key: key.into(),
        }
    };
    let refused = service
        .work_complete(complete(Some(Vec::new()), None, "strict-empty"), at(2))
        .expect("missing acceptance is a typed protocol refusal");
    let WorkCompleteResult::Refused(refusal) = refused else {
        panic!("explicit empty acceptance must not complete work with criteria");
    };
    assert_eq!(refusal.code, "missing_acceptance");
    let recovery = refusal.recovery;
    assert!(matches!(
        recovery.cause,
        WorkCompletionRecoveryCause::MissingAcceptance { ref criterion }
            if criterion == "Strict acceptance accepted"
    ));
    assert_eq!(recovery.item.title, "Strict acceptance");
    assert!(recovery.command.starts_with("engram work done "));
    assert!(matches!(
        service.work_complete(
            complete(
                Some(vec![WorkAcceptanceInput {
                    criterion: None,
                    satisfied: true,
                    evidence: Vec::new(),
                    note: "explicit".into(),
                }]),
                Some("stray note"),
                "strict-note-conflict",
            ),
            at(3),
        ),
        Err(StoreError::InvalidWork(_))
    ));
    let WorkCompleteResult::Completed(_) = service
        .work_complete(complete(None, Some("reviewed by hand"), "strict-ok"), at(4))
        .expect("omitted acceptance with a note completes")
    else {
        panic!("completion must seal");
    };
}

#[test]
fn completion_on_a_lapsed_claim_refuses_without_retaking() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("completion-lapsed-claim".into());
    let service = LocalWorkService::new(
        database,
        project,
        "agent".into(),
        SessionId("completion-lapsed-session".into()),
        Some("protocol-test".into()),
    );
    let work = match service
        .work_propose(
            root_input("Lapsed completion", "lapsed-completion-root"),
            at(0),
        )
        .expect("root")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(1),
                recovery_reason: None,
                idempotency_key: "lapsed-completion-claim".into(),
            },
            at(1),
        )
        .expect("claim");

    let input = WorkCompleteInput {
        capture: Some(WorkCompletionCaptureInput {
            summary: "delivered".into(),
            refs: Vec::new(),
        }),
        evidence: Vec::new(),
        acceptance: None,
        note: None,
        idempotency_key: "lapsed-completion".into(),
    };
    let store = service.store().expect("store before refusal");
    let claim = store
        .current_work_claim(work.work_id)
        .expect("claim projection")
        .expect("lapsed claim");
    let event_count = store
        .work_event_tail(work.work_id, 64)
        .expect("events")
        .len();
    drop(store);

    assert!(matches!(
        service.work_complete(input, at(3)),
        Err(StoreError::WorkClaimLapsed { work: refused, .. }) if refused == work.work_id
    ));
    let store = service.store().expect("store after refusal");
    assert_eq!(
        store
            .current_work_claim(work.work_id)
            .expect("claim projection after refusal"),
        Some(claim),
        "a lapsed completion refusal must not renew or retake the claim"
    );
    assert_eq!(
        store
            .work_event_tail(work.work_id, 64)
            .expect("events after refusal")
            .len(),
        event_count,
        "a lapsed completion refusal must not append a claim event"
    );
}

#[test]
fn lapsed_completion_refuses_before_capture_for_explicit_and_derived_keys() {
    for (case, caller_key) in [("explicit", "lapsed-explicit"), ("derived", "")] {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId(format!("completion-lapsed-{case}"));
        let service = LocalWorkService::new(
            database,
            project,
            "agent".into(),
            SessionId(format!("completion-lapsed-{case}-session")),
            Some("protocol-test".into()),
        );
        let work = match service
            .work_propose(
                root_input(
                    &format!("Lapsed completion {case}"),
                    &format!("lapsed-completion-{case}-root"),
                ),
                at(0),
            )
            .expect("root")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };
        service
            .work_update(
                WorkUpdateInput::Claim {
                    ttl_seconds: Some(1),
                    recovery_reason: None,
                    idempotency_key: format!("lapsed-completion-{case}-claim"),
                },
                at(1),
            )
            .expect("claim");
        let input = WorkCompleteInput {
            capture: Some(WorkCompletionCaptureInput {
                summary: "delivered once".into(),
                refs: Vec::new(),
            }),
            evidence: Vec::new(),
            acceptance: None,
            note: None,
            idempotency_key: caller_key.into(),
        };

        assert_lapsed_completion_refuses_without_mutation(&service, &work, &input, at(3));
    }
}

#[test]
fn completion_recovery_command_uses_full_id_when_target_is_beyond_ambiguity_page() {
    let work_id = WorkId::new();
    let item = WorkReferenceCandidate {
        work_id,
        short_ref: "w-collision".into(),
        title: "Ambiguous recovery target".into(),
        lifecycle: WorkLifecycle::Open,
    };
    let resolution = Err(StoreError::WorkReferenceAmbiguous {
        reference: item.short_ref.clone(),
        candidates: vec![WorkReferenceCandidate {
            work_id: WorkId::new(),
            short_ref: item.short_ref.clone(),
            title: "Earlier target".into(),
            lifecycle: WorkLifecycle::Open,
        }],
        more: 1,
    });

    assert_eq!(
        completion_command_ref_from_resolution(&item, resolution)
            .expect("ambiguous recovery target uses its full id"),
        work_id.0.to_string()
    );
}

#[test]
fn missing_contribution_recovery_names_the_participant_and_root() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("completion-missing-contribution".into());
    let service = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("completion-contribution-session".into()),
        Some("protocol-test".into()),
    );
    let work = match service
        .work_propose(
            root_input("Contribution barrier", "contribution-barrier-root"),
            at(0),
        )
        .expect("root")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    let participant = SessionId("missing-participant".into());
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "contribution-claim".into(),
            },
            at(1),
        )
        .expect("claim root");
    SqliteStore::open(&database)
        .expect("fixture store")
        .add_expected_root_contributor_fixture(work.work_id, &participant, at(2))
        .expect("seed an unaccounted expected participant");
    let completion = service
        .work_complete(
            WorkCompleteInput {
                capture: Some(WorkCompletionCaptureInput {
                    summary: "root implementation complete".into(),
                    refs: Vec::new(),
                }),
                evidence: Vec::new(),
                acceptance: None,
                note: None,
                idempotency_key: "contribution-completion".into(),
            },
            at(3),
        )
        .expect("missing contribution is a typed refusal");
    let WorkCompleteResult::Refused(refusal) = completion else {
        panic!("missing participant must block completion");
    };
    let recovery = refusal.recovery;

    assert_eq!(recovery.item.work_id, work.work_id);
    assert!(matches!(
        recovery.cause,
        WorkCompletionRecoveryCause::MissingContribution { participant: missing }
            if missing == participant
    ));
    assert_eq!(
        recovery.command,
        format!(
            "engram work handoff {} --to {} --summary \"transfer root so the missing participant can contribute\"",
            work.short_ref, participant.0
        )
    );
    let handoff = service
        .work_handoff(
            WorkHandoffInput::Offer {
                to: participant.0.clone(),
                ttl_seconds: None,
                checkpoint_summary: "transfer root so the missing participant can contribute"
                    .into(),
                idempotency_key: "contribution-recovery-handoff".into(),
            },
            at(4),
        )
        .expect("recovery command maps to a real handoff operation");
    assert_eq!(handoff.operation, "offer");
    let store = SqliteStore::open(&database).expect("inspect offered handoff");
    let offers = store
        .work_handoff_offers(work.work_id)
        .expect("load handoff history");
    assert!(
        offers
            .iter()
            .any(|offer| { offer.state == WorkHandoffState::Offered && offer.to == participant })
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one retry chain covers live, cancelled, waived, sealed, and unrelated child states"
)]
fn keyless_completion_rechecks_required_children_until_the_parent_seals() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("completion-unsealed-child".into());
    let service = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("completion-unsealed-session".into()),
        Some("protocol-test".into()),
    );
    let root = match service
        .work_propose(root_input("Parent barrier", "parent-barrier-root"), at(0))
        .expect("root")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    let decomposition = service
        .work_propose(
            WorkProposeInput::Decompose {
                children: [
                    ("waived-child", ChildRequirement::Required),
                    ("sealed-child", ChildRequirement::Required),
                    ("optional-sibling", ChildRequirement::Optional),
                ]
                .into_iter()
                .map(|(key, requirement)| WorkChildInput {
                    key: key.into(),
                    title: key.replace('-', " "),
                    outcome: format!("{key} outcome"),
                    acceptance: vec![format!("{key} accepted")],
                    requirement: Some(requirement),
                    kind: None,
                    priority: None,
                    labels: Vec::new(),
                    assigned_to: None,
                    deferred_until: None,
                })
                .collect(),
                prerequisites: Vec::new(),
                idempotency_key: "parent-decomposition".into(),
            },
            at(1),
        )
        .expect("decompose");
    let WorkProposeResult::Decomposition(decomposition) = decomposition else {
        panic!("expected decomposition");
    };
    let required = decomposition.children[..2].to_vec();
    let sibling = decomposition.children[2].clone();
    service
        .work_focus(&root.work_id.0.to_string(), at(2))
        .expect("refocus parent");
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "parent-claim".into(),
            },
            at(3),
        )
        .expect("claim parent");

    let parent_completion = completion_input("parent delivered", "");
    let first = service
        .work_complete(parent_completion.clone(), at(4))
        .expect("unsealed child is a typed completion refusal");
    let WorkCompleteResult::Refused(refusal) = first else {
        panic!("required child must block completion");
    };
    assert_eq!(refusal.code, "required_child_unsealed");
    let waived = required
        .iter()
        .find(|child| child.work_id == refusal.recovery.item.work_id)
        .expect("refusal names one required child")
        .clone();
    assert!(matches!(
        &refusal.recovery.cause,
        WorkCompletionRecoveryCause::RequiredChildUnsealed { child }
            if *child == waived.work_id
    ));
    let waived_item = SqliteStore::open(&database)
        .expect("required-child store")
        .get_work_item(waived.work_id)
        .expect("required child");
    assert_eq!(refusal.recovery.item.title, waived_item.title);
    assert_eq!(
        refusal.recovery.command,
        format!("engram work show {}", waived.short_ref)
    );
    let sealed = required
        .into_iter()
        .find(|child| child.work_id != waived.work_id)
        .expect("second required child");

    service
        .work_focus(&waived.short_ref, at(5))
        .expect("focus child to waive");
    service
        .work_update(
            WorkUpdateInput::Cancel {
                reason: "the child outcome is no longer required".into(),
                idempotency_key: "cancel-required-child".into(),
            },
            at(6),
        )
        .expect("cancel required child");
    service
        .work_focus(&root.short_ref, at(7))
        .expect("refocus parent after cancellation");
    let verbs = AgentVerbs::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("completion-unsealed-session".into()),
        Some("protocol-test".into()),
    );
    let cancelled = verbs
        .done(
            DoneInput {
                work_ref: Some(root.short_ref.clone()),
                summary: Some("parent delivered".into()),
                note: None,
            },
            at(8),
        )
        .expect("completion refusal is rendered from current state");
    assert!(cancelled.owed);
    let cancelled_text = cancelled.text();
    assert!(cancelled_text.contains(&format!("required child {} \"", waived.short_ref)));
    assert!(cancelled_text.contains("is cancelled without a completion seal or waiver"));
    let recovery_command = cancelled.next.first().expect("recovery command");
    assert_eq!(
        recovery_command,
        &format!(
            "engram work update {} --waive {} --reason \"account for disposed required child\"",
            root.short_ref, waived.short_ref
        )
    );
    verbs
        .update(
            UpdateInput {
                work_ref: Some(root.short_ref.clone()),
                action: UpdateAction::WaiveRequiredChild {
                    child: waived.short_ref,
                    reason: "the cancelled child is explicitly accounted for".into(),
                },
            },
            at(9),
        )
        .expect("waive cancelled child");
    let next = service
        .work_complete(parent_completion.clone(), at(10))
        .expect("completion advances to the remaining child");
    assert!(matches!(
        next,
        WorkCompleteResult::Refused(WorkCompleteRefusal {
            recovery: WorkCompletionRecovery {
                cause: WorkCompletionRecoveryCause::RequiredChildUnsealed { child },
                ..
            },
            ..
        }) if child == sealed.work_id
    ));

    let before_sibling_activity = completion_run_feed_head(&service, root.work_id);
    let peer = LocalWorkService::new(
        database,
        project,
        "peer".into(),
        SessionId("completion-sibling-session".into()),
        Some("protocol-test".into()),
    );
    peer.work_focus(&sibling.short_ref, at(11))
        .expect("peer focuses optional sibling");
    peer.work_update(
        WorkUpdateInput::Cancel {
            reason: "optional sibling activity".into(),
            idempotency_key: "cancel-optional-sibling".into(),
        },
        at(12),
    )
    .expect("peer changes optional sibling");
    assert_eq!(
        completion_run_feed_head(&service, root.work_id),
        before_sibling_activity,
        "optional sibling activity does not advance the parent run feed"
    );
    service
        .work_focus(&sealed.short_ref, at(13))
        .expect("focus remaining required child");
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "claim-required-child".into(),
            },
            at(14),
        )
        .expect("claim remaining required child");
    assert!(matches!(
        service
            .work_complete(completion_input("required child delivered", ""), at(15))
            .expect("seal required child"),
        WorkCompleteResult::Completed(_)
    ));
    service
        .work_focus(&root.short_ref, at(16))
        .expect("refocus parent after child seal");
    assert!(matches!(
        service
            .work_complete(parent_completion, at(17))
            .expect("parent completion rechecks current barriers"),
        WorkCompleteResult::Completed(_)
    ));
}

#[test]
fn explicit_completion_target_is_checked_before_replay() {
    let directory = tempdir().expect("temp directory");
    let service = LocalWorkService::new(
        directory.path().join("engram.sqlite3"),
        ProjectId("completion-replay-target".into()),
        "agent".into(),
        SessionId("completion-replay-target-session".into()),
        Some("protocol-test".into()),
    );
    let first = proposed_root(
        service
            .work_propose(root_input("First target", "first-target"), at(0))
            .expect("first root"),
    );
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "claim-first-target".into(),
            },
            at(1),
        )
        .expect("claim first target");
    let input = completion_input("shared completion intent", "shared-completion-key");
    assert!(matches!(
        service
            .work_complete_on(Some(&first.short_ref), input.clone(), at(2))
            .expect("complete first target"),
        WorkCompleteResult::Completed(_)
    ));
    let second = proposed_root(
        service
            .work_propose(root_input("Second target", "second-target"), at(3))
            .expect("second root"),
    );
    assert!(matches!(
        service.work_complete_on(Some(&second.short_ref), input, at(4)),
        Err(StoreError::WorkOperationIdempotencyConflict { operation, key })
            if operation == "work_complete" && key == "shared-completion-key"
    ));
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one scenario proves target binding and same-holder claim-epoch recovery"
)]
fn refused_explicit_completion_stays_target_bound_and_rotates_with_holder_claim_epoch() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("completion-refusal-target-binding".into());
    let session = SessionId("completion-refusal-session".into());
    let service = LocalWorkService::new(
        database.clone(),
        project,
        "agent".into(),
        session,
        Some("protocol-test".into()),
    );
    let parent = proposed_root(
        service
            .work_propose(
                root_input("Refused completion target", "refusal-target-root"),
                at(0),
            )
            .expect("parent root"),
    );
    service
        .work_propose(
            WorkProposeInput::Decompose {
                children: vec![WorkChildInput {
                    key: "required-child".into(),
                    title: "Required child".into(),
                    outcome: "Required child outcome".into(),
                    acceptance: vec!["Required child accepted".into()],
                    requirement: Some(ChildRequirement::Required),
                    kind: None,
                    priority: None,
                    labels: Vec::new(),
                    assigned_to: None,
                    deferred_until: None,
                }],
                prerequisites: Vec::new(),
                idempotency_key: "refusal-target-decomposition".into(),
            },
            at(1),
        )
        .expect("required child");
    service
        .work_focus(&parent.short_ref, at(2))
        .expect("focus parent");
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "refusal-target-claim".into(),
            },
            at(3),
        )
        .expect("claim parent");
    let input = completion_input("parent completion capture", "refusal-target-completion");
    assert!(matches!(
        service
            .work_complete(input.clone(), at(4))
            .expect("required child refusal"),
        WorkCompleteResult::Refused(WorkCompleteRefusal {
            recovery: WorkCompletionRecovery {
                cause: WorkCompletionRecoveryCause::RequiredChildUnsealed { .. },
                ..
            },
            ..
        })
    ));

    let other = proposed_root(
        service
            .work_propose(root_input("Other focus", "refusal-other-root"), at(5))
            .expect("other root"),
    );
    assert!(matches!(
        service.work_complete(input.clone(), at(6)),
        Err(StoreError::WorkOperationIdempotencyConflict { operation, key })
            if operation == "work_complete" && key == "refusal-target-completion"
    ));
    service
        .work_focus(&parent.short_ref, at(7))
        .expect("restore refused target");
    service
        .work_update(
            WorkUpdateInput::Release {
                reason: "rotate the holder claim epoch".into(),
                waiver_reason: None,
                idempotency_key: "release-refused-target".into(),
            },
            at(8),
        )
        .expect("release original claim");
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "reclaim-refused-target".into(),
            },
            at(9),
        )
        .expect("reclaim target");
    assert!(matches!(
        service
            .work_complete(input, at(10))
            .expect("same caller key advances under the new holder claim epoch"),
        WorkCompleteResult::Refused(WorkCompleteRefusal {
            recovery: WorkCompletionRecovery {
                cause: WorkCompletionRecoveryCause::RequiredChildUnsealed { .. },
                ..
            },
            ..
        })
    ));
    assert_ne!(other.work_id, parent.work_id);

    let store = SqliteStore::open(&database).expect("refusal binding store");
    assert!(
        store
            .verify_all()
            .expect("refusal binding integrity")
            .is_healthy()
    );
}

#[test]
fn refused_explicit_completion_cannot_refresh_across_work_revision() {
    let directory = tempdir().expect("temp directory");
    let service = LocalWorkService::new(
        directory.path().join("engram.sqlite3"),
        ProjectId("completion-refusal-revision-binding".into()),
        "agent".into(),
        SessionId("completion-refusal-revision-session".into()),
        Some("protocol-test".into()),
    );
    let parent = proposed_root(
        service
            .work_propose(
                root_input("Revision-bound completion", "revision-bound-root"),
                at(0),
            )
            .expect("parent root"),
    );
    service
        .work_propose(
            WorkProposeInput::Decompose {
                children: vec![WorkChildInput {
                    key: "required-child".into(),
                    title: "Required child".into(),
                    outcome: "Required child outcome".into(),
                    acceptance: vec!["Required child accepted".into()],
                    requirement: Some(ChildRequirement::Required),
                    kind: None,
                    priority: None,
                    labels: Vec::new(),
                    assigned_to: None,
                    deferred_until: None,
                }],
                prerequisites: Vec::new(),
                idempotency_key: "revision-bound-decomposition".into(),
            },
            at(1),
        )
        .expect("required child");
    service
        .work_focus(&parent.short_ref, at(2))
        .expect("focus parent");
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "revision-bound-claim".into(),
            },
            at(3),
        )
        .expect("claim parent");
    let input = completion_input(
        "completion against the original acceptance",
        "revision-bound-completion",
    );
    assert!(matches!(
        service
            .work_complete(input.clone(), at(4))
            .expect("required child refusal"),
        WorkCompleteResult::Refused(WorkCompleteRefusal {
            recovery: WorkCompletionRecovery {
                cause: WorkCompletionRecoveryCause::RequiredChildUnsealed { .. },
                ..
            },
            ..
        })
    ));

    service
        .work_update(
            WorkUpdateInput::Revise {
                patch: WorkRevisionPatch {
                    acceptance: Some(vec!["Revised acceptance must be assessed anew".into()]),
                    ..WorkRevisionPatch::default()
                },
                idempotency_key: "revise-after-completion-refusal".into(),
            },
            at(5),
        )
        .expect("revise refused target");

    assert!(matches!(
        service.work_complete(input, at(6)),
        Err(StoreError::WorkOperationIdempotencyConflict { operation, key })
            if operation == "work_complete" && key == "revision-bound-completion"
    ));
    let store = service.store().expect("revision-bound store");
    let revised = store
        .get_work_item(parent.work_id)
        .expect("revised parent projection");
    assert_eq!(
        revised.acceptance,
        vec!["Revised acceptance must be assessed anew"]
    );
    assert_eq!(revised.lifecycle, WorkLifecycle::Open);
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the table-driven failure-atomicity regression verifies every caller-controlled acceptance shape against all durable completion substeps"
)]
fn capture_completion_rejects_bad_acceptance_without_substeps() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("completion-prevalidation".into());
    let service = LocalWorkService::new(
        database.clone(),
        project,
        "agent".into(),
        SessionId("completion-prevalidation-session".into()),
        Some("protocol-test".into()),
    );
    let root = match service
        .work_propose(
            WorkProposeInput::Root {
                title: "Prevalidate completion".into(),
                outcome: "Invalid acceptance never writes capture substeps".into(),
                acceptance: vec!["criterion one".into(), "criterion two".into()],
                work_kind: None,
                priority: None,
                labels: Vec::new(),
                assigned_to: None,
                deferred_until: None,
                idempotency_key: "prevalidation-root".into(),
            },
            at(0),
        )
        .expect("root proposal")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "prevalidation-claim".into(),
            },
            at(1),
        )
        .expect("claim focused work");
    let run_id = root.active_run_id.expect("active run");
    let accepted = |criterion: &str, satisfied: bool, evidence: Vec<String>| WorkAcceptanceInput {
        criterion: Some(criterion.into()),
        satisfied,
        evidence,
        note: "prevalidation fixture".into(),
    };
    let cases = vec![
        ("missing", vec![accepted("criterion one", true, Vec::new())]),
        (
            "duplicate",
            vec![
                accepted("criterion one", true, Vec::new()),
                accepted("criterion one", true, Vec::new()),
            ],
        ),
        (
            "unknown",
            vec![
                accepted("criterion one", true, Vec::new()),
                accepted("unknown criterion", true, Vec::new()),
            ],
        ),
        (
            "unsatisfied",
            vec![
                accepted("criterion one", true, Vec::new()),
                accepted("criterion two", false, Vec::new()),
            ],
        ),
        (
            "malformed-evidence",
            vec![
                accepted("criterion one", true, vec!["not-a-hash".into()]),
                accepted("criterion two", true, Vec::new()),
            ],
        ),
    ];

    for (index, (name, acceptance)) in cases.into_iter().enumerate() {
        let key = format!("prevalidation-{name}");
        let before = SqliteStore::open(&database).expect("before store");
        let before_evidence = before.work_run_evidence(run_id).expect("before evidence");
        let before_checkpoint = before
            .get_work_run(run_id)
            .expect("before run")
            .last_checkpoint;
        let before_head = before
            .work_feed_head(&FeedId::RunExecution(run_id))
            .expect("before feed head");
        drop(before);

        let input = WorkCompleteInput {
            capture: Some(WorkCompletionCaptureInput {
                summary: format!("capture must not commit for {name}"),
                refs: vec![format!("test:{name}")],
            }),
            evidence: Vec::new(),
            acceptance: Some(acceptance),
            note: None,
            idempotency_key: key.clone(),
        };
        let completion = service.work_complete(
            input.clone(),
            at(2 + i64::try_from(index).expect("bounded case index")),
        );
        if matches!(name, "missing" | "unknown" | "unsatisfied") {
            let WorkCompleteResult::Refused(refusal) =
                completion.expect("missing acceptance is a typed refusal")
            else {
                panic!("{name} acceptance must not complete work");
            };
            assert_eq!(refusal.code, "missing_acceptance");
            assert!(matches!(
                refusal.recovery.cause,
                WorkCompletionRecoveryCause::MissingAcceptance { .. }
            ));
        } else {
            assert!(completion.is_err(), "{name} must be rejected");
        }

        let after = SqliteStore::open(&database).expect("after store");
        assert_eq!(
            after.work_run_evidence(run_id).expect("after evidence"),
            before_evidence
        );
        assert_eq!(
            after
                .get_work_run(run_id)
                .expect("after run")
                .last_checkpoint,
            before_checkpoint
        );
        assert_eq!(
            after
                .work_feed_head(&FeedId::RunExecution(run_id))
                .expect("after feed head"),
            before_head
        );
        for operation in ["record_work_evidence", "checkpoint_work"] {
            let scoped = service
                .core_operation_key("work_complete", &key, operation)
                .expect("scoped substep key");
            assert!(
                after
                    .work_operation_result_value(operation, &scoped)
                    .expect("substep lookup")
                    .is_none()
            );
        }
    }
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the crash-replay regression seeds each committed completion substep under the exact durable protocol attempt"
)]
fn capture_completion_replays_after_evidence_or_checkpoint_commit() {
    for (scenario, checkpoint_committed, claim_ttl, retry_at) in [
        ("evidence", false, 300, at(3)),
        ("checkpoint", true, 300, at(3)),
        ("short-claim-renewed", false, 2, at(4)),
    ] {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId(format!("completion-replay-{scenario}"));
        let session = SessionId("completion-session".into());
        let service = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            session.clone(),
            Some("protocol-test".into()),
        );
        let root = match service
            .work_propose(
                root_input("Crash-safe completion", "completion-root"),
                at(0),
            )
            .expect("root proposal")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };
        service
            .work_update(
                WorkUpdateInput::Claim {
                    ttl_seconds: Some(claim_ttl),
                    recovery_reason: None,
                    idempotency_key: "completion-claim".into(),
                },
                at(1),
            )
            .expect("claim focused work");
        let input = WorkCompleteInput {
            capture: Some(WorkCompletionCaptureInput {
                summary: "completion evidence was durably captured".into(),
                refs: vec!["test:completion-replay".into()],
            }),
            evidence: Vec::new(),
            acceptance: Some(vec![WorkAcceptanceInput {
                criterion: None,
                satisfied: true,
                evidence: Vec::new(),
                note: "the crash-replay path was verified".into(),
            }]),
            note: None,
            idempotency_key: "crash-safe-completion".into(),
        };

        let mut store = SqliteStore::open(&database).expect("store");
        let basis = service
            .protocol_basis(&store, true, false, None, at(2))
            .expect("completion basis");
        let intent = service.protocol_intent(&input);
        store
            .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
                project_id: &project,
                session_id: &session,
                operation: "work_complete",
                idempotency_key: &input.idempotency_key,
                intent: &intent,
                basis: &basis,
                now: at(2),
            })
            .expect("durable completion attempt");
        let work = basis.focused_work.clone().expect("focused work");
        assert_eq!(work.work_id, root.work_id);
        let claim = service
            .live_protocol_claim(&basis, &work, at(2))
            .expect("live completion claim");
        let capture = input.capture.as_ref().expect("completion capture");
        let evidence = store
            .record_work_evidence(
                &RecordWorkEvidenceRequest {
                    work_id: work.work_id,
                    run_id: claim.run_id,
                    expected_work_revision: work.revision,
                    holder: session.clone(),
                    claim_id: claim.claim_id,
                    claim_fence: claim.fence,
                    summary: capture.summary.clone(),
                    refs: capture.refs.clone(),
                    actor: service.actor(
                        "work_complete",
                        "capture completion evidence for ambient local work",
                    ),
                    idempotency_key: service
                        .core_operation_key(
                            "work_complete",
                            &completion_capture_key(&input.idempotency_key, &work, &claim)
                                .expect("completion capture key"),
                            "record_work_evidence",
                        )
                        .expect("evidence key"),
                    recorded_at: at(2),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("committed evidence substep");
        if checkpoint_committed {
            store
                .checkpoint_work_for_completion(
                    &CheckpointWorkRequest {
                        work_id: work.work_id,
                        run_id: claim.run_id,
                        expected_work_revision: work.revision,
                        holder: session.clone(),
                        claim_id: claim.claim_id,
                        claim_fence: claim.fence,
                        summary: capture.summary.clone(),
                        evidence: Some(vec![evidence]),
                        actor: service.actor(
                            "work_complete",
                            "checkpoint the exact completion evidence cut",
                        ),
                        idempotency_key: input.idempotency_key.clone(),
                        checkpointed_at: at(2),
                    },
                    |cut| {
                        let attempt_key = completion_attempt_key(&input.idempotency_key, cut)?;
                        service.core_operation_key("work_complete", &attempt_key, "checkpoint_work")
                    },
                    &DevelopmentNoopRedactor,
                )
                .expect("committed checkpoint substep");
        }
        let checkpoint_count_before_retry = store
            .work_feed_after(&FeedId::RunExecution(claim.run_id), 0, 100)
            .expect("run feed before retry")
            .into_iter()
            .filter(|entry| entry.object_kind == "work_checkpoint")
            .count();
        drop(store);

        let completed = service
            .work_complete(input.clone(), retry_at)
            .expect("retry resumes the durable attempt");
        let WorkCompleteResult::Completed(completed) = completed else {
            panic!("retry must complete work");
        };
        assert_eq!(completed.work_id, root.work_id);
        assert_eq!(completed.completed_at, retry_at);
        let checkpoint_count_after_retry = SqliteStore::open(&database)
            .expect("store after retry")
            .work_feed_after(&FeedId::RunExecution(claim.run_id), 0, 100)
            .expect("run feed after retry")
            .into_iter()
            .filter(|entry| entry.object_kind == "work_checkpoint")
            .count();
        assert_eq!(
            checkpoint_count_after_retry, 1,
            "a retry writes or reuses exactly one completion checkpoint"
        );
        assert_eq!(
            checkpoint_count_before_retry,
            usize::from(checkpoint_committed)
        );
        let replay = service
            .work_complete(input.clone(), retry_at + Duration::seconds(1))
            .expect("completed outer attempt replays");
        let WorkCompleteResult::Completed(replay) = replay else {
            panic!("completed outer attempt must replay completion");
        };
        assert_eq!(replay.seal, completed.seal);
        assert_eq!(replay.completed_at, retry_at);
    }
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one interrupted-success fixture covers focus changes and later work generations"
)]
fn interrupted_completion_replays_the_original_work_and_run() {
    for scenario in ["focus-change", "reopen", "recomplete"] {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId(format!("interrupted-completion-{scenario}"));
        let session = SessionId("interrupted-completion-session".into());
        let service = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            session,
            Some("protocol-test".into()),
        );
        let original = proposed_root(
            service
                .work_propose(
                    root_input("Original completion target", "original-target"),
                    at(0),
                )
                .expect("original root"),
        );
        service
            .work_update(
                WorkUpdateInput::Claim {
                    ttl_seconds: Some(300),
                    recovery_reason: None,
                    idempotency_key: "original-claim".into(),
                },
                at(1),
            )
            .expect("claim original work");
        let input = completion_input("original run completed", "interrupted-completion");
        let original_seal = commit_completion_core_without_finishing(&service, &input, at(2));
        let original_seal_hash = CanonicalObject::freeze(&original_seal)
            .expect("original seal object")
            .hash()
            .clone();

        match scenario {
            "focus-change" => {
                let peer = LocalWorkService::new(
                    database.clone(),
                    project.clone(),
                    "peer".into(),
                    SessionId("interrupted-completion-peer".into()),
                    Some("protocol-test".into()),
                );
                let other = proposed_root(
                    peer.work_propose(root_input("Other completed work", "other-target"), at(3))
                        .expect("other root"),
                );
                peer.work_update(
                    WorkUpdateInput::Claim {
                        ttl_seconds: Some(300),
                        recovery_reason: None,
                        idempotency_key: "other-claim".into(),
                    },
                    at(4),
                )
                .expect("claim other work");
                assert!(matches!(
                    peer.work_complete(
                        completion_input("other work completed", "other-completion"),
                        at(5)
                    )
                    .expect("complete other work"),
                    WorkCompleteResult::Completed(_)
                ));
                service
                    .work_focus(&other.short_ref, at(6))
                    .expect("move focus to other completed work");
                assert!(matches!(
                    service.work_complete(input.clone(), at(7)),
                    Err(StoreError::WorkOperationIdempotencyConflict { .. })
                ));
                service
                    .work_focus(&original.short_ref, at(8))
                    .expect("restore original focus");
            }
            "reopen" => {
                service
                    .work_update(
                        WorkUpdateInput::Reopen {
                            reason: "exercise interrupted replay after reopen".into(),
                            idempotency_key: "reopen-original".into(),
                        },
                        at(3),
                    )
                    .expect("reopen original work");
            }
            "recomplete" => {
                service
                    .work_update(
                        WorkUpdateInput::Reopen {
                            reason: "exercise interrupted replay after a later generation".into(),
                            idempotency_key: "reopen-original".into(),
                        },
                        at(3),
                    )
                    .expect("reopen original work");
                service
                    .work_update(
                        WorkUpdateInput::Claim {
                            ttl_seconds: Some(300),
                            recovery_reason: None,
                            idempotency_key: "later-generation-claim".into(),
                        },
                        at(4),
                    )
                    .expect("claim later generation");
                let later = service
                    .work_complete(
                        completion_input("later generation completed", "later-completion"),
                        at(5),
                    )
                    .expect("complete later generation");
                let WorkCompleteResult::Completed(later) = later else {
                    panic!("later generation must complete");
                };
                assert_ne!(later.run_id, original_seal.run_id);
                assert_ne!(later.seal, original_seal_hash);
            }
            _ => unreachable!("fixture scenario is exhaustive"),
        }

        let replay = service
            .work_complete(input.clone(), at(20))
            .expect("recover interrupted completion");
        let WorkCompleteResult::Completed(replay) = replay else {
            panic!("interrupted success must replay");
        };
        assert_eq!(replay.work_id, original.work_id);
        assert_eq!(replay.run_id, original_seal.run_id);
        assert_eq!(replay.seal, original_seal_hash);
        assert_eq!(replay.completed_at, original_seal.completed_at);
        let second = service
            .work_complete(input, at(21))
            .expect("finished interrupted replay is stable");
        let WorkCompleteResult::Completed(second) = second else {
            panic!("finished outer attempt must replay");
        };
        assert_eq!(second.seal, original_seal_hash);
    }
}

#[test]
fn pending_completion_resumes_after_holder_evidence_and_seals_the_current_set() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("completion-retry-current-evidence".into());
    let session = SessionId("completion-current-evidence-session".into());
    let service = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        session.clone(),
        Some("protocol-test".into()),
    );
    let root = match service
        .work_propose(
            root_input("Seal current evidence", "current-evidence-root"),
            at(0),
        )
        .expect("root proposal")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: None,
                recovery_reason: None,
                idempotency_key: "current-evidence-claim".into(),
            },
            at(1),
        )
        .expect("claim focused work");
    let input = WorkCompleteInput {
        capture: Some(WorkCompletionCaptureInput {
            summary: "completion checkpoint includes current evidence".into(),
            refs: vec!["test:current-evidence-completion".into()],
        }),
        evidence: Vec::new(),
        acceptance: Some(vec![WorkAcceptanceInput {
            criterion: None,
            satisfied: true,
            evidence: Vec::new(),
            note: "the current evidence set is sealed".into(),
        }]),
        note: None,
        idempotency_key: "current-evidence-completion".into(),
    };

    let mut store = SqliteStore::open(&database).expect("store");
    let basis = service
        .protocol_basis(&store, true, false, None, at(2))
        .expect("completion basis");
    let intent = service.protocol_intent(&input);
    store
        .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
            project_id: &project,
            session_id: &session,
            operation: "work_complete",
            idempotency_key: &input.idempotency_key,
            intent: &intent,
            basis: &basis,
            now: at(2),
        })
        .expect("pending completion attempt");
    let claim = basis.claim.as_ref().expect("completion claim");
    let unrelated = store
        .record_work_evidence(
            &RecordWorkEvidenceRequest {
                work_id: root.work_id,
                run_id: claim.run_id,
                expected_work_revision: root.revision,
                holder: session.clone(),
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                summary: "independent holder evidence committed after attempt start".into(),
                refs: vec!["test:independent-evidence".into()],
                actor: service.actor("work_update", "record independent holder evidence"),
                idempotency_key: "independent-holder-evidence".into(),
                recorded_at: at(3),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("independent evidence");
    drop(store);

    let completed = service
        .work_complete(input, at(4))
        .expect("same-key retry resumes against current evidence");
    let WorkCompleteResult::Completed(receipt) = completed else {
        panic!("completion must seal");
    };
    let store = SqliteStore::open(&database).expect("sealed store");
    let seal: CompletionSeal = store
        .get(&receipt.seal)
        .expect("load seal")
        .expect("canonical seal");
    assert!(seal.evidence.contains(&unrelated));
    assert!(store.verify_all().expect("integrity").is_healthy());
}

#[test]
fn stored_completion_refusal_is_a_corrupt_projection() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("stored-completion-refusal".into());
    let session = SessionId("stored-completion-refusal-session".into());
    let service = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        session.clone(),
        Some("protocol-test".into()),
    );
    let root = proposed_root(
        service
            .work_propose(
                root_input("Stored completion refusal", "stored-refusal-root"),
                at(0),
            )
            .expect("root proposal"),
    );
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "stored-refusal-claim".into(),
            },
            at(1),
        )
        .expect("claim focused work");
    let input = WorkCompleteInput {
        capture: None,
        evidence: Vec::new(),
        acceptance: Some(Vec::new()),
        note: None,
        idempotency_key: "stored-refusal-completion".into(),
    };
    let refused = service
        .work_complete(input.clone(), at(2))
        .expect("missing acceptance is a typed refusal");
    assert!(matches!(refused, WorkCompleteResult::Refused(_)));
    let mut store = SqliteStore::open(&database).expect("store refusal fixture");
    store
        .finish_work_protocol_attempt(
            &project,
            &session,
            "work_complete",
            &input.idempotency_key,
            &refused,
        )
        .expect("seed corrupt stored refusal");
    drop(store);

    assert!(matches!(
        service.work_complete_on(Some(&root.short_ref), input, at(3)),
        Err(StoreError::InvalidWorkProjection(detail))
            if detail == "stored work_complete attempt contains a refusal result"
    ));
}

#[test]
fn pending_completion_conflicts_after_foreign_claim_fence_change() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("completion-retry-foreign-fence".into());
    let session = SessionId("completion-original-session".into());
    let service = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        session.clone(),
        Some("protocol-test".into()),
    );
    let root = match service
        .work_propose(
            root_input("Reject foreign retry", "foreign-fence-root"),
            at(0),
        )
        .expect("root proposal")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(2),
                recovery_reason: None,
                idempotency_key: "foreign-fence-original-claim".into(),
            },
            at(1),
        )
        .expect("short original claim");
    let input = WorkCompleteInput {
        capture: Some(WorkCompletionCaptureInput {
            summary: "this stale attempt must never commit".into(),
            refs: Vec::new(),
        }),
        evidence: Vec::new(),
        acceptance: Some(vec![WorkAcceptanceInput {
            criterion: None,
            satisfied: true,
            evidence: Vec::new(),
            note: "stale completion".into(),
        }]),
        note: None,
        idempotency_key: "foreign-fence-completion".into(),
    };

    let mut store = SqliteStore::open(&database).expect("store");
    let basis = service
        .protocol_basis(&store, true, false, None, at(2))
        .expect("completion basis");
    let intent = service.protocol_intent(&input);
    store
        .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
            project_id: &project,
            session_id: &session,
            operation: "work_complete",
            idempotency_key: &input.idempotency_key,
            intent: &intent,
            basis: &basis,
            now: at(2),
        })
        .expect("pending completion attempt");
    let run_id = root.active_run_id.expect("active run");
    let peer = SessionId("completion-peer-session".into());
    let mut peer_actor = service.actor("work_update", "recover expired foreign claim");
    peer_actor.session_id = Some(peer.clone());
    let recovered = store
        .claim_work(
            &ClaimWorkRequest {
                work_id: root.work_id,
                expected_work_revision: root.revision,
                expected_run_id: Some(run_id),
                holder: peer,
                ttl_seconds: 300,
                recovery_reason: Some("the original holder claim expired".into()),
                actor: peer_actor,
                idempotency_key: "foreign-fence-recovery".into(),
                claimed_at: at(4),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("peer recovers claim");
    assert_ne!(
        recovered.fence,
        basis.claim.as_ref().expect("old claim").fence
    );
    drop(store);

    assert!(matches!(
        service.work_complete(input, at(5)),
        Err(StoreError::WorkOperationIdempotencyConflict { operation, key })
            if operation == "work_complete" && key == "foreign-fence-completion"
    ));
}
