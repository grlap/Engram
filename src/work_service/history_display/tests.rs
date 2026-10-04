use super::*;
use crate::work_service::{test_support::*, *};
use crate::{ObjectId, ProjectId, SessionId, WorkId, WorkPlanningAuthority};

fn fixture() -> (
    crate::test_support::TempHome,
    LocalWorkService,
    WorkEvent,
    FeedPosition,
) {
    let home = crate::test_support::temp_home().unwrap();
    let service = LocalWorkService::new(
        home.path().join("work.db"),
        ProjectId("history-display".into()),
        "writer".into(),
        SessionId("writer".into()),
        None,
    );
    let work = proposed_root(
        service
            .work_propose(
                root_input("A title repeated only in peer context", "create"),
                at(0),
            )
            .unwrap(),
    );
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: None,
                recovery_reason: None,
                idempotency_key: "claim".into(),
            },
            at(1),
        )
        .unwrap();
    let store = service.store().unwrap();
    let entry = store.work_event_tail(work.work_id, 1).unwrap().remove(0);
    let event = store.get::<WorkEvent>(&entry.object_id).unwrap().unwrap();
    drop(store);
    (home, service, event, entry.position)
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the table names every transition arm and its discriminating facts"
)]
fn every_transition_keeps_its_detail_before_the_title_with_a_utf8_byte_bound() {
    use WorkTransition as T;

    let (_home, _service, mut event, _) = fixture();
    let claim = event.claim.clone().unwrap();
    let object = ObjectId::mint();
    let child = WorkId(uuid::Uuid::new_v4());
    let offer = crate::WorkHandoffOfferId(uuid::Uuid::new_v4());
    let authority = WorkPlanningAuthority::Project;
    let reason = format!("Specific reason {}", "界".repeat(1000));
    let rows = vec![
        (
            T::Created {
                prerequisites: Vec::new(),
            },
            "without prerequisites",
        ),
        (
            T::Created {
                prerequisites: vec![child],
            },
            "1 prerequisite",
        ),
        (
            T::Created {
                prerequisites: vec![child, child],
            },
            "2 prerequisites",
        ),
        (
            T::Decomposed {
                children: Vec::new(),
                authority: authority.clone(),
            },
            "added 0 child items",
        ),
        (
            T::Decomposed {
                children: vec![child],
                authority: authority.clone(),
            },
            "added 1 child item",
        ),
        (
            T::Decomposed {
                children: vec![child, child],
                authority: authority.clone(),
            },
            "added 2 child items",
        ),
        (
            T::Revised {
                authority: authority.clone(),
            },
            "title, acceptance",
        ),
        (
            T::PrerequisiteAdded {
                prerequisite_id: child,
                authority: authority.clone(),
            },
            "added prerequisite w-",
        ),
        (
            T::PrerequisiteRemoved {
                prerequisite_id: child,
                authority,
            },
            "removed prerequisite w-",
        ),
        (
            T::Blocked {
                blocker_id: "needs a decision".into(),
            },
            "blocker ",
        ),
        (
            T::Unblocked {
                blocker_id: "needs a decision".into(),
            },
            "cleared blocker ",
        ),
        (
            T::Claimed {
                claim: claim.clone(),
                recovered: false,
            },
            "by a session",
        ),
        (
            T::Claimed {
                claim: claim.clone(),
                recovered: true,
            },
            "after recovery by a session",
        ),
        (
            T::ClaimRenewed {
                claim: claim.clone(),
            },
            "renewed by its holder",
        ),
        (
            T::Released {
                claim_id: claim.claim_id,
                fence: claim.fence,
                reason: reason.clone(),
            },
            "because Specific reason",
        ),
        (
            T::Checkpointed {
                checkpoint: object.clone(),
            },
            "progress checkpoint",
        ),
        (
            T::HandoffOffered {
                offer_id: offer,
                to: SessionId("peer".into()),
                checkpoint: object.clone(),
                offer: object.clone(),
            },
            "to another session",
        ),
        (
            T::HandoffExpired {
                offer_id: offer,
                offer: object.clone(),
            },
            "offer expired",
        ),
        (
            T::HandoffCancelled {
                offer_id: offer,
                offer: object.clone(),
                reason: reason.clone(),
            },
            "because Specific reason",
        ),
        (
            T::HandedOff {
                offer_id: offer,
                claim_id: claim.claim_id,
                from: claim.holder.clone(),
                to: SessionId("peer".into()),
                fence: claim.fence,
                checkpoint: object.clone(),
                offer: object.clone(),
            },
            "from one session to another",
        ),
        (
            T::EvidenceAdded {
                evidence: object.clone(),
            },
            "recorded evidence",
        ),
        (
            T::MemoryCaptured {
                version: object.clone(),
                assertion: object.clone(),
            },
            "shared work memory",
        ),
        (
            T::TypedEvidenceAdded {
                evidence: object.clone(),
                evidence_kind: crate::WorkEvidenceKind::Generic,
            },
            "generic evidence",
        ),
        (T::Completed { seal: object }, "completed"),
        (
            T::Disposed {
                lifecycle: crate::WorkLifecycle::Superseded,
                replacement_id: Some(child),
                reason: reason.clone(),
            },
            "to superseded by w-",
        ),
        (
            T::RequiredChildWaived {
                child_id: child,
                child_revision: 1,
                reason: reason.clone(),
            },
            "waived required child w-",
        ),
        (
            T::Reopened {
                run_id: claim.run_id,
                generation: 7,
                reason,
            },
            "generation 7 because Specific reason",
        ),
    ];
    for (transition, expected) in rows {
        event.transition = transition;
        let display = HistoryDisplay {
            event: event.clone(),
            fields: vec!["title", "acceptance"],
            cleared: None,
        };
        let own = display.summary(false);
        assert!(own.starts_with(expected), "{own}");
        if matches!(event.transition, T::Created { .. } | T::Decomposed { .. }) {
            assert_eq!(own, expected);
        }
        assert!(!own.contains(&event.work.title));
        for line in [own, display.summary(true)] {
            assert!(line.len() <= 192);
        }
        if matches!(event.transition, T::RequiredChildWaived { .. }) {
            let line = display.summary(false);
            assert!(line.contains(&child.0.simple().to_string()[20..]));
            assert!(line.contains("because Specific reason"));
            assert!(
                line.len() > 100,
                "the reason uses the available space: {line}"
            );
            assert!(line.ends_with("..."));
        }
    }
}

#[test]
fn created_and_unchanged_revision_keep_their_distinct_history_facts() {
    let (_home, _service, mut event, _) = fixture();
    event.transition = WorkTransition::Created {
        prerequisites: Vec::new(),
    };
    let mut display = HistoryDisplay {
        event,
        fields: Vec::new(),
        cleared: None,
    };
    assert_eq!(display.summary(false), "without prerequisites");
    display.event.actor = crate::domain::peer_child_proposal_actor(display.event.actor);
    assert_eq!(display.summary(false), "peer optional-child proposal");
    display.event.transition = WorkTransition::Revised {
        authority: WorkPlanningAuthority::Project,
    };
    assert_eq!(display.summary(false), "no planning change");
}

#[test]
fn native_and_replayed_history_use_private_facts_without_changing_delivery_bytes() {
    let (_home, writer, event, position) = fixture();
    let store = writer.store().unwrap();
    let expected = super::super::project_work_event(&store, &event, &position).unwrap();
    let display = HistoryDisplay::load(&store, &event, &position).unwrap();
    assert_eq!(display.summary(false), "by a session");
    let reader = LocalWorkService::new(
        writer.database.clone(),
        writer.project_id.clone(),
        "reader".into(),
        SessionId("reader".into()),
        None,
    );
    let query = || WorkNextQuery {
        sections: vec![WorkNextSection::Changes],
        ..Default::default()
    };
    let page = reader.work_next(50, query(), at(2)).unwrap();
    let bytes = store
        .staged_work_session_delivery_payload(&writer.project_id, &SessionId("reader".into()))
        .unwrap()
        .unwrap();
    let cursor = store
        .work_session_state(&writer.project_id, &SessionId("reader".into()), at(2))
        .unwrap()
        .project_cursor;
    let replay = reader
        .work_next_with_delivery_token(50, Some(cursor), None, query(), at(3))
        .unwrap();
    assert_eq!(
        bytes,
        store
            .staged_work_session_delivery_payload(&writer.project_id, &SessionId("reader".into()))
            .unwrap()
            .unwrap()
    );
    assert_eq!(
        serde_json::to_vec(&page.changes).unwrap(),
        serde_json::to_vec(&replay.changes).unwrap()
    );
    let changed = page.changes.unwrap().into_iter().find(|change| matches!(&change.delivery, WorkChangeProjection::Visible(summary) if summary.change_kind == "claimed")).unwrap();
    assert_eq!(
        serde_json::to_value(&changed.delivery).unwrap(),
        serde_json::to_value(expected).unwrap()
    );
    assert!(changed.history_display.is_some());
    drop(store);
    let focus = writer
        .work_focus_for_agent(&event.work.short_ref, at(3))
        .unwrap();
    assert!(
        focus
            .history
            .items
            .iter()
            .all(|change| change.history_display.is_some())
    );
}
