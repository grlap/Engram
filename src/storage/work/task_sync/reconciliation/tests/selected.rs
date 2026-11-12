//! Reconciliation retains its root/prerequisite scope without a project scan.

use super::*;
use crate::{WorkId, WorkLifecycle};

#[test]
fn reconciliation_affected_capture_reads_only_connected_root_and_prerequisite_members() {
    let mut tree = candidate();
    let template = tree
        .tasks
        .iter()
        .find(|task| task.lifecycle == WorkLifecycle::Open && task.parent_id.is_none())
        .unwrap()
        .clone();
    let make = |title: &str| {
        let mut task = template.clone();
        task.id = WorkId::new();
        task.title = title.into();
        task.outcome = format!("Deliver {title}");
        task.external_ref = None;
        task.prerequisites.clear();
        task.acceptance_bindings.clear();
        task.notes.clear();
        task.activity.clear();
        task
    };
    let root = make("selected root");
    let mut child = make("selected child");
    child.parent_id = Some(root.id);
    let mut peer = make("reverse prerequisite root");
    peer.prerequisites = vec![child.id];
    let mut sibling = make("peer root member");
    sibling.parent_id = Some(peer.id);
    let mut retired = make("retained cancelled child");
    retired.parent_id = Some(root.id);
    retired.lifecycle = WorkLifecycle::Cancelled;
    retired.disposal_reason = Some("reported cancellation".into());
    let expected = BTreeSet::from([root.id, child.id, peer.id, sibling.id, retired.id]);
    tree.tasks = vec![root, child.clone(), peer, sibling, retired];
    tree.tasks
        .extend((0..2_000).map(|index| make(&format!("disconnected {index}"))));
    tree.tasks.sort_by_key(|task| task.id.0);
    let mut store = SqliteStore::open_in_memory().unwrap();
    store
        .import_task_tree(
            &tree.project.project_id,
            &actor("importer"),
            &tree,
            false,
            at(5),
        )
        .unwrap();
    let before = state(&store);
    AFFECTED_ITEM_READS.with(|count| count.set(0));
    let basis =
        capture_affected_on(&store.connection, &tree.project.project_id, &[child.id]).unwrap();
    assert_eq!(
        basis
            .iter()
            .map(|item| item.work_id)
            .collect::<BTreeSet<_>>(),
        expected
    );
    assert_eq!(
        AFFECTED_ITEM_READS.with(std::cell::Cell::get),
        expected.len()
    );
    assert!(basis.iter().all(|item| item.runs.is_empty()
        && item.active_run_id.is_none()
        && item.imported_head.is_some()));
    assert_eq!(state(&store), before);
    AFFECTED_ITEM_READS.with(|count| count.set(0));
    assert!(
        capture_affected_on(
            &store.connection,
            &tree.project.project_id,
            &[child.id, child.id]
        )
        .is_err()
    );
    assert_eq!(AFFECTED_ITEM_READS.with(std::cell::Cell::get), 0);
    assert_eq!(state(&store), before);
}
