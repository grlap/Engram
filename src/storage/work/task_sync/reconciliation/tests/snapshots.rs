use super::*;
use crate::storage::work::{WorkEventDraft, feeds, planning};

pub(super) fn roundtrip(store: &mut SqliteStore, project: &ProjectId, kind: &str) {
    let saved = store
        .save_work_graph_snapshot(
            project,
            &actor("saver"),
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(9),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let bytes = serde_json::to_vec_pretty(&saved.document).unwrap();
    assert!(String::from_utf8_lossy(&bytes).contains(kind));
    let mut destination = SqliteStore::open_in_memory().unwrap();
    destination
        .load_work_graph_snapshot(
            project,
            &actor("loader"),
            &bytes,
            false,
            at(10),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    for table in ["work_runs", "work_claims", "work_completion_seals"] {
        let count: i64 = destination
            .connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0, "history must not restore {table}");
    }
    let again = destination
        .save_work_graph_snapshot(
            project,
            &actor("saver"),
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(11),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let histories = again
        .document
        .body
        .records
        .iter()
        .map(|record| match &record.payload {
            crate::WorkGraphSnapshotRecordPayload::Native { history } => history.as_ref().clone(),
            crate::WorkGraphSnapshotRecordPayload::Restored { canonical_json, .. } => {
                serde_json::from_value::<crate::RestoredRecord>(canonical_json.clone())
                    .unwrap()
                    .history
            }
        })
        .collect::<Vec<_>>();
    let events = histories
        .iter()
        .flat_map(|history| &history.events)
        .filter(|event| event.kind == kind)
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 1);
    assert!(
        events[0]
            .reason
            .as_ref()
            .is_some_and(|reason| !reason.is_empty())
    );
    assert!(
        events[0].lifecycle.is_none()
            && events[0].related_work_id.is_none()
            && events[0].related_work_revision.is_none()
    );
    assert!(destination.verify_all().unwrap().is_healthy());
}

#[test]
fn reconciliation_abandonment_snapshot_is_inert_and_roundtrips() {
    let home = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(home.path().join("work.db")).unwrap();
    let work = store
        .create_work(
            &root_request("task-sync", "abandon", 1),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let (binding, prepared, intent) = prepared(&mut store, home.path());
    let held = hold(&store, binding, prepared, &intent, vec![work.work_id]);
    let hold_id = store.place_task_sync_reconciliation_hold(&held).unwrap();
    let tx = store.begin_work_mutation().unwrap();
    let mut item = query::load_work_item(&tx, work.work_id).unwrap();
    item.revision += 1;
    item.updated_at = at(8);
    planning::persist_work_item(&tx, &item).unwrap();
    let run = item
        .active_run_id
        .map(|id| query::load_work_run(&tx, id).unwrap());
    let event = feeds::append_work_event(
        &tx,
        &WorkEventDraft {
            schema_version: 1,
            project_id: item.project_id.clone(),
            root_id: item.root_id,
            work_id: item.work_id,
            run_id: item.active_run_id,
            revision: item.revision,
            work: item,
            run,
            root_execution: None,
            claim: None,
            handoff_offer: None,
            blocker: None,
            transition: WorkTransition::ReconciliationHoldAbandoned {
                hold: hold_id.clone(),
                reason: "original placer abandons candidate".into(),
            },
            actor: held.actor.clone(),
            created_at: at(8),
        },
    )
    .unwrap();
    tx.execute(
        "UPDATE work_task_sync_reconciliation_holds SET resolution_id=?1 WHERE hold_id=?2",
        params![event.0.as_str(), hold_id.as_str()],
    )
    .unwrap();
    super::super::active::resolve_on(&tx, &hold_id).unwrap();
    tx.commit().unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
    roundtrip(
        &mut store,
        &work.project_id,
        "reconciliation_hold_abandoned",
    );
}
