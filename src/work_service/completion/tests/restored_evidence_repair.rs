//! Projection repair of a restored completed item whose late evidence cannot
//! all be projected: each unprojectable restored evidence object is named
//! once as a typed finding, and the repair refuses and rolls back.

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

fn evidence_rows(connection: &rusqlite::Connection) -> Vec<(String, String, i64)> {
    connection
        .prepare(
            "SELECT evidence_id, work_id, sequence FROM work_restored_evidence
             ORDER BY evidence_id",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn projection_repair_names_every_unprojectable_restored_evidence_and_rolls_back() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let destination_database = directory.path().join("restored-evidence-repair.sqlite3");
    let project = ProjectId("restored-evidence-repair".into());
    let source = LocalWorkService::new(
        directory.path().join("source.sqlite3"),
        project.clone(),
        "source-agent".into(),
        SessionId("source-session".into()),
        Some("protocol-test".into()),
    );
    let root = proposed_root(
        source
            .work_propose(root_input("Completed before review", "repair-root"), at(0))
            .expect("root"),
    );
    source
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "claim-repair-root".into(),
            },
            at(1),
        )
        .expect("claim root");
    assert!(matches!(
        source
            .work_complete(completion_input("complete", "complete-repair-root"), at(2))
            .expect("complete root"),
        WorkCompleteResult::Completed(_)
    ));
    let snapshot = source
        .save_work_graph_snapshot(None, WorkGraphSnapshotDestinationKind::Stdout, at(3))
        .expect("save completed root");
    let destination = LocalWorkService::new(
        destination_database.clone(),
        project,
        "review-agent".into(),
        SessionId("review-session".into()),
        Some("protocol-test".into()),
    );
    destination
        .load_work_graph_snapshot(
            &serde_json::to_vec_pretty(&snapshot.document).expect("snapshot bytes"),
            false,
            at(4),
        )
        .expect("load completed root");
    // Three late findings on the restored completion: a note and two gates.
    destination
        .work_note_on(Some(&root.short_ref), "late detail", &[], at(5))
        .expect("late note");
    destination
        .work_gate_on(
            Some(&root.short_ref),
            "cargo-test",
            &["late::x".into()],
            None,
            at(6),
        )
        .expect("late failed gate");
    destination
        .work_gate_on(Some(&root.short_ref), "cargo-test", &[], None, at(7))
        .expect("late passing gate");

    let connection = rusqlite::Connection::open(&destination_database).unwrap();
    let healthy_rows = evidence_rows(&connection);
    assert_eq!(healthy_rows.len(), 3, "one row per late finding");
    let healthy_objects = objects(&connection);
    let original = |evidence: &str| {
        healthy_objects
            .iter()
            .find(|(id, _)| id == evidence)
            .unwrap()
            .1
            .clone()
    };
    let ids: Vec<String> = healthy_rows.iter().map(|row| row.0.clone()).collect();
    // One evidence object names a missing item, one does not decode, and one
    // breaks the projection's sequence constraint.
    let (missing_item, malformed, constrained) = (&ids[0], &ids[1], &ids[2]);
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
    connection
        .execute(
            "UPDATE objects SET canonical_json =
                 CAST(json_set(canonical_json, '$.sequence', 0) AS BLOB)
             WHERE object_id = ?1",
            [constrained],
        )
        .unwrap();
    let damaged_objects = objects(&connection);
    let damaged_rows = evidence_rows(&connection);

    let error = SqliteStore::repair_rebuildable_projections(&destination_database)
        .expect_err("the repair refuses")
        .to_string();
    for finding in [
        format!("work_restored_evidence:{missing_item}:missing_item"),
        format!("work_restored_evidence:{malformed}:decode:"),
        format!("work_restored_evidence:{constrained}:projection_constraint"),
    ] {
        assert!(error.contains(&finding), "{finding}: {error}");
    }
    for evidence in [missing_item, malformed, constrained] {
        assert!(
            !error.contains(&format!(
                "work_restored_evidence:{evidence}:missing_projection"
            )),
            "the evidence's own finding names it once: {error}"
        );
    }
    // Nothing the repair wrote survives, and no object was rewritten or
    // deleted.
    assert_eq!(objects(&connection), damaged_objects);
    assert_eq!(evidence_rows(&connection), damaged_rows);

    // With the original bytes back, the repair rebuilds every row and
    // reports health.
    for evidence in [missing_item, malformed, constrained] {
        connection
            .execute(
                "UPDATE objects SET canonical_json = ?1 WHERE object_id = ?2",
                rusqlite::params![original(evidence), evidence],
            )
            .unwrap();
    }
    assert_eq!(objects(&connection), healthy_objects);
    let report =
        SqliteStore::repair_rebuildable_projections(&destination_database).expect("repair");
    assert!(report.is_healthy(), "{report:?}");
    assert_eq!(evidence_rows(&connection), healthy_rows);
}
