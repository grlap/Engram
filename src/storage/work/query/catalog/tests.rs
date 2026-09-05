use super::*;
use crate::WorkLifecycle;
use crate::storage::work::test_support::*;
use crate::storage::work::*;

thread_local! {
    static AFTER_CATALOG_COUNT: std::cell::RefCell<Option<Box<dyn FnOnce()>>> = const { std::cell::RefCell::new(None) };
}

pub(super) fn after_catalog_count() {
    let hook = AFTER_CATALOG_COUNT.with(|hook| hook.borrow_mut().take());
    if let Some(hook) = hook {
        hook();
    }
}

#[test]
fn phoenix_catalog_count_uses_the_same_filters_and_deduplicated_mine_union() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let project = ProjectId("catalog-count-union".into());
    let mut items = Vec::new();
    for index in 0..6 {
        let mut request = root_request(&project.0, &format!("item-{index}"), index);
        request.title = format!("Matching Straße {index}");
        request.labels = vec!["Größe".into()];
        if index < 3 {
            request.assigned_to = Some("Straße".into());
        }
        let item = store
            .create_work(&request, &DevelopmentNoopRedactor)
            .expect("item");
        // Assigned-only, assigned+held, held-only, and neither all exist.
        if (2..=3).contains(&index) {
            claim(
                &mut store,
                &item,
                "holder",
                &format!("claim-{index}"),
                10,
                300,
            );
        }
        items.push(item);
    }
    let query = WorkCatalogQuery {
        assigned_to: Some("STRASSE".into()),
        held_by: Some(SessionId("holder".into())),
        search: Some("MATCHING STRASSE".into()),
        label: Some("GRÖSSE".into()),
        lifecycles: vec![WorkLifecycle::Open],
        limit: 2,
        ..WorkCatalogQuery::default()
    };
    reset_work_item_projection_decode_count();
    let (page, total, _) = store
        .query_work_catalog_listing(&project, at(11), &query)
        .expect("page");
    assert_eq!(total, 4);
    assert_eq!(page.items.len(), 2);
    assert_eq!(
        work_item_projection_decode_count(),
        3,
        "decode only page plus sentinel, not the count"
    );
    let (second, total, _) = store
        .query_work_catalog_listing(
            &project,
            at(11),
            &WorkCatalogQuery {
                after: page.next_after,
                ..query.clone()
            },
        )
        .expect("second");
    assert_eq!(total, 4, "total precedes cursor");
    assert_eq!(second.items.len(), 2);
    assert!(second.next_after.is_none());
    let mut ids = page
        .items
        .iter()
        .chain(&second.items)
        .map(|row| row.work.work_id)
        .collect::<Vec<_>>();
    ids.sort_by_key(|id| id.0);
    ids.dedup();
    assert_eq!(ids.len(), 4);
    let (exhausted, total, _) = store
        .query_work_catalog_listing(
            &project,
            at(11),
            &WorkCatalogQuery {
                after: ids.last().copied(),
                ..query.clone()
            },
        )
        .expect("past last");
    assert_eq!(total, 4);
    assert!(exhausted.items.is_empty());
    let (_, total, _) = store
        .query_work_catalog_listing(&project, at(310), &query)
        .expect("claims expired");
    assert_eq!(total, 3);
    let (empty, total, _) = store
        .query_work_catalog_listing(
            &project,
            at(11),
            &WorkCatalogQuery {
                label: Some("different".into()),
                ..query
            },
        )
        .expect("no matching label");
    assert_eq!(total, 0);
    assert!(empty.items.is_empty());
    assert!(store.verify_all().expect("integrity").is_healthy());
}

#[test]
fn phoenix_catalog_cursor_plan_seeks_without_sorting_with_availability_filters() {
    let store = SqliteStore::open_in_memory().expect("store");
    let query = WorkCatalogQuery {
        lifecycles: vec![WorkLifecycle::Open],
        availabilities: vec![WorkAvailability::Ready, WorkAvailability::Claimed],
        after: Some(WorkId::new()),
        limit: 2,
        ..WorkCatalogQuery::default()
    };
    let project = ProjectId("catalog-plan".into());
    let (sql, parameters) = work_catalog_sql(&project, at(0), &query, true).expect("page SQL");
    assert!(!sql.contains("COUNT(*)"));
    let mut statement = store
        .connection
        .prepare(&format!("EXPLAIN QUERY PLAN {sql}"))
        .expect("plan");
    let plan = statement
        .query_map(rusqlite::params_from_iter(parameters.iter()), |row| {
            row.get::<_, String>(3)
        })
        .expect("explain")
        .collect::<Result<Vec<_>, _>>()
        .expect("plan rows")
        .join("\n");
    assert!(
        plan.contains(
            "SEARCH candidate USING INDEX work_items_catalog_after (project_id=? AND work_id>?)"
        ),
        "{plan}"
    );
    assert!(!plan.contains("USE TEMP B-TREE"), "{plan}");
    let (count, _) = work_catalog_sql(&project, at(0), &query, false).expect("count SQL");
    assert!(count.contains("COUNT(*)"));
    assert!(!count.contains("candidate.work_id >"));
}

#[test]
fn phoenix_catalog_count_page_and_holders_share_one_snapshot() {
    let directory = tempfile::tempdir().expect("temp");
    let database = directory.path().join("catalog.sqlite3");
    let mut store = SqliteStore::open(&database).expect("store");
    let item = store
        .create_work(
            &root_request("catalog-snapshot", "held", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("item");
    let held = claim(&mut store, &item, "holder", "held-claim", 1, 300);
    let item = store.get_work_item(item.work_id).expect("claimed item");
    let mut writer = SqliteStore::open(&database).expect("concurrent writer");
    let release = ReleaseWorkRequest {
        work_id: item.work_id,
        run_id: held.run_id,
        expected_work_revision: item.revision,
        holder: held.holder.clone(),
        claim_id: held.claim_id,
        claim_fence: held.fence,
        reason: "release between count and page".into(),
        waiver_reason: Some("no execution was performed; release test fixture authority".into()),
        actor: actor("holder"),
        idempotency_key: "interleaved-release".into(),
        released_at: at(2),
    };
    AFTER_CATALOG_COUNT.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            writer
                .release_work(&release, &DevelopmentNoopRedactor)
                .expect("release during reader snapshot");
        }));
    });
    let query = WorkCatalogQuery {
        held_by: Some(held.holder.clone()),
        limit: 2,
        ..WorkCatalogQuery::default()
    };
    let (page, total, holders) = store
        .query_work_catalog_listing(&item.project_id, at(3), &query)
        .expect("snapshot list");
    assert_eq!(total, 1);
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].work.work_id, item.work_id);
    assert_eq!(holders, vec![held]);
    let (page, total, holders) = store
        .query_work_catalog_listing(&item.project_id, at(3), &query)
        .expect("later list");
    assert_eq!(total, 0);
    assert!(page.items.is_empty());
    assert!(holders.is_empty());
    assert!(store.verify_all().expect("integrity").is_healthy());
}

#[test]
fn catalog_uses_unicode_keys_and_ready_ranking_is_deterministic() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let project = "project-catalog-index";

    let mut oldest_request = root_request(project, "catalog-oldest", 0);
    oldest_request.title = "Maße der Größe".into();
    oldest_request.labels = vec!["Größe".into()];
    oldest_request.assigned_to = Some("Straße".into());
    let oldest = store
        .create_work(&oldest_request, &DevelopmentNoopRedactor)
        .expect("oldest root");

    let mut unblocks_request = root_request(project, "catalog-unblocks", 1);
    unblocks_request.title = "Dependency provider".into();
    let unblocks = store
        .create_work(&unblocks_request, &DevelopmentNoopRedactor)
        .expect("dependency provider");

    let mut dependent_request = root_request(project, "catalog-dependent", 2);
    dependent_request.title = "Dependent work".into();
    let dependent = store
        .create_work(&dependent_request, &DevelopmentNoopRedactor)
        .expect("dependent root");
    store
        .add_work_prerequisite(
            &ChangeWorkPrerequisiteRequest {
                work_id: dependent.work_id,
                prerequisite_id: unblocks.work_id,
                expected_revision: dependent.revision,
                authority: delegated(project, "planner"),
                actor: actor("planner"),
                idempotency_key: "catalog-prerequisite".into(),
                changed_at: at(3),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("add ranking prerequisite");

    let terminal = store
        .create_work(
            &root_request(project, "catalog-terminal-dependent", 3),
            &DevelopmentNoopRedactor,
        )
        .expect("terminal dependent");
    let terminal = store
        .add_work_prerequisite(
            &ChangeWorkPrerequisiteRequest {
                work_id: terminal.work_id,
                prerequisite_id: oldest.work_id,
                expected_revision: terminal.revision,
                authority: delegated(project, "planner"),
                actor: actor("planner"),
                idempotency_key: "catalog-terminal-prerequisite".into(),
                changed_at: at(4),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("add terminal prerequisite");
    let terminal = store
        .dispose_work(
            &DisposeWorkRequest {
                work_id: terminal.work_id,
                expected_work_revision: terminal.revision,
                disposition: WorkDisposition::Cancelled,
                replacement_id: None,
                reason: "terminal dependant must not affect ready rank".into(),
                actor: actor("planner"),
                idempotency_key: "catalog-terminal-dispose".into(),
                disposed_at: at(5),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("dispose terminal dependent");

    reset_work_event_decode_count();
    reset_work_item_projection_decode_count();
    let ready = store
        .ready_work(&crate::domain::ProjectId(project.into()), at(6), 10)
        .expect("rank ready work");
    assert_eq!(
        ready
            .iter()
            .map(|candidate| candidate.work.work_id)
            .collect::<Vec<_>>(),
        vec![unblocks.work_id, oldest.work_id]
    );
    assert_eq!(work_event_decode_count(), 0);
    assert_eq!(work_item_projection_decode_count(), 2);

    for (query, expected) in [
        (
            WorkCatalogQuery {
                assigned_to: Some("STRASSE".into()),
                limit: 10,
                ..WorkCatalogQuery::default()
            },
            oldest.work_id,
        ),
        (
            WorkCatalogQuery {
                label: Some("GRÖSSE".into()),
                limit: 10,
                ..WorkCatalogQuery::default()
            },
            oldest.work_id,
        ),
        (
            WorkCatalogQuery {
                search: Some("MASSE DER GRÖSSE".into()),
                limit: 10,
                ..WorkCatalogQuery::default()
            },
            oldest.work_id,
        ),
        (
            WorkCatalogQuery {
                availabilities: vec![WorkAvailability::Blocked],
                limit: 10,
                ..WorkCatalogQuery::default()
            },
            dependent.work_id,
        ),
    ] {
        reset_work_event_decode_count();
        let page = store
            .query_work_catalog(&crate::domain::ProjectId(project.into()), at(6), &query)
            .expect("indexed catalog query");
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].work.work_id, expected);
        assert_eq!(work_event_decode_count(), 0);
    }

    store
        .add_work_blocker(
            &AddWorkBlockerRequest {
                work_id: oldest.work_id,
                expected_work_revision: oldest.revision,
                kind: crate::domain::WorkBlockerKind::Manual,
                detail: "deferred work still has an independent blocker".into(),
                authority: delegated(project, "planner"),
                actor: actor("planner"),
                idempotency_key: "catalog-deferred-blocker".into(),
                blocked_at: at(7),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("block deferred candidate");
    let oldest = store.get_work_item(oldest.work_id).expect("blocked oldest");
    store
        .revise_work(
            &ReviseWorkRequest {
                work_id: oldest.work_id,
                expected_revision: oldest.revision,
                patch: WorkRevisionPatch {
                    deferred_until: Some(at(100)),
                    ..WorkRevisionPatch::default()
                },
                authority: delegated(project, "planner"),
                actor: actor("planner"),
                idempotency_key: "catalog-deferred-revision".into(),
                updated_at: at(8),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("defer blocked candidate");
    let blocked_only = store
        .query_work_catalog(
            &crate::domain::ProjectId(project.into()),
            at(9),
            &WorkCatalogQuery {
                blocked_only: true,
                limit: 10,
                ..WorkCatalogQuery::default()
            },
        )
        .expect("independent blocked-only query");
    let blocked_availability = blocked_only
        .items
        .iter()
        .map(|item| (item.work.work_id, item.availability))
        .collect::<HashMap<_, _>>();
    assert_eq!(
        blocked_availability.get(&oldest.work_id),
        Some(&WorkAvailability::Deferred)
    );
    assert_eq!(
        blocked_availability.get(&terminal.work_id),
        Some(&WorkAvailability::Closed)
    );
    assert_eq!(
        blocked_availability.get(&dependent.work_id),
        Some(&WorkAvailability::Blocked)
    );
    assert!(store.verify_all().expect("catalog integrity").is_healthy());
}

#[test]
fn doctor_exercises_work_catalog_fts_index() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let item = store
        .create_work(
            &root_request("project-catalog-fts-integrity", "fts-root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("indexed root");
    assert!(store.verify_all().expect("healthy catalog").is_healthy());

    store
        .connection
        .execute("DELETE FROM work_catalog_fts_data WHERE id > 1", [])
        .expect("corrupt only the FTS segment data");
    let report = store.verify_all().expect("catalog corruption report");
    assert!(
        report
            .invalid_work_records
            .iter()
            .any(|record| { record == &format!("work_catalog:{}:fts_index", item.work_id.0) })
    );
}
