use super::*;
use crate::storage::work::integrity::take_graph_scan_metrics;
use crate::storage::work::planning::take_descendant_scan_count;

fn single_root(nested: bool) -> ProposeWorkPlanRequest {
    let mut input = large::forest_with_edges(MAX_WORK_PLAN_EDGES);
    for index in 1..input.plan.tasks.len() {
        let parent = if nested { (index - 1) / 8 } else { 0 };
        input.plan.tasks[index].parent_key = Some(format!("task{parent}"));
    }
    input
}

#[test]
fn atomic_plan_single_root_boundary_preserves_graph_and_bounded_admission() {
    for nested in [false, true] {
        let mut store = SqliteStore::open_in_memory().expect("store");
        let input = single_root(nested);
        crate::canonical::reset_canonical_decode_count();
        take_graph_scan_metrics();
        take_descendant_scan_count();
        let start = std::time::Instant::now();
        let receipt = store
            .propose_work_plan(&input, &DevelopmentNoopRedactor)
            .expect("256-task single root");
        let decodes = crate::canonical::canonical_decode_count();
        // Calibrated after measuring 6,430 flat and 6,861 nested decodes;
        // this deterministic counter needs no allowance for timing variance.
        // 7,000 leaves about 2% headroom and rejects one extra decode per edge
        // even for the cheaper flat fixture (6,430 + 1,024 > 7,000).
        assert!(
            decodes < 7_000,
            "single root exceeds the calibrated admission decode budget: {decodes}"
        );
        assert_eq!(
            take_graph_scan_metrics(),
            (1, MAX_WORK_PLAN_TASKS + MAX_WORK_PLAN_EDGES)
        );
        assert_eq!(take_descendant_scan_count(), 1);
        eprintln!(
            "single root nested={nested} tasks=256 edges=1024 decodes={decodes} elapsed={:?}",
            start.elapsed()
        );
        let root = receipt.tasks[0].work_id;
        assert_eq!(receipt.tasks.len(), 256);
        for (draft, mapping) in input.plan.tasks.iter().zip(&receipt.tasks) {
            let item = store.get_work_item(mapping.work_id).expect("item");
            assert_eq!(item.root_id, root);
            assert_eq!(mapping.key, draft.key);
            let parent = draft.parent_key.as_ref().map(|key| {
                receipt
                    .tasks
                    .iter()
                    .find(|row| &row.key == key)
                    .expect("parent mapping")
                    .work_id
            });
            assert_eq!(item.parent_id, parent);
        }
        assert!(store.verify_all().expect("doctor").is_healthy());
        let before = test_database_shape_snapshot(&store.connection).expect("before replay");
        assert_eq!(
            store
                .propose_work_plan(&input, &DevelopmentNoopRedactor)
                .expect("replay"),
            receipt
        );
        assert_eq!(
            test_database_shape_snapshot(&store.connection).expect("after replay"),
            before
        );
    }
}

#[test]
fn atomic_plan_single_root_round_trip_and_restore_limit_refusal() {
    let input = single_root(false);
    let mut source = SqliteStore::open_in_memory().expect("source");
    let receipt = source
        .propose_work_plan(&input, &DevelopmentNoopRedactor)
        .expect("plan");
    let extra = source
        .create_work(
            &root_request("atomic-plan", "separate root", 1),
            &DevelopmentNoopRedactor,
        )
        .expect("separate root");
    let mut saved = source
        .save_work_graph_snapshot(
            &input.project_id,
            &actor("exporter"),
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(2),
            &DevelopmentNoopRedactor,
        )
        .expect("export")
        .document;
    let bytes = serde_json::to_vec(&saved).expect("bytes");
    let mut restored = SqliteStore::open_in_memory().expect("destination");
    restored
        .load_work_graph_snapshot(
            &input.project_id,
            &actor("importer"),
            &bytes,
            false,
            at(3),
            &DevelopmentNoopRedactor,
        )
        .expect("restore at boundary");
    assert!(restored.verify_all().expect("restored doctor").is_healthy());
    assert_eq!(
        root_open_descendant_count(&restored.connection, receipt.tasks[0].work_id).expect("count"),
        255
    );

    // Change the otherwise valid exported item's hierarchy and recompute the
    // document digest. The count guard must refuse before history is imported.
    let item = saved
        .body
        .items
        .iter_mut()
        .find(|item| item.work_id == extra.work_id)
        .expect("extra item");
    item.parent_id = Some(receipt.tasks[0].work_id);
    item.root_id = receipt.tasks[0].work_id;
    saved.manifest.body_sha256 = CanonicalObject::freeze(&saved.body)
        .expect("body")
        .hash()
        .clone();
    let bytes = serde_json::to_vec(&saved).expect("oversized bytes");
    let mut destination = SqliteStore::open_in_memory().expect("empty destination");
    let before = test_database_shape_snapshot(&destination.connection).expect("before");
    for dry_run in [true, false] {
        let error = destination
            .load_work_graph_snapshot(
                &input.project_id,
                &actor("importer"),
                &bytes,
                dry_run,
                at(3),
                &DevelopmentNoopRedactor,
            )
            .expect_err("overfull root");
        assert!(
            matches!(error, StoreError::InvalidGraphSnapshot(reason) if reason == "open descendant count exceeds the work limit")
        );
        assert_eq!(
            test_database_shape_snapshot(&destination.connection).expect("after"),
            before
        );
    }
}
