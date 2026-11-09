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
fn text_catalog_search_cannot_read_is_refused_and_stored_text_keeps_working() {
    use crate::storage::work::search_budget::{Limits, with_limits};
    let long = "x".repeat(crate::storage::work::search_budget::unit_bytes_limit() + 1);
    let mut store = SqliteStore::open_in_memory().unwrap();
    let plain = create(&mut store, "plain", "Findable plain item");
    let refused = |result: Result<WorkItem, StoreError>| {
        assert!(
            matches!(&result, Err(StoreError::InvalidWork(message)) if message.contains("catalog search")),
            "{:?}",
            result.map(|item| item.work_id)
        );
    };

    let mut assigned = root_request("fts-refresh", "long-assignee", 0);
    assigned.assigned_to = Some(long.clone());
    refused(store.create_work(&assigned, &DevelopmentNoopRedactor));
    let mut titled = root_request("fts-refresh", "long-title", 0);
    titled.title.clone_from(&long);
    refused(store.create_work(&titled, &DevelopmentNoopRedactor));
    refused(store.revise_work(
        &ReviseWorkRequest {
            work_id: plain.work_id,
            expected_revision: plain.revision,
            patch: WorkRevisionPatch {
                assigned_to: Some(long.clone()),
                ..WorkRevisionPatch::default()
            },
            authority: delegated("fts-refresh", "planner"),
            actor: actor("planner"),
            idempotency_key: "revise-long-assignee".into(),
            updated_at: at(1),
        },
        &DevelopmentNoopRedactor,
    ));

    // Nothing was written, so a search for the plain item still reads every row.
    let search = |store: &SqliteStore, text: &str| {
        store
            .query_work_catalog_listing(
                &plain.project_id,
                at(2),
                &crate::WorkCatalogQuery {
                    search: Some(text.into()),
                    limit: 20,
                    ..crate::WorkCatalogQuery::default()
                },
            )
            .unwrap()
            .1
    };
    assert_eq!(search(&store, "findable plain"), 1);
    assert_eq!(search(&store, "fi"), 1);

    // An item an earlier build stored over the limit stays usable: a write
    // that leaves its long text alone goes through.
    let stored = with_limits(
        Limits {
            unit_bytes: i32::MAX,
            ..Limits::default()
        },
        || {
            store
                .create_work(&assigned, &DevelopmentNoopRedactor)
                .unwrap()
        },
    );
    let revised = revise(&mut store, &stored, "Renamed");
    assert_eq!(revised.title, "Renamed");
    assert_eq!(revised.assigned_to.as_deref(), Some(long.as_str()));
}

fn over_limit<T>(read: impl FnOnce() -> T) -> T {
    use crate::storage::work::search_budget::{Limits, with_limits};
    with_limits(
        Limits {
            unit_bytes: i32::MAX,
            ..Limits::default()
        },
        read,
    )
}

fn block(
    store: &mut SqliteStore,
    item: &WorkItem,
    key: &str,
    detail: String,
) -> Result<crate::domain::WorkBlocker, StoreError> {
    store.add_work_blocker(
        &crate::AddWorkBlockerRequest {
            work_id: item.work_id,
            expected_work_revision: item.revision,
            kind: crate::domain::WorkBlockerKind::Manual,
            detail,
            authority: delegated("fts-refresh", "planner"),
            actor: actor("planner"),
            idempotency_key: key.into(),
            blocked_at: at(item.revision + 1),
        },
        &DevelopmentNoopRedactor,
    )
}

fn current(store: &SqliteStore, item: &WorkItem) -> WorkItem {
    store.get_work_item(item.work_id).unwrap()
}

fn search_total(store: &SqliteStore, item: &WorkItem, text: &str) -> usize {
    store
        .query_work_catalog_listing(
            &item.project_id,
            at(100),
            &crate::WorkCatalogQuery {
                search: Some(text.into()),
                limit: 20,
                ..crate::WorkCatalogQuery::default()
            },
        )
        .unwrap()
        .1
}

#[test]
fn a_search_returning_the_longest_accepted_text_still_succeeds() {
    let limit = crate::storage::work::search_budget::unit_bytes_limit();
    let mut store = SqliteStore::open_in_memory().unwrap();
    let plain = create(&mut store, "plain", "Findable plain item");

    // The longest texts the write refusal lets through, in a stored form that
    // escaping doesn't lengthen and that a search returning the item must read.
    let longest = limit - 64 * 1024 - 4 * 1024;
    let mut titled = root_request("fts-refresh", "longest-title", 0);
    titled.title = format!("findable {}", "x".repeat(longest));
    store
        .create_work(&titled, &DevelopmentNoopRedactor)
        .unwrap();
    let mut criteria = root_request("fts-refresh", "longest-acceptance", 0);
    criteria.title = "Findable criteria item".into();
    criteria.acceptance = vec!["y".repeat(longest)];
    store
        .create_work(&criteria, &DevelopmentNoopRedactor)
        .unwrap();
    assert_eq!(search_total(&store, &plain, "findable"), 3);
    assert_eq!(search_total(&store, &plain, "fi"), 3);

    // Text whose stored form a search couldn't read is refused, whichever
    // field holds it and whether or not it fits a search key.
    let refused = |result: Result<WorkItem, StoreError>| {
        assert!(
            matches!(&result, Err(StoreError::InvalidWork(message)) if message.contains("catalog search")),
            "{:?}",
            result.map(|item| item.work_id)
        );
    };
    let mut criteria = root_request("fts-refresh", "long-acceptance", 0);
    criteria.acceptance = vec!["y".repeat(limit + 1)];
    refused(store.create_work(&criteria, &DevelopmentNoopRedactor));
    let mut both = root_request("fts-refresh", "long-together", 0);
    both.title = "x".repeat(limit / 2 + 1);
    both.assigned_to = Some("z".repeat(limit / 2 + 1));
    refused(store.create_work(&both, &DevelopmentNoopRedactor));
    let mut escaped = root_request("fts-refresh", "long-escaped", 0);
    escaped.title = "\"".repeat(limit * 3 / 4);
    refused(store.create_work(&escaped, &DevelopmentNoopRedactor));

    let blocked = block(
        &mut store,
        &plain,
        "long-blocker",
        "\"".repeat(limit * 3 / 4),
    );
    assert!(
        matches!(&blocked, Err(StoreError::InvalidWork(message)) if message.contains("catalog search")),
        "{blocked:?}"
    );
    assert_eq!(search_total(&store, &plain, "plain"), 1);
}

#[test]
fn a_revision_cannot_grow_the_stored_form_past_the_limit_without_lengthening_its_text() {
    let limit = crate::storage::work::search_budget::unit_bytes_limit();
    let mut store = SqliteStore::open_in_memory().unwrap();
    let refused = |result: Result<WorkItem, StoreError>| {
        assert!(
            matches!(&result, Err(StoreError::InvalidWork(message)) if message.contains("catalog search")),
            "{:?}",
            result.map(|item| item.work_id)
        );
    };
    let patched = |item: &WorkItem, key: &str, patch: WorkRevisionPatch| ReviseWorkRequest {
        work_id: item.work_id,
        expected_revision: item.revision,
        patch,
        authority: delegated("fts-refresh", "planner"),
        actor: actor("planner"),
        idempotency_key: key.into(),
        updated_at: at(item.revision + 1),
    };

    // Shorter raw text that escapes to more than the limit.
    let item = create(&mut store, "escaping", &"x".repeat(limit * 3 / 5));
    refused(store.revise_work(
        &patched(
            &item,
            "escaping-title",
            WorkRevisionPatch {
                title: Some("\"".repeat(limit * 11 / 20)),
                ..WorkRevisionPatch::default()
            },
        ),
        &DevelopmentNoopRedactor,
    ));

    // The same length of acceptance, in a field no search key holds.
    let mut criteria = root_request("fts-refresh", "escaping-criteria", 0);
    criteria.acceptance = vec!["y".repeat(limit * 3 / 5)];
    let item = store
        .create_work(&criteria, &DevelopmentNoopRedactor)
        .unwrap();
    refused(store.revise_work(
        &patched(
            &item,
            "escaping-acceptance",
            WorkRevisionPatch {
                acceptance: Some(vec!["\"".repeat(limit * 3 / 5)]),
                ..WorkRevisionPatch::default()
            },
        ),
        &DevelopmentNoopRedactor,
    ));

    // Bindings alone: no text changes, and each adds to the stored form.
    let count = 1000;
    let each = (limit - 64 * 1024 - 24 * 1024) / count;
    let mut criteria = root_request("fts-refresh", "bound-criteria", 0);
    criteria.acceptance = (0..count)
        .map(|index| format!("{index:04} {}", "z".repeat(each - 8)))
        .collect();
    let item = store
        .create_work(&criteria, &DevelopmentNoopRedactor)
        .unwrap();
    refused(
        store.revise_work(
            &patched(
                &item,
                "bound-all",
                WorkRevisionPatch {
                    acceptance_bindings: Some(
                        (1..=count)
                            .map(|criterion| crate::domain::AcceptanceBinding {
                                criterion,
                                requirement: crate::domain::VerificationRequirement {
                                    check_kind: crate::domain::VerificationKind::Test,
                                    check_fingerprint: None,
                                },
                            })
                            .collect(),
                    ),
                    ..WorkRevisionPatch::default()
                },
            ),
            &DevelopmentNoopRedactor,
        ),
    );

    // A true shrink of escaped text still goes through.
    let mut stored = root_request("fts-refresh", "stored-escaped", 0);
    stored.title = "\"".repeat(limit * 3 / 4);
    let stored = over_limit(|| {
        store
            .create_work(&stored, &DevelopmentNoopRedactor)
            .unwrap()
    });
    revise(&mut store, &stored, &"\"".repeat(limit * 2 / 3));
}

#[test]
fn a_blocker_is_refused_only_when_its_own_stored_form_is_too_long() {
    let limit = crate::storage::work::search_budget::unit_bytes_limit();
    let mut store = SqliteStore::open_in_memory().unwrap();
    let mut request = root_request("fts-refresh", "large-acceptance", 0);
    request.title = "x".repeat(limit + 8);
    let item = over_limit(|| {
        store
            .create_work(&request, &DevelopmentNoopRedactor)
            .unwrap()
    });
    // Escaping makes this blocker too long though its detail fits.
    let refused = block(&mut store, &item, "escaped", "\"".repeat(limit * 3 / 4));
    assert!(matches!(refused, Err(StoreError::InvalidWork(_))));
    // An item already stored over the limit can still be blocked.
    block(&mut store, &item, "small", "b".repeat(1024)).unwrap();
}

#[test]
fn text_folding_past_the_limit_is_accepted_and_fails_no_other_search() {
    let limit = crate::storage::work::search_budget::unit_bytes_limit();
    let mut store = SqliteStore::open_in_memory().unwrap();
    let plain = create(&mut store, "plain", "Findable plain item");
    // U+0130 is two bytes and case-folds to three, so the stored form fits
    // while the search keys don't.
    let mut titled = root_request("fts-refresh", "folding-title", 0);
    titled.title = "\u{0130}".repeat(limit * 2 / 5);
    store
        .create_work(&titled, &DevelopmentNoopRedactor)
        .unwrap();
    let mut assigned = root_request("fts-refresh", "folding-assignee", 0);
    assigned.assigned_to = Some("\u{0130}".repeat(limit * 2 / 5));
    store
        .create_work(&assigned, &DevelopmentNoopRedactor)
        .unwrap();
    assert_eq!(search_total(&store, &plain, "findable plain"), 1);
    assert_eq!(search_total(&store, &plain, "fi"), 1);
    assert_eq!(search_total(&store, &plain, "absent-needle"), 0);
}

#[test]
fn an_item_stored_over_the_limit_can_shrink_but_not_grow() {
    let limit = crate::storage::work::search_budget::unit_bytes_limit();
    let mut store = SqliteStore::open_in_memory().unwrap();
    let mut request = root_request("fts-refresh", "stored-long", 0);
    request.title = "x".repeat(limit + 8);
    let stored = over_limit(|| {
        store
            .create_work(&request, &DevelopmentNoopRedactor)
            .unwrap()
    });

    // A write that leaves the long text alone is untouched by the limit.
    let prioritized = store
        .revise_work(
            &ReviseWorkRequest {
                work_id: stored.work_id,
                expected_revision: stored.revision,
                patch: WorkRevisionPatch {
                    priority: Some(2),
                    ..WorkRevisionPatch::default()
                },
                authority: delegated("fts-refresh", "planner"),
                actor: actor("planner"),
                idempotency_key: "stored-long-priority".into(),
                updated_at: at(1),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();

    // Shorter but still over the limit goes through, longer doesn't.
    let shrunk = revise(&mut store, &prioritized, &"x".repeat(limit + 4));
    let grown = store.revise_work(
        &ReviseWorkRequest {
            work_id: shrunk.work_id,
            expected_revision: shrunk.revision,
            patch: WorkRevisionPatch {
                title: Some("x".repeat(limit + 16)),
                ..WorkRevisionPatch::default()
            },
            authority: delegated("fts-refresh", "planner"),
            actor: actor("planner"),
            idempotency_key: "stored-long-grow".into(),
            updated_at: at(10),
        },
        &DevelopmentNoopRedactor,
    );
    assert!(matches!(grown, Err(StoreError::InvalidWork(_))));
}

#[test]
fn clearing_a_blocker_on_an_item_stored_over_the_limit_succeeds() {
    let limit = crate::storage::work::search_budget::unit_bytes_limit();
    let mut store = SqliteStore::open_in_memory().unwrap();
    let item = create(&mut store, "stored-blocked", "Stored blocked item");
    let first = over_limit(|| block(&mut store, &item, "first", "a".repeat(limit)).unwrap());
    let blocked = current(&store, &item);
    let second = over_limit(|| block(&mut store, &blocked, "second", "b".repeat(limit)).unwrap());
    let blocked = current(&store, &item);

    // A search that wouldn't select it isn't failed by its long key, and
    // clearing one blocker leaves that key over the limit.
    let plain = create(&mut store, "beside-blocked", "Plain neighbour");
    assert_eq!(search_total(&store, &plain, "pl"), 1);
    let cleared = store
        .clear_work_blocker(
            &crate::ClearWorkBlockerRequest {
                work_id: item.work_id,
                expected_work_revision: blocked.revision,
                blocker_id: first.blocker_id,
                authority: delegated("fts-refresh", "planner"),
                actor: actor("planner"),
                idempotency_key: "clear-first".into(),
                cleared_at: at(20),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    store
        .clear_work_blocker(
            &crate::ClearWorkBlockerRequest {
                work_id: item.work_id,
                expected_work_revision: cleared.revision,
                blocker_id: second.blocker_id,
                authority: delegated("fts-refresh", "planner"),
                actor: actor("planner"),
                idempotency_key: "clear-second".into(),
                cleared_at: at(21),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    assert_eq!(search_total(&store, &item, "stored blocked"), 1);
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
