use super::*;

#[test]
fn import_apply_decode_budget_does_not_grow_with_native_or_inherited_history() {
    for restored in [false, true] {
        let project = ProjectId(format!("bounded-apply-{restored}"));
        let mut store = SqliteStore::open_in_memory().unwrap();
        let mut candidate = input();
        apply(&mut store, &project, &candidate, 1);
        candidate.draft = None;
        let mut baseline = None;
        for revision in 2..=33 {
            candidate.snapshot.source_revision = Some(revision.to_string());
            let time = revision * 4;
            if restored {
                store = recover(&mut store, &project, time);
            }
            let preview = store
                .preview_work_import(&project, &candidate, at(time + 2))
                .unwrap();
            crate::canonical::reset_canonical_decode_count();
            let receipt = store
                .apply_work_import(
                    &project,
                    &candidate,
                    &preview.preview_token,
                    &actor("importer"),
                    at(time + 2),
                    &DevelopmentNoopRedactor,
                )
                .unwrap();
            let count = crate::canonical::canonical_decode_count();
            assert_eq!(receipt.effect, WorkImportEffect::Notify);
            if revision == 3 || revision == 33 {
                if let Some(baseline) = baseline {
                    assert_eq!(count, baseline, "restored={restored}, revision={revision}");
                } else {
                    baseline = Some(count);
                }
                assert!(
                    count <= 32,
                    "restored={restored}: {count} canonical decodes"
                );
            }
        }
        assert_eq!(
            store
                .lookup_work_source(&project, &key(&candidate.snapshot))
                .unwrap()
                .unwrap()
                .notice_count,
            32
        );
        assert!(store.verify_all().unwrap().is_healthy());
    }
}

#[test]
fn import_apply_checks_latest_capture_while_doctor_and_export_check_omitted_history() {
    for restored in [false, true] {
        let project = ProjectId(format!("apply-integrity-{restored}"));
        let mut store = SqliteStore::open_in_memory().unwrap();
        let mut candidate = input();
        apply(&mut store, &project, &candidate, 1);
        candidate.draft = None;
        candidate.snapshot.source_revision = Some("older".into());
        let older = apply(&mut store, &project, &candidate, 2);
        candidate.snapshot.source_revision = Some("latest".into());
        let latest = apply(&mut store, &project, &candidate, 3);
        if restored {
            store = recover(&mut store, &project, 4);
        }
        store
            .connection
            .execute(
                "DELETE FROM objects WHERE object_hash = ?1",
                [older.snapshot.as_str()],
            )
            .unwrap();
        assert!(!store.verify_all().unwrap().is_healthy());
        assert!(
            store
                .save_work_graph_snapshot(
                    &project,
                    &actor("save"),
                    None,
                    crate::WorkGraphSnapshotDestinationKind::Stdout,
                    at(6),
                    &DevelopmentNoopRedactor
                )
                .is_err()
        );
        // The latest capture is still selected and must refuse, even though the
        // older missing capture is intentionally not part of apply validation.
        store
            .connection
            .execute(
                "DELETE FROM objects WHERE object_hash = ?1",
                [latest.snapshot.as_str()],
            )
            .unwrap();
        candidate.snapshot.source_revision = Some("new".into());
        let preview = store
            .preview_work_import(&project, &candidate, at(7))
            .unwrap();
        let before = crate::storage::test_database_shape_snapshot(&store.connection).unwrap();
        assert!(
            store
                .apply_work_import(
                    &project,
                    &candidate,
                    &preview.preview_token,
                    &actor("importer"),
                    at(7),
                    &DevelopmentNoopRedactor
                )
                .is_err()
        );
        assert_eq!(
            before,
            crate::storage::test_database_shape_snapshot(&store.connection).unwrap()
        );
    }
}

#[test]
fn import_graph_restore_refuses_keys_outside_the_lookup_contract() {
    let project = ProjectId("restored-source-key".into());
    let mut store = SqliteStore::open_in_memory().unwrap();
    apply(&mut store, &project, &input(), 1);
    let saved = store
        .save_work_graph_snapshot(
            &project,
            &actor("save"),
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(2),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    for field in ["adapter_kind", "canonical_ref"] {
        for invalid in [
            "a".repeat(257),
            "a\nb".into(),
            "a\u{202e}b".into(),
            " leading".into(),
        ] {
            let mut document = saved.document.clone();
            document.body.sources[0].canonical_json[field] = serde_json::json!(invalid);
            document.body.sources[0].hash =
                CanonicalObject::freeze(&document.body.sources[0].canonical_json)
                    .unwrap()
                    .hash()
                    .clone();
            document.manifest.body_sha256 = CanonicalObject::freeze(&document.body)
                .unwrap()
                .hash()
                .clone();
            let mut destination = SqliteStore::open_in_memory().unwrap();
            let before =
                crate::storage::test_database_shape_snapshot(&destination.connection).unwrap();
            for dry_run in [true, false] {
                let error = destination
                    .load_work_graph_snapshot(
                        &project,
                        &actor("load"),
                        &serde_json::to_vec(&document).unwrap(),
                        dry_run,
                        at(3),
                        &DevelopmentNoopRedactor,
                    )
                    .unwrap_err();
                assert!(
                    matches!(error, StoreError::InvalidWork(ref reason) if reason.starts_with("source key fields")),
                    "{error:?}"
                );
                assert_eq!(
                    before,
                    crate::storage::test_database_shape_snapshot(&destination.connection).unwrap()
                );
            }
        }
    }
}
