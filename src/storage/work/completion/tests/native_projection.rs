use super::*;

// Seed valid canonical representations with omitted serde defaults, not a
// captured store or a pinned object digest. Only this synthetic fixture is edited.
fn omit_native_restore_defaults(store: &SqliteStore, work: WorkId) {
    let (old_seal, seal_bytes): (String, Vec<u8>) = store
        .connection
        .query_row(
            "SELECT seal_hash, seal_json FROM work_completion_seals WHERE work_id = ?1",
            [work.0.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("native seal");
    let mut seal_json: serde_json::Value = serde_json::from_slice(&seal_bytes).expect("seal JSON");
    let fields = seal_json.as_object_mut().expect("seal object");
    assert_eq!(fields.remove("restored"), Some(serde_json::json!(false)));
    assert_eq!(
        fields.remove("restored_child_completions"),
        Some(serde_json::json!([]))
    );
    assert_eq!(
        serde_json::from_value::<CompletionSeal>(seal_json.clone()).expect("defaulted seal"),
        serde_json::from_slice::<CompletionSeal>(&seal_bytes).expect("original seal"),
    );
    let seal = CanonicalObject::freeze(&seal_json).expect("canonical omitted-default seal");
    SqliteStore::insert_object(&store.connection, "completion_seal", &seal).expect("seed seal");

    let (old_event, event_bytes): (String, Vec<u8>) = store.connection.query_row(
        "SELECT item.latest_event_hash, object.canonical_json FROM work_items item
         JOIN objects object ON object.object_hash = item.latest_event_hash WHERE item.work_id = ?1",
        [work.0.to_string()], |row| Ok((row.get(0)?, row.get(1)?)),
    ).expect("latest native event");
    let mut event_json: serde_json::Value =
        serde_json::from_slice(&event_bytes).expect("event JSON");
    assert_eq!(
        event_json["work"]
            .as_object_mut()
            .expect("item object")
            .remove("restored"),
        Some(serde_json::json!(false))
    );
    event_json["transition"]["seal"] = serde_json::json!(seal.hash());
    event_json["run"]["completion_seal"] = serde_json::json!(seal.hash());
    let event = CanonicalObject::freeze(&event_json).expect("canonical omitted-default event");
    SqliteStore::insert_object(&store.connection, "work_event", &event).expect("seed event");
    store
        .connection
        .execute(
            "UPDATE work_completion_seals SET seal_hash = ?1, seal_json = ?2 WHERE work_id = ?3",
            params![seal.hash().as_str(), seal.bytes(), work.0.to_string()],
        )
        .expect("bind seal projection");
    store
        .connection
        .execute(
            "UPDATE work_runs SET completion_seal_hash = ?1, run_json = ?2 WHERE work_id = ?3",
            params![
                seal.hash().as_str(),
                serde_json::to_vec(&event_json["run"]).expect("run bytes"),
                work.0.to_string()
            ],
        )
        .expect("bind completed run");
    store
        .connection
        .execute(
            "UPDATE work_items SET latest_event_hash = ?1, item_json = ?2 WHERE work_id = ?3",
            params![
                event.hash().as_str(),
                serde_json::to_vec(&event_json["work"]).expect("item bytes"),
                work.0.to_string()
            ],
        )
        .expect("bind item projection");
    for (old, new) in [(&old_seal, seal.hash()), (&old_event, event.hash())] {
        store
            .connection
            .execute(
                "UPDATE work_feed_entries SET object_hash = ?1 WHERE object_hash = ?2",
                params![new.as_str(), old],
            )
            .expect("bind canonical feed entry");
        store
            .connection
            .execute("DELETE FROM objects WHERE object_hash = ?1", [old])
            .expect("discard replaced synthetic object");
    }
}

fn native_history(store: &mut SqliteStore) -> WorkId {
    let project = "native-projection-repair";
    let done = store
        .create_work(
            &root_request(project, "completed", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("native root");
    let dependent = store
        .create_work(
            &root_request(project, "dependent", 1),
            &DevelopmentNoopRedactor,
        )
        .expect("native dependent");
    let dependent = store
        .add_work_prerequisite(
            &ChangeWorkPrerequisiteRequest {
                work_id: dependent.work_id,
                prerequisite_id: done.work_id,
                expected_revision: dependent.revision,
                authority: delegated(project, "planner"),
                actor: actor("planner"),
                idempotency_key: "dependency".into(),
                changed_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("native prerequisite");
    store
        .add_work_blocker(
            &AddWorkBlockerRequest {
                work_id: dependent.work_id,
                expected_work_revision: dependent.revision,
                kind: crate::domain::WorkBlockerKind::Manual,
                detail: "waiting for review".into(),
                authority: delegated(project, "planner"),
                actor: actor("planner"),
                idempotency_key: "blocker".into(),
                blocked_at: at(3),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("native blocker");
    let disposed = store
        .create_work(
            &root_request(project, "disposed", 4),
            &DevelopmentNoopRedactor,
        )
        .expect("native disposal root");
    store
        .dispose_work(
            &DisposeWorkRequest {
                work_id: disposed.work_id,
                expected_work_revision: disposed.revision,
                disposition: WorkDisposition::Cancelled,
                replacement_id: None,
                reason: "no longer required".into(),
                actor: actor("planner"),
                idempotency_key: "cancel".into(),
                disposed_at: at(5),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("native cancelled history");
    let held = claim(store, &done, "executor", "claim", 6, 300);
    let proof = evidence(store, &done, &held, "executor", "evidence", 7);
    checkpoint(
        store,
        &done,
        &held,
        "executor",
        "checkpoint",
        8,
        std::slice::from_ref(&proof),
    );
    complete(store, &done, &held, "executor", &proof, "complete", 9).expect("native seal");
    done.work_id
}

#[test]
fn native_only_repair_preserves_canonical_defaults_and_detects_projection_drift() {
    let directory = tempfile::tempdir().expect("temporary native store");
    let database = directory.path().join("engram.db");
    let mut store = SqliteStore::open(&database).expect("native store");
    let completed = native_history(&mut store);
    omit_native_restore_defaults(&store, completed);
    assert_eq!(
        store
            .connection
            .query_row("SELECT COUNT(*) FROM work_restored_records", [], |row| row
                .get::<_, i64>(
                0
            ))
            .expect("restored count"),
        0
    );
    let report = store.verify_all().expect("verify omitted defaults");
    assert!(report.is_healthy(), "{report:?}");
    let canonical_before = store
        .connection
        .prepare("SELECT object_hash, canonical_json FROM objects ORDER BY object_hash")
        .expect("canonical inventory")
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .expect("canonical rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("canonical inventory rows");
    store
        .connection
        .execute_batch("DROP INDEX objects_graph_snapshot_audit")
        .expect("remove rebuildable index");
    drop(store);
    assert!(
        SqliteStore::open(&database).is_err(),
        "ordinary open refuses missing index"
    );
    let repaired =
        SqliteStore::repair_rebuildable_projections(&database).expect("native-only repair");
    assert!(repaired.is_healthy(), "{repaired:?}");
    let store = SqliteStore::open(&database).expect("ordinary open after repair");
    let canonical_after = store
        .connection
        .prepare("SELECT object_hash, canonical_json FROM objects ORDER BY object_hash")
        .expect("canonical inventory")
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .expect("canonical rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("canonical inventory rows");
    assert_eq!(
        canonical_before, canonical_after,
        "repair must never rewrite canonical bytes"
    );
    for field in ["restored", "title"] {
        store
            .connection
            .execute_batch("SAVEPOINT corrupt")
            .expect("corruption savepoint");
        store.connection.execute(
            "UPDATE work_items SET item_json = CAST(json_set(item_json, ?1, json(?2)) AS BLOB) WHERE work_id = ?3",
            params![format!("$.{field}"), if field == "restored" { "true" } else { "\"corrupt\"" }, completed.0.to_string()],
        ).expect("drift item projection");
        let report = store.verify_all().expect("detect item drift");
        assert!(
            report
                .invalid_work_records
                .contains(&format!("work_item:{}", completed.0)),
            "{report:?}"
        );
        restore_savepoint(&store);
    }
    store.connection.execute(
        "UPDATE work_completion_seals SET seal_json = CAST(json_set(seal_json, '$.restored', json('true')) AS BLOB) WHERE work_id = ?1",
        [completed.0.to_string()],
    ).expect("drift seal projection");
    let report = store.verify_all().expect("detect seal drift");
    assert!(
        report
            .invalid_work_records
            .iter()
            .any(|label| label.starts_with("completion_seal:")
                && label.ends_with(":projection_binding")),
        "{report:?}"
    );
}

#[test]
fn repair_refusal_names_invalid_labels_and_rolls_back_rebuildable_changes() {
    let directory = tempfile::tempdir().expect("temporary store");
    let database = directory.path().join("engram.db");
    let mut store = SqliteStore::open(&database).expect("store");
    let item = store
        .create_work(
            &root_request("repair-labels", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    store.connection.execute_batch(
        "DROP INDEX objects_graph_snapshot_audit;
         UPDATE work_items SET item_json = CAST(json_set(item_json, '$.restored', json('true')) AS BLOB);",
    ).expect("missing index and invalid durable projection");
    drop(store);
    let error =
        SqliteStore::repair_rebuildable_projections(&database).expect_err("refuse invalid state");
    assert!(
        error
            .to_string()
            .contains(&format!("work_item:{}", item.work_id.0)),
        "{error}"
    );
    let connection = Connection::open(&database).expect("inspect refused repair");
    assert_eq!(
        connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE name = 'objects_graph_snapshot_audit'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .expect("index count"),
        0,
        "refused repair rolls back DDL"
    );
}
