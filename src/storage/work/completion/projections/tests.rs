use super::*;
use crate::storage::work::test_support::*;
use crate::storage::work::*;

use crate::storage::work::query::load_work_claim_optional;

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
fn native_projection_refresh_materializes_defaults_without_changing_canonical_history() {
    let mut store = SqliteStore::open_in_memory().expect("native store");
    let completed = native_history(&mut store);
    omit_native_restore_defaults(&store, completed);
    let before = canonical_inventory(&store);
    let item = store.get_work_item(completed).expect("typed native item");
    let seal_bytes: Vec<u8> = store
        .connection
        .query_row(
            "SELECT seal_json FROM work_completion_seals WHERE work_id = ?1",
            [completed.0.to_string()],
            |row| row.get(0),
        )
        .expect("native seal projection");
    let seal: CompletionSeal = serde_json::from_slice(&seal_bytes).expect("typed native seal");
    let transaction = store.connection.transaction().expect("projection refresh");
    persist_work_item(&transaction, &item).expect("current writer refreshes typed item");
    transaction
        .execute(
            "UPDATE work_completion_seals SET seal_json = ?1 WHERE work_id = ?2",
            params![
                serde_json::to_vec(&seal).expect("current seal projection"),
                completed.0.to_string()
            ],
        )
        .expect("current writer refreshes typed seal");
    transaction
        .commit()
        .expect("refresh without any new work event");
    let refreshed: Vec<u8> = store
        .connection
        .query_row(
            "SELECT item_json FROM work_items WHERE work_id = ?1",
            [completed.0.to_string()],
            |row| row.get(0),
        )
        .expect("refreshed item");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&refreshed).unwrap()["restored"],
        false
    );
    let report = store
        .verify_all()
        .expect("verify refreshed projections without repair");
    assert!(report.is_healthy(), "{report:?}");
    assert_eq!(
        canonical_inventory(&store),
        before,
        "refresh never rewrites canonical history"
    );
    store
        .save_work_graph_snapshot(
            &item.project_id,
            &actor("snapshot-agent"),
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(20),
            &DevelopmentNoopRedactor,
        )
        .expect("a healthy refreshed projection must save without repair");
    for (hash, bytes) in before {
        let after: Vec<u8> = store
            .connection
            .query_row(
                "SELECT canonical_json FROM objects WHERE object_hash = ?1",
                [hash],
                |row| row.get(0),
            )
            .expect("original canonical object remains");
        assert_eq!(after, bytes, "save preserves every original object's bytes");
    }
}

fn canonical_inventory(store: &SqliteStore) -> Vec<(String, Vec<u8>)> {
    store
        .connection
        .prepare("SELECT object_hash, canonical_json FROM objects ORDER BY object_hash")
        .expect("canonical inventory")
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("canonical rows")
        .collect::<Result<Vec<_>, _>>()
        .expect("canonical inventory rows")
}

#[test]
fn native_projections_can_omit_defaults_explicit_in_canonical_history() {
    let mut store = SqliteStore::open_in_memory().expect("native store");
    let completed = native_history(&mut store);
    let before = canonical_inventory(&store);
    store.connection.execute(
        "UPDATE work_items SET item_json = CAST(json_remove(item_json, '$.restored') AS BLOB) WHERE work_id = ?1",
        [completed.0.to_string()],
    ).expect("omit projection default without changing canonical event");
    store.connection.execute(
        "UPDATE work_completion_seals SET seal_json = CAST(json_remove(seal_json, '$.restored', '$.restored_child_completions') AS BLOB) WHERE work_id = ?1",
        [completed.0.to_string()],
    ).expect("omit projection defaults without changing canonical seal");
    let report = store
        .verify_all()
        .expect("verify absent projection defaults");
    assert!(report.is_healthy(), "{report:?}");
    assert_eq!(canonical_inventory(&store), before);
    store
        .save_work_graph_snapshot(
            &crate::ProjectId("native-projection-repair".into()),
            &actor("snapshot-agent"),
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(20),
            &DevelopmentNoopRedactor,
        )
        .expect("save without repairing the projection");
    let after = canonical_inventory(&store);
    assert!(
        before.iter().all(|entry| after.contains(entry)),
        "save only appends its audit"
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one table exercises semantic drift in all seven native projection families"
)]
fn typed_projection_checks_reject_drift_in_every_work_snapshot_family() {
    let store = all_projection_families();
    let unrelated = CanonicalObject::freeze(&serde_json::json!({"unrelated": true}))
        .expect("runtime-derived unrelated hash");
    let before = canonical_inventory(&store);
    for (table, column, field, value) in [
        (
            "work_items",
            "item_json",
            "title",
            serde_json::json!("drifted title"),
        ),
        (
            "work_items",
            "item_json",
            "lifecycle",
            serde_json::json!("proposed"),
        ),
        (
            "work_items",
            "item_json",
            "revision",
            serde_json::json!(9999),
        ),
        ("work_runs", "run_json", "revision", serde_json::json!(9999)),
        (
            "work_runs",
            "run_json",
            "completion_seal",
            serde_json::json!(unrelated.hash()),
        ),
        (
            "work_root_executions",
            "execution_json",
            "revision",
            serde_json::json!(9999),
        ),
        (
            "work_claims",
            "claim_json",
            "fence",
            serde_json::json!(9999),
        ),
        (
            "work_handoff_offers",
            "offer_json",
            "state",
            serde_json::json!("cancelled"),
        ),
        (
            "work_blockers",
            "blocker_json",
            "detail",
            serde_json::json!("drifted detail"),
        ),
        (
            "work_completion_seals",
            "seal_json",
            "claim_fence",
            serde_json::json!(9999),
        ),
    ] {
        assert_projection_corruption(&store, table, column, &format!("$.{field}"), &value);
    }
    assert_eq!(
        canonical_inventory(&store),
        before,
        "diagnostics never rewrite canonical objects"
    );
}

fn all_projection_families() -> SqliteStore {
    let mut store = SqliteStore::open_in_memory().expect("native store");
    native_history(&mut store);
    let work = store
        .create_work(
            &root_request("native-projection-repair", "handoff", 10),
            &DevelopmentNoopRedactor,
        )
        .expect("handoff work");
    let held = claim(&mut store, &work, "sender", "handoff-claim", 11, 300);
    store
        .offer_work_handoff(
            &OfferWorkHandoffRequest {
                work_id: work.work_id,
                run_id: held.run_id,
                expected_work_revision: work.revision,
                from: held.holder.clone(),
                to: SessionId("recipient".into()),
                claim_id: held.claim_id,
                claim_fence: held.fence,
                ttl_seconds: 100,
                checkpoint_summary: "hand off verified work".into(),
                actor: actor("sender"),
                idempotency_key: "offer".into(),
                offered_at: at(12),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("handoff snapshot");
    assert!(store.verify_all().expect("healthy baseline").is_healthy());
    store
}

fn exact_projection_labels(store: &SqliteStore, table: &str) -> Vec<String> {
    let (id, kind, suffix) = match table {
        "work_items" => ("work_id", "work_item", ""),
        "work_runs" => ("run_id", "work_run", ""),
        "work_root_executions" => ("root_execution_id", "work_root_execution", ""),
        "work_claims" => ("run_id", "work_claim", ""),
        "work_handoff_offers" => ("offer_id", "work_handoff_offer", ""),
        "work_blockers" => ("blocker_id", "work_blocker", ""),
        "work_completion_seals" => ("seal_hash", "completion_seal", ":projection_binding"),
        _ => panic!("unexpected fixture table {table}"),
    };
    let mut labels = store
        .connection
        .prepare(&format!("SELECT {id} FROM {table}"))
        .expect("projection ids")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("projection rows")
        .map(|id| format!("{kind}:{}{suffix}", id.expect("projection id")))
        .collect::<Vec<_>>();
    if table == "work_completion_seals" || table == "work_handoff_offers" {
        let hash = if table == "work_completion_seals" {
            "seal_hash"
        } else {
            "offer_hash"
        };
        labels.extend(
            store
                .connection
                .prepare(&format!("SELECT {hash} FROM {table}"))
                .expect("canonical projection hashes")
                .query_map([], |row| row.get::<_, String>(0))
                .expect("canonical projection rows")
                .map(|hash| format!("{kind}:{}", hash.expect("hash"))),
        );
    }
    labels
}

fn assert_projection_corruption(
    store: &SqliteStore,
    table: &str,
    column: &str,
    path: &str,
    value: &serde_json::Value,
) {
    let labels = exact_projection_labels(store, table);
    assert!(
        !labels.is_empty(),
        "{table} fixture must contain projections"
    );
    store
        .connection
        .execute_batch("SAVEPOINT corrupt")
        .expect("corruption savepoint");
    store
        .connection
        .execute(
            &format!(
                "UPDATE {table} SET {column} = CAST(json_set({column}, ?1, json(?2)) AS BLOB)"
            ),
            params![path, value.to_string()],
        )
        .expect("drift projection");
    let report = store.verify_all().expect("diagnose projection drift");
    for label in labels {
        assert!(
            report.invalid_work_records.contains(&label),
            "{table}{path} must emit exact {label}, not only a scalar-binding suffix: {report:?}"
        );
    }
    restore_savepoint(store);
    assert!(
        store
            .verify_all()
            .expect("healthy after rollback")
            .is_healthy()
    );
}

#[test]
fn typed_projection_checks_reject_unknown_fields() {
    let store = all_projection_families();
    let before = canonical_inventory(&store);
    for (table, column, path) in [
        ("work_items", "item_json", "$.injected"),
        ("work_runs", "run_json", "$.injected"),
        ("work_root_executions", "execution_json", "$.injected"),
        ("work_claims", "claim_json", "$.injected"),
        ("work_handoff_offers", "offer_json", "$.injected"),
        ("work_blockers", "blocker_json", "$.injected"),
        ("work_completion_seals", "seal_json", "$.injected"),
        ("work_items", "item_json", "$.created_by.injected"),
        ("work_blockers", "blocker_json", "$.created_by.injected"),
        (
            "work_completion_seals",
            "seal_json",
            "$.completion_cut.feed.injected",
        ),
        (
            "work_completion_seals",
            "seal_json",
            "$.acceptance[0].injected",
        ),
    ] {
        assert_projection_corruption(
            &store,
            table,
            column,
            path,
            &serde_json::json!("ignored by ordinary serde"),
        );
    }
    assert_eq!(canonical_inventory(&store), before);
}

#[test]
fn typed_projection_checks_reject_duplicate_known_fields() {
    let store = all_projection_families();
    let before = canonical_inventory(&store);
    let expected_blockers = blocker_projection_basis(&store);
    for (table, column, pointer, key) in [
        ("work_completion_seals", "seal_json", "", "claim_fence"),
        ("work_items", "item_json", "", "title"),
        ("work_runs", "run_json", "", "generation"),
        ("work_root_executions", "execution_json", "", "generation"),
        ("work_claims", "claim_json", "", "fence"),
        ("work_handoff_offers", "offer_json", "", "state"),
        ("work_blockers", "blocker_json", "", "detail"),
        ("work_items", "item_json", "/created_by", "actor_id"),
        ("work_blockers", "blocker_json", "/created_by", "actor_id"),
        (
            "work_completion_seals",
            "seal_json",
            "/completion_cut/feed",
            "kind",
        ),
        (
            "work_completion_seals",
            "seal_json",
            "/acceptance/0",
            "criterion",
        ),
    ] {
        store
            .connection
            .execute_batch("SAVEPOINT corrupt")
            .expect("duplicate savepoint");
        duplicate_projection_members(&store, table, column, pointer, key);
        let invalid = if table == "work_blockers" {
            let mut invalid = Vec::new();
            let mut checked = 0;
            verify_json_projection::<WorkBlocker>(
                &store.connection,
                "work_blocker",
                "SELECT blocker_id, blocker_json FROM work_blockers",
                &expected_blockers,
                &mut checked,
                &mut invalid,
            )
            .expect("dedicated blocker projection verifier");
            assert_eq!(checked, expected_blockers.len());
            assert!(
                matches!(store.verify_all(), Err(StoreError::Json(_))),
                "the existing relation reader must refuse malformed blocker JSON"
            );
            invalid
        } else {
            store
                .verify_all()
                .expect("diagnose duplicate members")
                .invalid_work_records
        };
        for label in exact_projection_labels(&store, table) {
            assert!(
                invalid.contains(&label),
                "{table}{pointer}/{key}: exact {label} required: {invalid:?}"
            );
        }
        restore_savepoint(&store);
        assert!(
            store
                .verify_all()
                .expect("healthy after rollback")
                .is_healthy()
        );
    }
    assert_eq!(canonical_inventory(&store), before);
}

// Malformed blocker JSON already makes the relation reader abort doctor.
// Retain its healthy basis to require the dedicated verifier's exact labels.
fn blocker_projection_basis(store: &SqliteStore) -> HashMap<String, serde_json::Value> {
    store
        .connection
        .prepare("SELECT blocker_id, blocker_json FROM work_blockers")
        .expect("blocker basis")
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .expect("blocker rows")
        .map(|row| {
            let (id, bytes) = row.expect("blocker row");
            (id, serde_json::from_slice(&bytes).expect("healthy blocker"))
        })
        .collect()
}

fn duplicate_projection_members(
    store: &SqliteStore,
    table: &str,
    column: &str,
    pointer: &str,
    key: &str,
) {
    let rows = store
        .connection
        .prepare(&format!("SELECT rowid, {column} FROM {table}"))
        .expect("projection rows")
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .expect("projection query")
        .collect::<Result<Vec<_>, _>>()
        .expect("projection bytes");
    assert!(!rows.is_empty(), "{table} fixture contains projections");
    for (rowid, bytes) in rows {
        let value: serde_json::Value = serde_json::from_slice(&bytes).expect("original projection");
        let member = format!(
            "{}:{}",
            serde_json::to_string(key).unwrap(),
            serde_json::to_string(&value.pointer(pointer).expect("nested object")[key]).unwrap()
        );
        let original = serde_json::to_string(&value).expect("normalized fixture");
        assert_eq!(
            original.matches(&member).count(),
            1,
            "unambiguous fixture member"
        );
        let duplicate = original.replacen(&member, &format!("{member},{member}"), 1);
        assert_ne!(duplicate, original);
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&duplicate).unwrap(),
            value,
            "Value collapses the duplicate: the regression must inspect original bytes"
        );
        store
            .connection
            .execute(
                &format!("UPDATE {table} SET {column} = ?1 WHERE rowid = ?2"),
                params![duplicate.as_bytes(), rowid],
            )
            .expect("duplicate known projection member");
    }
}

#[test]
fn typed_projection_fractional_timestamp_encodings_agree_with_operational_readers() {
    // This fractional spelling is refused by the operational millis binding.
    // Whole-second offsets may parse there; the verifier deliberately requires
    // the writer's spelling regardless. This is not a new SQL spelling policy.
    let mut store = SqliteStore::open_in_memory().expect("store");
    let mut request = root_request("timestamp-representation", "root", 0);
    request.created_at += Duration::milliseconds(123);
    request.deferred_until = Some(at(1) + Duration::milliseconds(123));
    let work = store
        .create_work(&request, &DevelopmentNoopRedactor)
        .expect("fractional timestamp root");
    let held = store
        .claim_work(
            &ClaimWorkRequest {
                work_id: work.work_id,
                expected_work_revision: work.revision,
                expected_run_id: work.active_run_id,
                holder: SessionId("executor".into()),
                ttl_seconds: 300,
                recovery_reason: None,
                actor: actor("executor"),
                idempotency_key: "claim".into(),
                claimed_at: at(2) + Duration::milliseconds(123),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("fractional claim");
    let before = canonical_inventory(&store);
    assert!(store.verify_all().expect("healthy baseline").is_healthy());
    for (table, column, field) in [
        ("work_items", "item_json", "deferred_until"),
        ("work_items", "item_json", "created_at"),
        ("work_items", "item_json", "updated_at"),
        ("work_claims", "claim_json", "expires_at"),
    ] {
        store
            .get_work_item(work.work_id)
            .expect("baseline item read");
        load_work_claim_optional(&store.connection, held.run_id).expect("baseline claim read");
        let original: String = store
            .connection
            .query_row(
                &format!("SELECT json_extract({column}, ?1) FROM {table}"),
                [format!("$.{field}")],
                |row| row.get(0),
            )
            .expect("writer timestamp");
        let offset = format!(
            "{}+00:00",
            original.strip_suffix('Z').expect("UTC writer spelling")
        );
        assert_eq!(
            original.parse::<DateTime<Utc>>().unwrap(),
            offset.parse::<DateTime<Utc>>().unwrap()
        );
        store
            .connection
            .execute_batch("SAVEPOINT corrupt")
            .expect("timestamp savepoint");
        store
            .connection
            .execute(
                &format!("UPDATE {table} SET {column} = CAST(json_set({column}, ?1, ?2) AS BLOB)"),
                params![format!("$.{field}"), offset],
            )
            .expect("equivalent timestamp encoding");
        let read_refused = if table == "work_items" {
            store.get_work_item(work.work_id).is_err()
        } else {
            load_work_claim_optional(&store.connection, held.run_id).is_err()
        };
        assert!(
            read_refused,
            "{table}.{field}: operational reader must witness the mismatch"
        );
        let report = store
            .verify_all()
            .expect("diagnose timestamp representation");
        for label in exact_projection_labels(&store, table) {
            assert!(
                report.invalid_work_records.contains(&label),
                "{table}.{field}: doctor must agree with the refused read: {report:?}"
            );
        }
        restore_savepoint(&store);
    }
    assert_eq!(canonical_inventory(&store), before);
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
