use super::*;
use crate::storage::work::{
    task_sync::{
        baseline::TaskSyncImportPlanning,
        closure::{documents, source::delta::MembershipDelta},
        folder::IMPORT_PLANNING_KIND,
        tests::{candidate, state},
    },
    test_support::{actor, at},
};
use crate::task_sync::{
    SharedActor,
    delta::OutputMembership,
    inventory::InventoryRoot,
    notes::{SharedNote, SharedNoteKind},
    progress::ExportProfile,
};
use crate::{CanonicalObject, ObjectId, WorkLifecycle, storage::SqliteStore};
use std::collections::BTreeMap;

fn note() -> SharedNote {
    SharedNote {
        id: ObjectId::mint(),
        kind: SharedNoteKind::Status,
        body: "Shared status".into(),
        author: SharedActor::from_actor(&actor("shared")),
        recorded_at: at(1),
        origin_machine: "peer".into(),
    }
}

// These role rows model an identity lookup only. They are not a complete source
// graph and grant no admission. Original rows come from the real import owner.
fn planning_member(store: &SqliteStore, task: &TaskDocument) -> ObjectId {
    let object = CanonicalObject::mint(&TaskSyncImportPlanning {
        project_id: crate::ProjectId("task-sync".into()),
        previous: None,
        changes: BTreeMap::from([(task.id, Some(task.clone()))]),
        inventory_root: InventoryRoot::ZERO.including(task).unwrap().hex(),
        export_basis: "fixture member".into(),
        profile: ExportProfile {
            include_notes: true,
            origin_machine: Some("peer".into()),
        },
        created_at: at(2),
    })
    .unwrap();
    store.connection.execute("INSERT INTO objects(object_id,object_kind,canonical_json,created_at) VALUES(?1,?2,?3,'fixture')",
        params![object.key().as_str(), IMPORT_PLANNING_KIND, object.bytes()]).unwrap();
    documents::record_on(&store.connection, IMPORT_PLANNING_KIND, &object).unwrap();
    object.key().clone()
}

fn source_node(store: &SqliteStore, scope: &str, owner: &ObjectId, task: &TaskDocument) {
    let mut node = super::super::node("source", "task-sync", owner, task, None, true);
    node.scope = scope.into();
    super::super::write_node(
        &store.connection,
        &node,
        &super::super::edges("source", scope, task),
        &rows(&node, task),
    )
    .unwrap();
}

#[test]
fn graph_identity_queries_separate_original_consumption_current_members_and_historical_copies() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let mut tree = candidate();
    tree.project.notes_included = true;
    let original_note = note();
    let root = tree
        .tasks
        .iter_mut()
        .find(|task| task.lifecycle == WorkLifecycle::Open && task.parent_id.is_none())
        .unwrap();
    root.notes.push(original_note.clone());
    let root_id = root.id;
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
    assert!(original_held(&store.connection, "note", "", original_note.id.as_str()).unwrap());
    assert_eq!(state(&store), before);

    let copied_note = note();
    let mut source = tree
        .tasks
        .iter()
        .find(|task| task.id == root_id)
        .unwrap()
        .clone();
    source.notes = vec![copied_note.clone()];
    let owner = planning_member(&store, &source);
    let scope = ObjectId::mint().to_string();
    source_node(&store, &scope, &owner, &source);
    assert!(!original_held(&store.connection, "note", "", copied_note.id.as_str()).unwrap());
    assert_eq!(
        current_owners(
            &store.connection,
            "source",
            &scope,
            "note",
            "",
            copied_note.id.as_str()
        )
        .unwrap(),
        BTreeSet::from([root_id])
    );

    let mut added = source.clone();
    added.id = WorkId::new();
    added.parent_id = None;
    added.prerequisites.clear();
    let membership = OutputMembership {
        inventory_root: InventoryRoot::ZERO.including(&source).unwrap(),
        profile: ExportProfile {
            include_notes: true,
            origin_machine: Some("peer".into()),
        },
    };
    let mut delta = MembershipDelta {
        role: "source",
        before_head: ObjectId::mint(),
        before: membership.clone(),
        output: membership,
        changes: BTreeMap::from([(added.id, Some((owner.clone(), added)))]),
    };
    let before = state(&store);
    assert!(
        validate_replacements(&store.connection, &scope, &delta)
            .unwrap_err()
            .to_string()
            .contains("retained source/export member")
    );
    assert_eq!(state(&store), before);
    delta.changes.insert(root_id, None);
    validate_replacements(&store.connection, &scope, &delta).unwrap();
    delta.changes.remove(&root_id);
    store.connection.execute("DELETE FROM work_task_sync_graph_nodes WHERE view_kind='source' AND scope_id=?1 AND work_id=?2",
        params![scope, root_id.0.to_string()]).unwrap();
    assert!(
        current_owners(
            &store.connection,
            "source",
            &scope,
            "note",
            "",
            copied_note.id.as_str()
        )
        .unwrap()
        .is_empty()
    );
    // The immutable document copy remains; it is no longer current membership.
    assert!(store.connection.query_row("SELECT EXISTS(SELECT 1 FROM work_task_sync_shared_identity_members WHERE value=?1)",
        [copied_note.id.as_str()], |row| row.get::<_, bool>(0)).unwrap());
    validate_replacements(&store.connection, &scope, &delta).unwrap();
    assert!(original_held(&store.connection, "note", "", original_note.id.as_str()).unwrap());
}
