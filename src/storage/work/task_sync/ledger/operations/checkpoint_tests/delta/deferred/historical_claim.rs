use super::*;

#[test]
fn historical_claim_defers_replays_and_survives_projection_rebuild() {
    let mut tree = candidate();
    for task in &mut tree.tasks {
        // The historical fixture needs only a live claim, without later
        // acceptance obligations that would cite its temporary minted event.
        task.acceptance_bindings.clear();
        task.evaluation_mode = None;
    }
    let mut historical = None;
    let mut f = claimed_fixture_with_setup(&tree, |store, selected| {
        let claim = store.claim_fixture(selected);
        let (id, bytes): (String, Vec<u8>) = store
            .connection
            .query_row(
                "SELECT object_id,canonical_json FROM objects WHERE object_kind='work_event'
             AND json_extract(canonical_json,'$.claim.claim_id')=?1
             ORDER BY json_extract(canonical_json,'$.revision') DESC LIMIT 1",
                [claim.claim_id.0.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let event: crate::WorkEvent = serde_json::from_slice(&bytes).unwrap();
        let object = CanonicalObject::freeze(&event).unwrap();
        assert_eq!(object.key().as_str().len(), 64);
        assert_eq!(object.bytes(), bytes);
        // Seed the historical store before capturing the incoming delta's cut.
        // The live claim and original event bytes stay exactly as authored.
        let tx = store.begin_work_mutation().unwrap();
        SqliteStore::insert_object(&tx, "work_event", &object).unwrap();
        assert_eq!(
            tx.execute(
                "UPDATE work_feed_entries SET object_id=?2 WHERE object_id=?1",
                rusqlite::params![id, object.key().as_str()],
            )
            .unwrap(),
            3,
        );
        tx.execute(
            "UPDATE work_items SET latest_event_id=?2 WHERE latest_event_id=?1",
            rusqlite::params![id, object.key().as_str()],
        )
        .unwrap();
        assert_eq!(
            tx.execute("DELETE FROM objects WHERE object_id=?1", [&id])
                .unwrap(),
            1
        );
        tx.commit().unwrap();
        historical = Some(object);
    });
    let historical = historical.unwrap();
    let before_item = f.store.get_work_item(f.selected).unwrap();
    let before_claim = f.store.claim_fixture(f.selected);
    let original = f.apply().unwrap();
    let Transition::PlanningDeferred {
        hold,
        blocking_claims,
        ..
    } = &original.event.transition
    else {
        panic!("historical active claim must defer incoming planning");
    };
    assert_eq!(blocking_claims, std::slice::from_ref(historical.key()));
    for id in [hold, &original.prepared, &original.outcome] {
        assert_eq!(id.as_str().len(), 32);
    }
    assert!(
        f.store
            .task_sync_reconciliation_hold(hold)
            .unwrap()
            .is_some()
    );
    assert_eq!(f.store.get_work_item(f.selected).unwrap(), before_item);
    assert_eq!(f.store.claim_fixture(f.selected), before_claim);
    let before_replay = state(&f.store);
    assert_eq!(f.apply().unwrap(), original);
    assert_eq!(state(&f.store), before_replay);
    let report = f.store.verify_all().unwrap();
    assert!(report.is_healthy(), "{report:#?}");

    drop(f.store);
    SqliteStore::repair_rebuildable_projections(&f.input.binding.store.database).unwrap();
    let store = SqliteStore::open(&f.input.binding.store.database).unwrap();
    assert_eq!(
        store
            .completed_task_sync_delta_import(&f.input.binding_id, &f.input.intent)
            .unwrap(),
        Some(original.clone()),
    );
    assert_eq!(store.get_work_item(f.selected).unwrap(), before_item);
    assert_eq!(store.claim_fixture(f.selected), before_claim);
    assert!(store.task_sync_reconciliation_hold(hold).unwrap().is_some());
    let retained: Vec<u8> = store
        .connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_id=?1",
            [historical.key().as_str()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(retained, historical.bytes());
    let report = store.verify_all().unwrap();
    assert!(report.is_healthy(), "{report:#?}");
}
