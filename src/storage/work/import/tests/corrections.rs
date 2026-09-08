use super::*;

mod bounded_apply;

#[test]
fn import_retry_ignores_only_default_markers_and_retains_original_actor() {
    use crate::domain::{ProvenanceLink, ProvenanceRelation};
    let project = ProjectId("import-retry-attribution".into());
    let mut store = SqliteStore::open_in_memory().unwrap();
    let mut candidate = input();
    let explicit = actor("author");
    let mut defaulted = explicit.clone();
    for (source, reference) in [
        ("defaulted:process_session", "session_id"),
        ("defaulted:os_user_environment", "actor_id"),
        ("defaulted:process_actor", "actor_id"),
    ] {
        defaulted.provenance_chain.push(ProvenanceLink {
            relation: ProvenanceRelation::DerivedFrom,
            source: source.into(),
            reference: Some(reference.into()),
        });
    }
    assert_eq!(defaulted.retry_stable(), explicit);
    for field in ["principal", "reason", "unknown", "context", "assurance"] {
        let mut other = defaulted.clone();
        match field {
            "principal" => other.actor_id = "someone-else".into(),
            "reason" => other.reason = "different intent".into(),
            "assurance" => other.assurance = crate::domain::AssuranceLevel::Authenticated,
            _ => other.provenance_chain.push(ProvenanceLink {
                relation: ProvenanceRelation::DerivedFrom,
                source: if field == "unknown" {
                    "defaulted:unknown"
                } else {
                    "host-context"
                }
                .into(),
                reference: Some("actor_context".into()),
            }),
        }
        assert_ne!(
            other.retry_stable(),
            explicit,
            "{field} still binds identity"
        );
    }
    for revision in 1..=2 {
        let preview = store
            .preview_work_import(&project, &candidate, at(revision))
            .unwrap();
        let receipt = store
            .apply_work_import(
                &project,
                &candidate,
                &preview.preview_token,
                &defaulted,
                at(revision),
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        let before = crate::storage::test_database_shape_snapshot(&store.connection).unwrap();
        assert_eq!(
            receipt,
            store
                .apply_work_import(
                    &project,
                    &candidate,
                    &preview.preview_token,
                    &explicit,
                    at(revision + 10),
                    &DevelopmentNoopRedactor
                )
                .unwrap()
        );
        assert_eq!(
            before,
            crate::storage::test_database_shape_snapshot(&store.connection).unwrap()
        );
        let stored_actor =
            if revision == 1 {
                let bytes: Vec<u8> = store.connection.query_row(
                "SELECT canonical_json FROM objects WHERE object_kind = 'work_event' LIMIT 1",
                [], |row| row.get(0)).unwrap();
                serde_json::from_slice::<serde_json::Value>(&bytes).unwrap()["actor"].clone()
            } else {
                serde_json::to_value(
                    store
                        .work_source_detail(&project, &key(&candidate.snapshot))
                        .unwrap()
                        .unwrap()
                        .lookup
                        .latest_notice
                        .unwrap()
                        .actor,
                )
                .unwrap()
            };
        assert_eq!(stored_actor, serde_json::to_value(&defaulted).unwrap());
        candidate.draft = None;
        candidate.snapshot.source_revision = Some("2".into());
    }
}

#[test]
fn import_restored_required_shape_refuses_at_use_and_before_repair_writes() {
    let home = crate::test_support::temp_home().unwrap();
    let database = home.path().join("restored-shape.db");
    let project = ProjectId("restored-shape".into());
    let mut source = SqliteStore::open_in_memory().unwrap();
    apply(&mut source, &project, &input(), 1);
    let saved = source
        .save_work_graph_snapshot(
            &project,
            &actor("save"),
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(2),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let mut destination = SqliteStore::open(&database).unwrap();
    destination
        .load_work_graph_snapshot(
            &project,
            &actor("load"),
            &serde_json::to_vec(&saved.document).unwrap(),
            false,
            at(3),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let item = destination
        .get_work_item(saved.document.body.items[0].work_id)
        .unwrap();
    drop(destination);
    assert!(SqliteStore::open_existing_read_only(&database).is_ok());
    let service = crate::LocalWorkService::new(
        database.clone(),
        project.clone(),
        "reader".into(),
        crate::SessionId("reader".into()),
        None,
    );
    let memory_query = || crate::work_service::WorkNextQuery {
        sections: vec![crate::work_service::WorkNextSection::Memories],
        ..Default::default()
    };
    service
        .select_work(&saved.document.body.items[0].short_ref, at(4))
        .unwrap();
    crate::canonical::reset_canonical_decode_count();
    let orientation_before = service
        .work_next_peek_for_agent(16, 16, false, memory_query(), at(4), |_| true)
        .unwrap();
    let orientation_decodes = crate::canonical::canonical_decode_count();
    let connection = rusqlite::Connection::open(&database).unwrap();
    let (old_hash, bytes): (String, Vec<u8>) = connection.query_row(
        "SELECT object_hash, canonical_json FROM objects WHERE object_kind = 'work_restored_record'",
        [], |row| Ok((row.get(0)?, row.get(1)?))).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let healthy = CanonicalObject::freeze(&value).unwrap();
    crate::canonical::reset_canonical_decode_count();
    let _: RestoredRecord =
        crate::storage::work::decode_work_object("work_restored_record", &healthy).unwrap();
    assert_eq!(
        crate::canonical::canonical_decode_count(),
        1,
        "healthy use performs one normal decode"
    );
    value["history"]
        .as_object_mut()
        .unwrap()
        .remove("source_notices");
    let incompatible = CanonicalObject::freeze(&value).unwrap();
    let baseline = incompatible.decode::<RestoredRecord>().unwrap_err();
    assert!(!crate::storage::is_different_build_store_error(&baseline));
    assert!(
        baseline
            .to_string()
            .contains("missing field `source_notices`")
    );
    // Present-but-invalid data is corruption, not the missing required shape.
    value["history"]["source_notices"] = serde_json::Value::Null;
    let corrupt = CanonicalObject::freeze(&value).unwrap();
    let corrupt_error = crate::storage::work::decode_work_object::<RestoredRecord>(
        "work_restored_record",
        &corrupt,
    )
    .unwrap_err();
    assert!(!crate::storage::is_different_build_store_error(
        &corrupt_error
    ));
    // An intact canonical object of the incompatible shape, not a hash-corrupt
    // object that could fail for an unrelated integrity reason.
    connection
        .pragma_update(None, "foreign_keys", false)
        .unwrap();
    connection
        .execute(
            "UPDATE objects SET object_hash = ?1, canonical_json = ?2 WHERE object_hash = ?3",
            rusqlite::params![incompatible.hash().as_str(), incompatible.bytes(), old_hash],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE work_restored_records SET record_hash = ?1 WHERE record_hash = ?2",
            rusqlite::params![incompatible.hash().as_str(), old_hash],
        )
        .unwrap();
    let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
    let current = SqliteStore::open_existing_read_only(&database).unwrap();
    let work_id = saved.document.body.items[0].work_id;
    for result in [
        source_notice_summary_on(&current.connection, &item).map(|_| ()),
        current.work_restored_records(work_id).map(|_| ()),
        current.verify_all().map(|_| ()),
        SqliteStore::repair_rebuildable_projections(&database).map(|_| ()),
    ] {
        assert!(
            matches!(result, Err(ref error) if crate::storage::is_different_build_store_error(error)),
            "{result:?}"
        );
        assert_eq!(
            before,
            crate::storage::test_database_shape_snapshot(&connection).unwrap()
        );
    }
    let mapped = crate::storage::work::decode_work_object::<RestoredRecord>(
        "work_restored_record",
        &incompatible,
    )
    .unwrap_err();
    assert_eq!(
        crate::work_service::advisory_error_class(&mapped),
        "store_different_build"
    );
    crate::canonical::reset_canonical_decode_count();
    let orientation_after = service
        .work_next_peek_for_agent(16, 16, false, memory_query(), at(4), |_| true)
        .unwrap();
    assert_eq!(
        serde_json::to_value(orientation_after).unwrap(),
        serde_json::to_value(orientation_before).unwrap()
    );
    assert_eq!(
        crate::canonical::canonical_decode_count(),
        orientation_decodes,
        "unrelated orientation does not decode restored history at open"
    );
    let reference = &saved.document.body.items[0].short_ref;
    // Cold source detail and full show must not swallow the format refusal.
    // Opening itself remains cheap and permits unrelated compatible reads.
    let focused_query = || crate::work_service::WorkNextQuery {
        sections: vec![crate::work_service::WorkNextSection::Focus],
        ..Default::default()
    };
    for result in [
        service
            .work_next_peek_for_agent(16, 16, false, focused_query(), at(4), |_| true)
            .map(|_| ()),
        service
            .work_next_for_agent(16, 16, false, focused_query(), at(4))
            .map(|_| ()),
        service
            .lookup_work_source(&key(&input().snapshot), at(4))
            .map(|_| ()),
        service.work_focus_for_agent(reference, at(4)).map(|_| ()),
        SqliteStore::open(&database)
            .and_then(|store| store.work_restored_records(saved.document.body.items[0].work_id))
            .map(|_| ()),
        SqliteStore::open(&database)
            .and_then(|store| store.verify_all())
            .map(|_| ()),
    ] {
        assert!(
            matches!(result, Err(StoreError::InvalidControlProjection(message))
            if message == crate::storage::DIFFERENT_BUILD_STORE_MESSAGE)
        );
    }
}

#[test]
fn import_snapshot_source_validation_decodes_each_source_once() {
    let project = ProjectId("source-validation-cost".into());
    let mut store = SqliteStore::open_in_memory().unwrap();
    let mut candidate = input();
    apply(&mut store, &project, &candidate, 1);
    candidate.draft = None;
    for revision in 2..=33 {
        candidate.snapshot.source_revision = Some(revision.to_string());
        apply(&mut store, &project, &candidate, revision);
    }
    let saved = store
        .save_work_graph_snapshot(
            &project,
            &actor("save"),
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(34),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let bytes = serde_json::to_vec(&saved.document).unwrap();
    let mut destination = SqliteStore::open_in_memory().unwrap();
    crate::canonical::reset_canonical_decode_count();
    destination
        .load_work_graph_snapshot(
            &project,
            &actor("load"),
            &bytes,
            true,
            at(35),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    assert_eq!(
        crate::canonical::canonical_decode_count(),
        saved.document.body.sources.len(),
        "native dry-run validation decodes each selected source exactly once"
    );
}

fn apply(
    store: &mut SqliteStore,
    project: &ProjectId,
    input: &WorkImportInput,
    time: i64,
) -> WorkImportReceipt {
    let preview = store.preview_work_import(project, input, at(time)).unwrap();
    store
        .apply_work_import(
            project,
            input,
            &preview.preview_token,
            &actor("importer"),
            at(time),
            &DevelopmentNoopRedactor,
        )
        .unwrap()
}

fn recover(store: &mut SqliteStore, project: &ProjectId, time: i64) -> SqliteStore {
    let saved = store
        .save_work_graph_snapshot(
            project,
            &actor("saver"),
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(time),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let mut destination = SqliteStore::open_in_memory().unwrap();
    destination
        .load_work_graph_snapshot(
            project,
            &actor("loader"),
            &serde_json::to_vec(&saved.document).unwrap(),
            false,
            at(time + 1),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    destination
}

fn terminal_refresh(disposition: crate::WorkDisposition) {
    let project = ProjectId("terminal-import-refresh".into());
    let mut store = SqliteStore::open_in_memory().unwrap();
    let mut original = input();
    let receipt = apply(&mut store, &project, &original, 1);
    let replacement_id = if disposition == crate::WorkDisposition::Superseded {
        let mut replacement = input();
        replacement.snapshot.canonical_ref = "plan/replacement".into();
        Some(apply(&mut store, &project, &replacement, 2).work_id)
    } else {
        None
    };
    store
        .dispose_work(
            &crate::DisposeWorkRequest {
                work_id: receipt.work_id,
                expected_work_revision: 1,
                disposition,
                replacement_id,
                reason: "Author deliberately closed this item".into(),
                actor: actor("importer"),
                idempotency_key: "dispose-import".into(),
                disposed_at: at(3),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    store = recover(&mut store, &project, 4);
    let inherited_before: Vec<(String, Vec<u8>)> = store.connection.prepare("SELECT object_hash, canonical_json FROM objects WHERE object_kind = 'work_restored_record' ORDER BY object_hash").unwrap().query_map([], |row| Ok((row.get(0)?, row.get(1)?))).unwrap().collect::<Result<_, _>>().unwrap();
    original.draft = None;
    original.snapshot.source_revision = Some("2".into());
    let before = store.get_work_item(receipt.work_id).unwrap();
    let notice = apply(&mut store, &project, &original, 6);
    assert_eq!(notice.effect, WorkImportEffect::Notify);
    assert_eq!(store.get_work_item(receipt.work_id).unwrap(), before);
    store = recover(&mut store, &project, 7);
    for (hash, bytes) in inherited_before {
        let retained: Vec<u8> = store
            .connection
            .query_row(
                "SELECT canonical_json FROM objects WHERE object_hash = ?1",
                [hash],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(
            retained, bytes,
            "inherited canonical bytes must remain unchanged"
        );
    }
    assert_eq!(
        store.get_work_item(receipt.work_id).unwrap().lifecycle,
        before.lifecycle
    );
    assert_eq!(
        store
            .lookup_work_source(&project, &key(&original.snapshot))
            .unwrap()
            .unwrap()
            .notice_count,
        1
    );
    assert!(store.verify_all().unwrap().is_healthy());
    store = recover(&mut store, &project, 9);
    assert_terminal_readers(&mut store, &project, &receipt.work_ref);
}

fn assert_terminal_readers(store: &mut SqliteStore, project: &ProjectId, reference: &str) {
    let saved = store
        .save_work_graph_snapshot(
            project,
            &actor("saver"),
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(11),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let home = crate::test_support::temp_home().unwrap();
    let database = home.path().join("terminal-readers.db");
    let mut destination = SqliteStore::open(&database).unwrap();
    destination
        .load_work_graph_snapshot(
            project,
            &actor("loader"),
            &serde_json::to_vec(&saved.document).unwrap(),
            false,
            at(12),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let verbs = crate::verbs::AgentVerbs::new(
        database,
        project.clone(),
        "reader".into(),
        crate::SessionId("reader".into()),
        None,
    );
    let ordinary = verbs.show(reference, at(13)).unwrap();
    let window = verbs
        .show_records(
            reference,
            &crate::verbs::ShowInput {
                history: true,
                ..Default::default()
            },
            at(13),
        )
        .unwrap();
    for (receipt, section) in [(&ordinary, "restored_history"), (&window, "history")] {
        let rows = receipt.value[section]["items"].as_array().unwrap();
        assert_eq!(
            rows.iter().filter(|row| row["kind"] == "disposed").count(),
            1,
            "{section}: {:?}",
            receipt.value
        );
        assert_eq!(receipt.value[section]["total"], rows.len());
        assert_eq!(
            receipt
                .text()
                .matches("Author deliberately closed this item")
                .count(),
            1
        );
    }
    assert_eq!(
        ordinary.value["status"]["work"]["lifecycle"],
        window.value["status"]["work"]["lifecycle"]
    );
    let work_id = destination
        .resolve_work_ref(project, reference)
        .unwrap()
        .work_id;
    let mut records = destination.work_restored_records(work_id).unwrap();
    let carried = crate::graph_snapshot::carried_disposal_layers(&records);
    assert_eq!(carried.iter().filter(|carried| **carried).count(), 1);
    let last = records.last_mut().unwrap();
    last.history.events[0].actor.reason = "A genuinely different attributed capture".into();
    assert!(
        !crate::graph_snapshot::carried_disposal_layers(&records)
            .last()
            .unwrap(),
        "same rendered disposal reason is not identity"
    );
}

#[test]
fn import_terminal_refresh_survives_recovery_cancelled() {
    terminal_refresh(crate::WorkDisposition::Cancelled);
}

#[test]
fn import_terminal_refresh_survives_recovery_superseded() {
    terminal_refresh(crate::WorkDisposition::Superseded);
}

#[test]
fn import_redactor_inspects_actor_without_writes() {
    for notify in [false, true] {
        for provenance in [false, true] {
            let project = ProjectId("actor-inspection".into());
            let mut store = SqliteStore::open_in_memory().unwrap();
            let mut candidate = input();
            if notify {
                apply(&mut store, &project, &candidate, 1);
                candidate.draft = None;
                candidate.snapshot.source_revision = Some("2".into());
            }
            let preview = store
                .preview_work_import(&project, &candidate, at(2))
                .unwrap();
            let mut author = actor("author");
            if provenance {
                author.provenance_chain.push(crate::domain::ProvenanceLink {
                    relation: crate::domain::ProvenanceRelation::RelayedBy,
                    source: "reject-me".into(),
                    reference: None,
                });
            } else {
                author.reason = "reject-me".into();
            }
            let before = crate::storage::test_database_shape_snapshot(&store.connection).unwrap();
            let error = store
                .apply_work_import(
                    &project,
                    &candidate,
                    &preview.preview_token,
                    &author,
                    at(2),
                    &crate::storage::test_support::SentinelRedactor,
                )
                .unwrap_err();
            assert!(
                matches!(error, StoreError::RedactionRefused(_)),
                "{error:?}"
            );
            assert_eq!(
                before,
                crate::storage::test_database_shape_snapshot(&store.connection).unwrap()
            );
        }
    }
}

#[test]
fn import_doctor_detects_missing_inherited_source() {
    let project = ProjectId("inherited-source-audit".into());
    let mut store = SqliteStore::open_in_memory().unwrap();
    let mut candidate = input();
    apply(&mut store, &project, &candidate, 1);
    candidate.draft = None;
    candidate.snapshot.source_revision = Some("2".into());
    let older = apply(&mut store, &project, &candidate, 2);
    candidate.snapshot.source_revision = Some("3".into());
    apply(&mut store, &project, &candidate, 3);
    store = recover(&mut store, &project, 4);
    assert!(store.verify_all().unwrap().is_healthy());
    store
        .connection
        .execute(
            "DELETE FROM objects WHERE object_hash = ?1",
            [older.snapshot.as_str()],
        )
        .unwrap();
    let before = crate::storage::test_database_shape_snapshot(&store.connection).unwrap();
    let lookup = store
        .lookup_work_source(&project, &key(&candidate.snapshot))
        .unwrap()
        .unwrap();
    assert_eq!(
        lookup.notice_count, 2,
        "bounded lookup deliberately omits the older body"
    );
    let report = store.verify_all().unwrap();
    assert!(
        !report.is_healthy(),
        "doctor must audit omitted inherited captures"
    );
    assert!(
        report
            .invalid_work_records
            .iter()
            .any(|entry| entry.contains("source_notice")),
        "{:?}",
        report.invalid_work_records
    );
    assert_eq!(
        before,
        crate::storage::test_database_shape_snapshot(&store.connection).unwrap()
    );
    assert!(
        store
            .save_work_graph_snapshot(
                &project,
                &actor("saver"),
                None,
                crate::WorkGraphSnapshotDestinationKind::Stdout,
                at(7),
                &DevelopmentNoopRedactor
            )
            .is_err()
    );
}

#[test]
fn import_preview_membership_is_bounded() {
    let project = ProjectId("bounded-preview".into());
    let mut store = SqliteStore::open_in_memory().unwrap();
    let mut candidate = input();
    apply(&mut store, &project, &candidate, 1);
    candidate.draft = None;
    let mut baseline = None;
    let mut older = None;
    for revision in 2..=33 {
        candidate.snapshot.source_revision = Some(revision.to_string());
        let receipt = apply(&mut store, &project, &candidate, revision);
        if revision == 2 {
            older = Some(receipt.snapshot);
        }
        if revision == 3 || revision == 33 {
            let mut fresh = candidate.clone();
            fresh.snapshot.source_revision = Some("not-yet-recorded".into());
            let before = crate::storage::test_database_shape_snapshot(&store.connection).unwrap();
            crate::canonical::reset_canonical_decode_count();
            assert_eq!(
                store
                    .preview_work_import(&project, &fresh, at(34))
                    .unwrap()
                    .effect,
                WorkImportEffect::Notify
            );
            let decodes = crate::canonical::canonical_decode_count();
            if let Some(baseline) = baseline {
                assert_eq!(decodes, baseline);
            } else {
                baseline = Some(decodes);
            }
            assert!(decodes <= 8, "preview decoded {decodes} canonical bodies");
            assert_eq!(
                store
                    .preview_work_import(&project, &candidate, at(34))
                    .unwrap()
                    .effect,
                WorkImportEffect::AlreadyKnown
            );
            assert_eq!(
                before,
                crate::storage::test_database_shape_snapshot(&store.connection).unwrap()
            );
        }
    }
    store
        .connection
        .execute(
            "DELETE FROM objects WHERE object_hash = ?1",
            [older.unwrap().as_str()],
        )
        .unwrap();
    candidate.snapshot.source_revision = Some("new-after-corruption".into());
    let preview = store
        .preview_work_import(&project, &candidate, at(35))
        .unwrap();
    assert_eq!(preview.effect, WorkImportEffect::Notify);
    assert_eq!(
        store
            .apply_work_import(
                &project,
                &candidate,
                &preview.preview_token,
                &actor("importer"),
                at(35),
                &DevelopmentNoopRedactor
            )
            .unwrap()
            .effect,
        WorkImportEffect::Notify,
        "apply validates the selected latest capture, not omitted history"
    );
    assert!(!store.verify_all().unwrap().is_healthy());
    assert!(
        store
            .save_work_graph_snapshot(
                &project,
                &actor("save"),
                None,
                crate::WorkGraphSnapshotDestinationKind::Stdout,
                at(36),
                &DevelopmentNoopRedactor
            )
            .is_err()
    );
}

#[test]
fn import_corrupt_notice_preserves_show_context() {
    let home = crate::test_support::temp_home().unwrap();
    let database = home.path().join("source-show.db");
    let project = ProjectId("source-show".into());
    let mut store = SqliteStore::open(&database).unwrap();
    let mut candidate = input();
    candidate.draft.as_mut().unwrap().acceptance = vec!["Authored criterion".into()];
    let created = apply(&mut store, &project, &candidate, 1);
    let verbs = crate::verbs::AgentVerbs::new(
        database,
        project.clone(),
        "reader".into(),
        crate::SessionId("reader".into()),
        None,
    );
    verbs
        .claim(
            crate::verbs::ClaimInput {
                work_ref: created.work_ref.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(2),
        )
        .unwrap();
    verbs
        .note(
            &crate::verbs::NoteInput {
                status: true,
                work_ref: Some(created.work_ref.clone()),
                text: "Local status remains readable".into(),
                refs: Vec::new(),
            },
            at(3),
        )
        .unwrap();
    candidate.draft = None;
    candidate.snapshot.source_revision = Some("2".into());
    let notice = apply(&mut store, &project, &candidate, 4);
    let before = verbs.show(&created.work_ref, at(5)).unwrap();
    store
        .connection
        .execute(
            "DELETE FROM objects WHERE object_hash = ?1",
            [notice.snapshot.as_str()],
        )
        .unwrap();
    let database_before = crate::storage::test_database_shape_snapshot(&store.connection).unwrap();
    let after = verbs.show(&created.work_ref, at(5)).unwrap();
    assert!(after.value.get("source").is_none());
    assert_eq!(after.value["source_error_class"], "work_projection_invalid");
    let mut expected = before.value;
    expected.as_object_mut().unwrap().remove("source");
    expected["source_error_class"] = after.value["source_error_class"].clone();
    assert_eq!(
        after.value, expected,
        "all intact local context is retained"
    );
    assert!(after.text().contains("source disclosure unavailable:"));
    assert!(after.text().contains("Local status remains readable"));
    assert!(!after.text().contains(notice.snapshot.as_str()));
    assert_eq!(
        database_before,
        crate::storage::test_database_shape_snapshot(&store.connection).unwrap()
    );
    assert!(
        store
            .lookup_work_source(&project, &key(&candidate.snapshot))
            .is_err()
    );
    assert!(!store.verify_all().unwrap().is_healthy());
}
