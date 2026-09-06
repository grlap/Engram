use super::*;

#[test]
fn decomposition_correction_concurrent_finish_refusal_is_agent_safe() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("work.db");
    let writer = service(&database);
    let parent = proposed_root(
        writer
            .work_propose(root_input("Parent", "root"), at(0))
            .unwrap(),
    );
    let input = child_input("Pending child");
    let request = pending_request(&writer, &input, at(1));
    let peer = service(&database);
    peer.work_propose(child_input("Revision advance"), at(2))
        .unwrap();
    let mut store = SqliteStore::open(&database).unwrap();
    let current = writer
        .protocol_basis(&store, true, false, None, at(3))
        .unwrap();
    let intent = writer.protocol_intent(&input);
    let key = writer
        .effective_idempotency_key(
            "",
            crate::storage::DECOMPOSE_PROTOCOL_OPERATION,
            &current,
            &intent,
            at(3),
        )
        .unwrap();
    let attempt = store
        .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
            project_id: &writer.project_id,
            session_id: &writer.session_id,
            operation: crate::storage::DECOMPOSE_PROTOCOL_OPERATION,
            idempotency_key: &key,
            intent: &intent,
            basis: &current,
            now: at(3),
        })
        .unwrap();
    assert!(!attempt.basis_matches);
    assert!(attempt.result.is_none());
    assert!(
        store
            .work_operation_result_value("decompose_work", &request.idempotency_key)
            .unwrap()
            .is_none()
    );
    let stored = attempt.basis.unwrap();
    super::super::super::replay::guard_decomposition_retry(&stored, &current, None).unwrap();
    // Another connection refreshes and finishes after this caller's reads,
    // before it attempts the exact production pending-basis CAS.
    let completed = peer.work_propose(input.clone(), at(3)).unwrap();
    let connection = rusqlite::Connection::open(&database).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
    let error = writer
        .refresh_decomposition_retry_basis(&mut store, &key, &stored, &current)
        .unwrap_err();
    let value = crate::mcp::store_error_value(&error);
    assert_eq!(value["error"]["code"], "work_decomposition_retry_conflict");
    assert_eq!(value["error"]["details"]["parent_ref"], parent.short_ref);
    assert_eq!(
        value["error"]["details"]["reason"],
        "the original attempt changed or completed concurrently"
    );
    let verb = crate::verbs::VerbError::from(error);
    let guidance = verb.guidance();
    assert_eq!(
        guidance.next,
        vec![format!("engram work show {}", parent.short_ref)]
    );
    assert_eq!(
        guidance.reminders,
        vec![crate::storage::DECOMPOSITION_RETRY_REMEDY]
    );
    let rendered = format!("{verb} {value} {guidance:?}");
    assert!(!rendered.contains(&key));
    assert!(!rendered.contains("auto:"));
    assert!(!rendered.contains("idempotency"));
    assert!(
        !rendered
            .as_bytes()
            .windows(64)
            .any(|bytes| bytes.iter().all(u8::is_ascii_hexdigit))
    );
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&connection).unwrap(),
        before
    );
    let replay = writer.work_propose(input, at(4)).unwrap();
    assert_eq!(child_id(&replay), child_id(&completed));
    assert_eq!(
        store
            .work_observation_tail(child_id(&completed), 10)
            .unwrap()
            .0,
        1
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn decomposition_retry_guard_preserves_non_revision_work_and_authority_fields() {
    let directory = crate::test_support::temp_home().unwrap();
    let writer = service(&directory.path().join("work.db"));
    writer
        .work_propose(root_input("Parent", "root"), at(0))
        .unwrap();
    writer
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "claim".into(),
            },
            at(1),
        )
        .unwrap();
    let store = writer.store_at(at(2)).unwrap();
    let mut basis = writer
        .protocol_basis(&store, true, false, None, at(2))
        .unwrap();
    basis.focused_work.as_mut().unwrap().external_ref = Some("planner:original".into());
    let stored = serde_json::to_value(&basis).unwrap();
    let mut allowed = basis.clone();
    let work = allowed.focused_work.as_mut().unwrap();
    work.revision += 1;
    work.updated_at = at(3);
    let claim = allowed.claim.as_mut().unwrap();
    claim.accepted_work_revision += 1;
    claim.revision += 1;
    claim.expires_at = at(500);
    let guard = super::super::super::replay::guard_decomposition_retry;
    guard(&stored, &allowed, None).unwrap();
    let other = WorkId::new();
    let run = WorkRunId::new();
    let mut classified =
        std::collections::BTreeMap::<&str, std::collections::BTreeSet<&str>>::from([
            (
                "focused_work",
                ["revision", "updated_at"].into_iter().collect(),
            ),
            (
                "claim",
                ["revision", "expires_at", "accepted_work_revision"]
                    .into_iter()
                    .collect(),
            ),
        ]);
    let mut actor = stored["focused_work"]["created_by"].clone();
    actor["actor_id"] = serde_json::json!("other");
    let source_hash = CanonicalObject::freeze(&serde_json::json!({"source": "other"})).unwrap();
    for (section, field, value) in [
        (
            "focused_work",
            "schema_version",
            serde_json::json!(basis.focused_work.as_ref().unwrap().schema_version + 1),
        ),
        ("focused_work", "project_id", serde_json::json!("other")),
        ("focused_work", "work_id", serde_json::json!(other)),
        ("focused_work", "short_ref", serde_json::json!("w-other")),
        ("focused_work", "root_id", serde_json::json!(other)),
        ("focused_work", "parent_id", serde_json::json!(other)),
        (
            "focused_work",
            "child_requirement",
            serde_json::json!("optional"),
        ),
        ("focused_work", "kind", serde_json::json!("bug")),
        ("focused_work", "deferred_until", serde_json::json!(at(700))),
        ("focused_work", "superseded_by", serde_json::json!(other)),
        ("focused_work", "origin", serde_json::json!("imported")),
        (
            "focused_work",
            "source_snapshot_id",
            serde_json::json!(source_hash.hash()),
        ),
        ("focused_work", "created_by", actor),
        ("focused_work", "created_at", serde_json::json!(at(900))),
        ("focused_work", "title", serde_json::json!("Other title")),
        (
            "focused_work",
            "outcome",
            serde_json::json!("Other outcome"),
        ),
        (
            "focused_work",
            "acceptance",
            serde_json::json!(["Other criterion"]),
        ),
        ("focused_work", "priority", serde_json::json!(4)),
        ("focused_work", "labels", serde_json::json!(["other"])),
        (
            "focused_work",
            "external_ref",
            serde_json::json!("planner:other"),
        ),
        ("focused_work", "assigned_to", serde_json::json!("other")),
        ("focused_work", "lifecycle", serde_json::json!("cancelled")),
        ("focused_work", "active_run_id", serde_json::json!(run)),
        ("focused_work", "restored", serde_json::json!(true)),
        ("claim", "claim_id", serde_json::json!(WorkClaimId::new())),
        ("claim", "work_id", serde_json::json!(other)),
        ("claim", "run_id", serde_json::json!(run)),
        ("claim", "holder", serde_json::json!("other")),
        (
            "claim",
            "fence",
            serde_json::json!(basis.claim.as_ref().unwrap().fence + 1),
        ),
        ("claim", "state", serde_json::json!("released")),
    ] {
        let mut changed = serde_json::to_value(&allowed).unwrap();
        assert_ne!(
            changed[section][field], value,
            "{section}.{field} must actually change"
        );
        assert!(classified.get_mut(section).unwrap().insert(field));
        changed[section][field] = value;
        let current = serde_json::from_value(changed).unwrap();
        assert!(
            matches!(
                guard(&stored, &current, None),
                Err(StoreError::WorkDecompositionRetryConflict { .. })
            ),
            "{section}.{field}"
        );
    }
    for (section, fields) in classified {
        assert_eq!(
            fields,
            stored[section]
                .as_object()
                .unwrap()
                .keys()
                .map(String::as_str)
                .collect(),
            "each {section} field must be classified as compared or normalized"
        );
    }
    allowed.claim = None;
    assert!(matches!(
        guard(&stored, &allowed, None),
        Err(StoreError::WorkDecompositionRetryConflict { .. })
    ));
}

#[test]
fn decomposition_retry_conflicts_on_changed_parent_instead_of_creating_a_new_key() {
    for stage in 0..=2 {
        let directory = crate::test_support::temp_home().unwrap();
        let database = directory.path().join("work.db");
        let writer = service(&database);
        writer
            .work_propose(root_input("Parent", "root"), at(0))
            .unwrap();
        let input = child_input("Child");
        if stage == 2 {
            writer.work_propose(input.clone(), at(1)).unwrap();
        } else {
            let request = pending_request(&writer, &input, at(1));
            if stage == 1 {
                SqliteStore::open(&database)
                    .unwrap()
                    .decompose_work(&request, &DevelopmentNoopRedactor)
                    .unwrap();
            }
        }
        writer
            .work_update(
                WorkUpdateInput::Revise {
                    patch: WorkRevisionPatch {
                        title: Some("Changed parent".into()),
                        ..Default::default()
                    },
                    idempotency_key: "revise".into(),
                },
                at(2),
            )
            .unwrap();
        let connection = rusqlite::Connection::open(&database).unwrap();
        let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
        for now in [at(3), at(4)] {
            assert!(matches!(writer.work_propose(input.clone(), now),
                Err(StoreError::WorkDecompositionRetryConflict { reason, .. })
                    if reason == "the parent planning state changed since the first attempt"));
        }
        let pending: i64 = connection
            .query_row(
                "SELECT count(*) FROM work_protocol_attempts WHERE result_json IS NULL",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            pending,
            i64::from(stage != 2),
            "strict refusal must not finish an interrupted attempt"
        );
        assert_eq!(
            crate::storage::test_database_shape_snapshot(&connection).unwrap(),
            before
        );
    }
}

#[test]
fn decomposition_correction_absent_completed_basis_is_healthy_but_cannot_replay() {
    for caller_key in ["", "explicit-decomposition"] {
        let directory = crate::test_support::temp_home().unwrap();
        let database = directory.path().join("work.db");
        let writer = service(&database);
        writer
            .work_propose(root_input("Parent", "root"), at(0))
            .unwrap();
        let mut input = child_input("Child");
        let WorkProposeInput::Decompose {
            idempotency_key, ..
        } = &mut input
        else {
            unreachable!()
        };
        *idempotency_key = caller_key.into();
        writer.work_propose(input.clone(), at(1)).unwrap();
        let mut store = SqliteStore::open(&database).unwrap();
        let connection = rusqlite::Connection::open(&database).unwrap();
        let basis: Vec<u8> = connection.query_row(
        "SELECT basis_json FROM work_protocol_attempts WHERE operation = 'work_propose:decompose'",
        [], |row| row.get(0)).unwrap();
        let root_basis: Option<Vec<u8>> = connection
        .query_row(
            "SELECT basis_json FROM work_protocol_attempts WHERE operation = 'work_propose:root'",
            [],
            |row| row.get(0),
        )
        .unwrap();
        assert!(root_basis.is_none());
        assert!(store.verify_all().unwrap().is_healthy());
        connection.execute("UPDATE work_protocol_attempts SET basis_json = NULL WHERE operation = 'work_propose:decompose'", []).unwrap();
        assert!(
            store.verify_all().unwrap().is_healthy(),
            "completed attempts without retained bases are valid data"
        );
        let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
        let current = writer
            .protocol_basis(&store, true, false, None, at(2))
            .unwrap();
        let intent = writer.protocol_intent(&input);
        let key = writer
            .effective_idempotency_key(
                caller_key,
                crate::storage::DECOMPOSE_PROTOCOL_OPERATION,
                &current,
                &intent,
                at(2),
            )
            .unwrap();
        assert!(matches!(
            store.begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
                project_id: &writer.project_id,
                session_id: &writer.session_id,
                operation: crate::storage::DECOMPOSE_PROTOCOL_OPERATION,
                idempotency_key: &key,
                intent: &intent,
                basis: &current,
                now: at(2),
            }),
            Err(StoreError::WorkOperationIdempotencyConflict { .. })
        ));
        let refusal = writer.work_propose(input.clone(), at(2)).unwrap_err();
        if caller_key.is_empty() {
            assert!(matches!(
                refusal,
                StoreError::WorkDecompositionRetryConflict { .. }
            ));
        } else {
            assert!(matches!(
                refusal,
                StoreError::WorkOperationIdempotencyConflict { .. }
            ));
        }
        assert_eq!(
            crate::storage::test_database_shape_snapshot(&connection).unwrap(),
            before
        );
        connection.execute("UPDATE work_protocol_attempts SET basis_json = ?1 WHERE operation = 'work_propose:decompose'", [&basis]).unwrap();
        assert!(store.verify_all().unwrap().is_healthy());
        let root = CanonicalObject::freeze(&WorkProtocolBasis {
            focused_work: None,
            claim: None,
            handoffs: Vec::new(),
        })
        .unwrap();
        connection.execute("UPDATE work_protocol_attempts SET basis_json = ?1 WHERE operation = 'work_propose:root'", [root.bytes()]).unwrap();
        assert!(
            store.verify_all().unwrap().is_healthy(),
            "retention is a write policy; integrity verifies any retained basis"
        );
        connection.execute("UPDATE work_protocol_attempts SET basis_json = NULL WHERE operation = 'work_propose:root'", []).unwrap();
        connection.execute("UPDATE work_protocol_attempts SET basis_json = ?1 WHERE operation = 'work_propose:decompose'", [b"{}".as_slice()]).unwrap();
        assert!(
            !store.verify_all().unwrap().is_healthy(),
            "retained basis must verify against its hash"
        );
        connection.execute("UPDATE work_protocol_attempts SET basis_json = ?1 WHERE operation = 'work_propose:decompose'", [&basis]).unwrap();
        writer.work_propose(input, at(3)).unwrap();
        assert!(store.verify_all().unwrap().is_healthy());
    }
}
