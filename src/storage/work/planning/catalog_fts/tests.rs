use super::*;
use crate::storage::SqliteStore;
use crate::storage::work::test_support::*;
use crate::{
    ChildRequirement, DecomposeWorkRequest, ReviseWorkRequest, WorkItem, WorkPlanningAuthority,
    WorkRevisionPatch,
};
use rusqlite::trace::{TraceEvent, TraceEventCodes};
use std::cell::RefCell;

thread_local! {
    static CATALOG_SQL: RefCell<Vec<String>> = const { RefCell::new(Vec::new()) };
}

fn trace_catalog(event: &TraceEvent<'_>) {
    let TraceEvent::Stmt(_, sql) = event else {
        return;
    };
    if sql.starts_with("SELECT rowid FROM work_catalog_fts")
        || sql.starts_with("DELETE FROM work_catalog_fts")
    {
        CATALOG_SQL.with(|statements| statements.borrow_mut().push((*sql).to_owned()));
    }
}

fn start_trace(store: &SqliteStore) {
    CATALOG_SQL.with(|statements| statements.borrow_mut().clear());
    store.connection.trace_v2(
        TraceEventCodes::SQLITE_TRACE_STMT,
        Some(|event| trace_catalog(&event)),
    );
}

fn finish_trace(store: &SqliteStore) -> Vec<String> {
    store.connection.trace_v2(TraceEventCodes::empty(), None);
    CATALOG_SQL.with(|statements| std::mem::take(&mut *statements.borrow_mut()))
}

fn create(store: &mut SqliteStore, key: &str, title: &str) -> WorkItem {
    let mut request = root_request("fts-refresh", key, 0);
    request.title = title.into();
    store
        .create_work(&request, &DevelopmentNoopRedactor)
        .unwrap()
}

fn revise(store: &mut SqliteStore, item: &WorkItem, title: &str) -> WorkItem {
    store
        .revise_work(
            &ReviseWorkRequest {
                work_id: item.work_id,
                expected_revision: item.revision,
                patch: WorkRevisionPatch {
                    title: Some(title.into()),
                    ..WorkRevisionPatch::default()
                },
                authority: delegated("fts-refresh", "planner"),
                actor: actor("planner"),
                idempotency_key: format!("revise-{}-{}", item.work_id.0, item.revision),
                updated_at: at(item.revision + 1),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap()
}

fn rows(store: &SqliteStore) -> Vec<(i64, String, String)> {
    store
        .connection
        .prepare("SELECT rowid, work_id, search_text FROM work_catalog_fts ORDER BY rowid")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
}

#[test]
fn catalog_fts_refresh_creation_inserts_without_deleting_or_scanning() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    create(&mut store, "neighbor", "neighborword");
    let before = rows(&store);
    start_trace(&store);
    let root = create(&mut store, "fresh", "freshword");
    let sql = finish_trace(&store);
    assert!(
        sql.is_empty(),
        "creation must not look up or delete FTS rows: {sql:?}"
    );
    let after = rows(&store);
    assert_eq!(after.len(), before.len() + 1);
    for row in &before {
        assert!(after.contains(row));
    }
    assert_eq!(
        after
            .iter()
            .filter(|row| row.1 == root.work_id.0.to_string())
            .count(),
        1
    );

    start_trace(&store);
    let decomposition = store
        .decompose_work(
            &DecomposeWorkRequest {
                parent_id: root.work_id,
                expected_parent_revision: root.revision,
                children: vec![
                    child("first", ChildRequirement::Required, "First child"),
                    child("second", ChildRequirement::Required, "Second child"),
                ],
                prerequisites: Vec::new(),
                authority: WorkPlanningAuthority::Project,
                actor: actor("planner"),
                idempotency_key: "fresh-children".into(),
                created_at: at(1),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let sql = finish_trace(&store);
    // Only the existing parent is refreshed; neither new child needs a lookup.
    assert_eq!(
        sql.iter().filter(|sql| sql.contains(" MATCH ")).count(),
        1,
        "{sql:?}"
    );
    assert_eq!(
        sql.iter()
            .filter(|sql| sql.starts_with("DELETE FROM work_catalog_fts"))
            .count(),
        1,
        "{sql:?}"
    );
    assert!(
        !sql.iter()
            .any(|sql| sql.starts_with("DELETE FROM work_catalog_fts WHERE work_id")),
        "{sql:?}"
    );
    let after = rows(&store);
    for child in &decomposition.children {
        assert_eq!(
            after
                .iter()
                .filter(|row| row.1 == child.work_id.0.to_string())
                .count(),
            1
        );
    }
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn catalog_fts_refresh_uses_indexed_lookup_and_only_replaces_the_target() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let target = create(&mut store, "target", "obsoleteword");
    create(&mut store, "neighbor", "unrelatedword");
    let target_id = target.work_id.0.to_string();
    // FTS locators need not equal the rowids of the relational work table.
    let locator: i64 = store
        .connection
        .query_row("SELECT MAX(rowid) + 100 FROM work_catalog_fts", [], |row| {
            row.get(0)
        })
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE work_catalog_fts SET rowid = ?1 WHERE work_id = ?2",
            params![locator, target_id],
        )
        .unwrap();
    let before = rows(&store);
    start_trace(&store);
    let target = revise(&mut store, &target, "replacementword");
    let sql = finish_trace(&store);
    assert_eq!(
        sql.iter()
            .filter(|sql| sql.contains("WHERE rowid ="))
            .count(),
        1,
        "{sql:?}"
    );
    assert!(
        !sql.iter()
            .any(|sql| sql.starts_with("DELETE FROM work_catalog_fts WHERE work_id")),
        "{sql:?}"
    );
    assert!(sql.iter().any(|sql| sql.contains(" MATCH ")), "{sql:?}");
    let after = rows(&store);
    assert_eq!(after.iter().filter(|row| row.1 == target_id).count(), 1);
    assert_ne!(
        after.iter().find(|row| row.1 == target_id).unwrap().0,
        locator
    );
    assert_eq!(
        before
            .iter()
            .filter(|row| row.1 != target_id)
            .collect::<Vec<_>>(),
        after
            .iter()
            .filter(|row| row.1 != target_id)
            .collect::<Vec<_>>()
    );
    let text = &after.iter().find(|row| row.1 == target_id).unwrap().2;
    assert!(text.contains("replacementword"));
    assert!(!text.contains("obsoleteword"));
    let mut statement = store.connection.prepare("SELECT COUNT(*) FROM work_catalog_fts WHERE work_catalog_fts MATCH ?1 AND work_id = ?2").unwrap();
    for (term, expected) in [("obsoleteword", 0_i64), ("replacementword", 1)] {
        let count: i64 = statement
            .query_row(params![term, target_id], |row| row.get(0))
            .unwrap();
        assert_eq!(count, expected);
    }
    drop(statement);
    revise(&mut store, &target, "thirdword");
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn catalog_fts_refresh_preserves_many_items_mentioning_the_same_reference() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let target = create(&mut store, "target", "originalword");
    for index in 0..32 {
        create(
            &mut store,
            &format!("mention-{index}"),
            &format!("mention {}", target.short_ref),
        );
    }
    let before = rows(&store);
    let target_id = target.work_id.0.to_string();
    let mentions: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM work_catalog_fts WHERE work_catalog_fts MATCH ?1",
            [format!("\"{}\"", target.short_ref)],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(mentions, 33);
    start_trace(&store);
    revise(&mut store, &target, "changedword");
    let sql = finish_trace(&store);
    assert_eq!(
        sql.iter()
            .filter(|sql| sql.contains("WHERE rowid ="))
            .count(),
        1,
        "{sql:?}"
    );
    assert!(
        !sql.iter()
            .any(|sql| sql.starts_with("DELETE FROM work_catalog_fts WHERE work_id")),
        "{sql:?}"
    );
    let after = rows(&store);
    assert_eq!(
        before
            .iter()
            .filter(|row| row.1 != target_id)
            .collect::<Vec<_>>(),
        after
            .iter()
            .filter(|row| row.1 != target_id)
            .collect::<Vec<_>>()
    );
    assert_eq!(after.iter().filter(|row| row.1 == target_id).count(), 1);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn catalog_fts_refresh_falls_back_when_the_indexed_text_loses_its_reference() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let target = create(&mut store, "target", "expectedword");
    create(&mut store, "neighbor", "neighborword");
    store
        .connection
        .execute(
            "UPDATE work_catalog_fts SET search_text = 'damagedword' WHERE work_id = ?1",
            [target.work_id.0.to_string()],
        )
        .unwrap();
    let before = rows(&store);
    let target_id = target.work_id.0.to_string();
    start_trace(&store);
    revise(&mut store, &target, "repairedword");
    let sql = finish_trace(&store);
    assert_eq!(
        sql.iter()
            .filter(|sql| sql.starts_with("DELETE FROM work_catalog_fts WHERE work_id"))
            .count(),
        1,
        "{sql:?}"
    );
    let after = rows(&store);
    assert_eq!(
        before
            .iter()
            .filter(|row| row.1 != target_id)
            .collect::<Vec<_>>(),
        after
            .iter()
            .filter(|row| row.1 != target_id)
            .collect::<Vec<_>>()
    );
    assert_eq!(after.iter().filter(|row| row.1 == target_id).count(), 1);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn catalog_fts_refresh_keeps_doctor_duplicate_and_orphan_detection() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let target = create(&mut store, "target", "normalword");
    let target_id = target.work_id.0.to_string();
    store.connection.execute("INSERT INTO work_catalog_fts (work_id, search_text) SELECT work_id, search_text FROM work_catalog_fts WHERE work_id = ?1", [&target_id]).unwrap();
    store
        .connection
        .execute(
            "INSERT INTO work_catalog_fts (work_id, search_text) VALUES ('orphan', 'orphanword')",
            [],
        )
        .unwrap();
    let findings = store.verify_all().unwrap().invalid_work_records;
    assert!(
        findings.contains(&format!("work_catalog:{target_id}:projection_binding")),
        "{findings:?}"
    );
    assert!(
        findings.contains(&"work_catalog:orphaned_fts_rows".to_owned()),
        "{findings:?}"
    );
    start_trace(&store);
    revise(&mut store, &target, "replacementword");
    let sql = finish_trace(&store);
    assert_eq!(
        sql.iter()
            .filter(|sql| sql.contains("WHERE rowid ="))
            .count(),
        2,
        "{sql:?}"
    );
    assert_eq!(
        rows(&store).iter().filter(|row| row.1 == target_id).count(),
        1
    );
    let remaining = store.verify_all().unwrap().invalid_work_records;
    assert!(
        !remaining.contains(&format!("work_catalog:{target_id}:projection_binding")),
        "{remaining:?}"
    );
    assert!(
        remaining.contains(&"work_catalog:orphaned_fts_rows".to_owned()),
        "{remaining:?}"
    );
}

#[test]
fn catalog_fts_refresh_lookup_plan_uses_match_and_rowid() {
    let store = SqliteStore::open_in_memory().unwrap();
    let plan = store
        .connection
        .prepare(&format!("EXPLAIN QUERY PLAN {MATCHING_ROWIDS}"))
        .unwrap()
        .query_map(params!["\"reference\"", "not-an-item"], |row| {
            row.get::<_, String>(3)
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .join("\n");
    assert!(
        plan.contains("VIRTUAL TABLE INDEX") && plan.contains('M'),
        "{plan}"
    );
    let lookup = store
        .connection
        .prepare("EXPLAIN QUERY PLAN SELECT rowid FROM work_catalog_fts WHERE rowid = ?1")
        .unwrap()
        .query_map([1_i64], |row| row.get::<_, String>(3))
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap()
        .join("\n");
    assert!(
        lookup.contains("VIRTUAL TABLE INDEX") && lookup.contains('='),
        "{lookup}"
    );
}
