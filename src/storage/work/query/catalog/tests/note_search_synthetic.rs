//! Generated canonical histories only: this never opens or copies a live store.
use super::*;
use crate::domain::{GateEvidenceRecord, SCHEMA_VERSION, WorkEvidence};
use crate::storage::work::feeds::append_to_work_feeds;
use std::time::Instant;

/// Bulk-load typed immutable evidence, its projections and dense feeds.
/// This avoids timing thousands of unrelated claim/root-contribution writes.
/// The measured reader searches the same normalized public-note index that
/// ordinary writers maintain; setup is outside the measured call.
fn seed_history(store: &mut SqliteStore, items: usize, records: usize) -> (ProjectId, usize) {
    let project = ProjectId("synthetic-note-search".into());
    let mut subjects = Vec::new();
    for index in 0..items {
        let item = store
            .create_work(
                &root_request(&project.0, &format!("subject-{index}"), 0),
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        let held = claim(store, &item, "holder", &format!("held-{index}"), 1, 600);
        subjects.push((item, held));
    }
    let transaction = store.connection.transaction().unwrap();
    let mut canonical_bytes = 0;
    for (item, held) in &subjects {
        for index in 0..records {
            let is_gate = index % 4 == 0;
            let evidence = WorkEvidence {
                schema_version: SCHEMA_VERSION,
                work_id: item.work_id,
                run_id: held.run_id,
                claim_id: held.claim_id,
                claim_fence: held.fence,
                summary: format!("{} record-{index}", "generated history text ".repeat(48)),
                refs: vec![format!("fixture/subject/{}/record/{index}", item.work_id.0)],
                gate: is_gate.then(|| GateEvidenceRecord {
                    schema_version: SCHEMA_VERSION,
                    name: "synthetic gate".into(),
                    passed: false,
                    failed: vec!["synthetic failure".into()],
                    previous: None,
                }),
                actor: actor("holder"),
                created_at: at(2),
            };
            crate::domain::validate_gate_evidence_payload(&evidence).unwrap();
            let object = crate::CanonicalObject::mint(&evidence).unwrap();
            canonical_bytes += object.bytes().len();
            SqliteStore::insert_object(&transaction, "work_evidence", &object).unwrap();
            transaction.execute(
                "INSERT INTO work_run_evidence (evidence_id, work_id, run_id) VALUES (?1, ?2, ?3)",
                params![object.key().as_str(), item.work_id.0.to_string(), held.run_id.0.to_string()],
            ).unwrap();
            let positions = append_to_work_feeds(
                &transaction,
                &project,
                item.root_id,
                Some(held.run_id),
                None,
                "work_evidence",
                &object,
            )
            .unwrap();
            crate::storage::work::note_index::append_native(
                &transaction,
                &positions,
                object.key(),
                &crate::storage::work::note_index::NativeNote::Evidence(&evidence),
            )
            .unwrap();
        }
    }
    transaction.commit().unwrap();
    (project, canonical_bytes)
}

#[test]
#[ignore = "explicit generated-history measurement; no live store or source path"]
fn note_search_synthetic_history_measurement() {
    const RECORDS: usize = 256;
    let items = std::env::var("ENGRAM_SYNTHETIC_SEARCH_ITEMS")
        .ok()
        .map_or(512, |value| value.parse::<usize>().unwrap());
    assert!((1..=2048).contains(&items));
    let home = crate::test_support::temp_home().unwrap();
    let database = home.path().join("synthetic-search.db");
    let started = Instant::now();
    let mut writer = SqliteStore::open(&database).unwrap();
    let (project, bytes) = seed_history(&mut writer, items, RECORDS);
    let indexed: i64 = writer
        .connection
        .query_row("SELECT COUNT(*) FROM work_note_fts", [], |row| row.get(0))
        .unwrap();
    assert_eq!(usize::try_from(indexed).unwrap(), items * RECORDS);
    let generation_us = started.elapsed().as_micros();
    drop(writer);
    let open_started = Instant::now();
    let reader = SqliteStore::open_existing_read_only(&database).unwrap();
    let open_us = open_started.elapsed().as_micros();
    assert!(reader.connection.is_readonly("main").unwrap());
    for (case, query, expected) in [
        ("absent", "absent synthetic needle", 0),
        ("present-common", "generated history text", items),
    ] {
        cost::start();
        let started = Instant::now();
        let result = reader.query_work_catalog_continuation(
            &project,
            at(3),
            &WorkCatalogQuery {
                search: Some(query.into()),
                limit: 1,
                ..WorkCatalogQuery::default()
            },
            None,
        );
        let scan_us = started.elapsed().as_micros();
        let counters = cost::finish();
        let outcome = match result {
            Ok((page, count, _, _, _, _)) => {
                assert_eq!(count, expected);
                assert_eq!(page.items.len(), expected.min(1));
                serde_json::json!({"completed": true, "matches": count})
            }
            Err(error @ StoreError::SearchBudgetExhausted { .. }) => {
                serde_json::json!({"completed": false, "refusal": crate::verbs::store_error_value(&error)})
            }
            Err(error) => panic!("unexpected search failure: {error}"),
        };
        println!(
            "synthetic note search: {}",
            serde_json::json!({
                "case": case, "query": query, "result_limit": 1,
                "items": items, "records_per_item": RECORDS, "records": items * RECORDS,
                "gate_records": items * RECORDS / 4, "canonical_evidence_bytes": bytes,
                "generation_us": generation_us, "read_only_open_us": open_us,
                "storage_scan_us": scan_us, "cost": counters, "outcome": outcome,
                "build": crate::build_identity::current(),
                "coverage": "generated upper workload bracket, not live-equivalent; storage call after independent read-only admission; no shared writable service mutex, MCP queue or transport in this measurement; reported SQL labels are partial attribution",
            })
        );
    }
}
