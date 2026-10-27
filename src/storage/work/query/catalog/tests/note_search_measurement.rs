//! Explicit measurements only; the copied store may contain private state.

/// Input is an old-build export/import copy, never the operational database.
/// Read the recorded CLI cutover, then time one all-items query.
#[test]
#[ignore = "acceptance measurement on an Engram export/import copy under target/tmp"]
#[expect(
    clippy::assertions_on_constants,
    reason = "the manual acceptance measurement must refuse an unoptimized runner"
)]
fn note_index_copy_no_match_measurement() {
    assert!(
        !cfg!(debug_assertions),
        "use the optimized measurement build"
    );
    let path = snapshot_path();
    let project = ProjectId(std::env::var("ENGRAM_NOTE_SEARCH_PROJECT").unwrap());
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let copy_home =
        std::fs::canonicalize(std::env::var_os("ENGRAM_NOTE_SEARCH_HOME").unwrap()).unwrap();
    assert!(copy_home.starts_with(root.join("target/tmp").canonicalize().unwrap()));
    assert_eq!(
        crate::project_database_path(&copy_home, &project)
            .canonicalize()
            .unwrap(),
        path
    );
    let cutover: serde_json::Value =
        serde_json::from_slice(&std::fs::read(copy_home.join("cutover.json")).unwrap()).unwrap();
    assert_eq!(
        PathBuf::from(cutover["database"].as_str().unwrap())
            .canonicalize()
            .unwrap(),
        path
    );
    assert_eq!(cutover["before"]["ready"], false);
    assert_eq!(cutover["before"]["code"], "projection_repair_required");
    assert_eq!(cutover["repair"]["healthy"], true);
    assert_eq!(cutover["repair"]["mode"], "note_search_index_build");
    assert_eq!(cutover["repair"]["full_audit"], "not_run");
    assert_eq!(cutover["after"]["ready"], true);
    SqliteStore::readiness(&path, None).unwrap();
    let before = file_digest(&path);
    crate::canonical::reset_canonical_decode_count();
    let store = SqliteStore::open_existing_read_only(&path).unwrap();
    let admission_decodes = crate::canonical::canonical_decode_count();
    let items: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM work_items WHERE project_id = ?1",
            [project.0.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    let notes: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM work_note_fts note
        JOIN work_items item ON item.work_id = note.work_id WHERE item.project_id = ?1",
            [project.0.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    let service = crate::work_service::LocalWorkService::new(
        path.clone(),
        project.clone(),
        "copy-measurement".into(),
        crate::SessionId("note-index-copy-measurement".into()),
        None,
    );
    let query = WorkCatalogQuery {
        search: Some("note-index-absent-zqxjk-vbp".into()),
        limit: 20,
        ..WorkCatalogQuery::default()
    };
    cost::start();
    let started = Instant::now();
    let page =
        crate::storage::search_budget::Request::new(tokio_util::sync::CancellationToken::new())
            .run(|| Ok(service.work_catalog_page(&query, None, None, Utc::now())?))
            .unwrap();
    let elapsed = started.elapsed();
    let counters = cost::finish();
    drop(store);
    let after = file_digest(&path);
    println!(
        "note-index copy acceptance: {}",
        serde_json::json!({
            "copy": path, "copy_sha256": before, "copy_bytes": std::fs::metadata(&path).unwrap().len(),
            "project": project, "items": items, "indexed_note_records": notes,
            "elapsed_us": elapsed.as_micros(), "cost": counters,
            "outcome": if page.total == 0 { "no_match" } else { "matched" },
            "matches": page.total, "read_admission_canonical_decodes": admission_decodes,
            "database_unchanged": after == before,
            "before_repair": "projection_repair_required", "after_repair": "ready_and_healthy",
            "cutover": cutover,
            "build": crate::build_identity::current(),
            "coverage": "One all-lifecycle service catalog call, request allowance and read admission included; index freshly rebuilt from an old-build export/import copy. No live-store search, CLI startup, MCP scheduling or idle-host assertion."
        })
    );
    assert_eq!(page.total, 0);
    assert!(page.items.is_empty());
    // The complete service call opens its own read-only connection. Policy
    // admission is required; note search must add no canonical body decodes.
    assert_eq!(
        counters["total"]["canonical_decodes"],
        serde_json::json!(admission_decodes)
    );
    assert_eq!(counters["total"]["note_index_visits"], 0);
    assert_eq!(
        counters["total"]["typed_work_object_decodes"]
            .as_object()
            .unwrap()
            .len(),
        0
    );
    assert_eq!(after, before);
    assert!(
        elapsed < std::time::Duration::from_secs(1),
        "copy no-match target exceeded: {elapsed:?}"
    );
}

#[test]
#[ignore = "explicit supported-copy comparison; private store bytes stay under target/tmp"]
fn note_search_supported_copy_calibration() {
    let path = snapshot_path();
    let before = file_digest(&path);
    let project = ProjectId(std::env::var("ENGRAM_NOTE_SEARCH_PROJECT").unwrap());
    let byte_override = std::env::var("ENGRAM_NOTE_SEARCH_CALIBRATION_BYTES")
        .ok()
        .map(|value| value.parse::<u64>().unwrap());
    let now = DateTime::parse_from_rfc3339(&std::env::var("ENGRAM_NOTE_SEARCH_AT").unwrap())
        .unwrap()
        .with_timezone(&Utc);
    let opened = Instant::now();
    let store = SqliteStore::open_existing_read_only(&path).unwrap();
    let open_us = opened.elapsed().as_micros();
    assert!(store.connection.is_readonly("main").unwrap());
    assert!(
        store
            .connection
            .pragma_query_value(None, "query_only", |row| row.get::<_, bool>(0))
            .unwrap()
    );
    let service = crate::work_service::LocalWorkService::new(
        path.clone(),
        project.clone(),
        "calibration-reader".into(),
        crate::SessionId("search-copy-calibration".into()),
        None,
    );
    for (case, query) in [
        ("present-reference", "3312"),
        ("present-reference", "772b"),
        ("present-phrase", "gated control"),
        ("absent", "calibration-absent-zqxjkvbp"),
        ("broad-present", "the"),
    ] {
        cost::start();
        let started = Instant::now();
        let read = || {
            crate::storage::search_budget::Request::new(tokio_util::sync::CancellationToken::new())
                .run(|| {
                    Ok(service.work_catalog_page(
                        &WorkCatalogQuery {
                            search: Some(query.into()),
                            limit: 20,
                            ..WorkCatalogQuery::default()
                        },
                        None,
                        None,
                        now,
                    )?)
                })
        };
        let result = byte_override.map_or_else(read, |bytes| {
            crate::storage::search_budget::with_limits(
                crate::storage::search_budget::Limits {
                    bytes,
                    ..crate::storage::search_budget::Limits::default()
                },
                read,
            )
        });
        let elapsed_us = started.elapsed().as_micros();
        let counters = cost::finish();
        let outcome = match result {
            Ok(page) => serde_json::json!({
                "completed": true, "matches": page.total, "rows": page.items.len(),
                "ordered_ids": page.items.iter().map(|row| row.work.work_id).collect::<Vec<_>>(),
            }),
            Err(error) => {
                serde_json::json!({"completed": false, "refusal": crate::verbs::store_error_value(&error.error)})
            }
        };
        println!(
            "supported-copy search calibration: {}",
            serde_json::json!({
                "case": case, "query": query, "limit": 20, "lifecycles": "all",
                "elapsed_us": elapsed_us, "read_only_open_us": open_us, "cost": counters, "outcome": outcome,
                "snapshot_sha256": before, "snapshot_bytes": std::fs::metadata(&path).unwrap().len(),
                "observed_at": now, "build": crate::build_identity::current(),
                "debug_assertions": cfg!(debug_assertions), "measurement_byte_override": byte_override,
                "coverage": "Complete service catalog including successor/root/waiver enrichment and each service reader open, with request allowance created before opening; note_index_visits counts window entries, search_budget counters are policy charges; no CLI/MCP scheduling; supported captured copy only. Earlier master control was storage-only after admission, so elapsed comparison includes that surface difference.",
            })
        );
    }
    drop(store);
    assert_eq!(
        file_digest(&path),
        before,
        "query-only calibration changed snapshot bytes"
    );
}

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
