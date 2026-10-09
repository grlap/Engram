//! Explicit measurements only; the copied store may contain private state.
use super::*;
use crate::storage::work::record_windows::{WorkRecordContent, WorkRecordKind};
use sha2::{Digest, Sha256};
use std::{collections::BTreeMap, path::PathBuf, time::Instant};

fn snapshot_path() -> PathBuf {
    let path = PathBuf::from(std::env::var("ENGRAM_NOTE_SEARCH_SNAPSHOT").unwrap())
        .canonicalize()
        .unwrap();
    let scratch = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("target/tmp")
        .canonicalize()
        .unwrap();
    assert!(
        path.starts_with(&scratch),
        "copy must stay inside this worktree's target/tmp"
    );
    path
}

fn file_digest(path: &std::path::Path) -> String {
    let mut digest = Sha256::new();
    std::io::copy(&mut std::fs::File::open(path).unwrap(), &mut digest).unwrap();
    format!("{:x}", digest.finalize())
}

#[test]
#[ignore = "manual live-store preflight: supply the confirmed source path"]
fn note_search_backup_source_preflight() {
    let path = PathBuf::from(std::env::var("ENGRAM_NOTE_SEARCH_SOURCE").unwrap());
    let store = SqliteStore::open_existing_read_only(&path).unwrap();
    assert!(store.connection.is_readonly("main").unwrap());
    assert!(
        store
            .connection
            .pragma_query_value(None, "query_only", |row| row.get::<_, bool>(0))
            .unwrap()
    );
    let journal: String = store
        .connection
        .pragma_query_value(None, "journal_mode", |row| row.get(0))
        .unwrap();
    assert_eq!(
        journal, "wal",
        "ordinary backup opener must not change journal mode"
    );
    println!(
        "backup source admission: read_only=true, query_only=true, journal_mode={journal}; no rows written"
    );
}

#[test]
#[ignore = "manual query-only copy measurement: supply snapshot, project, text and time"]
fn note_search_live_copy_measurement() {
    let path = snapshot_path();
    let before = file_digest(&path);
    let project = ProjectId(std::env::var("ENGRAM_NOTE_SEARCH_PROJECT").unwrap());
    let now = DateTime::parse_from_rfc3339(&std::env::var("ENGRAM_NOTE_SEARCH_AT").unwrap())
        .unwrap()
        .with_timezone(&Utc);
    let text = std::env::var("ENGRAM_NOTE_SEARCH_TEXT").unwrap();
    let store = SqliteStore::open_existing_read_only(&path).unwrap();
    assert!(store.connection.is_readonly("main").unwrap());
    assert!(
        store
            .connection
            .pragma_query_value(None, "query_only", |row| row.get::<_, bool>(0))
            .unwrap()
    );
    let query = WorkCatalogQuery {
        search: Some(text),
        lifecycles: vec![WorkLifecycle::Open],
        limit: 2,
        ..WorkCatalogQuery::default()
    };
    cost::start();
    reset_work_catalog_count_queries();
    let started = Instant::now();
    let (first, total, _, _, fingerprint, _) = store
        .query_work_catalog_continuation(&project, now, &query, None)
        .unwrap();
    let first_elapsed = started.elapsed().as_micros();
    let first_cost = cost::finish();
    let first_classified = work_catalog_classified_queries();
    let anchor = first
        .next_after
        .expect("measurement requires a real second page");
    let continuation = WorkCatalogQuery {
        after: Some(anchor),
        ..query.clone()
    };
    cost::start();
    reset_work_catalog_count_queries();
    let started = Instant::now();
    let (second, second_total, preceding, _, second_fingerprint, _) = store
        .query_work_catalog_continuation(
            &project,
            now,
            &continuation,
            Some(ListingExpectation::Membership {
                observed_at: now,
                fingerprint: &fingerprint,
            }),
        )
        .unwrap();
    let second_elapsed = started.elapsed().as_micros();
    let second_cost = cost::finish();
    let second_classified = work_catalog_classified_queries();
    assert_eq!(second_total, total);
    assert_eq!(second_fingerprint, fingerprint);
    assert_eq!(preceding, first.items.len());
    assert_eq!(
        second.items.len(),
        (total - first.items.len()).min(usize::try_from(query.limit).unwrap())
    );
    assert!(second.items.iter().all(|row| {
        first
            .items
            .iter()
            .all(|old| old.work.work_id != row.work.work_id)
    }));

    // Describe the actual eligible canonical workload only after timing,
    // without warming the note scan before its initial page.
    let mut statement = store.connection.prepare(
        "SELECT work_id FROM work_items WHERE project_id = ?1 AND lifecycle = 'open' ORDER BY work_id"
    ).unwrap();
    let ids = statement
        .query_map([project.0.as_str()], |row| row.get::<_, String>(0))
        .unwrap()
        .map(|row| parse_work_id(&row.unwrap()).unwrap())
        .collect::<Vec<_>>();
    let mut families = BTreeMap::<String, usize>::new();
    let mut indexed_records = 0;
    let mut public_text_bytes = 0;
    let mut inherited_members = 0;
    for id in &ids {
        for entry in store
            .work_record_index(&project, *id, WorkRecordKind::NotesWithGates)
            .unwrap()
        {
            indexed_records += 1;
            inherited_members += usize::from(entry.address.member.is_some());
            *families
                .entry(
                    serde_json::to_value(entry.record_family)
                        .unwrap()
                        .as_str()
                        .unwrap()
                        .into(),
                )
                .or_default() += 1;
            let WorkRecordContent::Note(note) = store
                .work_record_content_for_search(&project, *id, &entry)
                .unwrap()
            else {
                panic!("note index returned history")
            };
            public_text_bytes +=
                note.summary.len() + note.refs.iter().map(String::len).sum::<usize>();
            if let Some(gate) = note.gate {
                public_text_bytes +=
                    gate.name.len() + gate.failed.iter().map(String::len).sum::<usize>();
            }
        }
    }
    let object_count: i64 = store
        .connection
        .query_row("SELECT COUNT(*) FROM objects", [], |row| row.get(0))
        .unwrap();
    drop(statement);
    drop(store);
    assert_eq!(
        file_digest(&path),
        before,
        "query-only measurement preserves snapshot bytes"
    );
    let result = serde_json::json!({
        "producer": crate::build_identity::current(), "snapshot_sha256": before,
        "snapshot_file_bytes": std::fs::metadata(&path).unwrap().len(),
        "source_session": std::env::var("TERMAL_SESSION_ID").ok(),
        "project": project, "observed_at": now, "filters": query,
        "workload": {"store_objects": object_count, "eligible_open_items": ids.len(),
            "indexed_note_gate_records": indexed_records, "families": families,
            "inherited_members": inherited_members, "public_text_bytes": public_text_bytes},
        "total_matches": total,
        "initial": {"elapsed_us": first_elapsed, "rows": first.items.len(), "cost": first_cost,
            "classified_queries": first_classified},
        "continuation": {"elapsed_us": second_elapsed, "rows": second.items.len(),
            "preceding": preceding, "cost": second_cost, "classified_queries": second_classified},
        "coverage": "Storage query timing after read-only schema admission; continuation reuses that connection. SQL cost labels cover attributed statements only, not all SQL; typed work-object kinds cover that reader only. Canonical decode total covers all canonical readers. Workload describes the entire open-item scope after timing, not just notes visited before first match. No CLI startup/transport timing."
    });
    let output = path.with_extension("measurement.json");
    assert!(
        !output.exists(),
        "do not replace earlier measurement evidence"
    );
    std::fs::write(&output, serde_json::to_vec_pretty(&result).unwrap()).unwrap();
    println!(
        "note-search live-copy measurement: {}",
        serde_json::to_string(&result).unwrap()
    );
}
