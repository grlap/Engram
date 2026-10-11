use super::*;
use crate::storage::work::test_support::{checkpoint, complete, evidence};
use crate::task_sync::{
    TaskSyncExportOptions,
    activity::{ActivityMap, ActivityNotice, ActivityState},
};

mod retention;
mod validation;

fn export(store: &SqliteStore, project: &ProjectId, machine: &str) -> TaskTree {
    store
        .export_task_tree_with_options(
            project,
            &TaskSyncExportOptions {
                include_notes: false,
                origin_machine: Some(machine.into()),
            },
        )
        .unwrap()
        .tree
}

fn notices(tree: &TaskTree) -> Vec<&ActivityNotice> {
    tree.tasks
        .iter()
        .flat_map(|task| task.activity.values())
        .flat_map(|map| map.values())
        .collect()
}

fn round_trip(tree: &TaskTree) {
    let bytes = tree.documents().unwrap();
    let parsed = TaskTree::from_documents(&bytes).unwrap();
    let mut target = SqliteStore::open_in_memory().unwrap();
    import(&mut target, &parsed);
    let before = state(&target);
    assert!(
        !target
            .import_task_tree(
                &tree.project.project_id,
                &actor("importer"),
                &parsed,
                false,
                at(6)
            )
            .unwrap()
            .changed
    );
    assert_eq!(
        target
            .export_task_tree(&tree.project.project_id)
            .unwrap()
            .documents()
            .unwrap(),
        bytes
    );
    assert!(target.verify_all().unwrap().is_healthy());
    assert_eq!(state(&target), before);
    for table in [
        "work_runs",
        "work_claims",
        "work_handoff_offers",
        "work_completion_seals",
        "work_feed_entries",
    ] {
        let count: i64 = target
            .connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0, "activity imported no authority into {table}");
    }
    for item in &tree.tasks {
        let restored = target.get_work_item(item.id).unwrap();
        assert!(restored.restored && restored.active_run_id.is_none());
    }
}

fn release(store: &mut SqliteStore, work: &WorkItem, live: &crate::WorkClaim, second: i64) {
    store
        .release_work(
            &crate::domain::ReleaseWorkRequest {
                work_id: work.work_id,
                run_id: live.run_id,
                expected_work_revision: work.revision,
                holder: live.holder.clone(),
                claim_id: live.claim_id,
                claim_fence: live.fence,
                reason: "private execution reason".into(),
                waiver_reason: Some("participant left".into()),
                actor: actor(&live.holder.0),
                idempotency_key: format!("release-{second}"),
                released_at: at(second),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
}

#[test]
fn task_sync_activity_start_renew_release_recover_preserve_original_identity() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let work = store
        .create_work(
            &root_request("activity", "create", 0),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let live = claim(&mut store, &work, "first", "claim", 1, 10);
    let work = store.get_work_item(work.work_id).unwrap();
    let started = export(&store, &work.project_id, "machine-A");
    assert!(!started.project.notes_included);
    assert_eq!(notices(&started).len(), 1);
    let notice = notices(&started)[0].clone();
    assert_eq!(notice.state, ActivityState::Started);
    assert_eq!(notice.state_revision, 1);
    assert_eq!(notice.event_id, notice.activity_id);
    assert_eq!(notice.actor.actor_id, "first");
    let event =
        query::canonical_work_event_records_for_item(&store.connection, work.work_id).unwrap();
    assert!(event.iter().any(|(id, event)| id == &notice.activity_id
        && matches!(event.transition, crate::WorkTransition::Claimed { .. })));
    round_trip(&started);
    let renewed = claim(&mut store, &work, "first", "renew", 2, 20);
    checkpoint(&mut store, &work, &renewed, "first", "checkpoint", 3, &[]);
    assert_eq!(
        export(&store, &work.project_id, "machine-A")
            .documents()
            .unwrap(),
        started.documents().unwrap()
    );
    // Recovery advances the fence but records no end to its expired predecessor.
    let records =
        query::canonical_work_event_records_for_item(&store.connection, work.work_id).unwrap();
    let recovered_at =
        records.last().unwrap().1.claim.as_ref().unwrap().expires_at + chrono::Duration::seconds(1);
    let recovered = store
        .claim_work(
            &crate::domain::ClaimWorkRequest {
                work_id: work.work_id,
                expected_work_revision: work.revision,
                expected_run_id: work.active_run_id,
                holder: crate::SessionId("second".into()),
                ttl_seconds: 100,
                recovery_reason: Some("recover expired work".into()),
                actor: actor("second"),
                idempotency_key: "recover".into(),
                claimed_at: recovered_at,
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let recovered_tree = export(&store, &work.project_id, "machine-A");
    assert_eq!(notices(&recovered_tree).len(), 2);
    assert!(
        notices(&recovered_tree)
            .iter()
            .all(|n| n.state == ActivityState::Started && n.state_revision == 1)
    );
    round_trip(&recovered_tree);
    let released_second = (recovered_at - at(0)).num_seconds() + 1;
    release(&mut store, &work, &recovered, released_second);
    let released = export(&store, &work.project_id, "machine-A");
    let first = notices(&released)
        .into_iter()
        .find(|n| n.actor.actor_id == "first")
        .unwrap();
    assert_eq!(first, &notice);
    let second = notices(&released)
        .into_iter()
        .find(|n| n.actor.actor_id == "second")
        .unwrap();
    assert_eq!(second.state, ActivityState::Released);
    assert_eq!(second.state_revision, 2);
    assert_ne!(second.event_id, second.activity_id);
    round_trip(&released);
    let text = String::from_utf8(serde_json::to_vec(&released.tasks[0].activity).unwrap()).unwrap();
    for excluded in [
        live.claim_id.0.to_string(),
        live.run_id.0.to_string(),
        "private execution reason".into(),
        "claim_fence".into(),
        "expires_at".into(),
        "session_id".into(),
    ] {
        assert!(
            !text.contains(&excluded),
            "exported private field {excluded}"
        );
    }
}

#[test]
fn task_sync_activity_handoff_finishes_successor_without_rewriting_first_actor() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let work = store
        .create_work(
            &root_request("handoff-activity", "create", 0),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let live = claim(&mut store, &work, "outgoing", "claim", 1, 100);
    let work = store.get_work_item(work.work_id).unwrap();
    let started = export(&store, &work.project_id, "machine-A");
    let first_id = notices(&started)[0].activity_id.clone();
    let offer = store
        .offer_work_handoff(
            &crate::domain::OfferWorkHandoffRequest {
                work_id: work.work_id,
                run_id: live.run_id,
                expected_work_revision: work.revision,
                from: live.holder.clone(),
                to: crate::SessionId("incoming".into()),
                claim_id: live.claim_id,
                claim_fence: live.fence,
                ttl_seconds: 100,
                checkpoint_summary: "private handoff".into(),
                actor: actor("outgoing"),
                idempotency_key: "offer".into(),
                offered_at: at(2),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    assert_eq!(
        export(&store, &work.project_id, "machine-A")
            .documents()
            .unwrap(),
        started.documents().unwrap()
    );
    let successor = store
        .accept_work_handoff(
            &crate::domain::AcceptWorkHandoffRequest {
                work_id: work.work_id,
                offer_id: offer.offer_id,
                to: crate::SessionId("incoming".into()),
                actor: actor("incoming"),
                idempotency_key: "accept".into(),
                accepted_at: at(3),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let transferred = export(&store, &work.project_id, "machine-A");
    let old = &transferred.tasks[0].activity["machine-A"][&first_id];
    assert_eq!(old.state, ActivityState::Released);
    assert_eq!(old.actor.actor_id, "outgoing");
    let next = notices(&transferred)
        .into_iter()
        .find(|n| n.actor.actor_id == "incoming")
        .unwrap();
    assert_eq!(next.state, ActivityState::Started);
    assert_eq!(old.event_id, next.activity_id);
    round_trip(&transferred);
    let proof = evidence(&mut store, &work, &successor, "incoming", "evidence", 4);
    checkpoint(
        &mut store,
        &work,
        &successor,
        "incoming",
        "checkpoint",
        5,
        std::slice::from_ref(&proof),
    );
    complete(
        &mut store, &work, &successor, "incoming", &proof, "complete", 6,
    )
    .unwrap();
    let finished = export(&store, &work.project_id, "machine-A");
    assert_eq!(finished.tasks[0].activity["machine-A"][&first_id], *old);
    let last = notices(&finished)
        .into_iter()
        .find(|n| n.actor.actor_id == "incoming")
        .unwrap();
    assert_eq!(last.state, ActivityState::Finished);
    assert_eq!(last.state_revision, 2);
    round_trip(&finished);
}

#[test]
fn task_sync_activity_disposal_abandons_only_current_recorded_episode() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let work = store
        .create_work(
            &root_request("disposal-activity", "create", 0),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    claim(&mut store, &work, "worker", "claim", 1, 100);
    let work = store.get_work_item(work.work_id).unwrap();
    store
        .dispose_work(
            &crate::domain::DisposeWorkRequest {
                work_id: work.work_id,
                expected_work_revision: work.revision,
                disposition: crate::WorkDisposition::Cancelled,
                replacement_id: None,
                reason: "cancelled by planner".into(),
                actor: actor("worker"),
                idempotency_key: "dispose".into(),
                disposed_at: at(2),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let tree = export(&store, &work.project_id, "machine-A");
    assert_eq!(notices(&tree)[0].state, ActivityState::Abandoned);
    assert_eq!(notices(&tree)[0].actor.actor_id, "worker");
    round_trip(&tree);
}
