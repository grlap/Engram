//! Graph attribution a public folder application no longer reaches: an
//! addition whose root in a valid graph differs from the folder's.

use super::*;

fn task(title: &str, parent: Option<WorkId>) -> TaskDocument {
    TaskDocument {
        id: WorkId(uuid::Uuid::new_v4()),
        external_ref: None,
        title: title.into(),
        outcome: format!("Deliver {title}"),
        acceptance: vec!["delivered".into()],
        acceptance_bindings: Vec::new(),
        evaluation_mode: None,
        kind: crate::WorkItemKind::Task,
        priority: 1,
        labels: Vec::new(),
        assigned_to: None,
        deferred_until: None,
        lifecycle: WorkLifecycle::Open,
        parent_id: parent,
        child_requirement: crate::ChildRequirement::Required,
        prerequisites: Vec::new(),
        blockers: Vec::new(),
        superseded_by: None,
        disposal_reason: None,
        completion: None,
        notes: Vec::new(),
        activity: std::collections::BTreeMap::default(),
    }
}

fn tree(mut tasks: Vec<TaskDocument>) -> TaskTree {
    tasks.sort_by_key(|task| task.id.0);
    TaskTree {
        project: ProjectDocument {
            format: FORMAT.into(),
            project_id: ProjectId("folder-admission-unit".into()),
            notes_included: false,
        },
        tasks,
    }
}

#[test]
fn a_differing_root_is_named_without_claiming_an_invalid_graph() {
    // The destination holds a task under the first root; the folder holds it
    // under the second and adds a task beneath it. With the addition, the
    // destination's planning is a valid task graph, but there the
    // addition's root is the first root, not the folder's second.
    let first = task("first root", None);
    let second = task("second root", None);
    let held = task("held", Some(first.id));
    let mut moved = held.clone();
    moved.parent_id = Some(second.id);
    let added = task("added", Some(held.id));
    let added_id = added.id;
    let current = tree(vec![first.clone(), second.clone(), held]);
    let resolved = tree(vec![first, second, moved, added]);
    let prospective = Prospective::new(&current, &resolved).unwrap();
    let base = current.tasks.iter().collect::<Vec<_>>();
    let findings = check_graph(
        &prospective,
        &resolved.project.project_id,
        false,
        &base,
        &[added_id],
        "retained_graph",
        "the destination's planning",
        || {},
    )
    .unwrap();
    assert!(findings.batch.is_empty(), "{:?}", findings.batch);
    assert_eq!(findings.conflicts.len(), 1);
    let named = &findings.conflicts[0];
    assert_eq!(named.id, added_id);
    assert_eq!(named.reason, "retained_graph");
    assert_eq!(
        named.detail,
        "the destination's planning with this addition is a valid task graph, but this addition's computed root there differs from the folder's"
    );
}

#[test]
fn connected_admission_applies_and_rolls_back_without_scanning_disconnected_tasks() {
    use crate::storage::SqliteStore;
    use crate::storage::work::{
        integrity,
        task_sync::closure::native,
        test_support::{actor, at},
    };
    use crate::task_sync::{folder_plan::plan_selected, progress::ExportProfile};
    use std::collections::{BTreeMap, BTreeSet};

    let root = task("selected root", None);
    let peer = task("selected prerequisite", None);
    let removed = task("selected removal", None);
    let disconnected = task("unselected root", None);
    let mut tasks = vec![
        root.clone(),
        peer.clone(),
        removed.clone(),
        disconnected.clone(),
    ];
    tasks.extend((0..2_000).map(|index| task(&format!("disconnected {index}"), None)));
    let inventory = tree(tasks);
    let mut store = SqliteStore::open_in_memory().unwrap();
    store
        .import_task_tree(
            &inventory.project.project_id,
            &actor("importer"),
            &inventory,
            false,
            at(5),
        )
        .unwrap();
    let before = crate::storage::work::task_sync::tests::state(&store);
    let unselected = store.get_work_item(disconnected.id).unwrap();

    let current = tree(vec![root.clone(), peer.clone(), removed.clone()]);
    let mut edited = root.clone();
    edited.title = "renamed root".into();
    edited.prerequisites = vec![peer.id];
    // A new child must not sit beneath the task whose relations change.
    let added = task("new selected child", Some(peer.id));
    let resolved = tree(vec![edited.clone(), peer.clone(), added.clone()]);
    let current_documents = current
        .tasks
        .iter()
        .map(|task| (task.id, task.clone()))
        .collect::<BTreeMap<_, _>>();
    let changes = BTreeMap::from([
        (root.id, Some(edited)),
        (added.id, Some(added.clone())),
        (removed.id, None),
    ]);
    let membership = current_documents.keys().copied().collect::<BTreeSet<_>>();
    let plan = plan_selected(&resolved.project, &current_documents, &changes, &membership).unwrap();
    let prospective = Prospective::from_documents(
        &resolved.project,
        current.tasks.iter().map(|task| (task.id, task)).collect(),
        resolved.tasks.iter().map(|task| (task.id, task)).collect(),
        current
            .snapshots()
            .unwrap()
            .into_iter()
            .map(|item| (item.work_id, item.root_id))
            .collect(),
        resolved
            .snapshots()
            .unwrap()
            .into_iter()
            .map(|item| (item.work_id, item))
            .collect(),
    );
    let origins = OriginGraph {
        documents: current.tasks.clone(),
        notes_included: false,
    };
    let seeds = BTreeSet::from([root.id, peer.id, added.id, removed.id]);
    let profile = ExportProfile {
        include_notes: false,
        origin_machine: None,
    };

    for commit in [false, true] {
        integrity::take_graph_scan_metrics();
        native::NODE_LOOKUPS.with(|count| count.set(0));
        let transaction = store.begin_work_mutation().unwrap();
        apply_on(
            &transaction,
            &plan,
            &prospective,
            Some(&origins),
            || {
                Ok(current_documents
                    .iter()
                    .map(|(&id, task)| (id, task.clone()))
                    .collect())
            },
            || {
                let captured = native::capture_on(
                    &transaction,
                    &resolved.project.project_id,
                    &profile,
                    seeds.clone(),
                )?;
                assert!(
                    dependency_cycles(&captured.documents.into_values().collect::<Vec<_>>())
                        .is_empty()
                );
                Ok(())
            },
            &actor("importer"),
            at(6),
        )
        .unwrap();
        assert_eq!(native::NODE_LOOKUPS.with(std::cell::Cell::get), 4);
        assert_eq!(integrity::take_graph_scan_metrics(), (0, 0));
        if commit {
            transaction.commit().unwrap();
        } else {
            transaction.rollback().unwrap();
            assert_eq!(
                crate::storage::work::task_sync::tests::state(&store),
                before
            );
        }
    }
    assert_eq!(store.get_work_item(root.id).unwrap().title, "renamed root");
    assert_eq!(
        store.get_work_item(added.id).unwrap().parent_id,
        Some(peer.id)
    );
    assert_eq!(
        store.get_work_item(removed.id).unwrap().lifecycle,
        WorkLifecycle::Cancelled
    );
    let after = store.get_work_item(disconnected.id).unwrap();
    assert_eq!(
        (after.title, after.revision),
        (unselected.title, unselected.revision)
    );
}
