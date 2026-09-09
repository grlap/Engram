use super::resolve_on;
use crate::{CanonicalObject, StoreError};

#[test]
fn migration_resolution_binds_both_addresses_and_refuses_missing_or_corrupt_provenance() {
    let directory = crate::test_support::temp_home().expect("fixture");
    let source_path = directory.path().join("source.db");
    let (_, seal, _, marker) = super::super::convert::tests::source(&source_path);
    let mut target = super::super::convert::tests::target();
    super::super::convert_aggregate_store_objects(&source_path, &mut target).expect("convert");
    let before = crate::storage::test_database_shape_snapshot(&target).expect("before");
    let mapped = resolve_on(&target, &seal).expect("historical alias");
    assert_ne!(mapped, seal);
    assert_eq!(
        resolve_on(&target, &marker).expect("identity entry"),
        marker
    );
    let native =
        CanonicalObject::freeze(&serde_json::json!({"native":"after migration"})).expect("native");
    assert_eq!(
        resolve_on(&target, native.hash()).expect("ordinary address unchanged"),
        *native.hash()
    );
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&target).expect("after"),
        before
    );
    let binding: String = target
        .query_row(
            "SELECT binding_hash FROM migration_object_map WHERE source_hash=?1",
            [seal.as_str()],
            |row| row.get(0),
        )
        .expect("binding");
    for fault in [
        format!("DELETE FROM migration_object_map WHERE source_hash='{seal}'"),
        format!("DELETE FROM objects WHERE object_hash='{binding}'"),
        format!("UPDATE objects SET canonical_json=X'7b7d' WHERE object_hash='{binding}'"),
        format!(
            "UPDATE migration_object_map SET target_hash='{marker}' WHERE source_hash='{seal}'"
        ),
        format!("UPDATE objects SET object_kind='wrong_kind' WHERE object_hash='{mapped}'"),
    ] {
        let mut damaged = super::super::convert::tests::target();
        super::super::convert_aggregate_store_objects(&source_path, &mut damaged)
            .expect("independent target");
        // Disable enforcement only for committed fault injection. A savepoint
        // would make PRAGMA foreign_keys=ON a no-op until the transaction ends.
        damaged
            .execute_batch("PRAGMA foreign_keys=OFF")
            .expect("inject");
        damaged
            .execute_batch(&fault)
            .expect("controlled fixture corruption");
        damaged
            .execute_batch("PRAGMA foreign_keys=ON")
            .expect("reader configuration");
        assert_eq!(
            damaged
                .query_row("PRAGMA foreign_keys", [], |row| row.get::<_, i64>(0))
                .expect("enforcement"),
            1
        );
        let damaged_before =
            crate::storage::test_database_shape_snapshot(&damaged).expect("damaged cut");
        let result = resolve_on(&damaged, &seal);
        assert!(
            matches!(result,Err(StoreError::InvalidWorkProjection(ref reason)) if reason.starts_with("migration provenance:")),
            "{result:?}"
        );
        assert_eq!(
            crate::storage::test_database_shape_snapshot(&damaged).expect("read only"),
            damaged_before
        );
    }
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&target).expect("restored"),
        before
    );
    target
        .execute(
            "DELETE FROM migration_object_map WHERE source_hash=?1",
            [marker.as_str()],
        )
        .expect("remove identity entry");
    assert!(
        resolve_on(&target, &marker).is_err(),
        "absence must not mean unchanged"
    );
}

#[test]
fn migration_reexpressed_replay_retains_original_bytes_and_missing_map_or_audit_refuses() {
    use super::validate_reexpressed_result_on;
    use rusqlite::params;
    let directory = crate::test_support::temp_home().expect("fixture");
    let source_path = directory.path().join("source.db");
    let (_, seal, _, _) = super::super::convert::tests::source(&source_path);
    let mut target = super::super::convert::tests::target();
    super::super::convert_aggregate_store_objects(&source_path, &mut target).expect("convert");
    let original: Vec<u8> = target
        .query_row(
            "SELECT canonical_json FROM migration_original_objects WHERE object_hash=?1",
            [seal.as_str()],
            |row| row.get(0),
        )
        .expect("full original bytes");
    let mapped = resolve_on(&target, &seal).expect("mapped seal");
    let current: Vec<u8> = target
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_hash=?1",
            [mapped.as_str()],
            |row| row.get(0),
        )
        .expect("current seal bytes");
    target.execute("INSERT INTO migration_reexpressed_results VALUES ('p','complete_work','old-key',?1,?2,?3,?4,?5,?6)",params![original,crate::ObjectHash::from_canonical_bytes(&original).as_str(),current,crate::ObjectHash::from_canonical_bytes(&current).as_str(),seal.as_str(),mapped.as_str()]).expect("per-key audit");
    let before = crate::storage::test_database_shape_snapshot(&target).expect("before replay");
    validate_reexpressed_result_on(&target, "complete_work", "old-key", &current)
        .expect("replay boundary accepts the recorded new representation");
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&target).expect("after replay"),
        before
    );
    for fault in [
        "DELETE FROM migration_object_map",
        "DELETE FROM migration_reexpressed_results",
        "UPDATE migration_reexpressed_results SET project_id='different-project'",
        "INSERT INTO migration_reexpressed_results SELECT 'different-project',operation,idempotency_key,source_result,source_result_hash,target_result,target_result_hash,source_seal,target_seal FROM migration_reexpressed_results",
    ] {
        target
            .execute_batch("SAVEPOINT missing")
            .expect("savepoint");
        target
            .execute(fault, [])
            .expect("remove required provenance");
        assert!(
            validate_reexpressed_result_on(&target, "complete_work", "old-key", &current).is_err()
        );
        target
            .execute_batch("ROLLBACK TO missing; RELEASE missing")
            .expect("restore");
    }
    let native =
        CanonicalObject::freeze(&serde_json::json!({"native":"new key"})).expect("native result");
    validate_reexpressed_result_on(&target, "complete_work", "new-key", native.bytes())
        .expect("native replay unaffected");
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&target).expect("final read-only state"),
        before
    );

    // Exercise the actual completion replay branch, not only its audit helper.
    // This deliberately has no executable work item: a second completion would
    // fail, and every write would be visible in the whole-store snapshot.
    let phase = directory.path().join("canonical-phase.db");
    target
        .execute("VACUUM INTO ?1", [phase.to_str().expect("fixture path")])
        .expect("phase copy");
    let mut store = crate::SqliteStore::open_in_memory().expect("current store");
    store
        .connection
        .execute(
            "ATTACH DATABASE ?1 AS migrated",
            [phase.to_str().expect("path")],
        )
        .expect("attach fixture");
    for table in [
        "objects",
        "migration_original_objects",
        "migration_object_map",
        "migration_reexpressed_results",
    ] {
        store
            .connection
            .execute(
                &format!("INSERT INTO {table} SELECT * FROM migrated.{table}"),
                [],
            )
            .expect("fixture provenance");
    }
    store
        .connection
        .execute_batch("DETACH DATABASE migrated")
        .expect("detach");
    let seal: crate::CompletionSeal = serde_json::from_slice(&current).expect("seal");
    let request = crate::domain::CompleteWorkRequest {
        work_id: seal.work_id,
        run_id: seal.run_id,
        holder: crate::SessionId("s".into()),
        expected_work_revision: seal.accepted_work_revision,
        claim_id: seal.claim_id,
        claim_fence: seal.claim_fence,
        evidence: seal.evidence.clone(),
        acceptance: seal.acceptance.clone(),
        drain: seal.drain.clone(),
        actor: seal.actor.clone(),
        idempotency_key: "old-key".into(),
        completed_at: seal.completed_at,
    };
    let intent = CanonicalObject::freeze(&request).expect("intent");
    store
        .connection
        .execute(
            "INSERT INTO work_operation_results VALUES ('complete_work','old-key',?1,?2)",
            params![intent.hash().as_str(), current],
        )
        .expect("historical result");
    let before = crate::storage::test_database_shape_snapshot(&store.connection)
        .expect("before actual replay");
    assert_eq!(
        store
            .complete_work(&request, &crate::memory::DevelopmentNoopRedactor)
            .expect("replay does not execute"),
        seal
    );
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&store.connection)
            .expect("after actual replay"),
        before
    );
    store
        .connection
        .execute(
            "DELETE FROM migration_object_map WHERE source_hash=?1",
            [original_seal_address(&original).as_str()],
        )
        .expect("remove required mapping");
    let damaged =
        crate::storage::test_database_shape_snapshot(&store.connection).expect("damaged before");
    assert!(
        matches!(store.complete_work(&request, &crate::memory::DevelopmentNoopRedactor), Err(StoreError::InvalidWorkProjection(reason)) if reason.starts_with("migration provenance:"))
    );
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&store.connection).expect("damaged after"),
        damaged
    );
}

fn original_seal_address(bytes: &[u8]) -> crate::ObjectHash {
    CanonicalObject::freeze(
        &serde_json::from_slice::<serde_json::Value>(bytes).expect("original value"),
    )
    .expect("original identity")
    .hash()
    .clone()
}
