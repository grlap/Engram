use super::*;
use crate::ObjectId;

fn ordinary_fixture(store: SqliteStore) -> Fixture {
    fixture_with_create_and_root(
        store,
        0,
        &root_request("project-a", "ordinary-work", 1),
        true,
    )
}

#[test]
fn work_session_receipt_audit_checks_scoped_key_and_frozen_intent() {
    let mut f = ordinary_fixture(SqliteStore::open_in_memory().unwrap());
    f.store
        .record_registered_verification(
            &f.project,
            &f.session,
            &f.connection,
            HostSessionAuthority::Work,
            &actor("runner"),
            &f.request,
            at(10),
        )
        .unwrap();
    assert!(f.store.verify_all().unwrap().is_healthy());
    let original = f.snapshot();
    for (column, value) in [
        (
            "request_hash",
            ObjectId::from_canonical_bytes(b"different intent")
                .as_str()
                .to_owned(),
        ),
        (
            "idempotency_key",
            serde_json::to_string(&(&f.project, "other-session", &f.request.operation_key))
                .unwrap(),
        ),
    ] {
        f.store
            .connection
            .execute_batch("SAVEPOINT corrupt_receipt")
            .unwrap();
        f.store.connection.execute(&format!(
            "UPDATE work_operation_results SET {column} = ?1 WHERE operation = 'host_verification_record'"
        ), [value]).unwrap();
        let report = f.store.verify_all().unwrap();
        assert!(
            report
                .invalid_work_records
                .iter()
                .any(|error| error.contains("host_verification_record")
                    && error.contains("scope or intent")),
            "{column}: {report:?}"
        );
        f.store
            .connection
            .execute_batch("ROLLBACK TO corrupt_receipt; RELEASE corrupt_receipt")
            .unwrap();
        assert_eq!(f.snapshot(), original);
    }
}
use crate::storage::HostSessionAuthority;

fn history(f: &Fixture) -> Vec<(String, Vec<Vec<u8>>)> {
    crate::storage::test_database_shape_snapshot(&f.store.connection)
        .unwrap()
        .rows
        .into_iter()
        .filter(|(name, _)| {
            matches!(
                name.as_str(),
                "control_anchors" | "control_changes" | "control_operation_results"
            )
        })
        .collect()
}

#[test]
fn work_session_names_reads_and_records_without_old_turn_authority() {
    let mut create = root_request("project-a", "ordinary-work", 1);
    create.acceptance_bindings = vec![AcceptanceBinding {
        criterion: 1,
        requirement: VerificationRequirement {
            check_kind: VerificationKind::Test,
            check_fingerprint: None,
        },
    }];
    let mut f =
        fixture_with_create_and_root(SqliteStore::open_in_memory().unwrap(), 0, &create, false);
    let before = history(&f);
    let binding = f.request.registration.binding.clone();
    let root = f
        .store
        .bind_named_root(
            &f.project,
            &f.session,
            &f.connection,
            HostSessionAuthority::Work,
            binding.claim_id,
            binding.claim_fence,
            "workspace-a",
            1,
            at(4),
            NamedRootBindingKind::Bound,
            None,
            &mut actor("runner"),
            "name",
            at(8),
        )
        .unwrap_or_else(|error| panic!("{error}"));
    assert_eq!(root.generation, 1);
    // The native run starts after the root was actually named, including
    // when earlier control history is already retained.
    f.request.registration.registered_at = at(8);
    f.request.run.started_at = at(8);
    f.request.checks[0].started_at = Some(at(8));
    f.store
        .read_named_root(
            &f.project,
            &f.session,
            &f.connection,
            HostSessionAuthority::Work,
            binding.run_id,
            binding.claim_id,
        )
        .unwrap();
    f.store
        .read_acceptance_bindings(
            &f.project,
            &f.session,
            &f.connection,
            HostSessionAuthority::Work,
            &crate::storage::BindingReadRequest {
                work_id: binding.work_id,
                expected_work_revision: binding.work_revision,
                run_id: binding.run_id,
                after: None,
            },
        )
        .unwrap();
    let receipt = f
        .store
        .record_registered_verification(
            &f.project,
            &f.session,
            &f.connection,
            HostSessionAuthority::Work,
            &actor("runner"),
            &f.request,
            at(10),
        )
        .unwrap();
    let snapshot = f.snapshot();
    assert_eq!(
        f.store
            .record_registered_verification(
                &f.project,
                &f.session,
                &f.connection,
                HostSessionAuthority::Work,
                &actor("runner"),
                &f.request,
                at(10),
            )
            .unwrap(),
        receipt
    );
    assert_eq!(snapshot, f.snapshot(), "exact retry is read-only");
    let run_cut = f
        .store
        .work_feed_head(&FeedId::RunExecution(binding.run_id))
        .unwrap();
    f.store
        .read_acceptance_verifications(
            &f.project,
            &f.session,
            &f.connection,
            HostSessionAuthority::Work,
            &crate::storage::VerificationReadRequest {
                work_id: binding.work_id,
                expected_work_revision: binding.work_revision,
                run_id: binding.run_id,
                run_cut,
                criterion: 1,
                after: None,
            },
        )
        .unwrap();
    assert_eq!(history(&f), before, "retained history changed");
    let report = f.store.verify_all().unwrap();
    assert!(report.is_healthy(), "{report:?}");
}

#[test]
fn work_session_receipts_survive_migration_without_a_control_session() {
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("source.db");
    let exported = directory.path().join("export.json");
    let destination = directory.path().join("imported.db");
    let mut f = ordinary_fixture(SqliteStore::open(&source).unwrap());
    let receipt = f
        .store
        .record_registered_verification(
            &f.project,
            &f.session,
            &f.connection,
            HostSessionAuthority::Work,
            &actor("runner"),
            &f.request,
            at(10),
        )
        .unwrap();
    drop(f.store);
    crate::storage::migration::export_json(&source, &exported).unwrap();
    crate::storage::migration::import_json(&exported, &destination).unwrap();
    f.store = SqliteStore::open(&destination).unwrap();
    f.connection = f.store.resume_work_connection(&f.session, at(11)).unwrap();
    assert_eq!(
        f.store
            .record_registered_verification(
                &f.project,
                &f.session,
                &f.connection,
                HostSessionAuthority::Work,
                &actor("runner"),
                &f.request,
                at(12)
            )
            .unwrap(),
        receipt
    );
    let report = f.store.verify_all().unwrap();
    assert!(report.is_healthy(), "{report:?}");
    assert_eq!(
        f.store
            .connection
            .query_row("SELECT count(*) FROM sqlite_schema WHERE type = 'table' AND name IN ('control_sessions', 'control_turn_results', 'control_turn_grants', 'control_turn_grant_supersessions')", [], |row| row
                .get::<_, i64>(0),)
            .unwrap(),
        0
    );
}

#[test]
fn work_session_replays_legacy_receipts_without_moving_or_rewriting_them() {
    let mut f = fixture();
    let old = f.record().unwrap();
    // Preserve an earlier receipt as passive stored history, with no grant,
    // control-session binding or executable legacy admission fixture.
    let scoped =
        serde_json::to_string(&(&f.project, &f.session, &f.request.operation_key)).unwrap();
    let inserted = f.store.connection.execute(
        "INSERT INTO control_operation_results
         (session_id, operation, idempotency_key, intent_hash, intent_json, result_json, created_at_ms)
         SELECT ?1, 'verification_record', ?2, request_hash,
             CAST(json_extract(CAST(result_json AS TEXT), '$.intent') AS BLOB),
             CAST(json_extract(CAST(result_json AS TEXT), '$.result') AS BLOB), ?3
         FROM work_operation_results WHERE operation = 'host_verification_record' AND idempotency_key = ?4",
        rusqlite::params![f.session.0, f.request.operation_key, at(10).timestamp_millis(), scoped],
    ).unwrap();
    assert_eq!(inserted, 1);
    assert_eq!(f.store.connection.execute(
        "DELETE FROM work_operation_results WHERE operation = 'host_verification_record' AND idempotency_key = ?1",
        [scoped],
    ).unwrap(), 1);

    f.connection = f.store.resume_work_connection(&f.session, at(11)).unwrap();
    let before = f.snapshot();
    assert_eq!(
        f.store
            .record_registered_verification(
                &f.project,
                &f.session,
                &f.connection,
                HostSessionAuthority::Work,
                &actor("runner"),
                &f.request,
                at(12)
            )
            .unwrap(),
        old
    );
    assert_eq!(f.snapshot(), before);
    f.request.run.command = "different command".into();
    assert!(matches!(
        f.store.record_registered_verification(
            &f.project,
            &f.session,
            &f.connection,
            HostSessionAuthority::Work,
            &actor("runner"),
            &f.request,
            at(13)
        ),
        Err(StoreError::ControlOperationIdempotencyConflict { .. })
    ));
    assert_eq!(f.snapshot(), before);
}

#[test]
fn work_session_wire_omits_control_routing_but_keeps_registered_credit_and_exact_retry() {
    let offset = (chrono::Utc::now() - at(10)).num_seconds();
    let f = fixture_offset(offset);
    let mut wire = serde_json::to_value(&f.request).unwrap();
    wire["operation"] = serde_json::json!("verification_record");
    let mut server =
        crate::HostControlServer::from_test_store(f.store, f.project, f.session, f.connection);
    let session = server
        .handle(
            serde_json::from_value(serde_json::json!({"operation": "work_session_read"})).unwrap(),
        )
        .unwrap();
    assert_eq!(session["work_session"], true);
    assert_eq!(
        session["capabilities"],
        serde_json::json!(["host_work_v1", "verification_record_v1"])
    );
    let first = server
        .handle(serde_json::from_value(wire.clone()).unwrap())
        .unwrap();
    let second = server
        .handle(serde_json::from_value(wire).unwrap())
        .unwrap();
    assert_eq!(first, second);
    assert_eq!(first["verifications"].as_array().unwrap().len(), 1);
    assert!(
        serde_json::from_value::<crate::HostControlRequest>(serde_json::json!({
            "operation": "turn_begin", "grant_id": "old-grant", "idempotency_key": "must-not-begin"
        }))
        .is_err()
    );
}

#[test]
fn work_session_root_naming_keeps_the_actual_holder_fence_and_project() {
    let mut f = fixture_with_create_and_root(
        SqliteStore::open_in_memory().unwrap(),
        0,
        &root_request("project-a", "ordinary-work", 1),
        false,
    );
    let binding = f.request.registration.binding.clone();
    let foreign = SessionId("another-host".into());
    let other_token = f.store.resume_work_connection(&foreign, at(7)).unwrap();
    for (project, session, token, fence) in [
        (
            f.project.clone(),
            f.session.clone(),
            f.connection.clone(),
            binding.claim_fence + 1,
        ),
        (f.project.clone(), foreign, other_token, binding.claim_fence),
        (
            ProjectId("another-project".into()),
            f.session.clone(),
            f.connection.clone(),
            binding.claim_fence,
        ),
    ] {
        let before = f.snapshot();
        assert!(
            f.store
                .bind_named_root(
                    &project,
                    &session,
                    &token,
                    HostSessionAuthority::Work,
                    binding.claim_id,
                    fence,
                    "workspace-a",
                    1,
                    at(4),
                    NamedRootBindingKind::Bound,
                    None,
                    &mut actor(&session.0),
                    "name",
                    at(8),
                )
                .is_err()
        );
        assert_eq!(f.snapshot(), before, "refused authority writes nothing");
    }
}

#[test]
fn work_session_reopen_fences_prior_connection_without_changing_passive_history() {
    let mut f = fixture();
    let before = history(&f);
    let previous = f.connection.clone();
    f.connection = f.store.resume_work_connection(&f.session, at(8)).unwrap();
    assert_eq!(history(&f), before, "work open changed old history");
    let binding = &f.request.registration.binding;
    assert!(matches!(
        f.store.read_named_root(
            &f.project,
            &f.session,
            &previous,
            HostSessionAuthority::Work,
            binding.run_id,
            binding.claim_id
        ),
        Err(StoreError::ControlConnectionSuperseded(_))
    ));
    f.store
        .record_registered_verification(
            &f.project,
            &f.session,
            &f.connection,
            HostSessionAuthority::Work,
            &actor("runner"),
            &f.request,
            at(10),
        )
        .unwrap();
}

#[test]
fn work_session_candidate_readiness_reads_a_copy_without_changing_it() {
    let directory = crate::test_support::temp_home().unwrap();

    let mut f = fixture();
    let before = history(&f);
    let copy = directory.path().join("ordinary-copy.db");
    f.store.backup_to(&copy).unwrap();
    let bytes = std::fs::read(&copy).unwrap();
    let ready = SqliteStore::readiness(&copy, None).unwrap();
    assert_eq!(
        ready.control.policy,
        SqliteStore::load_active_control_policy(&f.store.connection)
            .unwrap()
            .policy_id
    );
    assert_eq!(
        std::fs::read(&copy).unwrap(),
        bytes,
        "readiness is read-only"
    );
    f.store = SqliteStore::open(&copy).unwrap();
    assert_eq!(history(&f), before, "copy preserves history");
}
