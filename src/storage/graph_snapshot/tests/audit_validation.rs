use super::*;

#[test]
fn doctor_checks_semantic_save_audit_bindings_and_duplicate_attempts() {
    for case in ["binding", "duplicate", "historical-absent-count"] {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let project = ProjectId("save-audit-validation".into());
        store
            .save_work_graph_snapshot(
                &project,
                &actor("save"),
                None,
                WorkGraphSnapshotDestinationKind::Stdout,
                at(1),
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        let (id, bytes): (String, Vec<u8>) = store.connection.query_row(
            "SELECT object_id, canonical_json FROM objects WHERE object_kind = 'work_graph_snapshot_saved'",
            [], |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        let mut audit: WorkGraphSnapshotSavedEvent = serde_json::from_slice(&bytes).unwrap();
        let mut expected_invalid = Vec::new();
        match case {
            "binding" => {
                audit.widened = true; // Valid JSON, but no required widening reason.
                store
                    .connection
                    .execute(
                        "UPDATE objects SET canonical_json = ?1 WHERE object_id = ?2",
                        rusqlite::params![crate::canonical::canonical_bytes(&audit).unwrap(), id],
                    )
                    .unwrap();
                expected_invalid.push(format!("work_graph_snapshot_saved:{id}"));
            }
            "duplicate" => {
                let duplicate = CanonicalObject::mint(&audit).unwrap();
                let transaction = store.connection.transaction().unwrap();
                SqliteStore::insert_object(&transaction, "work_graph_snapshot_saved", &duplicate)
                    .unwrap();
                transaction.commit().unwrap();
                // The checker encounters the later object id second, regardless
                // of insertion order or the attempt's clock.
                let later = std::cmp::max(id.as_str(), duplicate.key().as_str());
                expected_invalid.push(format!("work_graph_snapshot_saved:{later}"));
            }
            "historical-absent-count" => {
                audit.secret_ref_bodies = None;
                let historical = crate::canonical::canonical_bytes(&audit).unwrap();
                assert!(!String::from_utf8_lossy(&historical).contains("secret_ref_bodies"));
                store
                    .connection
                    .execute(
                        "UPDATE objects SET canonical_json = ?1 WHERE object_id = ?2",
                        rusqlite::params![historical, id],
                    )
                    .unwrap();
                assert_eq!(
                    store.work_graph_snapshot_save_audits(&project).unwrap()[0].secret_ref_bodies,
                    None
                );
                assert!(store.verify_all().unwrap().is_healthy());
                let after: Vec<u8> = store
                    .connection
                    .query_row(
                        "SELECT canonical_json FROM objects WHERE object_id = ?1",
                        [&id],
                        |row| row.get(0),
                    )
                    .unwrap();
                assert_eq!(
                    after, historical,
                    "reading old audits never rewrites their bytes"
                );
            }
            _ => unreachable!(),
        }
        let (checked, invalid) =
            verify_work_graph_snapshot_saved_events_on(&store.connection).unwrap();
        assert_eq!(checked, if case == "duplicate" { 2 } else { 1 });
        assert_eq!(invalid, expected_invalid, "{case}");
        let report = store.verify_all().unwrap();
        assert_eq!(
            report.invalid_graph_snapshot_audits, expected_invalid,
            "{case}"
        );
        assert_eq!(report.is_healthy(), case == "historical-absent-count");
    }
}
