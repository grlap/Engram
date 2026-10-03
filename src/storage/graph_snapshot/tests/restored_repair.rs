//! Projection repair of a restored store whose canonical restored records
//! cannot all be projected: every such record is named as a typed finding,
//! the rest is still checked, and the repair refuses and rolls back.

use super::*;

type Inventory = Vec<(String, Vec<u8>)>;

fn objects(connection: &rusqlite::Connection) -> Inventory {
    connection
        .prepare("SELECT object_id, canonical_json FROM objects ORDER BY object_id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn restored_rows(connection: &rusqlite::Connection) -> Vec<(String, i64, String)> {
    connection
        .prepare(
            "SELECT work_id, generation_index, record_id FROM work_restored_records
             ORDER BY work_id, generation_index",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn catalog_rows(connection: &rusqlite::Connection) -> i64 {
    connection
        .query_row("SELECT COUNT(*) FROM work_catalog_fts", [], |row| {
            row.get(0)
        })
        .unwrap()
}

#[test]
fn projection_repair_names_every_unprojectable_restored_record_and_rolls_back() {
    let directory = crate::test_support::temp_home().expect("tempdir");
    let project = ProjectId("snapshot-restored-repair".into());
    let mut source = SqliteStore::open(directory.path().join("source.db")).expect("source");
    for index in 0..6 {
        create_root(
            &mut source,
            &project,
            &format!("Restored root {index}"),
            &format!("restored-root-{index}"),
        );
    }
    let document = {
        source
            .save_work_graph_snapshot(
                &project,
                &actor("save-session"),
                None,
                WorkGraphSnapshotDestinationKind::Stdout,
                at(3),
                &DevelopmentNoopRedactor,
            )
            .expect("save snapshot")
            .document
    };
    let database = directory.path().join("restored.db");
    let mut restored = SqliteStore::open(&database).expect("fresh store");
    restored
        .load_work_graph_snapshot(
            &project,
            &actor("load-session"),
            &serde_json::to_vec_pretty(&document).unwrap(),
            false,
            at(4),
            &DevelopmentNoopRedactor,
        )
        .expect("load snapshot");
    // A non-holder note on the first restored item makes an observation
    // whose planning basis is that item's restored record.
    let observed: String = restored
        .connection
        .query_row(
            "SELECT work_id FROM work_restored_records ORDER BY work_id, generation_index LIMIT 1",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let observed = crate::WorkId(uuid::Uuid::parse_str(&observed).unwrap());
    let item = restored.get_work_item(observed).unwrap();
    restored
        .record_work_observation(
            &crate::domain::RecordWorkObservationRequest {
                status: false,
                project_id: project.clone(),
                work_id: observed,
                expected_work_revision: item.revision,
                session_id: crate::SessionId("observer-session".into()),
                summary: "an observation on a restored item".into(),
                refs: Vec::new(),
                actor: {
                    let mut note_actor = actor("observer-session");
                    note_actor
                        .provenance_chain
                        .push(crate::domain::ProvenanceLink {
                            relation: crate::domain::ProvenanceRelation::DerivedFrom,
                            source: crate::domain::NON_HOLDER_NOTE_SOURCE.into(),
                            reference: Some(crate::domain::NON_HOLDER_NOTE_REFERENCE.into()),
                        });
                    note_actor
                },
                idempotency_key: "observer-note".into(),
                recorded_at: at(5),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("observe the restored item");
    drop(restored);

    let connection = rusqlite::Connection::open(&database).unwrap();
    let healthy_rows = restored_rows(&connection);
    assert_eq!(healthy_rows.len(), 6, "one inherited record per item");
    let observation: String = connection
        .query_row(
            "SELECT object_id FROM objects WHERE object_kind = 'work_observation'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let records: Vec<String> = healthy_rows.iter().map(|row| row.2.clone()).collect();
    let healthy_objects = objects(&connection);
    let original = |record: &str| {
        healthy_objects
            .iter()
            .find(|(id, _)| id == record)
            .unwrap()
            .1
            .clone()
    };
    // Records that cannot be projected beside one that can: an item that is
    // missing (with an observation anchored to that record), bytes that do
    // not decode, a generation SQLite cannot hold, and two records claiming
    // one item's generation. A rebuildable projection is damaged too, so a
    // rollback shows in it.
    let missing_item = &records[0];
    let malformed = &records[1];
    let out_of_range = &records[2];
    let (duplicate, duplicated) = (&records[3], &records[4]);
    connection
        .execute(
            "UPDATE objects SET canonical_json =
                 CAST(json_set(canonical_json, '$.work_id', ?1) AS BLOB)
             WHERE object_id = ?2",
            rusqlite::params![uuid::Uuid::now_v7().to_string(), missing_item],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE objects SET canonical_json = X'7B7D' WHERE object_id = ?1",
            [malformed],
        )
        .unwrap();
    let generation = String::from_utf8(original(out_of_range)).unwrap().replace(
        "\"generation_index\":0",
        "\"generation_index\":18446744073709551615",
    );
    assert_ne!(generation.as_bytes(), original(out_of_range));
    connection
        .execute(
            "UPDATE objects SET canonical_json = ?1 WHERE object_id = ?2",
            rusqlite::params![generation.as_bytes(), out_of_range],
        )
        .unwrap();
    connection
        .execute(
            "UPDATE objects SET canonical_json =
                 CAST(json_set(canonical_json, '$.work_id', ?1) AS BLOB)
             WHERE object_id = ?2",
            rusqlite::params![healthy_rows[4].0, duplicate],
        )
        .unwrap();
    connection
        .execute("DELETE FROM work_catalog_fts", [])
        .unwrap();
    let damaged_objects = objects(&connection);
    let damaged_rows = restored_rows(&connection);
    assert_eq!(catalog_rows(&connection), 0);

    let error = SqliteStore::repair_rebuildable_projections(&database)
        .expect_err("the repair refuses")
        .to_string();
    for finding in [
        format!("work_restored_record:{missing_item}:missing_item"),
        format!("work_restored_record:{malformed}:decode:"),
        format!("work_restored_record:{out_of_range}:generation_out_of_range"),
    ] {
        assert!(error.contains(&finding), "{finding}: {error}");
    }
    // The observation anchored to the damaged record is named too, rather
    // than ending the repair with an error that names none of them.
    assert!(
        error.contains(&format!("work_observation:{observation}:unprojectable")),
        "{error}"
    );
    assert!(
        [duplicate, duplicated]
            .iter()
            .any(|record| error.contains(&format!(
                "work_restored_record:{record}:projection_constraint"
            ))),
        "{error}"
    );
    for record in [missing_item, malformed, out_of_range] {
        assert!(
            !error.contains(&format!("work_restored_record:{record}:missing_projection")),
            "the record's own finding names it once: {error}"
        );
    }
    assert!(
        !error.contains(&records[5]),
        "the valid record projects: {error}"
    );
    // Nothing the repair wrote survives, and no record was rewritten or
    // deleted.
    assert_eq!(objects(&connection), damaged_objects);
    assert_eq!(restored_rows(&connection), damaged_rows);
    assert_eq!(catalog_rows(&connection), 0);

    // With the records' original bytes back, the repair rebuilds every
    // projection, the observation included, and reports health.
    for record in [missing_item, malformed, out_of_range, duplicate] {
        connection
            .execute(
                "UPDATE objects SET canonical_json = ?1 WHERE object_id = ?2",
                rusqlite::params![original(record), record],
            )
            .unwrap();
    }
    assert_eq!(objects(&connection), healthy_objects);
    let report = SqliteStore::repair_rebuildable_projections(&database).expect("repair");
    assert!(report.is_healthy(), "{report:?}");
    assert_eq!(restored_rows(&connection), healthy_rows);
    assert_eq!(catalog_rows(&connection), 6);
    let observations: i64 = connection
        .query_row("SELECT COUNT(*) FROM work_observations", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(observations, 1);
}
