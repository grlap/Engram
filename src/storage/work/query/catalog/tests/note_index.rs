use super::*;
use crate::storage::work::record_windows::{WorkRecordContent, WorkRecordKind};
use std::collections::BTreeMap;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

fn rebuild_vm_steps(store: &SqliteStore) -> usize {
    store
        .connection
        .execute("DELETE FROM work_note_fts", [])
        .unwrap();
    let steps = Arc::new(AtomicUsize::new(0));
    let observed = Arc::clone(&steps);
    store
        .connection
        .progress_handler(
            1,
            Some(move || {
                observed.fetch_add(1, Ordering::Relaxed);
                false
            }),
        )
        .unwrap();
    let result = crate::storage::work::note_index::rebuild(&store.connection);
    store
        .connection
        .progress_handler(0, None::<fn() -> bool>)
        .unwrap();
    result.unwrap();
    steps.load(Ordering::Relaxed)
}

#[test]
fn note_index_rebuild_vm_work_grows_linearly_with_unrelated_project_feed_entries() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let project = "note-build-work";
    for index in 0..8 {
        let mut request = root_request(project, &format!("noted-{index}"), 0);
        request.notes = vec!["retained public note".into()];
        store
            .create_work(&request, &DevelopmentNoopRedactor)
            .unwrap();
    }
    let before = i64::try_from(rebuild_vm_steps(&store)).unwrap();
    let before_items: i64 = store
        .connection
        .query_row("SELECT COUNT(*) FROM work_items", [], |row| row.get(0))
        .unwrap();
    for index in 0..128 {
        store
            .create_work(
                &root_request(project, &format!("unrelated-{index}"), 0),
                &DevelopmentNoopRedactor,
            )
            .unwrap();
    }
    let after = i64::try_from(rebuild_vm_steps(&store)).unwrap();
    let after_items: i64 = store
        .connection
        .query_row("SELECT COUNT(*) FROM work_items", [], |row| row.get(0))
        .unwrap();
    // Allow twice the observed per-item work. Repeated scans of the growing
    // project feed for each item exceed this linear allowance.
    assert!(
        after * before_items <= before * after_items * 2,
        "rebuild VM steps grew from {before} to {after} for {before_items} to {after_items} items"
    );
    let notes: i64 = store
        .connection
        .query_row("SELECT COUNT(*) FROM work_note_fts", [], |row| row.get(0))
        .unwrap();
    assert_eq!(notes, before_items);
}

fn no_match_service_cost(database: &std::path::Path, project: &ProjectId) -> serde_json::Value {
    let service = crate::work_service::LocalWorkService::new(
        database.to_path_buf(),
        project.clone(),
        "note-cost".into(),
        crate::SessionId("note-cost-reader".into()),
        None,
    );
    crate::storage::work::cost::start();
    let page = service
        .work_catalog_page(
            &WorkCatalogQuery {
                search: Some("absent-generated-note-cost-needle".into()),
                ..WorkCatalogQuery::default()
            },
            None,
            None,
            at(30),
        )
        .unwrap();
    let cost = crate::storage::work::cost::finish();
    assert_eq!(page.total, 0);
    assert_eq!(cost["total"]["note_index_visits"], 0);
    assert_eq!(
        cost["total"]["typed_work_object_decodes"]
            .as_object()
            .unwrap()
            .len(),
        0
    );
    cost
}

#[test]
fn note_index_no_match_service_decodes_stay_at_admission_cost_as_notes_grow() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("note-cost.db");
    let project = ProjectId("note-cost-growth".into());
    let mut store = SqliteStore::open(&database).unwrap();
    let mut request = root_request(&project.0, "initial", 0);
    request.notes = vec!["initial public note".into()];
    store
        .create_work(&request, &DevelopmentNoopRedactor)
        .unwrap();
    // Match the copy protocol's readiness precondition. The first admission
    // constructs a process-wide schema reference; that one-time bootstrap
    // must not be confused with a change caused by growing note history.
    SqliteStore::readiness(&database, None).unwrap();
    let first = no_match_service_cost(&database, &project);
    for index in 0..32 {
        let mut request = root_request(&project.0, &format!("additional-{index}"), 0);
        request.notes = vec!["additional public note".into()];
        store
            .create_work(&request, &DevelopmentNoopRedactor)
            .unwrap();
    }
    let second = no_match_service_cost(&database, &project);
    assert_eq!(
        second["total"]["canonical_decodes"],
        first["total"]["canonical_decodes"]
    );
}

/// The retired scan is a test oracle only. Derive results from the same
/// canonical fixture at runtime, including the first locator and family.
fn scan(
    store: &SqliteStore,
    project: &ProjectId,
    text: &str,
) -> BTreeMap<uuid::Uuid, WorkNoteSearchMatch> {
    let mut matches = BTreeMap::new();
    let needle = normalize_work_catalog_key(text);
    let ids = store
        .connection
        .prepare("SELECT work_id FROM work_items WHERE project_id = ?1 ORDER BY work_id")
        .unwrap()
        .query_map([project.0.as_str()], |row| row.get::<_, String>(0))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    for id in ids {
        let work = parse_work_id(&id).unwrap();
        for entry in store
            .work_record_index(project, work, WorkRecordKind::NotesWithGates)
            .unwrap()
        {
            let WorkRecordContent::Note(note) = store
                .work_record_content_for_search(project, work, &entry)
                .unwrap()
            else {
                panic!("note index returned history");
            };
            if note
                .search_fields()
                .iter()
                .any(|field| field.contains(&needle))
            {
                matches.insert(
                    work.0,
                    WorkNoteSearchMatch {
                        locator: entry.locator,
                        family: entry.record_family,
                    },
                );
                break;
            }
        }
    }
    matches
}

fn assert_equivalent(store: &SqliteStore, project: &ProjectId) {
    for text in [
        "STRASSE",
        "cafe\u{301}",
        "é",
        "ss",
        "x",
        "path/α",
        "quoted \"phrase\"",
        "literal.*",
        "left\nright",
        "nul\0tail",
        "after-nul",
        "note-gate",
        "failed-label",
        "absent-zqxjk",
    ] {
        let expected = scan(store, project, text);
        let query = WorkCatalogQuery {
            search: Some(text.into()),
            limit: 100,
            ..WorkCatalogQuery::default()
        };
        crate::canonical::reset_canonical_decode_count();
        let actual = store.catalog_note_matches(project, at(30), &query).unwrap();
        assert_eq!(crate::canonical::canonical_decode_count(), 0, "{text:?}");
        assert_eq!(
            serde_json::to_value(&actual).unwrap(),
            serde_json::to_value(&expected).unwrap(),
            "{text:?}"
        );
        let page = store.query_work_catalog(project, at(30), &query).unwrap();
        // Titles deliberately contain no test phrase, so every member came
        // from a note. Ordinary catalog order is ascending work identity.
        assert_eq!(
            page.items
                .iter()
                .map(|row| row.work.work_id.0)
                .collect::<Vec<_>>(),
            expected.keys().copied().collect::<Vec<_>>(),
            "{text:?}"
        );
    }
}

#[test]
fn note_index_matches_canonical_scan_across_native_inherited_and_restored_records() {
    let mut source = SqliteStore::open_in_memory().unwrap();
    let project = ProjectId("projection-equivalence".into());
    let mut subjects = Vec::new();
    for index in 0..2 {
        let mut request = root_request(&project.0, &format!("subject-{index}"), 0);
        request.notes = vec!["Straße Café path/α quoted \"phrase\" literal.* x".into()];
        let work = source
            .create_work(&request, &DevelopmentNoopRedactor)
            .unwrap();
        let held = claim(
            &mut source,
            &work,
            "holder",
            &format!("claim-{index}"),
            1,
            600,
        );
        let work = source.get_work_item(work.work_id).unwrap();
        source
            .record_work_evidence(
                &RecordWorkEvidenceRequest {
                    work_id: work.work_id,
                    run_id: held.run_id,
                    expected_work_revision: work.revision,
                    holder: held.holder.clone(),
                    claim_id: held.claim_id,
                    claim_fence: held.fence,
                    summary: "Straße nul\0tail after-nul left".into(),
                    refs: vec!["right".into()],
                    actor: actor("holder"),
                    idempotency_key: format!("literal-note-{index}"),
                    recorded_at: at(2),
                },
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        source
            .record_gate_evidence(
                &crate::domain::RecordGateEvidenceRequest {
                    work_id: work.work_id,
                    run_id: held.run_id,
                    expected_work_revision: work.revision,
                    holder: held.holder.clone(),
                    claim_id: held.claim_id,
                    claim_fence: held.fence,
                    name: "note-gate".into(),
                    failed: vec!["failed-label".into()],
                    evidence_ref: Some("right".into()),
                    actor: actor("holder"),
                    recorded_at: at(2),
                },
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        host_verification(
            &mut source,
            &work,
            &held,
            "holder",
            &format!("verify-{index}"),
            VerificationKind::Test,
            VerificationResult::Passed,
            3,
        );
        if index == 0 {
            let proof = evidence(&mut source, &work, &held, "holder", "proof", 4);
            checkpoint(
                &mut source,
                &work,
                &held,
                "holder",
                "checkpoint",
                5,
                std::slice::from_ref(&proof),
            );
            complete(&mut source, &work, &held, "holder", &proof, "complete", 6).unwrap();
        }
        subjects.push(work);
    }
    assert_equivalent(&source, &project);
    let report = source.verify_all().unwrap();
    assert!(
        report.is_healthy(),
        "equivalence fixture must be valid: {report:?}"
    );
    let saved = source
        .save_work_graph_snapshot(
            &project,
            &actor("saver"),
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(8),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let mut restored = SqliteStore::open_in_memory().unwrap();
    restored
        .load_work_graph_snapshot(
            &project,
            &actor("loader"),
            &serde_json::to_vec(&saved.document).unwrap(),
            false,
            at(9),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let closed = restored.get_work_item(subjects[0].work_id).unwrap();
    let mut late_actor = actor("late");
    late_actor
        .provenance_chain
        .push(crate::domain::ProvenanceLink {
            relation: crate::domain::ProvenanceRelation::DerivedFrom,
            source: crate::domain::POST_COMPLETION_EVIDENCE_PROVENANCE_SOURCE.into(),
            reference: Some(crate::domain::POST_COMPLETION_EVIDENCE_PROVENANCE_REFERENCE.into()),
        });
    let request = crate::domain::RecordRestoredWorkEvidenceRequest {
        work_id: closed.work_id,
        expected_work_revision: closed.revision,
        holder: SessionId("late".into()),
        input: crate::domain::RestoredWorkEvidenceInput::Note {
            status: false,
            summary: "Straße Café quoted \"phrase\"".into(),
            refs: vec!["path/α".into()],
        },
        actor: late_actor,
        idempotency_key: "late-note".into(),
        recorded_at: at(10),
    };
    let count = || {
        restored
            .connection
            .query_row("SELECT COUNT(*) FROM work_note_fts", [], |row| {
                row.get::<_, i64>(0)
            })
            .unwrap()
    };
    let before = count();
    let late = restored
        .record_restored_work_evidence(&request, &DevelopmentNoopRedactor)
        .unwrap();
    assert_eq!(
        restored
            .record_restored_work_evidence(&request, &DevelopmentNoopRedactor)
            .unwrap(),
        late
    );
    let after: i64 = restored
        .connection
        .query_row("SELECT COUNT(*) FROM work_note_fts", [], |row| row.get(0))
        .unwrap();
    assert_eq!(after, before + 1, "replay never appends another index row");
    assert_equivalent(&restored, &project);
    // Repair derives exactly the incremental rows from the same immutable
    // records. A repeat of the equivalence test exercises all rebuilt fields.
    assert_eq!(
        crate::storage::work::schema::repair_rebuildable_schema_on(&restored.connection).unwrap(),
        Vec::<String>::new()
    );
    assert_equivalent(&restored, &project);
}

#[test]
fn note_index_missing_projection_requires_repair_and_bad_rows_are_findings() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("note-index.db");
    let mut store = SqliteStore::open(&database).unwrap();
    let mut request = root_request("note-index-repair", "subject", 0);
    request.notes = vec!["index repair needle".into()];
    let item = store
        .create_work(&request, &DevelopmentNoopRedactor)
        .unwrap();
    let original: String = store
        .connection
        .query_row("SELECT fields_json FROM work_note_fts", [], |row| {
            row.get(0)
        })
        .unwrap();
    for sql in [
        "UPDATE work_note_fts SET fields_json = '[]'",
        "UPDATE work_note_fts SET family = NULL",
        "INSERT INTO work_note_fts SELECT locator,work_id,family,layer,generation,position,fields_json,search_text FROM work_note_fts",
        "DELETE FROM work_note_fts",
    ] {
        store
            .connection
            .execute_batch("SAVEPOINT damaged_index")
            .unwrap();
        store.connection.execute_batch(sql).unwrap();
        assert!(!store.verify_all().unwrap().is_healthy(), "{sql}");
        store
            .connection
            .execute_batch("ROLLBACK TO damaged_index; RELEASE damaged_index")
            .unwrap();
    }
    store
        .connection
        .execute_batch("DROP TABLE work_note_fts")
        .unwrap();
    drop(store);
    let error = SqliteStore::readiness(&database, None).unwrap_err();
    assert_eq!(
        crate::storage::store_open_refusal_kind(&error),
        crate::storage::StoreOpenRefusalKind::ProjectionRepairRequired
    );
    assert!(
        SqliteStore::repair_rebuildable_projections(&database)
            .unwrap()
            .is_healthy()
    );
    SqliteStore::readiness(&database, None).unwrap();
    let store = SqliteStore::open(&database).unwrap();
    assert_eq!(
        store
            .connection
            .query_row("SELECT fields_json FROM work_note_fts", [], |row| row
                .get::<_, String>(0))
            .unwrap(),
        original
    );
    assert_equivalent(&store, &item.project_id);
}

/// Snapshot exact SQLite values (including text/blob bytes) and schema SQL.
/// New note-index pages necessarily change the file layout, not other rows.
fn non_note_projection_snapshot(
    connection: &rusqlite::Connection,
) -> BTreeMap<String, Vec<Vec<rusqlite::types::Value>>> {
    let definitions = connection
        .prepare(
            "SELECT type, name, tbl_name, sql FROM sqlite_schema
         WHERE name != 'work_note_fts' AND name NOT GLOB 'work_note_fts_*'
         ORDER BY type, name",
        )
        .unwrap()
        .query_map([], |row| (0..4).map(|index| row.get(index)).collect())
        .unwrap()
        .collect::<Result<Vec<Vec<rusqlite::types::Value>>, _>>()
        .unwrap();
    let tables = definitions
        .iter()
        .filter_map(|row| match (&row[0], &row[1]) {
            (rusqlite::types::Value::Text(kind), rusqlite::types::Value::Text(name))
                if kind == "table" =>
            {
                Some(name.clone())
            }
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut snapshot = BTreeMap::from([("sqlite_schema".to_owned(), definitions)]);
    for table in tables {
        let mut statement = connection
            .prepare(&format!("SELECT * FROM \"{}\"", table.replace('"', "\"\"")))
            .unwrap();
        let columns = statement.column_count();
        let mut rows = statement
            .query_map([], |row| {
                (0..columns)
                    .map(|index| row.get::<_, rusqlite::types::Value>(index))
                    .collect()
            })
            .unwrap()
            .collect::<Result<Vec<Vec<_>>, _>>()
            .unwrap();
        rows.sort_by_cached_key(|row| format!("{row:?}"));
        snapshot.insert(table, rows);
    }
    snapshot
}

#[test]
fn note_index_targeted_build_preserves_every_other_projection() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("targeted.db");
    let mut store = SqliteStore::open(&database).unwrap();
    let mut request = root_request("targeted-note-index", "targeted subject", 0);
    request.notes = vec!["STRASSE index needle".into()];
    let item = store
        .create_work(&request, &DevelopmentNoopRedactor)
        .unwrap();
    store
        .connection
        .execute_batch("DROP TABLE work_note_fts")
        .unwrap();
    let before = non_note_projection_snapshot(&store.connection);
    drop(store);
    assert!(SqliteStore::open_existing_read_only(&database).is_err());
    assert_eq!(SqliteStore::build_note_search_index(&database).unwrap(), 1);
    SqliteStore::readiness(&database, None).unwrap();
    let store = SqliteStore::open_existing_read_only(&database).unwrap();
    assert_eq!(non_note_projection_snapshot(&store.connection), before);
    assert_equivalent(&store, &item.project_id);
    drop(store);
    let bytes = std::fs::read(&database).unwrap();
    let error = SqliteStore::build_note_search_index(&database).unwrap_err();
    assert!(error.to_string().contains("doctor --repair-projections"));
    assert_eq!(std::fs::read(&database).unwrap(), bytes);
}

#[test]
fn note_index_targeted_build_refuses_damaged_target_and_unrelated_schema() {
    let directory = crate::test_support::temp_home().unwrap();
    for (case, damage) in [
        ("damaged-target", "CREATE TABLE work_note_fts (wrong TEXT)"),
        (
            "orphan-target",
            "CREATE TABLE work_note_fts_data (wrong BLOB)",
        ),
        ("other-projection", "DROP INDEX work_items_ready"),
    ] {
        let database = directory.path().join(format!("{case}.db"));
        let store = SqliteStore::open(&database).unwrap();
        store
            .connection
            .execute_batch("DROP TABLE work_note_fts")
            .unwrap();
        store.connection.execute_batch(damage).unwrap();
        let before = non_note_projection_snapshot(&store.connection);
        drop(store);
        assert!(
            SqliteStore::build_note_search_index(&database).is_err(),
            "{case}"
        );
        let connection = rusqlite::Connection::open(&database).unwrap();
        assert_eq!(non_note_projection_snapshot(&connection), before, "{case}");
        let target: i64 = connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_schema WHERE name = 'work_note_fts'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(target, i64::from(case == "damaged-target"));
    }
}

#[test]
fn note_index_targeted_build_rolls_back_on_canonical_note_failure() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("canonical-refusal.db");
    let mut store = SqliteStore::open(&database).unwrap();
    let mut request = root_request("targeted-note-rollback", "subject", 0);
    request.notes = vec!["canonical note".into()];
    store
        .create_work(&request, &DevelopmentNoopRedactor)
        .unwrap();
    let (id, bytes): (String, Vec<u8>) = store
        .connection
        .query_row(
            "SELECT object_id, canonical_json FROM objects WHERE object_kind = 'work_observation'",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE objects SET canonical_json = X'7b7d' WHERE object_id = ?1",
            [&id],
        )
        .unwrap();
    store
        .connection
        .execute_batch("DROP TABLE work_note_fts")
        .unwrap();
    let before = non_note_projection_snapshot(&store.connection);
    drop(store);
    assert!(SqliteStore::build_note_search_index(&database).is_err());
    let connection = rusqlite::Connection::open(&database).unwrap();
    assert_eq!(non_note_projection_snapshot(&connection), before);
    let target: bool = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE name = 'work_note_fts')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert!(!target);
    connection
        .execute(
            "UPDATE objects SET canonical_json = ?2 WHERE object_id = ?1",
            rusqlite::params![id, bytes],
        )
        .unwrap();
    drop(connection);
    assert_eq!(SqliteStore::build_note_search_index(&database).unwrap(), 1);
    SqliteStore::readiness(&database, None).unwrap();
}
