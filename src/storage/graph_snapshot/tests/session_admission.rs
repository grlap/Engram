use super::*;

#[test]
fn save_work_graph_snapshot_refuses_an_ascii65_actor_session_before_effects() {
    refuse_save_before_effects(&ascii65_session());
}

#[test]
fn save_work_graph_snapshot_refuses_a_utf8_oversized_actor_session_before_effects() {
    refuse_save_before_effects(&utf8_oversized_session());
}

#[test]
fn load_work_graph_snapshot_refuses_an_ascii65_actor_session_before_effects() {
    refuse_load_before_effects(&ascii65_session());
}

#[test]
fn load_work_graph_snapshot_refuses_a_utf8_oversized_actor_session_before_effects() {
    refuse_load_before_effects(&utf8_oversized_session());
}

#[test]
fn save_work_graph_snapshot_preserves_an_exact_64_byte_actor_session() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let project = ProjectId("snapshot-admit-64".into());
    create_root(
        &mut store,
        &project,
        "Admission root",
        "snapshot-admit-64-root",
    );
    let session = exact_64_ascii_session();
    store
        .save_work_graph_snapshot(
            &project,
            &actor(&session),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(2),
            &DevelopmentNoopRedactor,
        )
        .expect("admitted save");
    let bytes: Vec<u8> = store
        .connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_kind = 'work_graph_snapshot_saved'",
            [],
            |row| row.get(0),
        )
        .expect("saved audit");
    let event: crate::WorkGraphSnapshotSavedEvent =
        serde_json::from_slice(&bytes).expect("decode save");
    assert_eq!(
        event.actor.session_id.as_ref().map(|id| id.0.as_str()),
        Some(session.as_str())
    );
}

#[test]
fn load_work_graph_snapshot_preserves_an_exact_64_byte_operator_session() {
    let mut source = SqliteStore::open_in_memory().expect("source");
    let project = ProjectId("snapshot-admit-64".into());
    let root = create_root(
        &mut source,
        &project,
        "Admission root",
        "snapshot-admit-64-load",
    );
    let saved = source
        .save_work_graph_snapshot(
            &project,
            &actor("saver"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(2),
            &DevelopmentNoopRedactor,
        )
        .expect("save");
    let session = exact_64_ascii_session();
    let mut destination = SqliteStore::open_in_memory().expect("destination");
    destination
        .load_work_graph_snapshot(
            &project,
            &actor(&session),
            &snapshot_bytes(&saved.document),
            false,
            at(3),
            &DevelopmentNoopRedactor,
        )
        .expect("admitted load");
    let bytes: Vec<u8> = destination
        .connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_kind = 'work_graph_snapshot_loaded'",
            [],
            |row| row.get(0),
        )
        .expect("loaded audit");
    let event: crate::WorkGraphSnapshotLoadedEvent =
        serde_json::from_slice(&bytes).expect("decode load");
    assert_eq!(
        event.actor.session_id.as_ref().map(|id| id.0.as_str()),
        Some(session.as_str())
    );
    let item = destination
        .get_work_item(root.work_id)
        .expect("restored item");
    assert_eq!(
        item.created_by.session_id.as_ref().map(|id| id.0.as_str()),
        Some(session.as_str())
    );
}
