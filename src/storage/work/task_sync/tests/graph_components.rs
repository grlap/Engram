use super::super::closure::component::{EDGE_READS, MEMBER_DECODES, NODE_LOOKUPS};
use super::*;
use crate::task_sync::TaskSyncGraphView;

fn reset() {
    MEMBER_DECODES.with(|count| count.set(0));
    NODE_LOOKUPS.with(|count| count.set(0));
    EDGE_READS.with(|count| count.set(0));
    origin::ENUMERATIONS.with(|count| count.set(0));
    crate::canonical::reset_canonical_decode_count();
}

#[test]
fn profiled_native_components_match_export_inside_the_writer_and_skip_disconnected_damage() {
    use super::super::closure::native::{DOCUMENT_CAPTURES, capture_on};
    use super::feed_changes::{document, tree};
    use crate::task_sync::{
        notes::{SharedNote, SharedNoteKind},
        progress::ExportProfile,
    };
    let mut store = SqliteStore::open_in_memory().unwrap();
    let mut selected = document(0);
    let disconnected = document(1);
    selected.notes.push(SharedNote {
        id: ObjectId::mint(),
        kind: SharedNoteKind::Status,
        body: "Shared planning status".into(),
        author: SharedActor::from_actor(&actor("note-author")),
        recorded_at: at(1),
        origin_machine: "source-host".into(),
    });
    let mut source = tree(vec![selected.clone(), disconnected.clone()]);
    source.project.notes_included = true;
    import(&mut store, &source);
    let project = source.project.project_id.clone();
    let profile = ExportProfile {
        include_notes: true,
        origin_machine: Some("writer-host".into()),
    };
    let before = state(&store);
    DOCUMENT_CAPTURES.with(|count| count.set(0));
    let captured = store
        .capture_task_sync_native_component_with_profile(&project, &profile, [selected.id])
        .unwrap();
    assert_eq!(DOCUMENT_CAPTURES.with(std::cell::Cell::get), 1);
    assert!(captured.notes_included);
    assert_eq!(captured.documents.len(), 1);
    assert_ne!(
        captured.documents[&selected.id].notes,
        Vec::<SharedNote>::new()
    );
    let reference = store
        .export_task_tree_with_notes(&project, "writer-host")
        .unwrap();
    assert_eq!(
        captured.documents[&selected.id],
        *reference
            .tree
            .tasks
            .iter()
            .find(|task| task.id == selected.id)
            .unwrap()
    );
    let transaction = store.connection.transaction().unwrap();
    let inside = origin::with_origin_scope(&transaction, || {
        capture_on(
            &transaction,
            &project,
            &profile,
            std::collections::BTreeSet::from([selected.id]),
        )
    })
    .unwrap();
    assert_eq!(inside.documents, captured.documents);
    transaction.rollback().unwrap();
    assert_eq!(state(&store), before);
    store
        .connection
        .execute(
            "UPDATE work_items SET item_json=X'7b' WHERE work_id=?1",
            [disconnected.id.0.to_string()],
        )
        .unwrap();
    DOCUMENT_CAPTURES.with(|count| count.set(0));
    assert_eq!(
        store
            .capture_task_sync_native_component_with_profile(&project, &profile, [selected.id])
            .unwrap()
            .documents,
        captured.documents
    );
    assert_eq!(DOCUMENT_CAPTURES.with(std::cell::Cell::get), 1);
    assert!(
        store
            .export_task_tree_with_notes(&project, "writer-host")
            .is_err()
    );
    DOCUMENT_CAPTURES.with(|count| count.set(0));
    assert!(
        store
            .capture_task_sync_native_component_with_profile(
                &project,
                &ExportProfile {
                    include_notes: true,
                    origin_machine: None
                },
                [selected.id]
            )
            .is_err()
    );
    assert_eq!(DOCUMENT_CAPTURES.with(std::cell::Cell::get), 0);
}

#[test]
fn graph_components_read_only_selected_independent_members_of_two_thousand() {
    use super::feed_changes::{document, exchange, project, tree};
    let mut store = SqliteStore::open_in_memory().unwrap();
    let original = tree((0..2000).map(document).collect());
    exchange(&mut store, &original, "component-enroll", 5);
    let mut changed = original.clone();
    let mut selected = std::collections::BTreeSet::new();
    for task in changed.tasks.iter_mut().take(5) {
        selected.insert(task.id);
        task.title.push_str(" edited");
    }
    let retired = changed.tasks.pop().unwrap().id;
    selected.insert(retired);
    let addition = document(2000);
    selected.insert(addition.id);
    changed.tasks.push(addition.clone());
    let changed = tree(changed.tasks);
    let patch = selected
        .iter()
        .map(|id| {
            (
                *id,
                changed.tasks.iter().find(|task| task.id == *id).cloned(),
            )
        })
        .collect::<std::collections::BTreeMap<_, _>>();
    reset();
    let unchanged = state(&store);
    let overlay = store
        .capture_task_sync_baseline_overlay(&project(), &patch)
        .unwrap();
    assert_eq!(NODE_LOOKUPS.with(std::cell::Cell::get), 7);
    assert_eq!(MEMBER_DECODES.with(std::cell::Cell::get), 6);
    assert_eq!(EDGE_READS.with(std::cell::Cell::get), 0);
    assert_eq!(crate::canonical::canonical_decode_count(), 0);
    assert_eq!(overlay.before.documents.len(), 6);
    assert_eq!(
        overlay.before.missing,
        std::collections::BTreeSet::from([addition.id])
    );
    assert_eq!(
        overlay.after,
        changed
            .tasks
            .iter()
            .filter(|task| selected.contains(&task.id))
            .cloned()
            .map(|task| (task.id, task))
            .collect()
    );
    assert_eq!(state(&store), unchanged);
    exchange(&mut store, &changed, "component-change", 6);
    reset();
    let baseline = store
        .capture_task_sync_graph_component(
            &project(),
            TaskSyncGraphView::Baseline,
            selected.clone(),
        )
        .unwrap();
    assert_eq!(NODE_LOOKUPS.with(std::cell::Cell::get), 7);
    assert_eq!(MEMBER_DECODES.with(std::cell::Cell::get), 6);
    assert_eq!(crate::canonical::canonical_decode_count(), 0);
    assert_eq!(origin::ENUMERATIONS.with(std::cell::Cell::get), 0);
    assert_eq!(
        baseline.documents,
        changed
            .tasks
            .iter()
            .filter(|task| selected.contains(&task.id))
            .cloned()
            .map(|task| (task.id, task))
            .collect()
    );
    assert_eq!(
        baseline.missing,
        std::collections::BTreeSet::from([retired])
    );
    reset();
    let origins = store
        .capture_task_sync_graph_component(&project(), TaskSyncGraphView::Origin, selected.clone())
        .unwrap();
    assert_eq!(NODE_LOOKUPS.with(std::cell::Cell::get), 7);
    assert_eq!(MEMBER_DECODES.with(std::cell::Cell::get), 7);
    let mut expected = original
        .tasks
        .iter()
        .filter(|task| selected.contains(&task.id))
        .cloned()
        .map(|task| (task.id, task))
        .collect::<std::collections::BTreeMap<_, _>>();
    expected.insert(addition.id, addition);
    assert_eq!(origins.documents, expected);
    assert!(origins.missing.is_empty());
    let durable = store
        .work_read_snapshot(|snapshot| {
            use super::super::closure::native::{DOCUMENT_CAPTURES, NODE_LOOKUPS};
            NODE_LOOKUPS.with(|count| count.set(0));
            DOCUMENT_CAPTURES.with(|count| count.set(0));
            origin::ENUMERATIONS.with(|count| count.set(0));
            // Read before the full reference can warm the origin cache.
            let captured =
                snapshot.capture_task_sync_native_component(&project(), selected.clone())?;
            assert_eq!(NODE_LOOKUPS.with(std::cell::Cell::get), 7);
            assert_eq!(DOCUMENT_CAPTURES.with(std::cell::Cell::get), 7);
            assert_eq!(origin::ENUMERATIONS.with(std::cell::Cell::get), 0);
            assert!(captured.missing.is_empty());
            assert!(captured.documents.contains_key(&retired));
            assert!(captured.members.is_empty());
            assert!(captured.baseline.is_none());
            assert_eq!(
                captured.documents,
                snapshot
                    .export_task_tree(&project())?
                    .tasks
                    .into_iter()
                    .filter(|task| selected.contains(&task.id))
                    .map(|task| (task.id, task))
                    .collect()
            );
            Ok(captured.documents)
        })
        .unwrap();
    let unselected = original
        .tasks
        .iter()
        .find(|task| !selected.contains(&task.id))
        .unwrap()
        .id;
    store
        .connection
        .execute(
            "UPDATE work_task_sync_document_members SET document_json=X'7b' WHERE work_id=?1",
            [unselected.0.to_string()],
        )
        .unwrap();
    reset();
    assert_eq!(
        store
            .capture_task_sync_graph_component(&project(), TaskSyncGraphView::Baseline, selected)
            .unwrap()
            .documents,
        baseline.documents
    );
    assert_eq!(MEMBER_DECODES.with(std::cell::Cell::get), 6);
    store
        .connection
        .execute(
            "UPDATE work_items SET item_json=X'7b' WHERE work_id=?1",
            [unselected.0.to_string()],
        )
        .unwrap();
    assert!(store.export_task_tree(&project()).is_err());
    assert_eq!(
        store
            .capture_task_sync_native_component(&project(), durable.keys().copied())
            .unwrap()
            .documents,
        durable
    );
    assert!(
        store
            .capture_task_sync_native_component(&project(), [unselected])
            .is_err()
    );
    assert!(
        store
            .capture_task_sync_graph_component(
                &project(),
                TaskSyncGraphView::Baseline,
                [unselected]
            )
            .is_err()
    );
}

#[test]
fn graph_components_follow_all_relations_both_ways_without_substituting_baseline_for_origin() {
    use super::feed_changes::{document, exchange, project, tree};
    let mut store = SqliteStore::open_in_memory().unwrap();
    let mut parent = document(0);
    let prerequisite = document(1);
    let mut optional = document(2);
    let mut replacement_owner = document(3);
    optional.parent_id = Some(parent.id);
    optional.child_requirement = ChildRequirement::Optional;
    parent.prerequisites.push(prerequisite.id);
    replacement_owner.lifecycle = WorkLifecycle::Superseded;
    replacement_owner.superseded_by = Some(prerequisite.id);
    replacement_owner.disposal_reason = Some("replaced".into());
    let parent_id = parent.id;
    let seed = prerequisite.id;
    let original = tree(vec![
        parent,
        prerequisite,
        optional,
        replacement_owner,
        document(4),
    ]);
    exchange(&mut store, &original, "relation-components", 5);
    {
        use super::super::closure::native::{DOCUMENT_CAPTURES, INCOMING_EDGES, NODE_LOOKUPS};
        NODE_LOOKUPS.with(|count| count.set(0));
        DOCUMENT_CAPTURES.with(|count| count.set(0));
        INCOMING_EDGES.with(|count| count.set(0));
        let captured = store
            .capture_task_sync_native_component(&project(), [seed, seed])
            .unwrap();
        assert_eq!(captured.documents.len(), 4);
        assert_eq!(NODE_LOOKUPS.with(std::cell::Cell::get), 4);
        assert_eq!(DOCUMENT_CAPTURES.with(std::cell::Cell::get), 4);
        assert_eq!(INCOMING_EDGES.with(std::cell::Cell::get), 3);
        assert_eq!(
            captured.documents,
            store
                .export_task_tree(&project())
                .unwrap()
                .tasks
                .into_iter()
                .filter(|task| task.id
                    != original
                        .tasks
                        .iter()
                        .find(|task| task.title == "task 4")
                        .unwrap()
                        .id)
                .map(|task| (task.id, task))
                .collect()
        );
        let component = tree(captured.documents.into_values().collect());
        component.validate_stored().unwrap();
        for snapshot in component.snapshots().unwrap() {
            assert_eq!(captured.expected_roots[&snapshot.work_id], snapshot.root_id);
        }
    }
    for view in [TaskSyncGraphView::Origin, TaskSyncGraphView::Baseline] {
        reset();
        let capture = store
            .capture_task_sync_graph_component(&project(), view, [seed, seed])
            .unwrap();
        assert_eq!(capture.documents.len(), 4);
        assert_eq!(NODE_LOOKUPS.with(std::cell::Cell::get), 4);
        assert_eq!(MEMBER_DECODES.with(std::cell::Cell::get), 4);
        assert_eq!(EDGE_READS.with(std::cell::Cell::get), 6);
        assert!(capture.missing.is_empty());
        let component = TaskTree {
            project: original.project.clone(),
            tasks: capture.documents.into_values().collect(),
        };
        component.validate_stored().unwrap();
        for snapshot in component.snapshots().unwrap() {
            if let Some(root) = capture.expected_roots.get(&snapshot.work_id) {
                assert_eq!(*root, snapshot.root_id);
            }
        }
    }
    let mut changed = original.clone();
    changed
        .tasks
        .iter_mut()
        .find(|task| task.id == parent_id)
        .unwrap()
        .prerequisites
        .clear();
    exchange(&mut store, &changed, "disconnect-components", 6);
    let before = state(&store);
    assert_eq!(
        store
            .capture_task_sync_graph_component(&project(), TaskSyncGraphView::Origin, [seed])
            .unwrap()
            .documents
            .len(),
        4
    );
    assert_eq!(
        store
            .capture_task_sync_graph_component(&project(), TaskSyncGraphView::Baseline, [seed])
            .unwrap()
            .documents
            .len(),
        2
    );
    assert_eq!(state(&store), before);
}

#[test]
fn graph_components_refuse_selected_missing_stale_and_invalid_projections_without_writes() {
    use super::feed_changes::{document, exchange, project, tree};
    for damage in ["node", "edge", "head", "member"] {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let mut owner = document(0);
        let target = document(1);
        owner.prerequisites.push(target.id);
        let id = owner.id;
        exchange(
            &mut store,
            &tree(vec![owner, target]),
            "damage-component",
            5,
        );
        match damage {
            "node" => {
                store.connection.execute("DELETE FROM work_task_sync_graph_nodes WHERE view_kind='baseline' AND work_id=?1",[id.0.to_string()]).unwrap();
            }
            "edge" => {
                store
                    .connection
                    .execute(
                        "DELETE FROM work_task_sync_graph_edges WHERE view_kind='baseline'",
                        [],
                    )
                    .unwrap();
            }
            "head" => {
                store
                    .connection
                    .execute("UPDATE work_task_sync_graph_heads SET notes_included=1", [])
                    .unwrap();
            }
            _ => {
                store.connection.execute("UPDATE work_task_sync_document_members SET document_json=X'7b' WHERE work_id=?1",[id.0.to_string()]).unwrap();
            }
        }
        let before = state(&store);
        let error = store
            .capture_task_sync_graph_component(&project(), TaskSyncGraphView::Baseline, [id])
            .unwrap_err();
        assert!(
            matches!(error, StoreError::InvalidWorkProjection(_)),
            "{damage}: {error}"
        );
        assert_eq!(state(&store), before, "{damage}");
    }
    let mut store = SqliteStore::open_in_memory().unwrap();
    let unknown = WorkId::new();
    let error = store
        .capture_task_sync_graph_component(&project(), TaskSyncGraphView::Baseline, [unknown])
        .unwrap_err();
    assert_eq!(
        refusal_code(&error),
        "task_sync_incremental_basis_unavailable"
    );
    assert_eq!(
        store
            .capture_task_sync_graph_component(&project(), TaskSyncGraphView::Origin, [unknown])
            .unwrap()
            .missing,
        std::collections::BTreeSet::from([unknown])
    );
    let foreign = store
        .create_work(
            &root_request("foreign-components", "foreign", 0),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let error = store
        .capture_task_sync_graph_component(&project(), TaskSyncGraphView::Origin, [foreign.work_id])
        .unwrap_err();
    assert_eq!(refusal_code(&error), "task_sync_origin_mismatch");
    let before = state(&store);
    assert_eq!(
        store
            .capture_task_sync_native_component(&project(), [unknown, unknown])
            .unwrap()
            .missing,
        std::collections::BTreeSet::from([unknown])
    );
    let error = store
        .capture_task_sync_native_component(&project(), [foreign.work_id])
        .unwrap_err();
    assert_eq!(refusal_code(&error), "task_sync_origin_mismatch");
    assert_eq!(state(&store), before);
}

#[test]
fn baseline_overlays_cover_old_incoming_edges_new_endpoints_cycles_and_noops() {
    use super::feed_changes::{document, exchange, project, tree};
    use std::collections::BTreeMap;

    let mut store = SqliteStore::open_in_memory().unwrap();
    let mut owner = document(0);
    let target = document(1);
    let spare = document(2);
    owner.prerequisites.push(target.id);
    let original = tree(vec![owner.clone(), target.clone(), spare.clone()]);
    exchange(&mut store, &original, "overlay-enroll", 5);
    let before = state(&store);

    // Deleting only the target must still read its unchanged incoming owner.
    reset();
    let error = store
        .capture_task_sync_baseline_overlay(&project(), &BTreeMap::from([(target.id, None)]))
        .unwrap_err();
    assert_eq!(NODE_LOOKUPS.with(std::cell::Cell::get), 2);
    assert_eq!(MEMBER_DECODES.with(std::cell::Cell::get), 2);
    assert_eq!(EDGE_READS.with(std::cell::Cell::get), 2);
    let full = tree(vec![owner.clone(), spare.clone()])
        .validate_stored()
        .unwrap_err();
    assert_eq!(error.to_string(), full.to_string());
    assert_eq!(refusal_code(&error), "task_sync_invalid_graph");

    let mut reverse = target.clone();
    reverse.prerequisites.push(owner.id);
    let error = store
        .capture_task_sync_baseline_overlay(
            &project(),
            &BTreeMap::from([(target.id, Some(reverse.clone()))]),
        )
        .unwrap_err();
    let full = tree(vec![owner.clone(), reverse.clone(), spare.clone()])
        .validate_stored()
        .unwrap_err();
    assert_eq!(error.to_string(), full.to_string());
    assert!(error.to_string().contains("cycle"));

    // A reversal in the same delta is valid. Checking either patch alone
    // would incorrectly refuse it or miss the old reverse relation.
    let mut disconnected = owner.clone();
    disconnected.prerequisites.clear();
    let overlay = store
        .capture_task_sync_baseline_overlay(
            &project(),
            &BTreeMap::from([
                (owner.id, Some(disconnected.clone())),
                (target.id, Some(reverse.clone())),
            ]),
        )
        .unwrap();
    assert_eq!(
        overlay.after,
        BTreeMap::from([(owner.id, disconnected), (target.id, reverse)])
    );

    // The new endpoint's old component must join the old owner's component.
    let mut moved = owner.clone();
    moved.prerequisites = vec![spare.id];
    reset();
    let overlay = store
        .capture_task_sync_baseline_overlay(
            &project(),
            &BTreeMap::from([(owner.id, Some(moved.clone()))]),
        )
        .unwrap();
    assert_eq!(NODE_LOOKUPS.with(std::cell::Cell::get), 3);
    assert_eq!(MEMBER_DECODES.with(std::cell::Cell::get), 3);
    assert_eq!(overlay.before.documents.len(), 3);
    assert_eq!(
        overlay.after,
        BTreeMap::from([(owner.id, moved), (target.id, target), (spare.id, spare)])
    );

    reset();
    let overlay = store
        .capture_task_sync_baseline_overlay(&project(), &BTreeMap::new())
        .unwrap();
    assert!(overlay.before.documents.is_empty());
    assert!(overlay.after.is_empty());
    assert_eq!(NODE_LOOKUPS.with(std::cell::Cell::get), 0);
    let overlay = store
        .capture_task_sync_baseline_overlay(
            &project(),
            &BTreeMap::from([(owner.id, Some(owner.clone()))]),
        )
        .unwrap();
    assert_eq!(overlay.after, overlay.before.documents);

    let error = store
        .capture_task_sync_baseline_overlay(&project(), &BTreeMap::from([(WorkId::new(), None)]))
        .unwrap_err();
    assert!(error.to_string().contains("not a member"));
    reset();
    let error = store
        .capture_task_sync_baseline_overlay(
            &project(),
            &BTreeMap::from([(WorkId::new(), Some(owner))]),
        )
        .unwrap_err();
    assert!(error.to_string().contains("task key differs"));
    assert_eq!(NODE_LOOKUPS.with(std::cell::Cell::get), 0);
    assert_eq!(state(&store), before);
}
