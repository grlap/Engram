use super::*;
use crate::task_sync::{
    activity::{ActivityNotice, ActivityState},
    notes::{SharedNote, SharedNoteKind},
};

fn count(store: &SqliteStore, table: &str) -> i64 {
    store
        .connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}

#[test]
fn origin_members_keep_identity_provenance_and_explicit_repair_restores_damage() {
    let home = crate::test_support::temp_home().unwrap();
    let database = home.path().join("members.db");
    let mut store = SqliteStore::open(&database).unwrap();
    let mut tree = candidate();
    tree.project.notes_included = true;
    let task = &mut tree.tasks[0];
    let note_id = CanonicalObject::mint(&"reported note")
        .unwrap()
        .key()
        .clone();
    task.notes.push(SharedNote {
        id: note_id.clone(),
        kind: SharedNoteKind::Status,
        body: "Reported progress".into(),
        author: SharedActor::from_actor(&actor("shared-author")),
        recorded_at: at(1),
        origin_machine: "host-A".into(),
    });
    let activity_id = CanonicalObject::mint(&"reported activity")
        .unwrap()
        .key()
        .clone();
    let notice = ActivityNotice {
        activity_id: activity_id.clone(),
        task_id: task.id,
        origin_machine: "host-A".into(),
        actor: SharedActor::from_actor(&actor("shared-author")),
        state: ActivityState::Started,
        state_revision: 1,
        event_id: activity_id.clone(),
    };
    task.activity
        .entry("host-A".into())
        .or_default()
        .insert(activity_id.clone(), notice);
    import(&mut store, &tree);
    let original = objects(&store);
    assert_eq!(
        count(&store, "work_task_sync_document_members"),
        i64::try_from(tree.tasks.len()).unwrap()
    );
    let identities = store
        .connection
        .prepare(
            "SELECT family,machine,value,member_path
        FROM work_task_sync_shared_identity_members ORDER BY family",
        )
        .unwrap()
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
            ))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(
        identities,
        vec![
            (
                "activity".into(),
                "host-A".into(),
                activity_id.to_string(),
                format!("/task/activity/host-A/{activity_id}")
            ),
            (
                "note".into(),
                String::new(),
                note_id.to_string(),
                "/task/notes/0".into()
            ),
        ]
    );
    assert!(store.verify_all().unwrap().is_healthy());
    assert_eq!(count(&store, "work_task_sync_graph_identity_members"), 2);
    store
        .connection
        .execute("DELETE FROM work_task_sync_graph_identity_members", [])
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE work_task_sync_document_members SET document_json = X'7b'",
            [],
        )
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE work_task_sync_shared_identity_members SET member_path='/wrong'",
            [],
        )
        .unwrap();
    let damaged = store.verify_all().unwrap();
    assert!(
        damaged
            .invalid_work_records
            .iter()
            .any(|label| label.starts_with("work_task_sync_graph_identity_members:"))
    );
    assert!(
        damaged
            .invalid_work_records
            .iter()
            .any(|label| label.starts_with("work_task_sync_document_members:"))
    );
    assert!(
        damaged
            .invalid_work_records
            .iter()
            .any(|label| label.starts_with("work_task_sync_shared_identity_members:"))
    );
    drop(store);
    assert!(
        SqliteStore::repair_rebuildable_projections(&database)
            .unwrap()
            .is_healthy()
    );
    let store = SqliteStore::open(&database).unwrap();
    assert_eq!(objects(&store), original);
    assert_eq!(count(&store, "work_task_sync_shared_identity_members"), 2);
    assert_eq!(count(&store, "work_task_sync_graph_identity_members"), 2);
    let record: String = store
        .connection
        .query_row(
            "SELECT record_id FROM work_task_sync_shared_identity_members LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    // Derived witnesses must not supply a new durable deletion fence. The
    // existing imported head still protects its canonical origin; removing
    // that head deliberately lets this fixture construct corrupt history.
    store
        .connection
        .execute(
            "DELETE FROM work_task_sync_import_heads WHERE head_id=?1",
            [&record],
        )
        .unwrap();
    store
        .connection
        .execute("DELETE FROM objects WHERE object_id=?1", [&record])
        .unwrap();
    for table in [
        "work_task_sync_document_members",
        "work_task_sync_shared_identity_members",
    ] {
        let retained: i64 = store
            .connection
            .query_row(
                &format!("SELECT COUNT(*) FROM {table} WHERE record_id=?1"),
                [&record],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            retained, 0,
            "derived rows follow their deleted canonical owner"
        );
    }
    assert!(
        !store.verify_all().unwrap().is_healthy(),
        "canonical damage remains visible"
    );
}

#[test]
fn baseline_members_append_only_changed_documents_and_explicit_departures() {
    use super::feed_changes::{document, exchange, tree};
    let mut store = SqliteStore::open_in_memory().unwrap();
    let enrolled = tree((0..2000).map(document).collect());
    exchange(&mut store, &enrolled, "enroll-members", 5);
    let before = count(&store, "work_task_sync_document_members");
    let mut next = enrolled.clone();
    for task in next.tasks.iter_mut().take(5) {
        task.title.push_str(" edited");
    }
    let retired = next.tasks.pop().unwrap().id;
    next.tasks.push(document(2000));
    exchange(&mut store, &tree(next.tasks), "change-members", 6);
    // Six upserts plus one departure in the planning append, and one origin
    // for the addition. Unchanged members produce no historical copies.
    assert_eq!(count(&store, "work_task_sync_document_members") - before, 8);
    let tombstones: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM work_task_sync_document_members
        WHERE work_id=?1 AND document_json IS NULL",
            [retired.0.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(tombstones, 1);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn native_notes_claims_and_gates_do_not_project_shared_document_members() {
    use super::super::closure::native::{DOCUMENT_CAPTURES, INCOMING_EDGES, NODE_LOOKUPS};
    use crate::domain::{RecordWorkObservationRequest, SessionId};
    use crate::storage::work::task_sync::closure::documents::PROJECTOR_CALLS;
    use crate::storage::work::test_support::evidence;
    let mut capture_costs = std::collections::BTreeSet::new();
    for history in [0, 100, 2000] {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let request = root_request("native-member-cost", "native", 0);
        PROJECTOR_CALLS.with(|count| count.set(0));
        let work = store
            .create_work(&request, &crate::DevelopmentNoopRedactor)
            .unwrap();
        for index in 0..history {
            store
                .record_work_observation(
                    &RecordWorkObservationRequest {
                        status: false,
                        project_id: work.project_id.clone(),
                        work_id: work.work_id,
                        expected_work_revision: work.revision,
                        session_id: SessionId("peer".into()),
                        summary: format!("note {index}"),
                        refs: Vec::new(),
                        actor: crate::domain::non_holder_note_actor(actor("peer")),
                        idempotency_key: format!("note-{index}"),
                        recorded_at: at(1),
                    },
                    &crate::DevelopmentNoopRedactor,
                )
                .unwrap();
        }
        let live = claim(&mut store, &work, "worker", "claim", 2, 100);
        evidence(&mut store, &work, &live, "worker", "gate", 3);
        assert_eq!(PROJECTOR_CALLS.with(std::cell::Cell::get), 0);
        assert_eq!(count(&store, "work_task_sync_document_members"), 0);
        assert_eq!(count(&store, "work_task_sync_shared_identity_members"), 0);
        assert_eq!(count(&store, "work_task_sync_graph_nodes"), 0);
        assert_eq!(count(&store, "work_task_sync_graph_edges"), 0);
        assert_eq!(count(&store, "work_task_sync_graph_heads"), 0);
        let selected = store
            .create_work(
                &root_request("native-member-cost", "disconnected selection", 0),
                &crate::DevelopmentNoopRedactor,
            )
            .unwrap();
        DOCUMENT_CAPTURES.with(|count| count.set(0));
        NODE_LOOKUPS.with(|count| count.set(0));
        INCOMING_EDGES.with(|count| count.set(0));
        crate::canonical::reset_canonical_decode_count();
        let captured = store
            .capture_task_sync_native_component(&selected.project_id, [selected.work_id])
            .unwrap();
        assert_eq!(captured.documents.len(), 1);
        assert_eq!(DOCUMENT_CAPTURES.with(std::cell::Cell::get), 1);
        assert_eq!(NODE_LOOKUPS.with(std::cell::Cell::get), 1);
        assert_eq!(INCOMING_EDGES.with(std::cell::Cell::get), 0);
        capture_costs.insert(crate::canonical::canonical_decode_count());
    }
    assert_eq!(
        capture_costs.len(),
        1,
        "disconnected history does not add selected reads"
    );
}

#[test]
fn graph_views_keep_original_relations_separate_and_repair_their_provenance() {
    use super::feed_changes::{document, exchange, project, tree};
    let home = crate::test_support::temp_home().unwrap();
    let database = home.path().join("graph-views.db");
    let mut store = SqliteStore::open(&database).unwrap();
    let mut owner = document(0);
    let target = document(1);
    owner.prerequisites.push(target.id);
    let owner_id = owner.id;
    let target_id = target.id;
    let initial = tree(vec![owner, target, document(2)]);
    exchange(&mut store, &initial, "graph-enroll", 5);
    let mut changed = initial.clone();
    changed
        .tasks
        .iter_mut()
        .find(|task| task.id == owner_id)
        .unwrap()
        .prerequisites
        .clear();
    exchange(&mut store, &changed, "graph-relation-change", 6);
    let canonical = objects(&store);
    for (view, expected) in [("origin", 1), ("baseline", 0)] {
        let incoming: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM work_task_sync_graph_edges
                 INDEXED BY work_task_sync_graph_edges_target
             WHERE view_kind=?1 AND scope_id=?2 AND target_id=?3 AND relation='prerequisite'",
                params![view, project().0, target_id.0.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(incoming, expected, "{view}");
    }
    assert!(store.verify_all().unwrap().is_healthy());
    store
        .connection
        .execute(
            "DELETE FROM work_task_sync_graph_nodes WHERE view_kind='baseline' AND work_id=?1",
            [owner_id.0.to_string()],
        )
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE work_task_sync_graph_edges SET target_id=owner_id WHERE view_kind='origin'",
            [],
        )
        .unwrap();
    store
        .connection
        .execute("UPDATE work_task_sync_graph_heads SET notes_included=1", [])
        .unwrap();
    let damaged = store.verify_all().unwrap();
    for table in [
        "work_task_sync_graph_nodes",
        "work_task_sync_graph_edges",
        "work_task_sync_graph_heads",
    ] {
        assert!(
            damaged
                .invalid_work_records
                .iter()
                .any(|label| label.starts_with(table)),
            "{table}: {damaged:?}"
        );
    }
    drop(store);
    assert!(
        SqliteStore::repair_rebuildable_projections(&database)
            .unwrap()
            .is_healthy()
    );
    let store = SqliteStore::open(&database).unwrap();
    assert_eq!(objects(&store), canonical);
    assert_eq!(count(&store, "work_task_sync_graph_nodes"), 6);
    assert_eq!(count(&store, "work_task_sync_graph_edges"), 1);
}
