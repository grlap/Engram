use super::*;
use crate::work_service::test_support::*;
use crate::work_service::{
    LocalWorkService, WorkChangeProjection, WorkNextQuery, WorkNextSection, WorkNextView,
    WorkUpdateInput,
};
use crate::{ProjectId, SessionId, WorkRunId};

#[test]
fn canonical_capture_context_is_rehydrated_on_replay_and_rejects_mismatched_basis() {
    let directory = crate::test_support::temp_home().unwrap();
    let path = directory.path().join("work.db");
    let project = ProjectId("capture-context".into());
    let writer = LocalWorkService::new(
        path.clone(),
        project.clone(),
        "writer".into(),
        SessionId("writer".into()),
        None,
    );
    writer
        .work_propose(root_input("Capture context", "create"), at(0))
        .unwrap();
    writer
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: None,
                recovery_reason: None,
                idempotency_key: "claim".into(),
            },
            at(1),
        )
        .unwrap();
    writer
        .work_note_on(None, "Native note", &[], at(2))
        .unwrap();
    writer
        .work_complete(completion_input("Delivered capture", "done"), at(3))
        .unwrap();
    let reader = LocalWorkService::new(
        path.clone(),
        project.clone(),
        "reader".into(),
        SessionId("reader".into()),
        None,
    );
    let query = || WorkNextQuery {
        sections: vec![WorkNextSection::Changes],
        ..Default::default()
    };
    let store = SqliteStore::open(&path).unwrap();
    let replacement = LocalWorkService::new(
        path,
        project.clone(),
        "reader".into(),
        SessionId("reader".into()),
        None,
    );
    let metadata = |view: &WorkNextView| {
        view.changes
            .as_ref()
            .unwrap()
            .iter()
            .map(|change| (change.capture.clone(), change.completion_checkpoint.clone()))
            .collect::<Vec<_>>()
    };
    let mut changes = Vec::new();
    // The changes section has its own byte budget: a completion can be on a
    // later page even when the requested count covers the whole feed.
    for _ in 0..10 {
        let first = reader.work_next(50, query(), at(4)).unwrap();
        if first.changes.as_ref().unwrap().is_empty() {
            break;
        }
        let before = store
            .staged_work_session_delivery_payload(&project, &SessionId("reader".into()))
            .unwrap()
            .unwrap();
        let confirmed = store
            .work_session_state(&project, &SessionId("reader".into()), at(4))
            .unwrap()
            .project_cursor;
        let replay = replacement
            .work_next_with_delivery_token(50, Some(confirmed), None, query(), at(5))
            .unwrap();
        assert_eq!(metadata(&first), metadata(&replay));
        assert_eq!(
            before,
            store
                .staged_work_session_delivery_payload(&project, &SessionId("reader".into()))
                .unwrap()
                .unwrap()
        );
        changes.extend(first.changes.unwrap());
    }
    let completed = changes
        .iter()
        .find(|change| {
            matches!(&change.delivery, WorkChangeProjection::Visible(summary)
                if summary.change_kind == "completed")
        })
        .unwrap();
    let event: WorkEvent = store.get(&completed.entry.object_id).unwrap().unwrap();
    let WorkTransition::Completed { seal: id } = &event.transition else {
        panic!("completion")
    };
    let seal: CompletionSeal = store.get(id).unwrap().unwrap();
    let checkpoint: WorkCheckpoint = store
        .get(seal.checkpoint.as_ref().unwrap())
        .unwrap()
        .unwrap();
    assert!(completion_owns_checkpoint(&event, &seal, &checkpoint));
    assert_eq!(completed.completion_checkpoint, seal.checkpoint);
    let capture = checkpoint_capture(&store, &checkpoint).unwrap().unwrap();
    let checkpoint_event = changes
        .iter()
        .find(|change| {
            change.entry.object_kind == "work_event"
                && change
                    .capture
                    .as_ref()
                    .is_some_and(|address| address.hash == capture)
        })
        .unwrap();
    let checkpoint_object: WorkEvent = store
        .get(&checkpoint_event.entry.object_id)
        .unwrap()
        .unwrap();
    assert!(matches!(
        checkpoint_object.transition,
        WorkTransition::Checkpointed { .. }
    ));
    let mut mismatched = checkpoint_object.clone();
    mismatched.claim.as_mut().unwrap().fence += 1;
    assert!(
        hydrate(
            &store,
            &checkpoint_event.entry,
            &serde_json::to_value(&mismatched).unwrap()
        )
        .unwrap()
        .0
        .is_none()
    );
    mismatched = checkpoint_object;
    mismatched.actor.session_id = Some(SessionId("different".into()));
    assert!(
        hydrate(
            &store,
            &checkpoint_event.entry,
            &serde_json::to_value(&mismatched).unwrap()
        )
        .unwrap()
        .0
        .is_none()
    );
    let note: WorkEvidence = store.get(&capture).unwrap().unwrap();
    let cut = &checkpoint.acknowledged_run_position;
    let capture_entry = store
        .work_feed_between(&cut.feed, cut.position - 1, cut.position)
        .unwrap()
        .pop()
        .unwrap();
    let capture_event: WorkEvent = store.get(&capture_entry.object_id).unwrap().unwrap();
    assert!(capture_checkpoint_matches(
        &capture_event,
        &note,
        &checkpoint
    ));
    let mut wrong_note = note.clone();
    wrong_note.actor.source_tool = Some("work_update".into());
    assert!(!capture_checkpoint_matches(
        &capture_event,
        &wrong_note,
        &checkpoint
    ));
    let mut wrong_event = capture_event.clone();
    wrong_event.actor.reason = "ordinary evidence".into();
    assert!(!capture_checkpoint_matches(
        &wrong_event,
        &note,
        &checkpoint
    ));
    let mut wrong_checkpoint = checkpoint.clone();
    wrong_checkpoint.actor.reason = NOTE_CHECKPOINT_REASON.into();
    assert!(
        checkpoint_capture(&store, &wrong_checkpoint)
            .unwrap()
            .is_none()
    );
    let mut wrong_event = event.clone();
    wrong_event.claim.as_mut().unwrap().fence = seal.claim_fence;
    assert!(!completion_owns_checkpoint(
        &wrong_event,
        &seal,
        &checkpoint
    ));
    wrong_event = event.clone();
    wrong_event.claim.as_mut().unwrap().state = WorkClaimState::Active;
    assert!(!completion_owns_checkpoint(
        &wrong_event,
        &seal,
        &checkpoint
    ));
    let mut overflowed_seal = seal.clone();
    overflowed_seal.claim_fence = i64::MAX;
    assert!(!completion_owns_checkpoint(
        &event,
        &overflowed_seal,
        &checkpoint
    ));
    let mut wrong = checkpoint.clone();
    wrong.run_id = WorkRunId(uuid::Uuid::new_v4());
    assert!(!completion_owns_checkpoint(&event, &seal, &wrong));
    wrong = checkpoint.clone();
    wrong.claim_fence += 1;
    assert!(!completion_owns_checkpoint(&event, &seal, &wrong));
    wrong = checkpoint;
    wrong.actor.source_tool = Some("work_update".into());
    assert!(!completion_owns_checkpoint(&event, &seal, &wrong));

    let note_change = changes
        .iter()
        .find(|change| {
            change.entry.object_kind == "work_evidence"
                && change
                    .capture
                    .as_ref()
                    .is_some_and(|address| address.hash == change.entry.object_id)
        })
        .unwrap();
    let note: WorkEvidence = store.get(&note_change.entry.object_id).unwrap().unwrap();
    let note_checkpoint = changes
        .iter()
        .find(|change| {
            change.entry.object_kind == "work_checkpoint" && change.capture == note_change.capture
        })
        .unwrap();
    let checkpoint: WorkCheckpoint = store
        .get(&note_checkpoint.entry.object_id)
        .unwrap()
        .unwrap();
    let cut = &checkpoint.acknowledged_run_position;
    let entry = store
        .work_feed_between(&cut.feed, cut.position - 1, cut.position)
        .unwrap()
        .pop()
        .unwrap();
    let event: WorkEvent = store.get(&entry.object_id).unwrap().unwrap();
    assert!(capture_checkpoint_matches(&event, &note, &checkpoint));
    let mut wrong = checkpoint.clone();
    wrong.claim_fence += 1;
    assert!(!capture_checkpoint_matches(&event, &note, &wrong));
    wrong = checkpoint;
    wrong.actor.session_id = Some(SessionId("different".into()));
    assert!(!capture_checkpoint_matches(&event, &note, &wrong));
}
