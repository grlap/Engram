use super::super::load::{NEWEST_RECORD_VISITS, RELATION_BASIS_WORK};
use super::*;
use crate::{
    RestoredRelationBasis, ReviseWorkRequest, WorkGraphSnapshotBlocker,
    WorkGraphSnapshotLoadResult, WorkRevisionPatch,
};

/// One graph shape: `items` roots, each with `blockers_per_item` manual
/// blockers, and every root after the first taking the previous root as a
/// prerequisite, so each basis has something to sort without a cycle.
struct Shape {
    name: &'static str,
    items: usize,
    blockers_per_item: usize,
}

fn build(store: &mut SqliteStore, project: &ProjectId, shape: &Shape) -> Vec<crate::WorkItem> {
    let mut roots = Vec::with_capacity(shape.items);
    for index in 0..shape.items {
        let root = create_root(
            store,
            project,
            &format!("{} item {index:03}", shape.name),
            &format!("{}-item-{index:03}", shape.name),
        );
        if let Some(previous) = roots.last() {
            let previous: &crate::WorkItem = previous;
            store
                .add_work_prerequisite(
                    &ChangeWorkPrerequisiteRequest {
                        work_id: root.work_id,
                        prerequisite_id: previous.work_id,
                        expected_revision: root.revision,
                        authority: WorkPlanningAuthority::Project,
                        actor: actor("planner-session"),
                        idempotency_key: format!("{}-edge-{index:03}", shape.name),
                        changed_at: at(2),
                    },
                    &DevelopmentNoopRedactor,
                )
                .expect("add prerequisite");
        }
        for blocker in 0..shape.blockers_per_item {
            let current = store.get_work_item(root.work_id).expect("current item");
            store
                .add_work_blocker(
                    &AddWorkBlockerRequest {
                        work_id: root.work_id,
                        expected_work_revision: current.revision,
                        kind: WorkBlockerKind::Manual,
                        detail: format!("{} blocker {index:03}-{blocker}", shape.name),
                        authority: WorkPlanningAuthority::Project,
                        actor: actor("planner-session"),
                        idempotency_key: format!("{}-blocker-{index:03}-{blocker}", shape.name),
                        blocked_at: at(3),
                    },
                    &DevelopmentNoopRedactor,
                )
                .expect("add blocker");
        }
        roots.push(root);
    }
    roots
}

fn save(store: &mut SqliteStore, project: &ProjectId, second: i64) -> WorkGraphSnapshotDocument {
    store
        .save_work_graph_snapshot(
            project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(second),
            &DevelopmentNoopRedactor,
        )
        .expect("save graph")
        .document
}

/// Gives every item one more native event, so the next save appends a
/// native history layer to each item and the record count grows.
fn revise_every_item(store: &mut SqliteStore, document: &WorkGraphSnapshotDocument, round: usize) {
    for item in &document.body.items {
        let current = store.get_work_item(item.work_id).expect("restored item");
        store
            .revise_work(
                &ReviseWorkRequest {
                    work_id: item.work_id,
                    expected_revision: current.revision,
                    patch: WorkRevisionPatch {
                        title: Some(format!("{} (revised {round})", item.title)),
                        ..WorkRevisionPatch::default()
                    },
                    authority: WorkPlanningAuthority::Project,
                    actor: actor("planner-session"),
                    idempotency_key: format!("revise-{}-{round}", item.short_ref),
                    updated_at: at(100 + i64::try_from(round).expect("small round")),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("revise restored item");
    }
}

struct Measured {
    result: Result<WorkGraphSnapshotLoadResult, StoreError>,
    basis_work: usize,
    newest_visits: usize,
    destination: SqliteStore,
}

/// Loads into a fresh store with both counters reset right before the load
/// and read right after it, so a save's own validation never leaks in.
fn load_measured(project: &ProjectId, document: &WorkGraphSnapshotDocument) -> Measured {
    let mut destination = SqliteStore::open_in_memory().expect("destination");
    RELATION_BASIS_WORK.with(|work| work.set(0));
    NEWEST_RECORD_VISITS.with(|visits| visits.set(0));
    let result = destination.load_work_graph_snapshot(
        project,
        &actor("load-session"),
        &snapshot_bytes(document),
        false,
        at(900),
        &DevelopmentNoopRedactor,
    );
    Measured {
        result,
        basis_work: RELATION_BASIS_WORK.with(std::cell::Cell::get),
        newest_visits: NEWEST_RECORD_VISITS.with(std::cell::Cell::get),
        destination,
    }
}

fn record_counts(document: &WorkGraphSnapshotDocument) -> (usize, usize) {
    document
        .body
        .records
        .iter()
        .fold((0, 0), |(native, restored), record| match record.payload {
            WorkGraphSnapshotRecordPayload::Native { .. } => (native + 1, restored),
            WorkGraphSnapshotRecordPayload::Restored { .. } => (native, restored + 1),
        })
}

fn destination_work_count(store: &SqliteStore) -> i64 {
    store
        .connection
        .query_row("SELECT COUNT(*) FROM work_items", [], |row| row.get(0))
        .expect("count destination work")
}

/// A reference basis computed without the loader: the item's prerequisites
/// sorted by id, and its blockers in blocker-id order.
fn reference_basis(document: &WorkGraphSnapshotDocument, work_id: WorkId) -> RestoredRelationBasis {
    let item = document
        .body
        .items
        .iter()
        .find(|item| item.work_id == work_id)
        .expect("item in document");
    let mut prerequisites = item.prerequisites.clone();
    prerequisites.sort_by_key(|id| id.0);
    let mut blockers: Vec<WorkGraphSnapshotBlocker> = document
        .body
        .blockers
        .iter()
        .filter(|blocker| blocker.work_id == work_id)
        .cloned()
        .collect();
    blockers.sort_by(|left, right| left.blocker_id.cmp(&right.blocker_id));
    RestoredRelationBasis {
        prerequisites,
        blockers,
    }
}

fn restored_payloads(
    document: &WorkGraphSnapshotDocument,
) -> Vec<(WorkId, usize, ObjectId, serde_json::Value)> {
    document
        .body
        .records
        .iter()
        .filter_map(|record| match &record.payload {
            WorkGraphSnapshotRecordPayload::Restored {
                object_id,
                canonical_json,
            } => Some((
                record.work_id,
                record.generation_index,
                object_id.clone(),
                canonical_json.clone(),
            )),
            WorkGraphSnapshotRecordPayload::Native { .. } => None,
        })
        .collect()
}

// The bound: building the relation bases costs one unit per item indexed,
// one per blocker grouped and one per basis built, and the newest-record
// lookup examines exactly the records. The exact formula is the proof: a
// loader that rebuilt a basis for the lifecycle check as well as for the
// native record would read 3 × items + blockers, not 2 × items + blockers.
// The same graph is measured with one record per item and, after two
// save-load-save rounds with real revisions, with three: the added records
// are carried ones, which only the newest-record lookup examines, so the
// basis work stays put while the newest visits grow with the records. A
// second shape shows the blocker term.
#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one scenario measures both counters across record counts and shapes"
)]
fn relation_bases_are_built_once_per_item_whatever_the_record_count() {
    let directory = crate::test_support::temp_home().expect("tempdir");
    let mut source =
        SqliteStore::open(directory.path().join("bases-source.db")).expect("source store");
    let shapes = [
        Shape {
            name: "blocked",
            items: 64,
            blockers_per_item: 2,
        },
        Shape {
            name: "plain",
            items: 192,
            blockers_per_item: 0,
        },
    ];
    for shape in &shapes {
        let project = ProjectId(format!("snapshot-bases-{}", shape.name));
        build(&mut source, &project, shape);
        let single = save(&mut source, &project, 10);
        let items = single.body.items.len();
        let blockers = single.body.blockers.len();
        assert_eq!(items, shape.items, "{}: items", shape.name);
        assert_eq!(
            blockers,
            shape.items * shape.blockers_per_item,
            "{}: blockers",
            shape.name
        );
        assert_eq!(
            record_counts(&single),
            (items, 0),
            "{}: one native record per item",
            shape.name
        );
        let expected_work = items + blockers + items;

        let measured = load_measured(&project, &single);
        measured.result.expect("single-generation graph loads");
        assert_eq!(
            measured.basis_work, expected_work,
            "{}: basis work, one generation",
            shape.name
        );
        assert_eq!(
            measured.newest_visits, items,
            "{}: newest visits, one generation",
            shape.name
        );

        // Two more generations per item: revise every restored item so the
        // next save appends a native layer on top of the carried ones.
        let mut round_one = measured.destination;
        revise_every_item(&mut round_one, &single, 1);
        let double = save(&mut round_one, &project, 20);
        assert_eq!(
            record_counts(&double),
            (items, items),
            "{}: two generations",
            shape.name
        );
        let mut round_two = SqliteStore::open_in_memory().expect("round two store");
        round_two
            .load_work_graph_snapshot(
                &project,
                &actor("load-session"),
                &snapshot_bytes(&double),
                false,
                at(21),
                &DevelopmentNoopRedactor,
            )
            .expect("two-generation graph loads");
        revise_every_item(&mut round_two, &double, 2);
        let triple = save(&mut round_two, &project, 30);
        assert_eq!(
            record_counts(&triple),
            (items, 2 * items),
            "{}: three generations",
            shape.name
        );
        assert_eq!(triple.body.items.len(), items);
        assert_eq!(triple.body.blockers.len(), blockers);
        for item in &triple.body.items {
            let original = single
                .body
                .items
                .iter()
                .find(|candidate| candidate.work_id == item.work_id)
                .expect("same item set");
            assert_eq!(
                item.prerequisites, original.prerequisites,
                "{}: prerequisites kept",
                shape.name
            );
        }

        let measured = load_measured(&project, &triple);
        measured.result.expect("three-generation graph loads");
        assert_eq!(
            measured.basis_work, expected_work,
            "{}: basis work does not grow with the record count",
            shape.name
        );
        assert_eq!(
            measured.newest_visits,
            3 * items,
            "{}: the newest lookup examines each record once",
            shape.name
        );
    }
}

// The bases the loader builds equal a reference computed without it, the
// canonical bytes of a carried record are preserved exactly through further
// loads and saves, and a native record's basis is reused by the lifecycle
// check (one build per item, not two).
#[test]
fn restored_bases_match_a_reference_and_carried_bytes_survive() {
    let directory = crate::test_support::temp_home().expect("tempdir");
    let project = ProjectId("snapshot-bases-oracle".into());
    let mut source =
        SqliteStore::open(directory.path().join("oracle-source.db")).expect("source store");
    let shape = Shape {
        name: "oracle",
        items: 8,
        blockers_per_item: 3,
    };
    build(&mut source, &project, &shape);
    let first = save(&mut source, &project, 10);
    assert_eq!(record_counts(&first), (8, 0));

    let measured = load_measured(&project, &first);
    measured.result.expect("first load");
    assert_eq!(
        measured.basis_work,
        8 + 24 + 8,
        "one build per item, reused by the lifecycle check"
    );
    let mut carried = measured.destination;
    let second = save(&mut carried, &project, 20);
    let payloads = restored_payloads(&second);
    assert_eq!(
        payloads.len(),
        8,
        "every native layer became a carried record"
    );
    for (work_id, generation, _, canonical_json) in &payloads {
        assert_eq!(*generation, 0);
        let record: RestoredRecord =
            serde_json::from_value(canonical_json.clone()).expect("carried record decodes");
        assert_eq!(record.work_id, *work_id);
        let reference = reference_basis(&first, *work_id);
        assert_eq!(record.relations, reference, "basis equals the reference");
        assert_eq!(record.relations.blockers.len(), 3);
        assert!(
            record
                .relations
                .blockers
                .windows(2)
                .all(|pair| pair[0].blocker_id < pair[1].blocker_id),
            "blockers are in blocker-id order"
        );
    }

    let mut again = SqliteStore::open_in_memory().expect("third store");
    again
        .load_work_graph_snapshot(
            &project,
            &actor("load-session"),
            &snapshot_bytes(&second),
            false,
            at(21),
            &DevelopmentNoopRedactor,
        )
        .expect("carried graph loads");
    let third = save(&mut again, &project, 30);
    assert_eq!(
        restored_payloads(&third),
        payloads,
        "carried record ids and canonical bytes are preserved exactly"
    );
}

// Each basis refusal fires where it used to: a native record's duplicate
// prerequisite is refused while its record is materialized, and nothing is
// written; a carried-only item's duplicate prerequisite is refused by the
// lifecycle check; an earlier dangling record is refused before a later
// native item's duplicate prerequisite; an item without any record is
// refused by the record phase, before the lifecycle check.
#[test]
fn basis_refusals_keep_their_place_and_leave_no_partial_state() {
    let directory = crate::test_support::temp_home().expect("tempdir");
    let project = ProjectId("snapshot-bases-refusals".into());
    let mut source =
        SqliteStore::open(directory.path().join("refusals-source.db")).expect("source store");
    let shape = Shape {
        name: "refusals",
        items: 4,
        blockers_per_item: 1,
    };
    build(&mut source, &project, &shape);
    let native = save(&mut source, &project, 10);
    assert_eq!(record_counts(&native), (4, 0));
    let last = native
        .body
        .items
        .iter()
        .find(|item| !item.prerequisites.is_empty())
        .expect("a chained item has a prerequisite")
        .clone();
    assert_eq!(
        last.prerequisites.len(),
        1,
        "a chained item has one prerequisite"
    );

    let refuse = |name: &str, document: &WorkGraphSnapshotDocument, message: &str| {
        let measured = load_measured(&project, document);
        match measured.result {
            Err(StoreError::InvalidGraphSnapshot(actual)) => {
                assert_eq!(actual, message, "{name}: refusal text");
            }
            other => panic!("{name}: expected the refusal {message:?}, got {other:?}"),
        }
        assert_eq!(
            destination_work_count(&measured.destination),
            0,
            "{name}: a refused load left partial work"
        );
    };

    let mut duplicate_native = native.clone();
    let item = duplicate_native
        .body
        .items
        .iter_mut()
        .find(|item| item.work_id == last.work_id)
        .expect("last item");
    item.prerequisites.push(item.prerequisites[0]);
    rebind_snapshot_body(&mut duplicate_native);
    refuse(
        "native-duplicate-prerequisite",
        &duplicate_native,
        "work item duplicates a prerequisite",
    );

    // Carry the records so every item is Restored-only, then inject the same
    // duplicate: the first demand for that basis is now the lifecycle check.
    let mut carried = load_measured(&project, &native).destination;
    let restored_only = save(&mut carried, &project, 20);
    assert_eq!(record_counts(&restored_only), (0, 4));
    let mut duplicate_carried = restored_only.clone();
    let item = duplicate_carried
        .body
        .items
        .iter_mut()
        .find(|item| item.work_id == last.work_id)
        .expect("last item");
    item.prerequisites.push(item.prerequisites[0]);
    rebind_snapshot_body(&mut duplicate_carried);
    refuse(
        "carried-duplicate-prerequisite",
        &duplicate_carried,
        "work item duplicates a prerequisite",
    );

    let mut dangling_first = duplicate_native.clone();
    let mut dangling = dangling_first.body.records[0].clone();
    dangling.work_id = WorkId::new();
    dangling_first.body.records.insert(0, dangling);
    dangling_first.body.summary.section_counts.records += 1;
    rebind_snapshot_body(&mut dangling_first);
    refuse(
        "dangling-record-before-duplicate-prerequisite",
        &dangling_first,
        "duplicate or dangling restored record",
    );

    let mut missing_history = native.clone();
    missing_history
        .body
        .records
        .retain(|record| record.work_id != last.work_id);
    missing_history.body.summary.section_counts.records -= 1;
    rebind_snapshot_body(&mut missing_history);
    refuse(
        "item-without-records",
        &missing_history,
        "work item has no restored history record",
    );
}
