use super::super::feeds::reserve_feed_position;
use super::super::test_support::*;
use super::super::*;
use super::*;

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
fn resolver_sql_bounds_collisions_and_recovers_an_omitted_target_by_full_id() {
    let mut store = SqliteStore::open_in_memory().expect("collision fixture");
    let mut items = (0..9)
        .map(|index| {
            store
                .create_work(
                    &root_request("ambiguous-project", &format!("candidate-{index}"), index),
                    &DevelopmentNoopRedactor,
                )
                .expect("candidate")
        })
        .collect::<Vec<_>>();

    // Production enforces uniqueness. This deliberately corrupt-shaped
    // fixture replaces the constrained table with an unconstrained copy so
    // the real resolver SQL and canonical projection loader exercise the
    // defensive ambiguity path.
    store
        .connection
        .execute_batch(
            "PRAGMA foreign_keys = OFF;
             CREATE TABLE work_items_collision AS SELECT * FROM work_items;
             DROP TABLE work_items;
             ALTER TABLE work_items_collision RENAME TO work_items;",
        )
        .expect("remove short-ref uniqueness for collision fixture");
    let transaction = store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("collision transaction");
    for (index, item) in items.iter_mut().enumerate() {
        let mut event = latest_canonical_work_event_for_item(&transaction, item.work_id)
            .expect("latest candidate event");
        item.short_ref = "w-collision".into();
        item.title = format!("Collision candidate {index}");
        item.revision += 1;
        item.updated_at = at(10 + i64::try_from(index).expect("small index"));
        transaction
            .execute(
                "UPDATE work_items SET short_ref = ?2 WHERE work_id = ?1",
                params![item.work_id.0.to_string(), item.short_ref],
            )
            .expect("persist colliding short ref");
        persist_work_item(&transaction, item).expect("persist colliding candidate");
        event.revision = item.revision;
        event.work.clone_from(item);
        event.created_at = item.updated_at;
        append_work_event(&transaction, &WorkEventDraft::from(&event))
            .expect("append colliding candidate event");
    }
    transaction.commit().expect("commit collision fixture");
    store
        .connection
        .execute_batch("PRAGMA foreign_keys = ON;")
        .expect("restore foreign-key enforcement after collision fixture");

    let error = store
        .resolve_work_ref(&ProjectId("ambiguous-project".into()), "w-collision")
        .expect_err("nine matching rows must be ambiguous");
    let StoreError::WorkReferenceAmbiguous {
        reference,
        candidates,
        more,
    } = error
    else {
        panic!("expected typed ambiguous-reference error, got {error:?}");
    };
    assert_eq!(reference, "w-collision");
    assert_eq!(candidates.len(), MAX_AMBIGUOUS_WORK_CANDIDATES);
    assert_eq!(more, 1);
    let mut expected = items.iter().map(|item| item.work_id).collect::<Vec<_>>();
    expected.sort_by_key(|work_id| work_id.0);
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| candidate.work_id)
            .collect::<Vec<_>>(),
        expected[..MAX_AMBIGUOUS_WORK_CANDIDATES]
    );
    assert_eq!(
        candidates
            .iter()
            .map(|candidate| candidate.title.as_str())
            .collect::<Vec<_>>(),
        expected[..MAX_AMBIGUOUS_WORK_CANDIDATES]
            .iter()
            .map(|work_id| {
                items
                    .iter()
                    .find(|item| item.work_id == *work_id)
                    .expect("expected item")
                    .title
                    .as_str()
            })
            .collect::<Vec<_>>()
    );
    let omitted_id = expected[MAX_AMBIGUOUS_WORK_CANDIDATES];
    let omitted = items
        .iter()
        .find(|item| item.work_id == omitted_id)
        .expect("omitted target");
    let omitted_candidate = WorkReferenceCandidate {
        work_id: omitted.work_id,
        short_ref: omitted.short_ref.clone(),
        title: omitted.title.clone(),
        lifecycle: omitted.lifecycle,
    };
    assert_eq!(
        command_work_ref_on(
            &store.connection,
            &ProjectId("ambiguous-project".into()),
            &omitted_candidate,
        )
        .expect("known omitted target uses its full id"),
        omitted_id.0.to_string()
    );
}

#[test]
fn prerequisite_state_uses_one_hop_satisfaction_and_dead_edge_rules() {
    let prerequisite_id = WorkId::new();
    assert_eq!(
        classify_prerequisite_state(WorkLifecycle::Completed, None, prerequisite_id)
            .expect("completed state"),
        WorkPrerequisiteState::Satisfied
    );
    assert_eq!(
        classify_prerequisite_state(WorkLifecycle::Open, None, prerequisite_id)
            .expect("open state"),
        WorkPrerequisiteState::Pending
    );
    assert_eq!(
        classify_prerequisite_state(WorkLifecycle::Cancelled, None, prerequisite_id)
            .expect("cancelled state"),
        WorkPrerequisiteState::Dead
    );
    assert_eq!(
        classify_prerequisite_state(
            WorkLifecycle::Superseded,
            Some(WorkLifecycle::Completed),
            prerequisite_id,
        )
        .expect("completed replacement"),
        WorkPrerequisiteState::Satisfied
    );
    assert_eq!(
        classify_prerequisite_state(
            WorkLifecycle::Superseded,
            Some(WorkLifecycle::Open),
            prerequisite_id,
        )
        .expect("live replacement"),
        WorkPrerequisiteState::Pending
    );
    assert_eq!(
        classify_prerequisite_state(
            WorkLifecycle::Superseded,
            Some(WorkLifecycle::Cancelled),
            prerequisite_id,
        )
        .expect("cancelled replacement"),
        WorkPrerequisiteState::Dead
    );
}

#[test]
fn prerequisite_page_bounds_real_edges_and_counts_each_omitted_class() {
    const PAGE_LIMIT: usize = 8;

    let project = "bounded-prerequisite-page";
    let mut store = SqliteStore::open_in_memory().expect("prerequisite page fixture");
    let mut dependent = store
        .create_work(
            &root_request(project, "dependent", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("dependent work");
    let prerequisites = (0..14)
        .map(|index| {
            store
                .create_work(
                    &root_request(project, &format!("prerequisite-{index}"), index + 1),
                    &DevelopmentNoopRedactor,
                )
                .expect("prerequisite work")
        })
        .collect::<Vec<_>>();

    for (index, prerequisite) in prerequisites.iter().enumerate() {
        dependent = store
            .add_work_prerequisite(
                &ChangeWorkPrerequisiteRequest {
                    work_id: dependent.work_id,
                    prerequisite_id: prerequisite.work_id,
                    expected_revision: dependent.revision,
                    authority: delegated(project, "planner"),
                    actor: actor("planner"),
                    idempotency_key: format!("add-prerequisite-{index}"),
                    changed_at: at(100 + i64::try_from(index).expect("small index")),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("add prerequisite edge");
    }

    for (index, prerequisite) in prerequisites[..11].iter().enumerate() {
        store
            .dispose_work(
                &DisposeWorkRequest {
                    work_id: prerequisite.work_id,
                    expected_work_revision: prerequisite.revision,
                    disposition: WorkDisposition::Cancelled,
                    replacement_id: None,
                    reason: "exercise dead prerequisite paging".into(),
                    actor: actor("planner"),
                    idempotency_key: format!("cancel-prerequisite-{index}"),
                    disposed_at: at(200 + i64::try_from(index).expect("small index")),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("cancel prerequisite");
    }

    let completed = prerequisites.last().expect("completed prerequisite");
    let completed_claim = claim(
        &mut store,
        completed,
        "planner",
        "completed-prerequisite-claim",
        300,
        300,
    );
    let completed_evidence = evidence(
        &mut store,
        completed,
        &completed_claim,
        "planner",
        "completed-prerequisite-evidence",
        301,
    );
    checkpoint(
        &mut store,
        completed,
        &completed_claim,
        "planner",
        "completed-prerequisite-checkpoint",
        302,
        std::slice::from_ref(&completed_evidence),
    );
    complete(
        &mut store,
        completed,
        &completed_claim,
        "planner",
        &completed_evidence,
        "completed-prerequisite-complete",
        303,
    )
    .expect("complete prerequisite");

    let page = store
        .work_prerequisites_with_state(dependent.work_id, PAGE_LIMIT)
        .expect("bounded prerequisite page");
    assert_eq!(page.items.len(), PAGE_LIMIT);
    assert!(
        page.items
            .iter()
            .all(|(_, state)| *state == WorkPrerequisiteState::Dead)
    );
    assert_eq!(page.omitted_by_state, [3, 2, 1]);

    let mixed_page = store
        .work_prerequisites_with_state(dependent.work_id, 13)
        .expect("mixed-state prerequisite page");
    let mixed_states = mixed_page
        .items
        .iter()
        .map(|(_, state)| *state)
        .collect::<Vec<_>>();
    assert_eq!(
        mixed_states,
        [
            vec![WorkPrerequisiteState::Dead; 11],
            vec![WorkPrerequisiteState::Pending; 2],
        ]
        .concat()
    );
    assert_eq!(mixed_page.omitted_by_state, [0, 0, 1]);

    let mut expected_refs = prerequisites[..13]
        .iter()
        .map(|prerequisite| prerequisite.short_ref.clone())
        .collect::<Vec<_>>();
    expected_refs[..11].sort();
    expected_refs[11..].sort();
    assert_eq!(
        mixed_page
            .items
            .iter()
            .map(|(prerequisite, _)| prerequisite.short_ref.clone())
            .collect::<Vec<_>>(),
        expected_refs
    );
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

#[test]
fn work_feed_arithmetic_refuses_overflow() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let feed = FeedId::Project(ProjectId("project-feed-overflow".into()));

    let interval = store.work_feed_between(&feed, i64::MIN, i64::MAX);
    assert!(matches!(
        interval,
        Err(StoreError::InvalidWorkProjection(reason)) if reason.contains("overflowed")
    ));

    let (feed_kind, feed_id) = feed_parts(&feed);
    store
        .connection
        .execute(
            "INSERT INTO work_feed_heads (feed_kind, feed_id, position)
             VALUES (?1, ?2, ?3)",
            params![feed_kind, feed_id, i64::MAX],
        )
        .expect("install saturated feed head");
    let transaction = store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("overflow transaction");
    let allocation = reserve_feed_position(&transaction, &feed);
    assert!(matches!(
        allocation,
        Err(StoreError::InvalidWorkProjection(reason)) if reason.contains("position overflowed")
    ));
    transaction.rollback().expect("discard saturated feed head");

    assert!(matches!(
        checkpoint_feed_end(i64::MAX),
        Err(StoreError::InvalidWorkProjection(reason))
            if reason.contains("checkpoint run-feed position arithmetic overflowed")
    ));
}

#[test]
fn focused_work_memory_is_shared_once_while_private_scratch_stays_actor_local() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(
            &root_request("project-memory", "root-memory", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("create root");
    let owner_session = SessionId("planner".into());
    let work_claim = claim(&mut store, &root, "planner", "work-memory-claim", 1, 300);
    store
        .focus_work_session(&root.project_id, &owner_session, root.work_id, at(1))
        .expect("focus work before capture");
    let shared = store
        .capture_note(
            &NoteRequest {
                project_id: root.project_id.clone(),
                task_id: None,
                work_id: Some(root.work_id),
                prose: "Constraint: never bypass the focused work safety contract".into(),
                visibility: NoteVisibility::Shared,
                kind: None,
                authority: None,
                sensitivity: Some(Sensitivity::Internal),
                title: None,
                tags: Vec::new(),
                evidence: Vec::new(),
                refs: Vec::new(),
                actor: actor("planner"),
                idempotency_key: "shared-work-memory".into(),
                created_at: at(1),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("capture shared work memory");
    assert_eq!(
        shared.scope,
        Scope::Work {
            project: root.project_id.clone(),
            work: root.work_id,
        }
    );
    assert_eq!(shared.cursor, None);
    assert_eq!(shared.work_positions.len(), 9);

    let private = store
        .capture_note(
            &NoteRequest {
                project_id: root.project_id.clone(),
                task_id: None,
                work_id: Some(root.work_id),
                prose: "scratch: private focused hypothesis".into(),
                visibility: NoteVisibility::Private,
                kind: None,
                authority: None,
                sensitivity: Some(Sensitivity::Internal),
                title: None,
                tags: Vec::new(),
                evidence: Vec::new(),
                refs: Vec::new(),
                actor: actor("planner"),
                idempotency_key: "private-work-memory".into(),
                created_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("capture private work memory");
    assert!(matches!(private.scope, Scope::Agent { work: Some(work), .. } if work == root.work_id));
    assert!(private.work_positions.is_empty());

    let packet = store
        .build_context(&root.project_id, None, &owner_session, "planner", at(3))
        .expect("build focused work context");
    assert_eq!(packet.header.work_id, Some(root.work_id));
    assert!(
        packet
            .pinned
            .iter()
            .any(|item| item.version == shared.version)
    );
    store
        .show_memory(
            &private.version,
            &root.project_id,
            Some(crate::TaskId::new()),
            Some(root.work_id),
            &owner_session,
            "planner",
        )
        .expect("unrelated task binding does not hide owned work scratch");

    let restricted = store
        .capture_note(
            &NoteRequest {
                project_id: root.project_id.clone(),
                task_id: None,
                work_id: Some(root.work_id),
                prose: "restricted: must never appear in focus or search".into(),
                visibility: NoteVisibility::Shared,
                kind: None,
                authority: None,
                sensitivity: Some(Sensitivity::Restricted),
                title: None,
                tags: Vec::new(),
                evidence: Vec::new(),
                refs: Vec::new(),
                actor: actor("planner"),
                idempotency_key: "restricted-work-memory".into(),
                created_at: at(3),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("capture restricted work memory");

    let peer_session = SessionId("peer".into());
    store
        .focus_work_session(&root.project_id, &peer_session, root.work_id, at(3))
        .expect("focus peer on root work");
    let owner = store
        .search_work_memories(
            &root.project_id,
            root.work_id,
            &owner_session,
            "planner",
            None,
            Some(20),
        )
        .expect("owner work memories");
    assert_eq!(owner.len(), 2);
    let peer = store
        .search_work_memories(
            &root.project_id,
            root.work_id,
            &peer_session,
            "peer",
            None,
            Some(20),
        )
        .expect("peer work memories");
    assert_eq!(peer.len(), 1);
    assert_eq!(peer[0].version, shared.version);
    store
        .show_memory(
            &shared.version,
            &root.project_id,
            None,
            Some(root.work_id),
            &SessionId("peer".into()),
            "peer",
        )
        .expect("peer can inspect shared work memory");
    assert!(matches!(
        store.show_memory(
            &private.version,
            &root.project_id,
            None,
            Some(root.work_id),
            &SessionId("peer".into()),
            "peer",
        ),
        Err(StoreError::MemoryAccessDenied(_))
    ));
    assert!(matches!(
        store.show_memory(
            &restricted.version,
            &root.project_id,
            None,
            Some(root.work_id),
            &SessionId("planner".into()),
            "planner",
        ),
        Err(StoreError::MemoryAccessDenied(_))
    ));

    let project_feed = store
        .work_feed_after(&FeedId::Project(root.project_id.clone()), 0, 100)
        .expect("project feed");
    assert!(
        project_feed
            .iter()
            .any(|entry| entry.object_hash == shared.version)
    );
    assert!(
        project_feed
            .iter()
            .all(|entry| entry.object_hash != private.version)
    );

    let decomposition = store
        .decompose_work(
            &DecomposeWorkRequest {
                parent_id: root.work_id,
                expected_parent_revision: root.revision,
                children: vec![
                    child("memory-child", ChildRequirement::Optional, "Memory child"),
                    child(
                        "memory-sibling",
                        ChildRequirement::Optional,
                        "Memory sibling",
                    ),
                ],
                prerequisites: Vec::new(),
                authority: WorkPlanningAuthority::Claim {
                    run_id: work_claim.run_id,
                    holder: work_claim.holder.clone(),
                    claim_id: work_claim.claim_id,
                    claim_fence: work_claim.fence,
                },
                actor: actor("planner"),
                idempotency_key: "memory-child-decompose".into(),
                created_at: at(4),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("create child focus");
    let child_work = &decomposition.children[0];
    let sibling_work_id = decomposition.children[1].work_id;
    store
        .focus_work_session(&root.project_id, &peer_session, child_work.work_id, at(5))
        .expect("move peer focus to child work");
    claim(
        &mut store,
        child_work,
        "peer",
        "child-work-memory-claim",
        5,
        300,
    );
    let child_view = store
        .search_work_memories(
            &root.project_id,
            child_work.work_id,
            &peer_session,
            "peer",
            None,
            Some(20),
        )
        .expect("root-shared memory is applicable from child focus");
    assert!(
        child_view
            .iter()
            .any(|memory| memory.version == shared.version)
    );
    store
        .show_memory(
            &shared.version,
            &root.project_id,
            None,
            Some(child_work.work_id),
            &SessionId("peer".into()),
            "peer",
        )
        .expect("child focus can inspect root-shared memory");
    assert!(matches!(
        store.show_memory(
            &private.version,
            &root.project_id,
            None,
            Some(child_work.work_id),
            &owner_session,
            "planner",
        ),
        Err(StoreError::MemoryAccessDenied(_))
    ));

    assert!(matches!(
        store.search_work_memories(
            &root.project_id,
            root.work_id,
            &peer_session,
            "peer",
            None,
            Some(20),
        ),
        Err(StoreError::InvalidWork(_))
    ));

    let connection_token = store
        .resume_control_connection(&owner_session, at(6))
        .expect("open host control connection");
    let control_binding = store
        .bind_control_session(
            &root.project_id,
            "dummy:WORK-CONTEXT",
            "Fence focused work context",
            &owner_session,
            &connection_token,
            &actor("planner"),
            ControlAssurance::TurnGated,
            &[EffectClass::Observe],
            1,
            "bind-work-context",
            at(6),
        )
        .expect("bind control session");
    let synchronize = store
        .evaluate_control_turn(
            &root.project_id,
            &owner_session,
            &connection_token,
            &control_binding.routing_token,
            &TurnIntent {
                idempotency_key: "sync-work-context".into(),
                intent_fingerprint: ObjectHash::from_canonical_bytes(b"sync-work-context"),
                purpose: TurnPurpose::Ordinary,
                requested_effects: vec![EffectClass::Observe],
                resource_intents: Vec::new(),
            },
            at(7),
        )
        .expect("evaluate synchronization turn");
    let ControlTurnDecision::Grant { grant: synchronize } = synchronize else {
        panic!("initial work-context turn must grant");
    };
    let sync_token = synchronize
        .delivery
        .as_ref()
        .expect("initial task synchronization delivery")
        .page
        .delivery_token
        .clone();
    assert!(matches!(
        store
            .begin_control_turn(
                &root.project_id,
                &owner_session,
                &connection_token,
                &control_binding.routing_token,
                &synchronize.grant_id,
                &[sync_token],
                "begin-sync-work-context",
                at(8),
            )
            .expect("begin synchronization turn"),
        ControlTurnBeginDecision::Begin { .. }
    ));
    assert!(matches!(
        store
            .checkpoint_control_turn(
                &root.project_id,
                &owner_session,
                &connection_token,
                &control_binding.routing_token,
                &synchronize.grant_id,
                TurnNextIntent::Continue,
                "checkpoint-sync-work-context",
                at(9),
            )
            .expect("checkpoint synchronization turn"),
        ControlTurnCheckpointDecision::Checkpointed { .. }
    ));
    let guarded = store
        .evaluate_control_turn(
            &root.project_id,
            &owner_session,
            &connection_token,
            &control_binding.routing_token,
            &TurnIntent {
                idempotency_key: "guard-work-context".into(),
                intent_fingerprint: ObjectHash::from_canonical_bytes(b"guard-work-context"),
                purpose: TurnPurpose::Ordinary,
                requested_effects: vec![EffectClass::Observe],
                resource_intents: Vec::new(),
            },
            at(10),
        )
        .expect("evaluate context-only guarded turn");
    let ControlTurnDecision::Grant { grant: guarded } = guarded else {
        panic!("context-only work turn must grant");
    };
    let guarded_delivery = guarded
        .delivery
        .as_ref()
        .expect("work context is delivered even at the task-feed head");
    assert_eq!(
        guarded_delivery.page.from_cursor,
        guarded_delivery.page.to_cursor
    );
    let guarded_token = guarded_delivery.page.delivery_token.clone();
    let private_after_grant = store
        .capture_note(
            &NoteRequest {
                project_id: root.project_id.clone(),
                task_id: None,
                work_id: Some(root.work_id),
                prose: "Constraint: newly captured private work rules require a fresh packet"
                    .into(),
                visibility: NoteVisibility::Private,
                kind: None,
                authority: None,
                sensitivity: Some(Sensitivity::Internal),
                title: None,
                tags: Vec::new(),
                evidence: Vec::new(),
                refs: Vec::new(),
                actor: actor("planner"),
                idempotency_key: "private-work-after-grant".into(),
                created_at: at(11),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("capture owner-private work memory after grant");
    assert!(private_after_grant.work_positions.is_empty());
    assert!(matches!(
        store
            .begin_control_turn(
                &root.project_id,
                &owner_session,
                &connection_token,
                &control_binding.routing_token,
                &guarded.grant_id,
                &[guarded_token],
                "begin-stale-work-context",
                at(11),
            )
            .expect("stale work context is a typed refusal"),
        ControlTurnBeginDecision::Refuse {
            code: ControlRefusalCode::DeltaRequired
        }
    ));
    let conflicting = store
        .capture_note(
            &NoteRequest {
                project_id: root.project_id.clone(),
                task_id: None,
                work_id: Some(child_work.work_id),
                prose: "Constraint: bypass the focused work safety contract".into(),
                visibility: NoteVisibility::Shared,
                kind: None,
                authority: None,
                sensitivity: Some(Sensitivity::Internal),
                title: None,
                tags: Vec::new(),
                evidence: Vec::new(),
                refs: Vec::new(),
                actor: actor("peer"),
                idempotency_key: "conflicting-work-memory".into(),
                created_at: at(12),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("capture conflicting root-shared memory from child focus");
    let contradiction = store
        .record_memory_contradiction(
            &root.project_id,
            None,
            Some(child_work.work_id),
            &peer_session,
            "peer",
            &shared.version,
            &conflicting.version,
            "the focused work safety rules cannot both guide execution",
            "work-contradiction",
            actor("peer"),
            at(7),
            &DevelopmentNoopRedactor,
        )
        .expect("record pure-work contradiction");
    assert!(contradiction.cursor.is_none());
    assert!(!contradiction.work_positions.is_empty());
    assert!(matches!(
        store.build_context(&root.project_id, None, &peer_session, "peer", at(8)),
        Err(StoreError::PinnedContradiction { .. })
    ));
    store
        .focus_work_session(&root.project_id, &peer_session, sibling_work_id, at(9))
        .expect("move peer focus to sibling in the same root");
    assert!(matches!(
        store.build_context(&root.project_id, None, &peer_session, "peer", at(10)),
        Err(StoreError::PinnedContradiction { .. })
    ));
}

#[test]
fn context_explanation_requires_the_current_work_focus() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let first = store
        .create_work(
            &root_request("project-context-focus", "context-focus-first", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("first root");
    let second = store
        .create_work(
            &root_request("project-context-focus", "context-focus-second", 1),
            &DevelopmentNoopRedactor,
        )
        .expect("second root");
    let session = SessionId("planner".into());
    store
        .focus_work_session(&first.project_id, &session, first.work_id, at(2))
        .expect("focus first root");
    let packet = store
        .build_context(&first.project_id, None, &session, "planner", at(3))
        .expect("focused context");
    assert_eq!(
        store
            .explain_context(
                &packet.header.packet_hash,
                &first.project_id,
                &session,
                "planner",
            )
            .expect("current focus remains authorized")
            .work_id,
        Some(first.work_id)
    );

    store
        .focus_work_session(&first.project_id, &session, second.work_id, at(4))
        .expect("change focus");
    assert!(matches!(
        store.explain_context(
            &packet.header.packet_hash,
            &first.project_id,
            &session,
            "planner",
        ),
        Err(StoreError::PacketAccessDenied(_))
    ));
}
