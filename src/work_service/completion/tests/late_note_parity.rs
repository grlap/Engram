use super::*;
use crate::domain::{RecordRestoredWorkEvidenceRequest, RestoredWorkEvidenceInput};

/// One item completed natively in `native`, and the same item, by snapshot,
/// completed by its restored record in `restored`. Both services act as
/// the same actor and session.
fn completed_both_ways(
    directory: &std::path::Path,
) -> (LocalWorkService, LocalWorkService, WorkItemSummary) {
    let project = ProjectId("late-note-parity".into());
    let service = |name: &str| {
        LocalWorkService::new(
            directory.join(name),
            project.clone(),
            "review-agent".into(),
            SessionId("review-session".into()),
            Some("protocol-test".into()),
        )
    };
    let native = service("native.sqlite3");
    let root = proposed_root(
        native
            .work_propose(
                root_input("Reviewed after completion", "parity-root"),
                at(0),
            )
            .expect("root"),
    );
    native
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "claim-parity-root".into(),
            },
            at(1),
        )
        .expect("claim root");
    assert!(matches!(
        native
            .work_complete(completion_input("complete", "complete-parity-root"), at(2))
            .expect("complete root"),
        WorkCompleteResult::Completed(_)
    ));
    let snapshot = native
        .save_work_graph_snapshot(None, WorkGraphSnapshotDestinationKind::Stdout, at(3))
        .expect("save completed root");
    let restored = service("restored.sqlite3");
    restored
        .load_work_graph_snapshot(
            &serde_json::to_vec(&snapshot.document).expect("snapshot bytes"),
            false,
            at(4),
        )
        .expect("load completed root");
    let store = SqliteStore::open(directory.join("restored.sqlite3")).expect("restored store");
    assert!(
        store
            .work_completed_by_restored_record(root.work_id)
            .expect("restored completion")
    );
    (native, restored, root)
}

/// The note result as JSON with each evidence id replaced by its order of
/// first appearance, so results from two stores compare by meaning.
fn normalized(result: &WorkNoteResult, ids: &mut Vec<String>) -> serde_json::Value {
    let mut text = serde_json::to_string(result).expect("note result JSON");
    for id in [&result.evidence.result, &result.receipt.result] {
        if let Some(id) = id.as_str()
            && !ids.iter().any(|known| known == id)
        {
            ids.push(id.to_owned());
        }
    }
    for (position, id) in ids.iter().enumerate() {
        text = text.replace(id.as_str(), &format!("evidence-{position}"));
    }
    serde_json::from_str(&text).expect("normalized JSON")
}

// An identical late note, repeated at once and again later, behaves the
// same on a natively completed item and on one completed by its restored
// record: the same append-or-replay relationship, the same receipts up to
// the evidence ids, and the same number of late records.
#[test]
fn a_repeated_late_note_behaves_alike_on_native_and_restored_completion() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let (native, restored, root) = completed_both_ways(directory.path());
    let refs = vec!["review:late-note".to_owned()];
    let run = |service: &LocalWorkService| {
        [10, 10, 11].map(|second| {
            service
                .work_note_on(
                    Some(&root.short_ref),
                    "late review found one important detail",
                    &refs,
                    at(second),
                )
                .expect("late note")
        })
    };
    let native_results = run(&native);
    let restored_results = run(&restored);
    let evidence = |results: &[WorkNoteResult; 3]| {
        results
            .iter()
            .map(|result| result.evidence.result.clone())
            .collect::<Vec<_>>()
    };
    let relation = |ids: &[serde_json::Value]| (ids[0] == ids[1], ids[1] == ids[2]);
    // On a natively completed item an identical repeat replays, at once and
    // later alike.
    assert_eq!(relation(&evidence(&native_results)), (true, true));
    assert_eq!(
        relation(&evidence(&restored_results)),
        relation(&evidence(&native_results)),
        "the same append-or-replay relationship"
    );

    // Within each store the repeat's receipt relates to the first one's the
    // same way, whole receipt included. Across the stores the receipts carry
    // the same evidence pattern and the same words. The item's revision and
    // a native run's historical obligation page differ by model, not by
    // replay, so they are left out of the cross-store comparison.
    let (mut native_ids, mut restored_ids) = (Vec::new(), Vec::new());
    let native_receipts = native_results
        .iter()
        .map(|result| normalized(result, &mut native_ids))
        .collect::<Vec<_>>();
    let restored_receipts = restored_results
        .iter()
        .map(|result| normalized(result, &mut restored_ids))
        .collect::<Vec<_>>();
    let receipt_relation =
        |receipts: &[serde_json::Value]| (receipts[0] == receipts[1], receipts[1] == receipts[2]);
    assert_eq!(
        receipt_relation(&restored_receipts),
        receipt_relation(&native_receipts)
    );
    let shared = |receipt: &serde_json::Value| {
        serde_json::json!({
            "operation": receipt["operation"],
            "allowed_next": receipt["allowed_next"],
            "obligations": receipt["obligations"],
            "evidence": receipt["evidence"]["result"],
            "receipt": receipt["receipt"]["result"],
            "work_ref": receipt["evidence"]["work_ref"],
            "open_obligations": receipt["obligation_page"]["open_total"],
        })
    };
    for (native_receipt, restored_receipt) in native_receipts.iter().zip(&restored_receipts) {
        assert_eq!(shared(restored_receipt), shared(native_receipt));
    }

    let native_store = SqliteStore::open(directory.path().join("native.sqlite3")).expect("native");
    let completed_run = native_store
        .latest_work_run(root.work_id)
        .expect("run read")
        .expect("completed run");
    let native_late = native_store
        .work_run_evidence(completed_run.run_id)
        .expect("run evidence")
        .into_iter()
        .filter(|id| native_ids.iter().any(|late| late == id.as_str()))
        .count();
    let restored_store =
        SqliteStore::open(directory.path().join("restored.sqlite3")).expect("restored");
    let restored_evidence = restored_store
        .restored_work_evidence(root.work_id)
        .expect("restored evidence");
    let restored_late = restored_evidence.len();
    assert_eq!(restored_late, native_late);
    assert_eq!(restored_late, native_ids.len());

    // The evidence chain itself: the one late record each path stored holds
    // the same content, attribution and time.
    let native_record: crate::WorkEvidence = native_store
        .get(&ObjectId::from_stored(native_ids[0].clone()).expect("native id"))
        .expect("native read")
        .expect("native late note");
    let (_, restored_record) = &restored_evidence[0];
    let content = |summary: &str,
                   refs: &[String],
                   gate: bool,
                   actor: &crate::ActorContext,
                   at: chrono::DateTime<chrono::Utc>| {
        serde_json::json!({
            "summary": summary,
            "refs": refs,
            "gate": gate,
            "status": crate::domain::status_note_role(actor).is_some(),
            "actor_id": actor.actor_id,
            "session_id": actor.session_id,
            "recorded_at": at,
        })
    };
    assert_eq!(
        content(
            &restored_record.summary,
            &restored_record.refs,
            restored_record.gate.is_some(),
            &restored_record.actor,
            restored_record.created_at,
        ),
        content(
            &native_record.summary,
            &native_record.refs,
            native_record.gate.is_some(),
            &native_record.actor,
            native_record.created_at,
        )
    );
    assert_eq!(native_record.refs, refs);
}

// A restored late note whose response was lost after it committed is
// recovered on retry under the same key, later and with the same content,
// without appending again; a note under that key with other content is
// refused instead of adopted.
#[test]
fn an_interrupted_restored_late_note_recovers_only_its_own_content() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let (_native, restored, root) = completed_both_ways(directory.path());
    let refs = vec!["review:interrupted".to_owned()];
    let summary = "late note whose response was lost";
    // Begins the protocol attempt for `attempted` and commits a restored note
    // with `stored_summary` under its key, as an interrupted call leaves it.
    let interrupt = |attempted: &str, stored_summary: &str, second: i64| {
        let mut store = restored.store().expect("store");
        let basis = restored
            .protocol_basis(&store, true, false, Some(root.work_id), at(second))
            .expect("note basis");
        let note = WorkNoteIntent {
            status: false,
            summary: attempted,
            refs: &refs,
        };
        let intent = restored.protocol_intent(&note);
        let raw_key = restored
            .effective_idempotency_key("", "work_update:note", &basis, &intent, at(second))
            .expect("derived note key");
        store
            .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
                project_id: &restored.project_id,
                session_id: &restored.session_id,
                operation: "work_update:note",
                idempotency_key: &raw_key,
                intent: &intent,
                basis: &basis,
                now: at(second),
            })
            .expect("pending note attempt");
        let scoped_key = restored
            .core_operation_key("work_update:note", &raw_key, "record_work_note")
            .expect("note core key");
        let current = store.get_work_item(root.work_id).expect("item");
        store
            .record_restored_work_evidence(
                &RecordRestoredWorkEvidenceRequest {
                    work_id: root.work_id,
                    expected_work_revision: current.revision,
                    holder: restored.session_id.clone(),
                    input: RestoredWorkEvidenceInput::Note {
                        status: false,
                        summary: stored_summary.into(),
                        refs: refs.clone(),
                    },
                    actor: restored.post_completion_actor(
                        "work_update",
                        "simulate a lost restored note response",
                    ),
                    idempotency_key: scoped_key,
                    recorded_at: at(second),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("restored note capture")
    };
    let restored_count = || {
        SqliteStore::open(directory.path().join("restored.sqlite3"))
            .expect("restored store")
            .restored_work_evidence(root.work_id)
            .expect("restored evidence")
            .len()
    };

    let committed = interrupt(summary, summary, 10);
    // The retry comes later than the capture, so the stored request differs
    // by its time; the content decides.
    let recovered = restored
        .work_note_on(Some(&root.short_ref), summary, &refs, at(12))
        .expect("recover the interrupted note");
    assert_eq!(
        recovered.evidence.result,
        serde_json::to_value(&committed).expect("evidence id")
    );
    assert_eq!(restored_count(), 1, "nothing appended on recovery");
    let replayed = restored
        .work_note_on(Some(&root.short_ref), summary, &refs, at(12))
        .expect("replay the recovered note");
    assert_eq!(replayed.evidence.result, recovered.evidence.result);
    assert_eq!(restored_count(), 1);

    // Under another attempt's key, a stored note with other content is refused.
    let second_summary = "a second note whose response was lost";
    let other = interrupt(second_summary, "a different finding", 20);
    let refused = restored
        .work_note_on(Some(&root.short_ref), second_summary, &refs, at(22))
        .expect_err("other content under the key must refuse");
    assert!(
        matches!(
            refused,
            StoreError::WorkOperationIdempotencyConflict { ref operation, .. }
                if operation == "record_restored_work_evidence"
        ),
        "{refused:?}"
    );
    assert_eq!(restored_count(), 2, "the refusal appends nothing");
    assert_ne!(
        serde_json::to_value(&other).expect("other id"),
        recovered.evidence.result
    );
}
