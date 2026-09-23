use super::*;
use crate::domain::*;
use crate::storage::test_support::bind_control_for;

#[test]
fn control_conversion_merges_nondefault_epochs_independently_of_row_order() {
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("source.db");
    let file = directory.path().join("store.jsonl");
    populated_control(&source);
    let connection = Connection::open(&source).unwrap();
    preceding_control_tables(&connection);
    connection
        .execute("UPDATE task_control_state SET admission_epoch = 7", [])
        .unwrap();
    drop(connection);
    export_json(&source, &file).unwrap();
    let mut document: Vec<Json> = fs::read_to_string(&file)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    for reversed in [false, true] {
        let last = document.len() - 1;
        if reversed {
            document[1..last].reverse();
        }
        let reordered = directory.path().join(format!("order-{reversed}.jsonl"));
        fs::write(
            &reordered,
            document
                .iter()
                .map(Json::to_string)
                .collect::<Vec<_>>()
                .join("\n"),
        )
        .unwrap();
        let target = directory.path().join(format!("order-{reversed}.db"));
        import_json(&reordered, &target).unwrap();
        let converted = Connection::open(&target).unwrap();
        let epochs: Vec<i64> = converted
            .prepare("SELECT admission_epoch FROM control_anchors")
            .unwrap()
            .query_map([], |row| row.get(0))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        assert!(!epochs.is_empty());
        assert!(epochs.iter().all(|epoch| *epoch == 7));
        let obsolete: i64 = converted.query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name IN
             ('conversion_epochs', 'conversion_membership', 'task_control_state', 'task_participants', 'session_bindings')",
            [], |row| row.get(0),
        ).unwrap();
        assert_eq!(obsolete, 0);
    }
}

fn rewrite_json_rows(
    connection: &Connection,
    table: &str,
    column: &str,
    mut rewrite: impl FnMut(&mut Json),
) {
    let mut statement = connection
        .prepare(&format!("SELECT rowid, {column} FROM {table}"))
        .unwrap();
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    for (rowid, bytes) in rows {
        let mut value: Json = serde_json::from_slice(&bytes).unwrap();
        rewrite(&mut value);
        connection
            .execute(
                &format!("UPDATE {table} SET {column} = ?1 WHERE rowid = ?2"),
                rusqlite::params![crate::canonical::canonical_bytes(&value).unwrap(), rowid],
            )
            .unwrap();
    }
}

fn old_grant(grant: &mut Json) -> usize {
    let mut renamed = 0;
    if let Some(changes) = grant
        .pointer_mut("/delivery/delta/changes")
        .and_then(Json::as_array_mut)
    {
        for change in changes {
            let object = change.as_object_mut().unwrap();
            let id = object.remove("object_id").unwrap();
            object.insert("object_hash".into(), id);
            renamed += 1;
        }
    }
    if renamed != 0 {
        let digest = crate::CanonicalObject::freeze(&serde_json::json!({
            "context": grant["delivery"]["context"], "delta": grant["delivery"]["delta"],
        }))
        .unwrap();
        grant["delivery"]["page"]["content_digest"] = Json::from(digest.key().as_str());
        grant["basis"]["inline_delivery"]["content_digest"] = Json::from(digest.key().as_str());
    }
    grant["basis"]["leases"] = serde_json::json!([]);
    renamed
}

fn preceding_control_fields(connection: &Connection) {
    let mut renamed = 0;
    rewrite_json_rows(connection, "control_turn_grants", "grant_json", |grant| {
        renamed += old_grant(grant);
    });
    assert!(renamed > 0, "fixture must exercise a delivery delta");
    rewrite_json_rows(
        connection,
        "control_turn_results",
        "decision_json",
        |decision| {
            if let Some(grant) = decision.get_mut("grant") {
                old_grant(grant);
            }
        },
    );
    let mut statement = connection
        .prepare("SELECT sequence, decision_json FROM control_turn_results")
        .unwrap();
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    for (sequence, bytes) in rows {
        let value: Json = serde_json::from_slice(&bytes).unwrap();
        let frozen = crate::CanonicalObject::freeze(&value).unwrap();
        connection
            .execute(
                "UPDATE control_turn_results SET decision_hash = ?1 WHERE sequence = ?2",
                rusqlite::params![frozen.key().as_str(), sequence],
            )
            .unwrap();
    }
    rewrite_json_rows(
        connection,
        "control_turn_grant_supersessions",
        "supersession_json",
        |value| {
            let digest: String = connection.query_row(
            "SELECT decision_hash FROM control_turn_results WHERE session_id = ?1 AND idempotency_key = ?2",
            rusqlite::params![value["session_id"].as_str().unwrap(), value["replacement_request_key"].as_str().unwrap()],
            |row| row.get(0)).unwrap();
            value["replacement_decision"] = Json::from(digest);
        },
    );
    connection.execute("UPDATE control_turn_grant_supersessions SET replacement_decision_hash =
        (SELECT decision_hash FROM control_turn_results r WHERE r.session_id = control_turn_grant_supersessions.session_id
         AND r.idempotency_key = replacement_request_key)", []).unwrap();
    rewrite_json_rows(
        connection,
        "control_operation_results",
        "intent_json",
        |intent| {
            if let Some(entries) = intent
                .get_mut("verification_evidence")
                .and_then(Json::as_array_mut)
            {
                for entry in entries {
                    for field in ["producer_observation", "environment"] {
                        let reference = entry[field].as_object_mut().unwrap();
                        let id = reference.remove("object_id").unwrap();
                        reference.insert("object_hash".into(), id);
                        reference.insert("kind".into(), Json::from("object_hash"));
                    }
                }
            }
        },
    );
    let mut statement = connection
        .prepare("SELECT sequence, intent_json FROM control_operation_results")
        .unwrap();
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    for (sequence, bytes) in rows {
        let value: Json = serde_json::from_slice(&bytes).unwrap();
        let frozen = crate::CanonicalObject::freeze(&value).unwrap();
        connection
            .execute(
                "UPDATE control_operation_results SET intent_hash = ?1 WHERE sequence = ?2",
                rusqlite::params![frozen.key().as_str(), sequence],
            )
            .unwrap();
    }
}

#[test]
fn control_field_import_preserves_grants_supersessions_and_checkpoint_replays() {
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("source.db");
    let target = directory.path().join("target.db");
    let file = directory.path().join("store.jsonl");
    let binding = populated_control(&source);
    let now = DateTime::parse_from_rfc3339("2026-09-20T10:00:05Z")
        .unwrap()
        .with_timezone(&Utc);
    let mut store =
        SqliteStore::open_with_host_path_policy(&source, crate::HostPathPolicy::host_default())
            .unwrap();
    let turn = TurnIntent {
        idempotency_key: "migration-replacement".into(),
        intent_fingerprint: crate::ObjectId::from_canonical_bytes(b"migration-replacement"),
        purpose: TurnPurpose::Ordinary,
        requested_effects: vec![EffectClass::Observe],
        resource_intents: vec![],
    };
    let decision = store
        .evaluate_control_turn(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            &turn,
            now,
        )
        .unwrap();
    let ControlTurnDecision::Grant { ref grant } = decision else {
        panic!("saved grant");
    };
    let evidence = vec![VerificationEvidenceInput {
        producer_observation: ExecutionObservationReference::ObjectId {
            object_id: crate::ObjectId::mint(),
        },
        check_kind: VerificationKind::Test,
        environment: Some(EnvironmentEvidenceReference::ObjectId {
            object_id: crate::ObjectId::mint(),
        }),
        summary: None,
        refs: vec![],
    }];
    // An unbegun grant refuses before resolving the supplied evidence references.
    // This is a real persisted checkpoint intent and result, not a fabricated success.
    let checkpoint = store
        .checkpoint_control_turn_with_evidence(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            &grant.grant_id,
            TurnNextIntent::Continue,
            &[],
            &evidence,
            &[],
            "migration-checkpoint",
            now,
        )
        .unwrap();
    assert!(matches!(
        checkpoint,
        ControlTurnCheckpointDecision::Refuse { .. }
    ));
    assert!(store.verify_all().unwrap().is_healthy());
    drop(store);
    let expected = rows(&source);
    let connection = Connection::open(&source).unwrap();
    preceding_control_fields(&connection);
    export_json(&source, &file).unwrap();
    let report = import_json(&file, &target).unwrap();
    let actual = rows(&target);
    for (table, values) in &expected {
        if table != "work_schema_metadata" {
            assert_eq!(&actual[table], values, "{table}");
        }
    }
    for (table, field) in [
        (
            "control_turn_grants",
            "delivery.delta.changes[].object_hash",
        ),
        ("control_turn_grants", "basis.leases"),
        (
            "control_turn_results",
            "grant.delivery.delta.changes[].object_hash",
        ),
        ("control_turn_results", "grant.basis.leases"),
        ("control_turn_grant_supersessions", "replacement_decision"),
        (
            "control_operation_results",
            "verification_evidence[].{producer_observation,environment}.object_hash",
        ),
    ] {
        assert!(
            report
                .rewritten_fields
                .iter()
                .any(|entry| entry.table == table && entry.field == field && entry.values > 0),
            "{table}.{field}"
        );
    }
    let mut imported =
        SqliteStore::open_with_host_path_policy(&target, crate::HostPathPolicy::host_default())
            .unwrap();
    assert!(imported.verify_all().unwrap().is_healthy());
    assert_eq!(
        imported
            .evaluate_control_turn(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                &turn,
                now
            )
            .unwrap(),
        decision
    );
    assert_eq!(
        imported
            .checkpoint_control_turn_with_evidence(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                &grant.grant_id,
                TurnNextIntent::Continue,
                &[],
                &evidence,
                &[],
                "migration-checkpoint",
                now
            )
            .unwrap(),
        checkpoint
    );
    rewrite_json_rows(
        &connection,
        "control_turn_grant_supersessions",
        "supersession_json",
        |value| {
            value["replacement_decision"] = serde_json::json!(crate::ObjectId::mint());
        },
    );
    let invalid = directory.path().join("invalid.jsonl");
    export_json(&source, &invalid).unwrap();
    let refused = directory.path().join("refused.db");
    assert!(
        import_json(&invalid, &refused)
            .unwrap_err()
            .to_string()
            .contains("supersession replacement decision binding disagrees")
    );
    assert!(!refused.exists());
}

#[test]
fn historical_task_events_survive_conversion_and_rebinding_delivery() {
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("source.db");
    let target = directory.path().join("target.db");
    let file = directory.path().join("store.jsonl");
    let now = DateTime::parse_from_rfc3339("2026-09-20T10:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let mut store =
        SqliteStore::open_with_host_path_policy(&source, crate::HostPathPolicy::host_default())
            .unwrap();
    let creator = bind_control_for(
        &mut store,
        "historical-creator",
        "create",
        &[EffectClass::Observe],
        now,
    );
    let peer = bind_control_for(
        &mut store,
        "historical-peer",
        "join",
        &[EffectClass::Observe],
        now,
    );
    // Exact preceding-build event fields, kept as JSON after their types retire.
    let started = serde_json::json!({
        "schema_version": SCHEMA_VERSION, "task_id": creator.status.task_id,
        "project_id": "project-a", "title": "Exercise the host control lifecycle",
        "external_ref": "dummy:CONTROL-HOST-1", "participant": creator.status.session_id,
        "actor": actor("historical-creator"), "created_at": now,
    });
    let joined = serde_json::json!({
        "schema_version": SCHEMA_VERSION, "task_id": creator.status.task_id,
        "participant": peer.status.session_id, "actor": actor("historical-peer"),
        "created_at": now,
    });
    let (started_object, started_cursor) = store
        .append_task_object(creator.status.task_id, "task_started_event", &started)
        .unwrap();
    let (joined_object, joined_cursor) = store
        .append_task_object(creator.status.task_id, "task_joined_event", &joined)
        .unwrap();
    drop(store);
    let connection = Connection::open(&source).unwrap();
    preceding_control_tables(&connection);
    drop(connection);
    export_json(&source, &file).unwrap();
    import_json(&file, &target).unwrap();
    let mut imported =
        SqliteStore::open_with_host_path_policy(&target, crate::HostPathPolicy::host_default())
            .unwrap();
    assert!(imported.verify_all().unwrap().is_healthy());
    assert_eq!(
        imported.get::<Json>(started_object.key()).unwrap(),
        Some(started)
    );
    assert_eq!(
        imported.get::<Json>(joined_object.key()).unwrap(),
        Some(joined.clone())
    );
    let rebound = bind_control_for(
        &mut imported,
        "historical-creator",
        "rebind",
        &[EffectClass::Observe],
        now + chrono::Duration::seconds(1),
    );
    assert_eq!(
        rebound.status.confirmed_cursor, started_cursor,
        "skip only the own start event"
    );
    let grant = crate::storage::test_support::complete_control_turn(
        &mut imported,
        &rebound,
        "historical-peer-delivery",
        vec![EffectClass::Observe],
        vec![],
        now + chrono::Duration::seconds(2),
    );
    let delivery = grant.delivery.unwrap();
    assert_eq!(delivery.page.from_cursor, started_cursor);
    assert_eq!(delivery.delta.changes.len(), 1);
    let change = &delivery.delta.changes[0];
    assert_eq!(change.cursor, joined_cursor);
    assert_eq!(change.object_kind, "task_joined_event");
    assert_eq!(&change.object_id, joined_object.key());
    assert_eq!(change.object, joined);
    assert!(imported.verify_all().unwrap().is_healthy());
}

#[test]
fn direct_binding_import_refuses_unrepresentable_state_and_table_collisions() {
    for (case, sql, expected) in [
        (
            "inactive",
            "UPDATE tasks SET state = 'completed'",
            "tasks.state is not the supported active state",
        ),
        (
            "orphan",
            "UPDATE task_control_state SET task_id = 'missing-anchor'",
            "task_control_state references a missing control anchor",
        ),
        (
            "invalid-epoch",
            "UPDATE task_control_state SET admission_epoch = 0",
            "task_control_state lacks a positive admission_epoch",
        ),
        (
            "duplicate-epoch",
            "INSERT INTO task_control_state SELECT * FROM task_control_state",
            "task_control_state contains duplicate task_id rows",
        ),
        (
            "overlap",
            "CREATE TABLE control_anchors AS SELECT task_id, project_id, external_ref, title FROM tasks",
            "multiple source tables map to control_anchors",
        ),
        (
            "missing-participant",
            "DELETE FROM task_participants",
            "without matching task_participants and session_bindings rows",
        ),
        (
            "missing-binding",
            "DELETE FROM session_bindings",
            "without matching task_participants and session_bindings rows",
        ),
        (
            "wrong-participant",
            "UPDATE task_participants SET session_id = 'another-session'",
            "without matching task_participants and session_bindings rows",
        ),
        (
            "missing-rosters",
            "DROP TABLE task_participants; DROP TABLE session_bindings",
            "without matching task_participants and session_bindings rows",
        ),
    ] {
        let directory = crate::test_support::temp_home().unwrap();
        let source = directory.path().join("source.db");
        let target = directory.path().join("refused.db");
        let file = directory.path().join("store.jsonl");
        populated_control(&source);
        let connection = Connection::open(&source).unwrap();
        preceding_control_tables(&connection);
        connection.execute_batch(sql).unwrap();
        drop(connection);
        export_json(&source, &file).unwrap();
        let error = import_json(&file, &target).unwrap_err().to_string();
        assert!(error.contains(expected), "{case}: {error}");
        assert!(!target.exists());
    }
}

#[test]
fn checkpoint_reference_import_refuses_missing_ids_and_colliding_spellings() {
    for field in ["producer_observation", "environment"] {
        for (case, reference, expected) in [
            (
                "missing",
                serde_json::json!({"kind": "object_hash"}),
                "checkpoint object_hash reference lacks its object_hash field",
            ),
            (
                "collision",
                serde_json::json!({
                    "kind": "object_hash", "object_hash": crate::ObjectId::mint(),
                    "object_id": crate::ObjectId::mint(),
                }),
                "stored reply contains both object_hash and object_id",
            ),
        ] {
            let directory = crate::test_support::temp_home().unwrap();
            let source = directory.path().join("source.db");
            let target = directory.path().join("refused.db");
            let file = directory.path().join("store.jsonl");
            populated_control(&source);
            let connection = Connection::open(&source).unwrap();
            let (sequence, bytes): (i64, Vec<u8>) = connection
                .query_row(
                    "SELECT sequence, intent_json FROM control_operation_results
                 WHERE operation = 'turn_checkpoint' ORDER BY sequence LIMIT 1",
                    [],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .unwrap();
            let mut intent: Json = serde_json::from_slice(&bytes).unwrap();
            let mut evidence = serde_json::json!({"check_kind": "test"});
            evidence[field] = reference;
            intent["verification_evidence"] = serde_json::json!([evidence]);
            let frozen = crate::CanonicalObject::freeze(&intent).unwrap();
            connection.execute(
                "UPDATE control_operation_results SET intent_json = ?1, intent_hash = ?2 WHERE sequence = ?3",
                rusqlite::params![frozen.bytes(), frozen.key().as_str(), sequence],
            ).unwrap();
            drop(connection);
            export_json(&source, &file).unwrap();
            let error = import_json(&file, &target).unwrap_err();
            assert!(
                matches!(&error, MigrationError::Refused(reason) if reason == expected),
                "{field}, {case}: {error}"
            );
            assert!(!target.exists());
        }
    }
}

#[test]
fn control_field_import_refuses_missing_or_malformed_delivery_digests() {
    for (table, column) in [
        ("control_turn_grants", "grant_json"),
        ("control_turn_results", "decision_json"),
    ] {
        for malformed in [
            serde_json::json!({}),
            serde_json::json!([]),
            serde_json::json!("not-an-object"),
            Json::Null,
            serde_json::json!({"content_digest": null}),
            serde_json::json!({"content_digest": 42}),
        ] {
            let directory = crate::test_support::temp_home().unwrap();
            let source = directory.path().join("source.db");
            let target = directory.path().join("refused.db");
            let file = directory.path().join("store.jsonl");
            populated_control(&source);
            let connection = Connection::open(&source).unwrap();
            preceding_control_fields(&connection);
            let mut changed = 0;
            rewrite_json_rows(&connection, table, column, |value| {
                let grant = if table == "control_turn_results" {
                    let Some(grant) = value.get_mut("grant") else {
                        return;
                    };
                    grant
                } else {
                    value
                };
                if grant
                    .pointer("/delivery/delta/changes/0/object_hash")
                    .is_some()
                {
                    grant["delivery"]["page"] = malformed.clone();
                    grant["basis"]["inline_delivery"] = malformed.clone();
                    changed += 1;
                }
            });
            assert!(changed > 0, "{table}: fixture must reach the rewrite");
            drop(connection);
            export_json(&source, &file).unwrap();
            let before = rows(&source);
            let error = import_json(&file, &target).unwrap_err();
            assert!(
                matches!(&error, MigrationError::Refused(reason)
                    if reason == "control grant /delivery/page/content_digest must be an existing string"),
                "{table}, {malformed}: {error}"
            );
            assert!(!target.exists());
            assert_eq!(rows(&source), before);
        }
    }
}

#[test]
fn historical_control_operation_receipts_import_unchanged() {
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("source.db");
    let target = directory.path().join("target.db");
    let file = directory.path().join("store.jsonl");
    let binding = populated_control(&source);
    let connection = Connection::open(&source).unwrap();
    let bind_intent: String = connection
        .query_row(
            "SELECT bind_intent_hash FROM control_sessions WHERE session_id = ?1",
            [&binding.status.session_id.0],
            |row| row.get(0),
        )
        .unwrap();
    let definition = crate::ObjectId::mint();
    let obligation = uuid::Uuid::new_v4();
    // Preceding-build receipt shapes, retained as history rather than new authority.
    for (operation, mut intent, result) in [
        (
            "lease_acquire",
            serde_json::json!({
                "fingerprint_schema_version": 1, "bind_intent_hash": bind_intent,
                "kind": "execution", "mode": "exclusive",
                "subject": {"kind": "logical", "namespace": "historical",
                    "segments": ["resource"], "coverage": "exact"}, "ttl_seconds": 60
            }),
            serde_json::json!({"decision": "refuse", "directive": {
                "directive_id": "historical-policy-refusal", "code": "capability_not_permitted",
                "target": "host", "satisfaction": "host_transition", "recovery_effects": ["observe"]
            }}),
        ),
        (
            "lease_release",
            serde_json::json!({
                "control_schema_version": CONTROL_SCHEMA_VERSION, "lease_id": "historical-lease"
            }),
            serde_json::json!({
                "lease_id": "historical-lease", "task_id": binding.status.task_id,
                "holder": binding.status.session_id, "fence": 1, "cursor": 1,
                "released_at": "2026-09-20T10:00:00Z"
            }),
        ),
        (
            "obligation_waive",
            serde_json::json!({
                "control_schema_version": CONTROL_SCHEMA_VERSION, "bind_intent_hash": bind_intent,
                "obligation_id": obligation, "expected_definition": definition,
                "waived_by": "historical-operator", "reason": "historical waiver request"
            }),
            serde_json::json!({
                "decision": "refused", "code": "waiver_not_admitted", "obligation_id": obligation,
                "current_definition": definition,
                "remedy": "bind the host control session to the live claim for this obligation run"
            }),
        ),
    ] {
        let key = format!("historical-{operation}");
        intent["session_id"] = serde_json::json!(binding.status.session_id);
        intent["idempotency_key"] = serde_json::json!(key);
        let frozen = crate::CanonicalObject::freeze(&intent).unwrap();
        connection.execute(
            "INSERT INTO control_operation_results (session_id, operation, idempotency_key,
             intent_hash, intent_json, result_json, created_at_ms) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![binding.status.session_id.0, operation, key, frozen.key().as_str(),
                frozen.bytes(), crate::canonical::canonical_bytes(&result).unwrap(), 1_790_000_000_000_i64],
        ).unwrap();
    }
    drop(connection);
    let before = rows(&source)["control_operation_results"].clone();
    export_json(&source, &file).unwrap();
    import_json(&file, &target).unwrap();
    assert_eq!(rows(&target)["control_operation_results"], before);
    let imported =
        SqliteStore::open_with_host_path_policy(&target, crate::HostPathPolicy::host_default())
            .unwrap();
    assert!(imported.verify_all().unwrap().is_healthy());
}

#[test]
fn historical_lease_refusal_imports_and_replays_without_lease_authority() {
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("source.db");
    let target = directory.path().join("target.db");
    let file = directory.path().join("store.jsonl");
    populated(&source);
    let mut store =
        SqliteStore::open_with_host_path_policy(&source, crate::HostPathPolicy::host_default())
            .unwrap();
    let now = DateTime::parse_from_rfc3339("2026-09-20T10:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let binding = bind_control_for(
        &mut store,
        "historical-session",
        "historical-bind",
        &[EffectClass::Observe, EffectClass::MutateLocal],
        now,
    );
    let intent = TurnIntent {
        idempotency_key: "unleased-mutation".into(),
        intent_fingerprint: crate::ObjectId::mint(),
        purpose: TurnPurpose::Ordinary,
        requested_effects: vec![EffectClass::MutateLocal],
        resource_intents: vec![],
    };
    // Shape written by the preceding evaluator, not a current lease producer.
    let decision = serde_json::json!({"decision": "refuse", "directive": {
        "directive_id": "unleased-mutation:lease_required", "code": "lease_required",
        "target": "host", "satisfaction": "host_transition", "recovery_effects": ["observe"]
    }});
    let saved_intent = crate::CanonicalObject::freeze(&serde_json::json!({
        "control_schema_version": CONTROL_SCHEMA_VERSION,
        "session_id": binding.status.session_id,
        "task_id": binding.status.task_id,
        "intent": intent
    }))
    .unwrap();
    let saved_decision = crate::CanonicalObject::freeze(&decision).unwrap();
    drop(store);
    let connection = Connection::open(&source).unwrap();
    connection
        .execute(
            "INSERT INTO control_turn_results (session_id, task_id, idempotency_key,
         intent_hash, intent_json, decision_hash, decision_json, created_at_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                binding.status.session_id.0,
                binding.status.task_id.0.to_string(),
                intent.idempotency_key,
                saved_intent.key().as_str(),
                saved_intent.bytes(),
                saved_decision.key().as_str(),
                saved_decision.bytes(),
                now.timestamp_millis()
            ],
        )
        .unwrap();
    connection
        .execute_batch(
            "CREATE TABLE control_work_leases (
        lease_id TEXT PRIMARY KEY, task_id TEXT, holder_session_id TEXT,
        lease_json BLOB, state TEXT, expires_at_ms INTEGER, lease_hash TEXT
    ) STRICT;",
        )
        .unwrap();
    drop(connection);
    let before = rows(&source)["control_turn_results"].clone();
    export_json(&source, &file).unwrap();
    let report = import_json(&file, &target).unwrap();
    assert!(
        report
            .left_out
            .iter()
            .any(|entry| entry.name == "control_work_leases" && entry.rows == 0)
    );
    assert_eq!(rows(&target)["control_turn_results"], before);
    let mut imported =
        SqliteStore::open_with_host_path_policy(&target, crate::HostPathPolicy::host_default())
            .unwrap();
    assert!(imported.verify_all().unwrap().is_healthy());
    let replay = imported
        .evaluate_control_turn(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            &intent,
            now + chrono::Duration::seconds(1),
        )
        .unwrap();
    assert_eq!(serde_json::to_value(replay).unwrap(), decision);
    assert_eq!(rows(&target)["control_turn_results"], before);
}
