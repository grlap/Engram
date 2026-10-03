//! Doctor's read-only check of the memory full-text index: one pass over its
//! stored text, SQLite's own index check, and a finding for every row-binding
//! defect.

use super::*;

/// A store holding `count` memories, with short, empty-body-like, Unicode and
/// long texts among them.
fn memories(count: usize) -> SqliteStore {
    let mut store = SqliteStore::open_in_memory().expect("store");
    for index in 0..count {
        let body = match index % 4 {
            0 => "a".to_owned(),
            1 => "Zażółć gęślą jaźń — ünïcödé".to_owned(),
            2 => "_".to_owned(),
            _ => format!("memory {index} {}", "long text ".repeat(40)),
        };
        store
            .remember_project_memory(
                &project_memory_request(
                    "project-memory-fts-verification",
                    "fts-session",
                    Some(&format!("key-{index}")),
                    &body,
                    1_700_000_000_000 + i64::try_from(index).unwrap(),
                ),
                &DevelopmentNoopRedactor,
            )
            .expect("remember");
    }
    store
}

fn first_head(store: &SqliteStore) -> String {
    store
        .connection
        .query_row(
            "SELECT version_id FROM memory_heads ORDER BY version_id LIMIT 1",
            [],
            |row| row.get(0),
        )
        .expect("a memory head")
}

fn damaged(store: &SqliteStore, sql: &str) -> Vec<String> {
    store.connection.execute_batch("SAVEPOINT damage").unwrap();
    store.connection.execute_batch(sql).expect("damage");
    let report = store.verify_all().expect("doctor");
    store
        .connection
        .execute_batch("ROLLBACK TO damage; RELEASE damage")
        .unwrap();
    report.invalid_objects
}

// One content pass, however many memories there are, and no query per head;
// healthy short, Unicode, separator-only and long texts verify, read-only.
#[test]
fn doctor_reads_the_memory_index_once_whatever_the_memory_count() {
    for count in [1, 24] {
        let store = memories(count);
        store
            .connection
            .pragma_update(None, "query_only", true)
            .unwrap();
        let before = crate::storage::fts_verification::fts_content_scans();
        let report = store.verify_all().expect("doctor");
        assert!(report.is_healthy(), "{report:?}");
        // One pass for the memory index and one for the work catalog.
        assert_eq!(
            crate::storage::fts_verification::fts_content_scans() - before,
            2,
            "{count} memories"
        );
    }
}

// Each row-binding defect has its finding: changed text, a repeated row, a
// missing row and an orphan row.
#[test]
fn doctor_names_every_memory_index_binding_defect() {
    let store = memories(4);
    assert!(store.verify_all().unwrap().is_healthy());
    let head = first_head(&store);
    let binding = format!("object_fts:{head}:projection_binding");
    for (defect, sql) in [
        (
            "changed text",
            format!("UPDATE object_fts SET body = 'changed' WHERE object_id = '{head}'"),
        ),
        (
            "repeated row",
            format!(
                "INSERT INTO object_fts (object_id, title, body)
                 SELECT object_id, title, body FROM object_fts WHERE object_id = '{head}'"
            ),
        ),
        (
            "missing row",
            format!("DELETE FROM object_fts WHERE object_id = '{head}'"),
        ),
    ] {
        let invalid = damaged(&store, &sql);
        assert!(invalid.contains(&binding), "{defect}: {invalid:?}");
        assert!(
            !invalid
                .iter()
                .any(|label| label.starts_with("object_fts:fts_index:")),
            "{defect} keeps the index consistent with its text: {invalid:?}"
        );
    }
    let invalid = damaged(
        &store,
        "INSERT INTO object_fts (object_id, title, body) VALUES ('no-such-head', 't', 'b')",
    );
    assert!(
        invalid.contains(&"object_fts:orphaned_rows".to_owned()),
        "{invalid:?}"
    );
}

// Postings that disagree with the stored text are caught by the index check
// even when every row still binds.
#[test]
fn doctor_reports_memory_postings_that_disagree_with_the_text() {
    let store = memories(4);
    let head = first_head(&store);
    let invalid = damaged(
        &store,
        &format!(
            "UPDATE object_fts_content SET c2 = 'rewritten behind the index'
             WHERE c0 = '{head}'"
        ),
    );
    assert!(
        invalid
            .iter()
            .any(|label| label.starts_with("object_fts:fts_index:")),
        "{invalid:?}"
    );
}

/// Old and new memory index checks and the whole doctor, timed on the same
/// synthetic memory-heavy stores. Run on request:
/// `cargo test --lib memory_fts_check_cost_measurement -- --ignored --nocapture`.
#[test]
#[ignore = "measurement, run on request"]
fn memory_fts_check_cost_measurement() {
    for count in [1_000, 3_000, 9_000] {
        let started = std::time::Instant::now();
        let store = memories(count);
        let fixture = started.elapsed();
        let (rows, bytes): (i64, i64) = store
            .connection
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(LENGTH(title) + LENGTH(body)), 0) FROM object_fts",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        let heads: Vec<(String, String, String)> = store
            .connection
            .prepare("SELECT version_id, title, body FROM memory_heads")
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap();
        // The replaced path: an unindexed lookup and a full-text query per head.
        let old = std::time::Instant::now();
        for (version, title, body) in &heads {
            let mut statement = store
                .connection
                .prepare("SELECT title, body FROM object_fts WHERE object_id = ?1")
                .unwrap();
            assert_eq!(
                statement
                    .query_map([version.as_str()], |row| {
                        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
                    })
                    .unwrap()
                    .count(),
                1
            );
            if let Some(query) = crate::storage::task_memory::fts_query(&format!("{title} {body}"))
            {
                let _: bool = store
                    .connection
                    .query_row(
                        "SELECT EXISTS(SELECT 1 FROM object_fts WHERE object_id = ?1 AND object_fts MATCH ?2)",
                        rusqlite::params![version, query],
                        |row| row.get(0),
                    )
                    .unwrap_or(false);
            }
        }
        let old = old.elapsed();
        let new = std::time::Instant::now();
        let content = crate::storage::fts_verification::fts_content(
            &store.connection,
            "SELECT object_id, title, body FROM object_fts",
            |row| Ok((row.get::<_, String>(1)?, row.get::<_, String>(2)?)),
        )
        .unwrap()
        .unwrap();
        assert!(
            crate::storage::fts_verification::fts_index_finding(
                &store.connection,
                "object_fts",
                "object_fts:fts_index"
            )
            .unwrap()
            .is_none()
        );
        let new = new.elapsed();
        assert_eq!(content.len(), count);
        let doctor = std::time::Instant::now();
        assert!(store.verify_all().unwrap().is_healthy());
        let doctor = doctor.elapsed();
        println!(
            "memory heads={count} fts_rows={rows} fts_bytes={bytes} \
             old_per_head_probe_ms={:.1} new_pass_and_index_check_ms={:.1} whole_doctor_ms={:.1} fixture_ms={:.0}",
            old.as_secs_f64() * 1e3,
            new.as_secs_f64() * 1e3,
            doctor.as_secs_f64() * 1e3,
            fixture.as_secs_f64() * 1e3,
        );
    }
}
