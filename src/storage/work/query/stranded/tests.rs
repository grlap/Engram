use super::*;
use crate::storage::work::test_support::*;
use crate::{ChildRequirement, DecomposeWorkRequest, WorkPlanningAuthority};
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn completed(store: &mut SqliteStore, key: &str, open_child: bool) {
    let request = root_request("stranded-cost", key, 0);
    let parent = store
        .create_work(&request, &DevelopmentNoopRedactor)
        .unwrap();
    if open_child {
        store
            .decompose_work(
                &DecomposeWorkRequest {
                    parent_id: parent.work_id,
                    expected_parent_revision: parent.revision,
                    children: vec![child(
                        &format!("{key}-child"),
                        ChildRequirement::Optional,
                        "Follow-up",
                    )],
                    prerequisites: vec![],
                    authority: WorkPlanningAuthority::Project,
                    actor: actor("planner"),
                    idempotency_key: format!("{key}-plan"),
                    created_at: at(1),
                },
                &DevelopmentNoopRedactor,
            )
            .unwrap();
    }
    let parent = store.get_work_item(parent.work_id).unwrap();
    let held = claim(store, &parent, "planner", &format!("{key}-claim"), 2, 3600);
    let parent = store.get_work_item(parent.work_id).unwrap();
    let proof = evidence(store, &parent, &held, "planner", &format!("{key}-proof"), 3);
    checkpoint(
        store,
        &parent,
        &held,
        "planner",
        &format!("{key}-checkpoint"),
        4,
        std::slice::from_ref(&proof),
    );
    complete(
        store,
        &parent,
        &held,
        "planner",
        &proof,
        &format!("{key}-done"),
        5,
    )
    .unwrap();
}

fn measure(store: &SqliteStore, session: &str) -> (usize, usize) {
    // Count every VM instruction in production discovery, including canonical
    // item/anchor loads and snapshot statements, not just the candidate SQL.
    let steps = Arc::new(AtomicUsize::new(0));
    let counter = steps.clone();
    store
        .connection
        .progress_handler(
            1,
            Some(move || {
                counter.fetch_add(1, Ordering::Relaxed);
                false
            }),
        )
        .unwrap();
    let page = store
        .stranded_work_children(
            &ProjectId("stranded-cost".into()),
            &SessionId(session.into()),
        )
        .unwrap();
    store
        .connection
        .progress_handler(0, None::<fn() -> bool>)
        .unwrap();
    (
        page.items.len() + page.omitted,
        steps.load(Ordering::Relaxed),
    )
}

#[test]
fn stranded_discovery_vm_work_does_not_grow_with_unrelated_completed_history() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    completed(&mut store, "candidate", true);
    completed(&mut store, "seed-closed", false);
    let before = [measure(&store, "planner"), measure(&store, "stranger")];
    for index in 0..32 {
        completed(&mut store, &format!("unrelated-{index}"), false);
    }
    for (baseline, after) in before
        .into_iter()
        .zip([measure(&store, "planner"), measure(&store, "stranger")])
    {
        assert_eq!(after.0, baseline.0);
        assert!(
            after.1 <= baseline.1,
            "unrelated Completed history increased VM steps: {baseline:?} -> {after:?}"
        );
    }
}

#[test]
fn stranded_participation_stops_before_later_families_after_a_valid_event() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    completed(&mut store, "candidate", true);
    let before = measure(&store, "planner");
    // Breaking a later family's query is a discriminator: an eager UNION or
    // continued probe would fail, whereas a valid native anchor already suffices.
    store
        .connection
        .execute_batch("ALTER TABLE work_restored_records RENAME TO hidden_history")
        .unwrap();
    assert_eq!(measure(&store, "planner"), before);
    assert!(
        store
            .stranded_work_children(
                &ProjectId("stranded-cost".into()),
                &SessionId("stranger".into())
            )
            .is_err()
    );
}
