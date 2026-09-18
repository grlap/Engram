use super::*;
use crate::domain::{ChildWorkPrerequisite, ClaimNextReadyChildRequest, WorkDependencyRef};

fn request(
    parent: &WorkItem,
    holder: &str,
    key: &str,
    reason: Option<&str>,
    second: i64,
) -> ClaimNextReadyChildRequest {
    ClaimNextReadyChildRequest {
        parent_id: parent.work_id,
        holder: SessionId(holder.into()),
        ttl_seconds: 600,
        recovery_reason: reason.map(str::to_owned),
        actor: actor(holder),
        idempotency_key: key.into(),
        claimed_at: at(second),
    }
}

/// A root with four children: `Deferred` (priority 0, deferred), `Blocked`
/// (priority 1, waits on `Late`), `Early` (priority 1) and `Late` (priority 2).
/// The ready order is therefore `Early`, then `Late`.
fn planned_root(store: &mut SqliteStore, project: &str) -> (WorkItem, Vec<WorkItem>) {
    let root = store
        .create_work(&root_request(project, "root", 0), &DevelopmentNoopRedactor)
        .expect("root");
    let mut deferred = child("deferred", ChildRequirement::Required, "Deferred");
    deferred.priority = 0;
    deferred.deferred_until = Some(at(10_000));
    let mut blocked = child("blocked", ChildRequirement::Required, "Blocked");
    blocked.priority = 1;
    let mut early = child("early", ChildRequirement::Required, "Early");
    early.priority = 1;
    let mut late = child("late", ChildRequirement::Required, "Late");
    late.priority = 2;
    let decomposition = store
        .decompose_work(
            &DecomposeWorkRequest {
                parent_id: root.work_id,
                expected_parent_revision: root.revision,
                children: vec![deferred, blocked, early, late],
                prerequisites: vec![ChildWorkPrerequisite {
                    work_key: "blocked".into(),
                    prerequisite: WorkDependencyRef::Proposed("late".into()),
                }],
                authority: delegated(&root.project_id.0, "planner"),
                actor: actor("planner"),
                idempotency_key: "plan".into(),
                created_at: at(1),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("decompose");
    (decomposition.parent, decomposition.children)
}

fn titled<'a>(children: &'a [WorkItem], title: &str) -> &'a WorkItem {
    children
        .iter()
        .find(|child| child.title == title)
        .expect("planned child")
}

#[test]
fn the_next_ready_child_is_selected_in_ready_order_and_claimed_in_one_call() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let (root, children) = planned_root(&mut store, "project-next-ready");
    let early = titled(&children, "Early");
    let late = titled(&children, "Late");

    let first = store
        .claim_next_ready_child(
            &request(&root, "one", "one-first", None, 2),
            &DevelopmentNoopRedactor,
        )
        .expect("the first ready child is claimed");
    assert_eq!(first.parent_id, root.work_id);
    assert_eq!(first.work_id, early.work_id);
    assert_eq!(first.position, Some(1));
    assert_eq!(first.ready_count, 2);
    assert!(!first.renewed);
    assert_eq!(first.claim.holder, SessionId("one".into()));
    assert_eq!(
        store.current_work_claim(early.work_id).unwrap().as_ref(),
        Some(&first.claim)
    );

    // An exact keyed retry replays the same result.
    let replayed = store
        .claim_next_ready_child(
            &request(&root, "one", "one-first", None, 2),
            &DevelopmentNoopRedactor,
        )
        .expect("exact retry");
    assert_eq!(replayed, first);

    // A keyless repeat from the holder renews the child it already holds
    // instead of taking a second one.
    let renewed = store
        .claim_next_ready_child(
            &request(&root, "one", "", None, 3),
            &DevelopmentNoopRedactor,
        )
        .expect("renewal");
    assert_eq!(renewed.work_id, early.work_id);
    assert!(renewed.renewed);
    assert_eq!(renewed.position, None);
    assert_eq!(renewed.ready_count, 1);
    assert_eq!(renewed.claim.claim_id, first.claim.claim_id);
    assert_eq!(renewed.claim.fence, first.claim.fence);
    assert!(renewed.claim.expires_at >= first.claim.expires_at);

    // Another session is handed the next child in the order.
    let second = store
        .claim_next_ready_child(
            &request(&root, "two", "", None, 4),
            &DevelopmentNoopRedactor,
        )
        .expect("the second ready child is claimed");
    assert_eq!(second.work_id, late.work_id);
    assert_eq!(second.position, Some(1));
    assert_eq!(second.ready_count, 1);
    assert_eq!(second.claim.holder, SessionId("two".into()));

    // With nothing ready the call refuses with the reason and holds nothing.
    let refused = store
        .claim_next_ready_child(
            &request(&root, "three", "", None, 5),
            &DevelopmentNoopRedactor,
        )
        .expect_err("no ready child");
    let StoreError::InvalidWork(reason) = refused else {
        panic!("expected a plain refusal, got {refused:?}");
    };
    assert_eq!(
        reason,
        "no ready child to claim: 1 blocked, 2 claimed, 1 deferred"
    );
    for child in &children {
        let claim = store.current_work_claim(child.work_id).unwrap();
        assert!(
            claim.is_none_or(|claim| claim.holder.0 != "three"),
            "{} was claimed by the refused caller",
            child.title
        );
    }
}

#[test]
fn a_ready_child_lapsed_under_another_holder_is_passed_over_without_a_recovery_reason() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let (root, children) = planned_root(&mut store, "project-next-ready-lapsed");
    let early = titled(&children, "Early");
    let late = titled(&children, "Late");

    let old = store
        .claim_next_ready_child(
            &ClaimNextReadyChildRequest {
                ttl_seconds: 60,
                ..request(&root, "old", "", None, 2)
            },
            &DevelopmentNoopRedactor,
        )
        .expect("the old holder takes the first child");
    assert_eq!(old.work_id, early.work_id);

    // At second 100 the old claim has lapsed and Early is ready again, but a
    // caller without a recovery reason is handed Late, second in the order.
    let new = store
        .claim_next_ready_child(
            &request(&root, "new", "", None, 100),
            &DevelopmentNoopRedactor,
        )
        .expect("the lapsed child is passed over");
    assert_eq!(new.work_id, late.work_id);
    assert_eq!(new.position, Some(2));
    assert_eq!(new.ready_count, 2);

    // When every ready child is lapsed, the refusal says what a reason would do.
    let refused = store
        .claim_next_ready_child(
            &request(&root, "fourth", "", None, 100),
            &DevelopmentNoopRedactor,
        )
        .expect_err("only a lapsed child is ready");
    let StoreError::InvalidWork(reason) = refused else {
        panic!("expected a plain refusal, got {refused:?}");
    };
    assert_eq!(
        reason,
        "no ready child to claim: 1 blocked, 1 claimed, 1 deferred, 1 ready but lapsed under another holder; pass a recovery reason to take one over"
    );
    assert_eq!(
        store.current_work_claim(early.work_id).unwrap().as_ref(),
        Some(&old.claim)
    );

    // With a reason the lapsed child is taken over under its recovered claim.
    let taken = store
        .claim_next_ready_child(
            &request(&root, "third", "", Some("the old holder went silent"), 101),
            &DevelopmentNoopRedactor,
        )
        .expect("recovery with a reason");
    assert_eq!(taken.work_id, early.work_id);
    assert_eq!(taken.position, Some(1));
    assert_eq!(taken.ready_count, 1);
    assert_eq!(taken.claim.holder, SessionId("third".into()));
    assert_eq!(taken.claim.claim_id, old.claim.claim_id);
    assert!(taken.claim.fence > old.claim.fence);
}

#[test]
fn concurrent_callers_never_receive_the_same_child() {
    let directory = crate::test_support::temp_home().expect("temporary directory");
    let path = directory.path().join("engram.sqlite3");
    let (root, children) = {
        let mut store = SqliteStore::open(&path).expect("store");
        planned_root(&mut store, "project-next-ready-race")
    };
    let path = &path;
    let root = &root;
    let results = std::thread::scope(|scope| {
        ["left", "right"]
            .map(|holder| {
                scope.spawn(move || {
                    let mut store = SqliteStore::open(path).expect("store");
                    store.claim_next_ready_child(
                        &request(root, holder, "", None, 7),
                        &DevelopmentNoopRedactor,
                    )
                })
            })
            .map(|handle| handle.join().expect("caller thread"))
    });
    let claimed = results
        .iter()
        .map(|result| {
            let result = result.as_ref().expect("both callers are served");
            assert_eq!(
                result.position,
                Some(1),
                "each caller was first in its own view"
            );
            result.work_id
        })
        .collect::<Vec<_>>();
    assert_ne!(claimed[0], claimed[1]);
    let expected = [
        titled(&children, "Early").work_id,
        titled(&children, "Late").work_id,
    ];
    assert!(claimed.iter().all(|id| expected.contains(id)));
}
