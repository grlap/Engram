use super::*;
use crate::storage::work::test_support::*;
use crate::storage::work::*;

use crate::storage::work::query::load_work_claim_optional;

mod snapshot;

// Seed valid canonical representations with omitted serde defaults, not a
// captured store or a pinned object digest. Only this synthetic fixture is edited.
fn omit_native_restore_defaults(store: &SqliteStore, work: WorkId) {
    let (old_seal, seal_bytes): (String, Vec<u8>) = store
        .connection
        .query_row(
            "SELECT seal_id, seal_json FROM work_completion_seals WHERE work_id = ?1",
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

    let (old_event, event_bytes): (String, Vec<u8>) = store
        .connection
        .query_row(
            "SELECT item.latest_event_id, object.canonical_json FROM work_items item
         JOIN objects object ON object.object_id = item.latest_event_id WHERE item.work_id = ?1",
            [work.0.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("latest native event");
    let mut event_json: serde_json::Value =
        serde_json::from_slice(&event_bytes).expect("event JSON");
    assert_eq!(
        event_json["work"]
            .as_object_mut()
            .expect("item object")
            .remove("restored"),
        Some(serde_json::json!(false))
    );
    event_json["transition"]["seal"] = serde_json::json!(seal.key());
    event_json["run"]["completion_seal"] = serde_json::json!(seal.key());
    let event = CanonicalObject::freeze(&event_json).expect("canonical omitted-default event");
    SqliteStore::insert_object(&store.connection, "work_event", &event).expect("seed event");
    store
        .connection
        .execute(
            "UPDATE work_completion_seals SET seal_id = ?1, seal_json = ?2 WHERE work_id = ?3",
            params![seal.key().as_str(), seal.bytes(), work.0.to_string()],
        )
        .expect("bind seal projection");
    store
        .connection
        .execute(
            "UPDATE work_runs SET completion_seal_id = ?1, run_json = ?2 WHERE work_id = ?3",
            params![
                seal.key().as_str(),
                serde_json::to_vec(&event_json["run"]).expect("run bytes"),
                work.0.to_string()
            ],
        )
        .expect("bind completed run");
    store
        .connection
        .execute(
            "UPDATE work_items SET latest_event_id = ?1, item_json = ?2 WHERE work_id = ?3",
            params![
                event.key().as_str(),
                serde_json::to_vec(&event_json["work"]).expect("item bytes"),
                work.0.to_string()
            ],
        )
        .expect("bind item projection");
    for (old, new) in [(&old_seal, seal.key()), (&old_event, event.key())] {
        store
            .connection
            .execute(
                "UPDATE work_feed_entries SET object_id = ?1 WHERE object_id = ?2",
                params![new.as_str(), old],
            )
            .expect("bind canonical feed entry");
        store
            .connection
            .execute("DELETE FROM objects WHERE object_id = ?1", [old])
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
                "SELECT canonical_json FROM objects WHERE object_id = ?1",
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
        .prepare("SELECT object_id, canonical_json FROM objects ORDER BY object_id")
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
            serde_json::json!(unrelated.key()),
        ),
        (
            "work_root_executions",
            "header_json",
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
        "work_completion_seals" => ("seal_id", "completion_seal", ":projection_binding"),
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
            "seal_id"
        } else {
            "offer_object_id"
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
        ("work_root_executions", "header_json", "$.injected"),
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
        ("work_root_executions", "header_json", "", "generation"),
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
    let directory = crate::test_support::temp_home().expect("temporary native store");
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
        .prepare("SELECT object_id, canonical_json FROM objects ORDER BY object_id")
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
        .prepare("SELECT object_id, canonical_json FROM objects ORDER BY object_id")
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
    let directory = crate::test_support::temp_home().expect("temporary store");
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

/// Respells the UTC timestamp at `path` in one stored JSON cell with an
/// offset, which decodes to the same instant but is not the writer's
/// spelling.
fn respell_timestamp(
    store: &SqliteStore,
    table: &str,
    column: &str,
    key: &str,
    id: &str,
    path: &str,
) {
    let original: String = store
        .connection
        .query_row(
            &format!("SELECT json_extract({column}, ?1) FROM {table} WHERE {key} = ?2"),
            params![path, id],
            |row| row.get(0),
        )
        .expect("writer timestamp");
    let offset = format!(
        "{}+00:00",
        original.strip_suffix('Z').expect("UTC writer spelling")
    );
    let changed = store
        .connection
        .execute(
            &format!(
                "UPDATE {table} SET {column} = CAST(json_set({column}, ?1, ?2) AS BLOB) WHERE {key} = ?3"
            ),
            params![path, offset, id],
        )
        .expect("respell timestamp");
    assert_eq!(changed, 1);
}

// A canonical value that decodes but does not survive being written back is
// named as a canonical-side representation failure, apart from projection
// drift; a projection-only change keeps the projection label.
#[test]
fn canonical_representation_failures_are_labelled_apart_from_projection_drift() {
    let store = all_projection_families();
    let before = canonical_inventory(&store);
    let first = |sql: &str| -> (String, String) {
        store
            .connection
            .query_row(sql, [], |row| Ok((row.get(0)?, row.get(1)?)))
            .expect("fixture row")
    };
    let (offer_object, _) =
        first("SELECT offer_object_id, offer_id FROM work_handoff_offers LIMIT 1");
    let (work, event) = first(
        "SELECT item.work_id, item.latest_event_id FROM work_items item
         JOIN work_handoff_offers offer ON offer.work_id = item.work_id LIMIT 1",
    );
    let (seal, _) = first("SELECT seal_id, work_id FROM work_completion_seals LIMIT 1");
    for (table, column, key, id, path, canonical_label, projection_label, projection) in [
        (
            "objects",
            "canonical_json",
            "object_id",
            offer_object.clone(),
            "$.offered_at",
            format!("work_handoff_offer:{offer_object}:canonical_representation"),
            format!("work_handoff_offer:{offer_object}"),
            (
                "work_handoff_offers",
                "offer_json",
                "offer_object_id",
                offer_object.clone(),
            ),
        ),
        (
            "objects",
            "canonical_json",
            "object_id",
            event.clone(),
            "$.work.created_at",
            format!("work_item:{work}:canonical_representation"),
            format!("work_item:{work}"),
            ("work_items", "item_json", "work_id", work.clone()),
        ),
        (
            "objects",
            "canonical_json",
            "object_id",
            seal.clone(),
            "$.completed_at",
            format!("completion_seal:{seal}:canonical_representation"),
            format!("completion_seal:{seal}:projection_binding"),
            (
                "work_completion_seals",
                "seal_json",
                "seal_id",
                seal.clone(),
            ),
        ),
    ] {
        store
            .connection
            .execute_batch("SAVEPOINT corrupt")
            .expect("canonical savepoint");
        respell_timestamp(&store, table, column, key, &id, path);
        let report = store.verify_all().expect("diagnose canonical respelling");
        assert_eq!(
            label_count(&report, &canonical_label),
            1,
            "{path}: reported once: {report:?}"
        );
        assert!(
            !report.invalid_work_records.contains(&projection_label),
            "{path}: the projection did not drift: {report:?}"
        );
        // Projection damage in the same row is still reported beside the
        // canonical label, never hidden by it: a member the projection's own
        // guard refuses, and a well-typed member that differs from what the
        // canonical side decodes to.
        restore_savepoint(&store);
        let (projection_table, projection_column, projection_key, projection_id) = &projection;
        let semantic = match *projection_table {
            "work_handoff_offers" => ("$.to", "'someone-else'"),
            "work_items" => ("$.title", "'A title that drifted'"),
            _ => (
                "$.claim_fence",
                "json_extract(seal_json, '$.claim_fence') + 1",
            ),
        };
        for (member, value) in [("$.injected", "'x'"), semantic] {
            store
                .connection
                .execute_batch("SAVEPOINT corrupt")
                .expect("joint savepoint");
            respell_timestamp(&store, table, column, key, &id, path);
            let changed = store
                .connection
                .execute(
                    &format!(
                        "UPDATE {projection_table}                          SET {projection_column} =                          CAST(json_set({projection_column}, '{member}', {value}) AS BLOB)                          WHERE {projection_key} = ?1"
                    ),
                    [projection_id],
                )
                .expect("change the projection");
            assert_eq!(changed, 1);
            let report = store.verify_all().expect("diagnose both sides");
            for label in [&canonical_label, &projection_label] {
                assert!(
                    report.invalid_work_records.contains(label),
                    "{path} with {member}: {label} must be reported: {report:?}"
                );
            }
            assert_eq!(
                label_count(&report, &canonical_label),
                1,
                "{path} with {member}: the canonical label is reported once: {report:?}"
            );
            restore_savepoint(&store);
        }
    }
    // The same respelling in the projection alone is projection drift.
    store
        .connection
        .execute_batch("SAVEPOINT corrupt")
        .expect("projection savepoint");
    respell_timestamp(
        &store,
        "work_items",
        "item_json",
        "work_id",
        &work,
        "$.created_at",
    );
    let report = store.verify_all().expect("diagnose projection respelling");
    assert!(
        report
            .invalid_work_records
            .contains(&format!("work_item:{work}")),
        "{report:?}"
    );
    assert!(
        !report
            .invalid_work_records
            .iter()
            .any(|label| label.ends_with(":canonical_representation")),
        "{report:?}"
    );
    restore_savepoint(&store);
    assert!(
        store
            .verify_all()
            .expect("healthy after rollback")
            .is_healthy()
    );
    assert_eq!(canonical_inventory(&store), before);
}

fn seal_rows(store: &SqliteStore, seal: &str) -> (Vec<u8>, Vec<u8>) {
    let canonical = store
        .connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_id = ?1",
            [seal],
            |row| row.get(0),
        )
        .expect("canonical seal");
    let projection = store
        .connection
        .query_row(
            "SELECT seal_json FROM work_completion_seals WHERE seal_id = ?1",
            [seal],
            |row| row.get(0),
        )
        .expect("seal projection");
    (canonical, projection)
}

// A member the seal type skips when empty may be omitted, but an explicit
// empty list stored in it decodes and then does not survive being written
// back. Integrity verification refuses it, labelled by the side that carries
// it; an ordinary load still decodes it, and verification rewrites nothing.
#[test]
fn explicit_empty_skipped_seal_members_are_refused_by_integrity_verification() {
    let mut store = SqliteStore::open_in_memory().expect("native store");
    let completed = native_history(&mut store);
    let seal: String = store
        .connection
        .query_row(
            "SELECT seal_id FROM work_completion_seals WHERE work_id = ?1",
            [completed.0.to_string()],
            |row| row.get(0),
        )
        .expect("native seal");
    let (canonical, projection) = seal_rows(&store, &seal);
    let typed: CompletionSeal = serde_json::from_slice(&canonical).expect("typed seal");
    assert!(typed.obligations.is_empty() && typed.environment.is_empty());
    let encoded = serde_json::to_value(&typed).expect("encoded seal");
    for member in ["obligations", "environment"] {
        assert!(
            encoded.get(member).is_none(),
            "normal encoding omits an empty {member}"
        );
        for stored in [&canonical, &projection] {
            let stored: serde_json::Value = serde_json::from_slice(stored).expect("stored seal");
            assert!(stored.get(member).is_none(), "the fixture omits {member}");
        }
    }
    assert!(
        store.verify_all().expect("baseline").is_healthy(),
        "the omitted-member baseline is healthy"
    );
    let before = canonical_inventory(&store);
    let canonical_label = format!("completion_seal:{seal}:canonical_representation");
    let projection_label = format!("completion_seal:{seal}:projection_binding");
    let drift_label = format!("completion_seal:{seal}");
    for member in ["obligations", "environment"] {
        for (in_canonical, in_projection) in [(false, true), (true, false), (true, true)] {
            store
                .connection
                .execute_batch("SAVEPOINT corrupt")
                .expect("explicit-member savepoint");
            for (inject, table, column, key) in [
                (in_canonical, "objects", "canonical_json", "object_id"),
                (
                    in_projection,
                    "work_completion_seals",
                    "seal_json",
                    "seal_id",
                ),
            ] {
                if inject {
                    let changed = store
                        .connection
                        .execute(
                            &format!(
                                "UPDATE {table} SET {column} = \
                                 CAST(json_set({column}, '$.{member}', json('[]')) AS BLOB) \
                                 WHERE {key} = ?1"
                            ),
                            [&seal],
                        )
                        .expect("store an explicit empty member");
                    assert_eq!(changed, 1);
                }
            }
            let stored = seal_rows(&store, &seal);
            for (injected, bytes) in [(in_canonical, &stored.0), (in_projection, &stored.1)] {
                let value: serde_json::Value = serde_json::from_slice(bytes).expect("stored JSON");
                assert_eq!(
                    value.get(member) == Some(&serde_json::json!([])),
                    injected,
                    "{member}: the explicit [] is stored only where injected"
                );
                assert_eq!(
                    serde_json::from_slice::<CompletionSeal>(bytes).expect("decodes"),
                    typed,
                    "{member}: an explicit [] decodes as the empty default"
                );
            }
            let loaded: CompletionSeal = load_typed_work_object(
                &store.connection,
                &ObjectId::from_stored(seal.clone()).expect("stored seal id"),
                "completion_seal",
            )
            .expect("an ordinary load decodes the seal");
            assert_eq!(loaded, typed);
            let report = store.verify_all().expect("diagnose the explicit member");
            let labels: std::collections::BTreeMap<&str, usize> = report
                .invalid_work_records
                .iter()
                .filter(|label| label.contains(&seal))
                .fold(std::collections::BTreeMap::new(), |mut counts, label| {
                    *counts.entry(label.as_str()).or_default() += 1;
                    counts
                });
            // Two checks read each seal: its run binding, and the comparison of
            // the projection with the object it names. A projection-side
            // finding is named by each in its own words; a canonical-side one
            // is detected by both and reported once.
            let mut expected = std::collections::BTreeMap::new();
            if in_canonical {
                expected.insert(canonical_label.as_str(), 1);
            }
            if in_projection {
                expected.insert(projection_label.as_str(), 1);
                expected.insert(drift_label.as_str(), 1);
            }
            assert_eq!(
                labels, expected,
                "{member} (canonical {in_canonical}, projection {in_projection}): {report:?}"
            );
            assert_eq!(
                seal_rows(&store, &seal),
                stored,
                "{member}: verification rewrites nothing"
            );
            restore_savepoint(&store);
        }
    }
    assert!(
        store
            .verify_all()
            .expect("healthy after rollback")
            .is_healthy()
    );
    assert_eq!(canonical_inventory(&store), before);
    assert_eq!(seal_rows(&store, &seal), (canonical, projection));
}

fn label_count(report: &crate::storage::IntegrityReport, label: &str) -> usize {
    report
        .invalid_work_records
        .iter()
        .filter(|found| found.as_str() == label)
        .count()
}

fn seal_labels(
    report: &crate::storage::IntegrityReport,
    seal: &str,
) -> std::collections::BTreeMap<String, usize> {
    report
        .invalid_work_records
        .iter()
        .filter(|label| label.contains(seal))
        .fold(std::collections::BTreeMap::new(), |mut counts, label| {
            *counts.entry(label.clone()).or_default() += 1;
            counts
        })
}

fn native_seal_id(store: &SqliteStore, work: WorkId) -> String {
    store
        .connection
        .query_row(
            "SELECT seal_id FROM work_completion_seals WHERE work_id = ?1",
            [work.0.to_string()],
            |row| row.get(0),
        )
        .expect("native seal")
}

fn store_explicit_empty_canonical_obligations(store: &SqliteStore, seal: &str) {
    let changed = store
        .connection
        .execute(
            "UPDATE objects SET canonical_json = \
             CAST(json_set(canonical_json, '$.obligations', json('[]')) AS BLOB) \
             WHERE object_id = ?1",
            [seal],
        )
        .expect("store an explicit empty member");
    assert_eq!(changed, 1);
}

// Only the run-binding check reads a canonical seal through a JSON value, so
// only it can still see a lost member when the stored bytes also repeat a
// known member, which the typed comparison refuses before looking further.
// Keeping both detectors keeps that finding.
#[test]
fn a_seal_representation_loss_only_one_check_can_see_is_still_reported() {
    let mut store = SqliteStore::open_in_memory().expect("native store");
    let completed = native_history(&mut store);
    let seal = native_seal_id(&store, completed);
    let (canonical, _) = seal_rows(&store, &seal);
    let value: serde_json::Value = serde_json::from_slice(&canonical).expect("canonical seal");
    let fence = format!(
        "\"claim_fence\":{}",
        serde_json::to_string(&value["claim_fence"]).expect("fence")
    );
    let original = String::from_utf8(canonical.clone()).expect("UTF-8 seal");
    assert_eq!(original.matches(&fence).count(), 1, "unambiguous member");
    let damaged = original
        .replacen(&fence, &format!("{fence},{fence}"), 1)
        .replacen('{', "{\"obligations\":[],", 1);
    let reparsed: serde_json::Value = serde_json::from_str(&damaged).expect("still JSON");
    assert_eq!(reparsed["obligations"], serde_json::json!([]));
    assert!(
        serde_json::from_str::<CompletionSeal>(&damaged).is_err(),
        "the typed decode refuses the repeated member"
    );
    store
        .connection
        .execute(
            "UPDATE objects SET canonical_json = ?1 WHERE object_id = ?2",
            params![damaged.as_bytes(), seal],
        )
        .expect("store the damaged canonical seal");
    let report = store.verify_all().expect("diagnose the damaged seal");
    let expected: std::collections::BTreeMap<String, usize> = [
        (
            format!("completion_seal:{seal}:canonical_representation"),
            1,
        ),
        (format!("completion_seal:{seal}"), 1),
    ]
    .into_iter()
    .collect();
    assert_eq!(seal_labels(&report, &seal), expected, "{report:?}");
}

#[test]
fn each_seal_reports_its_own_representation_loss_once() {
    let mut store = SqliteStore::open_in_memory().expect("native store");
    let first = native_history(&mut store);
    let project = "native-projection-repair";
    let second = store
        .create_work(
            &root_request(project, "second", 30),
            &DevelopmentNoopRedactor,
        )
        .expect("second root");
    let held = claim(&mut store, &second, "second-executor", "claim-2", 31, 300);
    let proof = evidence(
        &mut store,
        &second,
        &held,
        "second-executor",
        "evidence-2",
        32,
    );
    checkpoint(
        &mut store,
        &second,
        &held,
        "second-executor",
        "checkpoint-2",
        33,
        std::slice::from_ref(&proof),
    );
    complete(
        &mut store,
        &second,
        &held,
        "second-executor",
        &proof,
        "complete-2",
        34,
    )
    .expect("second seal");
    let seals = [
        native_seal_id(&store, first),
        native_seal_id(&store, second.work_id),
    ];
    assert_ne!(seals[0], seals[1]);
    assert!(store.verify_all().expect("baseline").is_healthy());
    for seal in &seals {
        store_explicit_empty_canonical_obligations(&store, seal);
    }
    let report = store.verify_all().expect("diagnose both seals");
    for seal in &seals {
        let expected: std::collections::BTreeMap<String, usize> = [(
            format!("completion_seal:{seal}:canonical_representation"),
            1,
        )]
        .into_iter()
        .collect();
        assert_eq!(seal_labels(&report, seal), expected, "{report:?}");
    }
}

// Losing the canonical seal, or finding another kind of object under its id,
// is not a representation loss and keeps its own findings.
#[test]
fn a_missing_or_mistyped_canonical_seal_is_not_reported_as_representation_loss() {
    let mut store = SqliteStore::open_in_memory().expect("native store");
    let completed = native_history(&mut store);
    let seal = native_seal_id(&store, completed);
    // The mistyped object also carries an explicit empty member, so a
    // representation finding would appear if another kind of object were read
    // as this seal. An object stored as an event that no feed names is that
    // object's own finding.
    let seal_findings = [
        format!("completion_seal:{seal}"),
        format!("completion_seal:{seal}:projection_binding"),
    ];
    for (case, explicit_empty, statement, extra) in [
        (
            "missing",
            false,
            "DELETE FROM objects WHERE object_id = ?1",
            None,
        ),
        (
            "mistyped",
            true,
            "UPDATE objects SET object_kind = 'work_event' WHERE object_id = ?1",
            Some(format!("work_event:{seal}:missing_work_feeds")),
        ),
    ] {
        let expected: std::collections::BTreeMap<String, usize> = seal_findings
            .iter()
            .cloned()
            .chain(extra)
            .map(|label| (label, 1))
            .collect();
        // Simulated damage: a store guards these links itself, so the check
        // that reports their loss is reached only with that guard off.
        store
            .connection
            .execute_batch("PRAGMA foreign_keys = OFF; SAVEPOINT corrupt")
            .expect("canonical savepoint");
        if explicit_empty {
            store_explicit_empty_canonical_obligations(&store, &seal);
        }
        let changed = store
            .connection
            .execute(statement, [&seal])
            .expect("damage the canonical seal");
        assert_eq!(changed, 1);
        let report = store.verify_all().expect("diagnose the canonical seal");
        assert!(!report.is_healthy(), "{case}: {report:?}");
        assert_eq!(seal_labels(&report, &seal), expected, "{case}: {report:?}");
        restore_savepoint(&store);
        store
            .connection
            .execute_batch("PRAGMA foreign_keys = ON")
            .expect("restore the link guard");
    }
    assert!(store.verify_all().expect("restored").is_healthy());
}
