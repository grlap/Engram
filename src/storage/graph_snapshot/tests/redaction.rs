//! Coverage for restricted and secret-reference project memory: typed
//! redaction on save, widened disclosure, restored-placeholder identity,
//! and audit trail invariants for graph snapshot save/load.

use super::*;

#[test]
fn graph_save_and_doctor_reject_invalid_keyed_memory_shape() {
    let project = ProjectId("snapshot-keyed-memory-shape".into());
    for case in ["valid", "missing-tag", "wrong-classification-reason"] {
        let mut store = SqliteStore::open_in_memory().expect("isolated store");
        let mut version = classified_project_memory(
            &project,
            "shape-entry",
            "planning detail",
            Sensitivity::Internal,
            at(1),
        );
        match case {
            "missing-tag" => version.tags.clear(),
            "wrong-classification-reason" => version.classification_reason.clear(),
            _ => {}
        }
        // Freeze fresh canonical bytes and bind both hashes and the head to them.
        // Neither changed field is projected, so this is not projection drift.
        insert_project_memory_version(&mut store, &project, &version);
        let integrity = store.verify_all().expect("doctor scan");
        let saved = store.save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(2),
            &DevelopmentNoopRedactor,
        );
        if case == "valid" {
            assert!(integrity.is_healthy(), "{integrity:?}");
            assert_eq!(
                saved
                    .expect("valid shape saves")
                    .document
                    .body
                    .memories
                    .len(),
                1
            );
        } else {
            assert!(
                integrity
                    .invalid_objects
                    .contains(&format!("memory_head:{}", version.memory_id.0)),
                "{case}: {integrity:?}"
            );
            assert!(
                integrity
                    .invalid_objects
                    .iter()
                    .all(|entry| ObjectId::from_stored(entry.clone()).is_none()),
                "{case}: canonical hashes must remain valid: {integrity:?}"
            );
            assert!(
                matches!(
                    saved,
                    Err(StoreError::InvalidMemoryProjection(ref message))
                        if message == "keyed project memory has invalid canonical shape: version fields do not match the fixed project-episode contract"
                ),
                "{case}: {saved:?}"
            );
        }
        assert_eq!(
            store
                .work_graph_snapshot_save_audits(&project)
                .expect("save audits")
                .len(),
            usize::from(case == "valid"),
            "{case}: refused saves must not record disclosure"
        );
    }
}

#[test]
fn restricted_memory_is_typed_redaction_while_secret_reference_is_retained() {
    let project = ProjectId("snapshot-sensitive-memory".into());
    let restricted = classified_project_memory(
        &project,
        "restricted-entry",
        "restricted planning detail",
        Sensitivity::Restricted,
        at(1),
    );
    let (default_restricted_state, default_was_redacted) =
        snapshot_active_memory(restricted.clone(), false);
    let WorkGraphSnapshotMemoryState::Active {
        body: default_restricted,
        ..
    } = default_restricted_state
    else {
        panic!("restricted fixture must be active");
    };
    assert!(default_was_redacted);
    assert_eq!(
        default_restricted,
        WorkGraphSnapshotText::Redacted {
            sensitivity: Sensitivity::Restricted
        }
    );
    let (widened_restricted_state, widened_was_redacted) = snapshot_active_memory(restricted, true);
    let WorkGraphSnapshotMemoryState::Active {
        body: widened_restricted,
        ..
    } = widened_restricted_state
    else {
        panic!("widened restricted fixture must be active");
    };
    assert!(!widened_was_redacted);
    assert_eq!(
        widened_restricted,
        WorkGraphSnapshotText::Present {
            value: "restricted planning detail".into()
        }
    );
    let secret_reference = classified_project_memory(
        &project,
        "secret-reference",
        "vault://engram/snapshot-secret",
        Sensitivity::SecretRef,
        at(2),
    );
    for widened in [false, true] {
        let (state, was_redacted) = snapshot_active_memory(secret_reference.clone(), widened);
        assert!(!was_redacted);
        let WorkGraphSnapshotMemoryState::Active { body, .. } = state else {
            panic!("secret reference fixture must be active");
        };
        assert_eq!(
            body,
            WorkGraphSnapshotText::Present {
                value: "vault://engram/snapshot-secret".into()
            }
        );
    }
}

#[test]
fn saved_snapshot_redacts_restricted_memory_and_carries_secret_reference_verbatim() {
    let directory = crate::test_support::temp_home().expect("tempdir");
    let mut store = SqliteStore::open(directory.path().join("engram.db")).expect("store");
    let project = ProjectId("snapshot-sensitive-memory-save".into());
    insert_classified_project_memory(
        &mut store,
        &project,
        "restricted-entry",
        "restricted planning detail",
        Sensitivity::Restricted,
        at(1),
    );
    insert_classified_project_memory(
        &mut store,
        &project,
        "secret-reference",
        "writer-asserted opaque reference",
        Sensitivity::SecretRef,
        at(2),
    );

    let default = store
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(3),
            &DevelopmentNoopRedactor,
        )
        .expect("default snapshot");
    let widened = store
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            Some("restore restricted planning context"),
            WorkGraphSnapshotDestinationKind::Stdout,
            at(4),
            &DevelopmentNoopRedactor,
        )
        .expect("widened snapshot");

    assert_eq!(default.document.body.summary.redacted.memories, 1);
    assert_eq!(widened.document.body.summary.redacted.memories, 0);
    assert!(matches!(
        &default.document.body.memories[0].state,
        WorkGraphSnapshotMemoryState::Active {
            body: WorkGraphSnapshotText::Redacted {
                sensitivity: Sensitivity::Restricted
            },
            ..
        }
    ));
    assert!(matches!(
        &widened.document.body.memories[0].state,
        WorkGraphSnapshotMemoryState::Active {
            body: WorkGraphSnapshotText::Present { value },
            ..
        } if value == "restricted planning detail"
    ));
    for snapshot in [&default, &widened] {
        assert!(matches!(
            &snapshot.document.body.memories[1].state,
            WorkGraphSnapshotMemoryState::Active {
                body: WorkGraphSnapshotText::Present { value },
                sensitivity: Sensitivity::SecretRef,
                ..
            } if value == "writer-asserted opaque reference"
        ));
    }
    let audits = store
        .work_graph_snapshot_save_audits(&project)
        .expect("sensitive snapshot audits");
    assert_eq!(audits[0].redacted.memories, 1);
    assert_eq!(audits[1].redacted.memories, 0);
}

#[test]
fn restored_redacted_memory_stays_typed_when_a_later_save_is_widened() {
    let directory = crate::test_support::temp_home().expect("tempdir");
    let project = ProjectId("snapshot-restored-redaction".into());
    let mut source = SqliteStore::open(directory.path().join("source.db")).expect("source store");
    insert_classified_project_memory(
        &mut source,
        &project,
        "restricted-entry",
        "restricted planning detail",
        Sensitivity::Restricted,
        at(1),
    );
    let saved = source
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(2),
            &DevelopmentNoopRedactor,
        )
        .expect("save redacted source memory");
    assert_eq!(saved.document.body.summary.redacted.memories, 1);
    let bytes = serde_json::to_vec_pretty(&saved.document).expect("serialize snapshot");

    let mut restored =
        SqliteStore::open(directory.path().join("restored.db")).expect("restored store");
    restored
        .load_work_graph_snapshot(
            &project,
            &actor("load-session"),
            &bytes,
            false,
            at(3),
            &DevelopmentNoopRedactor,
        )
        .expect("load redacted memory");
    let restored_memory = restored
        .project_memory_full(
            &project,
            &crate::SessionId("reader-session".into()),
            &actor("reader-session"),
            "restricted-entry",
            None,
        )
        .expect("read restored placeholder");
    assert_eq!(restored_memory.body, REDACTED_MEMORY_PLACEHOLDER);

    let widened = restored
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            Some("carry every available restricted field"),
            WorkGraphSnapshotDestinationKind::Stdout,
            at(4),
            &DevelopmentNoopRedactor,
        )
        .expect("save widened restored graph");
    assert!(widened.document.body.summary.widened);
    assert_eq!(widened.document.body.summary.redacted.memories, 1);
    assert!(matches!(
        &widened.document.body.memories[0].state,
        WorkGraphSnapshotMemoryState::Active {
            body: WorkGraphSnapshotText::Redacted {
                sensitivity: Sensitivity::Restricted
            },
            ..
        }
    ));
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "checks restricted load disclosure, audit, and stable identity across two fresh destinations"
)]
fn widened_restricted_load_stores_only_audited_placeholders() {
    let project = ProjectId("snapshot-widened-load".into());
    let mut source = SqliteStore::open_in_memory().expect("source");
    insert_classified_project_memory(
        &mut source,
        &project,
        "restricted-entry",
        "private-plaintext-sentinel",
        Sensitivity::Restricted,
        at(1),
    );
    let saved = source
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            Some("human-readable recovery"),
            WorkGraphSnapshotDestinationKind::Stdout,
            at(2),
            &DevelopmentNoopRedactor,
        )
        .expect("widened save");
    assert_eq!(saved.document.body.summary.redacted.memories, 0);
    let bytes = snapshot_bytes(&saved.document);
    assert!(String::from_utf8_lossy(&bytes).contains("private-plaintext-sentinel"));
    let mut memory_ids = Vec::new();
    for _ in 0..2 {
        let mut destination = SqliteStore::open_in_memory().expect("destination");
        let preview = destination
            .load_work_graph_snapshot(
                &project,
                &actor("load-session"),
                &bytes,
                true,
                at(3),
                &DevelopmentNoopRedactor,
            )
            .expect("preview widened load");
        assert_eq!(preview.preview.placeholder_memories, ["restricted-entry"]);
        let loaded = destination
            .load_work_graph_snapshot(
                &project,
                &actor("load-session"),
                &bytes,
                false,
                at(3),
                &DevelopmentNoopRedactor,
            )
            .expect("load widened file");
        assert_eq!(loaded.preview, preview.preview);
        let memory = destination
            .project_memory_full(
                &project,
                &crate::SessionId("peer".into()),
                &actor("peer"),
                "restricted-entry",
                None,
            )
            .expect("peer reads only placeholder");
        assert_eq!(memory.body, REDACTED_MEMORY_PLACEHOLDER);
        let raw_bodies: i64 = destination
            .connection
            .query_row(
                "SELECT COUNT(*) FROM objects WHERE instr(CAST(canonical_json AS TEXT), ?1) > 0",
                ["private-plaintext-sentinel"],
                |row| row.get(0),
            )
            .expect("search all persisted canonical bytes");
        assert_eq!(raw_bodies, 0);
        let stored_id: String = destination
            .connection
            .query_row("SELECT memory_id FROM memory_heads", [], |row| row.get(0))
            .expect("restored memory identity");
        let memory_id = uuid::Uuid::parse_str(&stored_id).expect("valid UUID");
        assert_eq!(memory_id.get_version_num(), 8);
        assert_eq!(memory_id.get_variant(), uuid::Variant::RFC4122);
        memory_ids.push(memory_id);
        let (_, audits) = destination
            .recent_work_graph_snapshot_load_audits(&project, 8)
            .expect("load audits");
        let audit = &audits[0];
        assert!(audit.widened);
        assert_eq!(
            audit.widening_reason.as_deref(),
            Some("human-readable recovery")
        );
        assert_eq!(audit.redacted.memories, 1);
        let mut missing_reason = audit.clone();
        missing_reason.widening_reason = None;
        assert!(validate_loaded_event(&missing_reason, Some(&project)).is_err());
        let mut inconsistent_flag = audit.clone();
        inconsistent_flag.widened = false;
        assert!(validate_loaded_event(&inconsistent_flag, Some(&project)).is_err());
        let saved_again = destination
            .save_work_graph_snapshot(
                &project,
                &actor("save-session"),
                Some("save available text"),
                WorkGraphSnapshotDestinationKind::Stdout,
                at(4),
                &DevelopmentNoopRedactor,
            )
            .expect("resave never recovers restricted plaintext");
        assert_eq!(saved_again.document.body.summary.redacted.memories, 1);
        assert!(
            !String::from_utf8_lossy(&snapshot_bytes(&saved_again.document))
                .contains("private-plaintext-sentinel")
        );
        assert!(destination.verify_all().expect("integrity").is_healthy());
    }
    assert_eq!(memory_ids[0], memory_ids[1]);
}

#[test]
fn load_redactor_refusal_leaves_every_destination_section_and_audit_absent() {
    let project = ProjectId("snapshot-load-redaction".into());
    let mut source = SqliteStore::open_in_memory().expect("source");
    create_root(&mut source, &project, "Safe work title", "source-root");
    insert_classified_project_memory(
        &mut source,
        &project,
        "restricted-entry",
        "reject-me restricted plaintext",
        Sensitivity::Restricted,
        at(1),
    );
    let saved = source
        .save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            Some("human-readable recovery"),
            WorkGraphSnapshotDestinationKind::Stdout,
            at(2),
            &DevelopmentNoopRedactor,
        )
        .expect("widened source");
    let bytes = snapshot_bytes(&saved.document);
    let mut destination = SqliteStore::open_in_memory().expect("destination");
    let object_count = |store: &SqliteStore| {
        store
            .connection
            .query_row("SELECT COUNT(*) FROM objects", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("object count")
    };
    let before = object_count(&destination);
    for dry_run in [true, false] {
        assert!(matches!(
            destination.load_work_graph_snapshot(
                &project,
                &actor("load-session"),
                &bytes,
                dry_run,
                at(3),
                &SentinelRedactor,
            ),
            Err(StoreError::RedactionRefused(_))
        ));
        assert_eq!(object_count(&destination), before);
        let rows: i64 = destination
            .connection
            .query_row(
                "SELECT (SELECT COUNT(*) FROM work_items)
                  + (SELECT COUNT(*) FROM memory_heads)
                  + (SELECT COUNT(*) FROM work_restored_records)",
                [],
                |row| row.get(0),
            )
            .expect("all load sections remain empty");
        assert_eq!(rows, 0);
        assert_eq!(
            destination
                .recent_work_graph_snapshot_load_audits(&project, 8)
                .expect("load audit count")
                .0,
            0
        );
    }
}

#[test]
fn widening_reason_is_required_to_be_meaningful_before_audit() {
    let directory = crate::test_support::temp_home().expect("tempdir");
    let mut store = SqliteStore::open(directory.path().join("engram.db")).expect("store");
    let project = ProjectId("snapshot-widening-reason".into());
    create_root(&mut store, &project, "Snapshot root", "snapshot-root");

    assert!(matches!(
        store.save_work_graph_snapshot(
            &project,
            &actor("save-session"),
            Some("   "),
            WorkGraphSnapshotDestinationKind::Stdout,
            at(3),
            &DevelopmentNoopRedactor,
        ),
        Err(StoreError::InvalidWork(message)) if message.contains("widening reason")
    ));
    assert!(
        store
            .work_graph_snapshot_save_audits(&project)
            .expect("save audit query")
            .is_empty()
    );
}

#[test]
fn snapshot_audit_attribution_is_bounded_and_safe_for_diagnostics() {
    let directory = crate::test_support::temp_home().expect("tempdir");
    let mut store = SqliteStore::open(directory.path().join("engram.db")).expect("store");
    let project = ProjectId("snapshot-audit-attribution".into());
    create_root(&mut store, &project, "Snapshot root", "snapshot-root");

    for actor_id in [
        "actor\u{202e}".to_owned(),
        "x".repeat(MAX_PROJECT_MEMORY_ATTRIBUTION_TEXT_BYTES + 1),
    ] {
        let mut unsafe_actor = actor("save-session");
        unsafe_actor.actor_id = actor_id;
        assert!(matches!(
            store.save_work_graph_snapshot(
                &project,
                &unsafe_actor,
                None,
                WorkGraphSnapshotDestinationKind::Stdout,
                at(3),
                &DevelopmentNoopRedactor,
            ),
            Err(StoreError::InvalidWork(message))
                if message.contains("actor id") && message.contains("without control or format characters")
        ));
    }
    assert!(
        store
            .work_graph_snapshot_save_audits(&project)
            .expect("save audit query")
            .is_empty()
    );
    let mut long_actor = actor("save-session");
    long_actor.actor_id = "x".repeat(300);
    store
        .save_work_graph_snapshot(
            &project,
            &long_actor,
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(4),
            &DevelopmentNoopRedactor,
        )
        .expect("ordinary long attribution remains valid for save");
}
