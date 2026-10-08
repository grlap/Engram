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
fn note_search_uses_one_snapshot_for_matches_count_and_page() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("note-snapshot.db");
    let mut store = SqliteStore::open(&database).unwrap();
    let mut items = Vec::new();
    for index in 0..2 {
        let item = store
            .create_work(
                &root_request("note-snapshot", &format!("item-{index}"), index),
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        let held = claim(
            &mut store,
            &item,
            "holder",
            &format!("claim-{index}"),
            2,
            600,
        );
        let item = store.get_work_item(item.work_id).unwrap();
        items.push((item, held));
    }
    let request = |item: &WorkItem, held: &WorkClaim, key: &str| RecordWorkEvidenceRequest {
        work_id: item.work_id,
        run_id: held.run_id,
        expected_work_revision: item.revision,
        holder: held.holder.clone(),
        claim_id: held.claim_id,
        claim_fence: held.fence,
        summary: "snapshot-only-needle".into(),
        refs: Vec::new(),
        actor: actor("holder"),
        idempotency_key: key.into(),
        recorded_at: at(4),
    };
    store
        .record_work_evidence(
            &request(&items[0].0, &items[0].1, "first-note"),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let late_note = request(&items[1].0, &items[1].1, "late-note");
    let mut writer = SqliteStore::open(&database).unwrap();
    AFTER_CATALOG_COUNT.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            writer
                .record_work_evidence(&late_note, &DevelopmentNoopRedactor)
                .unwrap();
        }));
    });
    let query = WorkCatalogQuery {
        search: Some("snapshot-only-needle".into()),
        limit: 1,
        ..WorkCatalogQuery::default()
    };
    let (page, total, _) = store
        .query_work_catalog_listing(&items[0].0.project_id, at(5), &query)
        .unwrap();
    assert_eq!(total, 1);
    assert_eq!(page.items[0].work.work_id, items[0].0.work_id);
    assert!(page.next_after.is_none());
    let (page, total, _) = store
        .query_work_catalog_listing(&items[0].0.project_id, at(5), &query)
        .unwrap();
    assert_eq!(total, 2);
    assert!(page.next_after.is_some());
}

#[test]
fn note_search_finds_verification_text_and_excludes_identity_and_environment() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let work = store
        .create_work(
            &root_request("note-verification", "Unrelated", 0),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let held = claim(&mut store, &work, "private-actor-token", "claim", 1, 600);
    host_verification(
        &mut store,
        &work,
        &held,
        "private-actor-token",
        "verification-note-token",
        VerificationKind::Test,
        VerificationResult::Passed,
        2,
    );
    for (text, total) in [
        ("host observed verification-note-token", 1),
        ("private-actor-token", 0),
        ("workspace-verification-note-token", 0),
        ("revision-as-it-stands", 0),
    ] {
        let page = store
            .query_work_catalog(
                &work.project_id,
                at(3),
                &WorkCatalogQuery {
                    search: Some(text.into()),
                    limit: 10,
                    ..WorkCatalogQuery::default()
                },
            )
            .unwrap();
        assert_eq!(page.items.len(), total, "{text}");
    }
}

#[test]
fn note_search_cost_and_scope_use_eligible_canonical_notes_only() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let project = ProjectId("note-cost".into());
    let mut payload_bytes = 0;
    for index in 0..32 {
        let mut request = root_request(&project.0, &format!("item-{index}"), index);
        request.labels = vec![
            if index % 2 == 0 {
                "selected"
            } else {
                "excluded"
            }
            .into(),
        ];
        let item = store
            .create_work(&request, &DevelopmentNoopRedactor)
            .unwrap();
        let held = claim(
            &mut store,
            &item,
            "holder",
            &format!("claim-{index}"),
            33,
            600,
        );
        let item = store.get_work_item(item.work_id).unwrap();
        for number in 0..2 {
            let summary = format!("{} /citation/{index}/{number}", "note text ".repeat(256));
            if index % 2 == 0 {
                payload_bytes += summary.len();
            }
            store
                .record_work_evidence(
                    &RecordWorkEvidenceRequest {
                        work_id: item.work_id,
                        run_id: held.run_id,
                        expected_work_revision: item.revision,
                        holder: held.holder.clone(),
                        claim_id: held.claim_id,
                        claim_fence: held.fence,
                        summary,
                        refs: Vec::new(),
                        actor: actor("holder"),
                        idempotency_key: format!("note-{index}-{number}"),
                        recorded_at: at(34),
                    },
                    &DevelopmentNoopRedactor,
                )
                .unwrap();
        }
    }
    let query = WorkCatalogQuery {
        search: Some("/citation/".into()),
        label: Some("selected".into()),
        limit: 100,
        ..WorkCatalogQuery::default()
    };
    crate::storage::work::cost::start();
    let (page, total, _) = store
        .query_work_catalog_listing(&project, at(35), &query)
        .unwrap();
    let cost = crate::storage::work::cost::finish();
    assert_eq!(total, 16);
    assert_eq!(page.items.len(), 16);
    assert!(page.items.iter().all(|row| row.work.labels == ["selected"]));
    println!(
        "note-search representative fixture: 32 items, 64 notes, 16 eligible items, 32 eligible notes, {payload_bytes} eligible summary bytes; cost={cost}"
    );
    let excluded = store
        .query_work_catalog(
            &project,
            at(35),
            &WorkCatalogQuery {
                search: Some("/citation/1/".into()),
                ..query
            },
        )
        .unwrap();
    assert!(
        excluded.items.is_empty(),
        "other-label notes do not enter the match set"
    );
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
    assert!(exhausted.items.is_empty(), "{:?}", exhausted.items);
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
    assert!(empty.items.is_empty(), "{:?}", empty.items);
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
    let (sql, parameters) =
        work_catalog_sql(&project, at(0), &query, true, None).expect("page SQL");
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
    let (count, _) = work_catalog_sql(&project, at(0), &query, false, None).expect("count SQL");
    assert!(count.contains("COUNT(*)"));
    assert!(!count.contains("candidate.work_id >"));
}

#[test]
fn phoenix_catalog_count_page_and_holders_share_one_snapshot() {
    let directory = crate::test_support::temp_home().expect("temp");
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
    assert!(page.items.is_empty(), "{:?}", page.items);
    assert!(holders.is_empty(), "{holders:?}");
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
        None,
        "ended work with historical prerequisites is not a blocked candidate"
    );
    assert_eq!(
        blocked_availability.get(&dependent.work_id),
        Some(&WorkAvailability::Blocked)
    );
    assert!(store.verify_all().expect("catalog integrity").is_healthy());
}

#[test]
fn ready_catalog_orders_by_priority_then_work_id_and_continues_exactly_once() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let project = crate::domain::ProjectId("ready-priority-order".into());
    let mut low = Vec::new();
    for index in 0..4 {
        let mut request = root_request(&project.0, &format!("low-{index}"), index);
        request.priority = 3;
        low.push(
            store
                .create_work(&request, &DevelopmentNoopRedactor)
                .expect("low")
                .work_id,
        );
    }
    let mut high = root_request(&project.0, "high-later", 4);
    high.priority = 1;
    let high = store
        .create_work(&high, &DevelopmentNoopRedactor)
        .expect("high")
        .work_id;
    let query = WorkCatalogQuery {
        lifecycles: vec![WorkLifecycle::Open],
        availabilities: vec![WorkAvailability::Ready],
        ready_priority_order: true,
        limit: 3,
        ..WorkCatalogQuery::default()
    };
    let mut ranked = low
        .iter()
        .copied()
        .map(|work_id| (3, work_id))
        .chain(std::iter::once((1, high)))
        .collect::<Vec<_>>();
    ranked.sort_by(|left, right| left.0.cmp(&right.0).then(left.1.0.cmp(&right.1.0)));
    let ranked: Vec<_> = ranked.into_iter().map(|(_, work_id)| work_id).collect();
    let first = store
        .query_work_catalog(&project, at(10), &query)
        .expect("first");
    assert_eq!(
        first
            .items
            .iter()
            .map(|item| item.work.work_id)
            .collect::<Vec<_>>(),
        ranked[..3]
    );
    let second = store
        .query_work_catalog(
            &project,
            at(10),
            &WorkCatalogQuery {
                after: first.next_after,
                after_priority: first.items.last().map(|item| item.work.priority),
                ..query
            },
        )
        .expect("second");
    assert_eq!(
        second
            .items
            .iter()
            .map(|item| item.work.work_id)
            .collect::<Vec<_>>(),
        ranked[3..]
    );
    assert!(second.next_after.is_none());
}

#[test]
fn ready_catalog_continuation_without_after_priority_is_refused() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let project = crate::domain::ProjectId("ready-missing-after-priority".into());
    let mut query = WorkCatalogQuery {
        lifecycles: vec![WorkLifecycle::Open],
        availabilities: vec![WorkAvailability::Ready],
        ready_priority_order: true,
        limit: 1,
        ..WorkCatalogQuery::default()
    };
    for index in 0..2 {
        let mut request = root_request(&project.0, &format!("ready-{index}"), index);
        request.priority = 3;
        store
            .create_work(&request, &DevelopmentNoopRedactor)
            .expect("ready");
    }
    let first = store
        .query_work_catalog(&project, at(10), &query)
        .expect("first");
    query.after = first.next_after;
    let error = store
        .query_work_catalog(&project, at(10), &query)
        .expect_err("missing after_priority");
    assert!(matches!(
        error,
        StoreError::WorkCatalogCursorInvalid { reason }
            if reason.contains("after_priority")
    ));
    query.after_priority = first.items.last().map(|item| item.work.priority);
    let second = store
        .query_work_catalog(&project, at(10), &query)
        .expect("supplied after_priority");
    assert_eq!(second.items.len(), 1);
    assert_ne!(second.items[0].work.work_id, first.items[0].work.work_id);
}

#[test]
fn ready_catalog_without_priority_flag_stays_work_id_order() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let project = crate::domain::ProjectId("ready-id-order".into());
    let mut older = Vec::new();
    for index in 0..3 {
        let mut request = root_request(&project.0, &format!("older-{index}"), index);
        request.priority = 4;
        older.push(
            store
                .create_work(&request, &DevelopmentNoopRedactor)
                .expect("older")
                .work_id,
        );
    }
    let mut later_high = root_request(&project.0, "later-high", 4);
    later_high.priority = 0;
    let later_high = store
        .create_work(&later_high, &DevelopmentNoopRedactor)
        .expect("high")
        .work_id;
    let query = WorkCatalogQuery {
        lifecycles: vec![WorkLifecycle::Open],
        availabilities: vec![WorkAvailability::Ready],
        ready_priority_order: false,
        limit: 4,
        ..WorkCatalogQuery::default()
    };
    let page = store
        .query_work_catalog(&project, at(10), &query)
        .expect("page");
    let mut expected = older;
    expected.push(later_high);
    expected.sort_by_key(|work_id| work_id.0);
    assert_eq!(
        page.items
            .iter()
            .map(|item| item.work.work_id)
            .collect::<Vec<_>>(),
        expected
    );
}

#[test]
fn doctor_exercises_work_catalog_fts_index() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    store
        .create_work(
            &root_request("project-catalog-fts-integrity", "fts-root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("indexed root");
    store
        .connection
        .pragma_update(None, "query_only", true)
        .expect("read-only doctor connection");
    assert!(store.verify_all().expect("healthy catalog").is_healthy());
    store
        .connection
        .pragma_update(None, "query_only", false)
        .expect("allow fixture corruption");

    store
        .connection
        .execute("DELETE FROM work_catalog_fts_data WHERE id > 1", [])
        .expect("remove the FTS structure and segment records");
    store
        .connection
        .pragma_update(None, "query_only", true)
        .expect("read-only doctor connection");
    let report = store.verify_all().expect("catalog corruption report");
    assert!(
        report
            .invalid_work_records
            .iter()
            .any(|record| record.starts_with("work_catalog:fts_index:"))
    );
}

#[test]
fn doctor_detects_work_catalog_fts_content_posting_mismatch() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    store
        .create_work(
            &root_request("project-catalog-fts-content", "fts-root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("indexed root");
    assert!(store.verify_all().expect("healthy catalog").is_healthy());

    store
        .connection
        .execute("UPDATE work_catalog_fts_content SET c1 = c1 || 'zzz'", [])
        .expect("change FTS content without rebuilding its intact index");
    store
        .connection
        .pragma_update(None, "query_only", true)
        .expect("read-only doctor connection");
    let report = store.verify_all().expect("catalog content mismatch report");
    assert!(
        report
            .invalid_work_records
            .iter()
            .any(|record| record.starts_with("work_catalog:fts_index:")),
        "intact FTS index must be compared with its content: {:?}",
        report.invalid_work_records
    );
}

#[test]
fn catalog_held_by_refuses_an_oversized_session_before_sql() {
    let store = SqliteStore::open_in_memory().expect("store");
    let project = ProjectId("held-by-admission".into());
    let giant = SessionId("h".repeat(65));
    let error = store
        .query_work_catalog(
            &project,
            at(1),
            &WorkCatalogQuery {
                held_by: Some(giant.clone()),
                limit: 5,
                ..WorkCatalogQuery::default()
            },
        )
        .expect_err("oversized held_by");
    assert!(matches!(
        error,
        StoreError::InvalidWork(ref reason) if reason == crate::SessionIdAdmissionError::TooLong.as_str()
    ));
    assert!(!error.to_string().contains(&giant.0));
}

/// A store with `count` catalogued roots, short and Unicode titles among them.
fn catalogued(count: usize) -> (SqliteStore, Vec<String>) {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let ids = (0..count)
        .map(|index| {
            let mut request = root_request(
                "project-catalog-fts-scan",
                &format!("root-{index}"),
                i64::try_from(index).unwrap(),
            );
            request.title = match index % 3 {
                0 => "a".into(),
                1 => format!("Zażółć gęślą jaźń {index}"),
                _ => format!("root {index} {}", "words ".repeat(30)),
            };
            store
                .create_work(&request, &DevelopmentNoopRedactor)
                .expect("catalogued root")
                .work_id
                .0
                .to_string()
        })
        .collect();
    (store, ids)
}

fn catalog_findings(store: &SqliteStore, sql: &str) -> Vec<String> {
    store.connection.execute_batch("SAVEPOINT damage").unwrap();
    store.connection.execute_batch(sql).expect("damage");
    let report = store.verify_all().expect("doctor");
    store
        .connection
        .execute_batch("ROLLBACK TO damage; RELEASE damage")
        .unwrap();
    report.invalid_work_records
}

// The catalog's stored text is read in one pass however many items there are.
#[test]
fn doctor_reads_the_catalog_text_once_whatever_the_item_count() {
    for count in [1, 30] {
        let (store, _) = catalogued(count);
        store
            .connection
            .pragma_update(None, "query_only", true)
            .unwrap();
        let before = crate::storage::fts_verification::fts_content_scans();
        let report = store.verify_all().expect("doctor");
        assert!(report.is_healthy(), "{report:?}");
        // One pass for the work catalog and one for the memory index.
        assert_eq!(
            crate::storage::fts_verification::fts_content_scans() - before,
            2,
            "{count} items"
        );
    }
}

// Each row-binding defect has its finding: changed text, a repeated row, a
// missing row and an orphan; an item that does not decode still owns its row.
#[test]
fn doctor_names_every_catalog_binding_defect() {
    let (store, ids) = catalogued(3);
    let id = &ids[0];
    let binding = format!("work_catalog:{id}:projection_binding");
    for (defect, sql) in [
        (
            "changed text",
            format!("UPDATE work_catalog_fts SET search_text = 'changed' WHERE work_id = '{id}'"),
        ),
        (
            "repeated row",
            format!(
                "INSERT INTO work_catalog_fts (work_id, search_text)
                 SELECT work_id, search_text FROM work_catalog_fts WHERE work_id = '{id}'"
            ),
        ),
        (
            "missing row",
            format!("DELETE FROM work_catalog_fts WHERE work_id = '{id}'"),
        ),
    ] {
        let invalid = catalog_findings(&store, &sql);
        assert!(invalid.contains(&binding), "{defect}: {invalid:?}");
    }
    let invalid = catalog_findings(
        &store,
        "INSERT INTO work_catalog_fts (work_id, search_text) VALUES ('no-such-item', 'x')",
    );
    assert!(
        invalid.contains(&"work_catalog:orphaned_fts_rows".to_owned()),
        "{invalid:?}"
    );
    // A row whose key or text is not text is a finding, never a conversion
    // error that stops doctor.
    for key in ["NULL", "42"] {
        let invalid = catalog_findings(
            &store,
            &format!("INSERT INTO work_catalog_fts (work_id, search_text) VALUES ({key}, 'x')"),
        );
        assert!(
            invalid.contains(&"work_catalog:orphaned_fts_rows".to_owned()),
            "{key} key: {invalid:?}"
        );
    }
    for value in ["NULL", "X'FF'", "7"] {
        let invalid = catalog_findings(
            &store,
            &format!("UPDATE work_catalog_fts SET search_text = {value} WHERE work_id = '{id}'"),
        );
        assert!(invalid.contains(&binding), "{value}: {invalid:?}");
    }
    let invalid = catalog_findings(
        &store,
        &format!("UPDATE work_items SET item_json = X'7B7D' WHERE work_id = '{id}'"),
    );
    assert!(
        invalid
            .iter()
            .any(|label| label.starts_with(&format!("work_catalog:{id}:item_decode"))),
        "{invalid:?}"
    );
    assert!(
        !invalid.contains(&"work_catalog:orphaned_fts_rows".to_owned()),
        "an item that does not decode still owns its catalog row: {invalid:?}"
    );
}

/// Old and new catalog content scanners and the whole doctor, timed on the
/// same synthetic stores. Run on request:
/// `cargo test --lib catalog_fts_scan_cost_measurement -- --ignored --nocapture`.
#[test]
#[ignore = "measurement, run on request"]
fn catalog_fts_scan_cost_measurement() {
    for count in [1_000, 3_000, 9_000] {
        let started = std::time::Instant::now();
        let (store, ids) = catalogued(count);
        let fixture = started.elapsed();
        let (rows, bytes): (i64, i64) = store
            .connection
            .query_row(
                "SELECT COUNT(*), COALESCE(SUM(LENGTH(search_text)), 0) FROM work_catalog_fts",
                [],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        // The replaced path: one unindexed lookup per work item.
        let old = std::time::Instant::now();
        let mut found = 0;
        for id in &ids {
            let mut statement = store
                .connection
                .prepare("SELECT search_text FROM work_catalog_fts WHERE work_id = ?1")
                .unwrap();
            found += statement
                .query_map([id.as_str()], |row| row.get::<_, String>(0))
                .unwrap()
                .count();
        }
        let old = old.elapsed();
        assert_eq!(found, count);
        let new = std::time::Instant::now();
        let content = crate::storage::fts_verification::fts_content(
            &store.connection,
            "SELECT work_id, search_text FROM work_catalog_fts",
            |row| crate::storage::fts_verification::text(row, 1),
        )
        .unwrap()
        .unwrap();
        let new = new.elapsed();
        assert_eq!(content.len(), count);
        let doctor = std::time::Instant::now();
        assert!(store.verify_all().unwrap().is_healthy());
        let doctor = doctor.elapsed();
        println!(
            "catalog items={count} fts_rows={rows} fts_bytes={bytes} \
             old_per_item_scan_ms={:.1} new_one_pass_ms={:.1} whole_doctor_ms={:.1} fixture_ms={:.0}",
            old.as_secs_f64() * 1e3,
            new.as_secs_f64() * 1e3,
            doctor.as_secs_f64() * 1e3,
            fixture.as_secs_f64() * 1e3,
        );
    }
}
