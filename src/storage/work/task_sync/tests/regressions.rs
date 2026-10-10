use super::*;

fn many_tasks() -> TaskTree {
    let template = candidate()
        .tasks
        .into_iter()
        .find(|t| t.lifecycle == WorkLifecycle::Proposed)
        .unwrap();
    let mut tree = candidate();
    tree.tasks = (0..40)
        .map(|i| {
            let mut task = template.clone();
            task.id = WorkId::new();
            task.title = format!("task {i}");
            task
        })
        .collect();
    tree
}

#[test]
fn task_sync_next_snapshot_observes_another_connections_import() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("origin-snapshots.db");
    let mut reader = SqliteStore::open(&database).unwrap();
    let first = candidate();
    import(&mut reader, &first);
    let mut writer = SqliteStore::open(&database).unwrap();
    let mut second = candidate();
    second.project.project_id = ProjectId("second-import".into());
    let old_changes = reader.connection.total_changes();
    origin::ENUMERATIONS.with(|count| count.set(0));
    reader
        .work_read_snapshot(|store| {
            query::load_work_item(&store.connection, first.tasks[0].id)?;
            import(&mut writer, &second);
            assert_eq!(store.connection.total_changes(), old_changes);
            assert!(query::load_work_item(&store.connection, second.tasks[0].id).is_err());
            query::load_work_item(&store.connection, first.tasks[0].id)?;
            Ok(())
        })
        .unwrap();
    assert!(reader.connection.is_autocommit());
    assert_eq!(origin::ENUMERATIONS.with(std::cell::Cell::get), 2);
    reader
        .work_read_snapshot(|store| {
            for task in &second.tasks {
                query::load_work_item(&store.connection, task.id)?;
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(origin::ENUMERATIONS.with(std::cell::Cell::get), 2);
    assert_eq!(reader.connection.total_changes(), old_changes);
}

#[test]
fn task_sync_origin_enumeration_is_only_for_audits_and_never_for_graph_loads() {
    let tree = many_tasks();
    let mut store = SqliteStore::open_in_memory().unwrap();
    import(&mut store, &tree);
    // Unrelated canonical objects must not multiply enumeration per task.
    for i in 0..100 {
        let object = CanonicalObject::mint(&serde_json::json!({"value": i})).unwrap();
        SqliteStore::insert_object(&store.connection, "test_unrelated", &object).unwrap();
    }
    origin::ENUMERATIONS.with(|c| c.set(0));
    store
        .work_read_snapshot(|store| {
            for task in &tree.tasks {
                query::load_work_item(&store.connection, task.id)?;
            }
            Ok(())
        })
        .unwrap();
    assert_eq!(origin::ENUMERATIONS.with(std::cell::Cell::get), 0);
    origin::ENUMERATIONS.with(|c| c.set(0));
    store.export_task_tree(&tree.project.project_id).unwrap();
    assert_eq!(origin::ENUMERATIONS.with(std::cell::Cell::get), 0);
    origin::ENUMERATIONS.with(|c| c.set(0));
    assert!(!import(&mut store, &tree).changed);
    assert_eq!(origin::ENUMERATIONS.with(std::cell::Cell::get), 1);
    origin::ENUMERATIONS.with(|c| c.set(0));
    assert!(store.verify_all().unwrap().is_healthy());
    assert_eq!(origin::ENUMERATIONS.with(std::cell::Cell::get), 1);
    origin::ENUMERATIONS.with(|c| c.set(0));
    let saved = store
        .save_work_graph_snapshot(
            &tree.project.project_id,
            &actor("saver"),
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(6),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    // Graph save performs one whole-graph integrity audit, not a scan per item.
    assert_eq!(origin::ENUMERATIONS.with(std::cell::Cell::get), 1);
    origin::ENUMERATIONS.with(|c| c.set(0));
    {
        let transaction = store.begin_work_mutation().unwrap();
        for task in &tree.tasks {
            query::load_work_item(&transaction, task.id).unwrap();
        }
        assert_eq!(origin::ENUMERATIONS.with(std::cell::Cell::get), 0);
        // Mutation rollback drops its cache, even though this batch wrote nothing.
        transaction.rollback().unwrap();
    }
    store.export_task_tree(&tree.project.project_id).unwrap();
    assert_eq!(origin::ENUMERATIONS.with(std::cell::Cell::get), 0);
    let mut legacy = SqliteStore::open_in_memory().unwrap();
    legacy
        .load_work_graph_snapshot(
            &tree.project.project_id,
            &actor("loader"),
            &saved.document.container_bytes().unwrap(),
            false,
            at(7),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    origin::ENUMERATIONS.with(|c| c.set(0));
    legacy.export_task_tree(&tree.project.project_id).unwrap();
    assert_eq!(origin::ENUMERATIONS.with(std::cell::Cell::get), 0);
}

#[test]
fn task_sync_read_words_use_primary_keys_and_decode_only_requested_origins() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("read-words.db");
    let mut tree = many_tasks();
    for task in &mut tree.tasks {
        task.lifecycle = WorkLifecycle::Open;
        task.assigned_to = Some("reader".into());
        task.deferred_until = None;
    }
    let mut store = SqliteStore::open(&database).unwrap();
    import(&mut store, &tree);
    for i in 0..100 {
        let object = CanonicalObject::mint(&serde_json::json!({"value": i})).unwrap();
        SqliteStore::insert_object(&store.connection, "test_unrelated", &object).unwrap();
    }
    let before = state(&store);
    origin::ENUMERATIONS.with(|c| c.set(0));
    origin::DECODES.with(|c| c.set(0));
    store.get_work_item(tree.tasks[0].id).unwrap();
    assert_eq!(origin::ENUMERATIONS.with(std::cell::Cell::get), 0);
    assert_eq!(origin::DECODES.with(std::cell::Cell::get), 1);
    let verbs = crate::verbs::AgentVerbs::new(
        database.clone(),
        tree.project.project_id.clone(),
        "reader".into(),
        crate::SessionId("task-sync-read-word".into()),
        None,
    );
    origin::ENUMERATIONS.with(|c| c.set(0));
    origin::DECODES.with(|c| c.set(0));
    verbs
        .next(
            &crate::verbs::NextInput {
                peek: true,
                ..Default::default()
            },
            at(6),
        )
        .unwrap();
    assert_eq!(origin::ENUMERATIONS.with(std::cell::Cell::get), 0);
    let decoded = origin::DECODES.with(std::cell::Cell::get);
    assert!(decoded > 0 && decoded <= tree.tasks.len(), "{decoded}");
    origin::ENUMERATIONS.with(|c| c.set(0));
    origin::DECODES.with(|c| c.set(0));
    verbs
        .ls(
            &crate::verbs::LsInput {
                all: true,
                limit: Some(40),
                ..Default::default()
            },
            at(6),
        )
        .unwrap();
    // An ordinary catalog listing reads projections, without canonical origins.
    assert_eq!(origin::ENUMERATIONS.with(std::cell::Cell::get), 0);
    assert_eq!(origin::DECODES.with(std::cell::Cell::get), 0);
    verbs.show(&tree.tasks[0].id.0.to_string(), at(6)).unwrap();
    assert_eq!(origin::ENUMERATIONS.with(std::cell::Cell::get), 0);
    assert_eq!(origin::DECODES.with(std::cell::Cell::get), 1);
    // Each new word opens a new snapshot and cannot reuse a previous word's cache.
    verbs.show(&tree.tasks[0].id.0.to_string(), at(7)).unwrap();
    assert_eq!(origin::ENUMERATIONS.with(std::cell::Cell::get), 0);
    assert_eq!(origin::DECODES.with(std::cell::Cell::get), 2);
    assert_eq!(state(&store), before);
}

#[test]
fn task_sync_held_service_read_shares_origins_across_distinct_store_calls() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("held-service.db");
    let tree = many_tasks();
    let mut store = SqliteStore::open(&database).unwrap();
    import(&mut store, &tree);
    let before = state(&store);
    let service = crate::work_service::LocalWorkService::new(
        database,
        tree.project.project_id.clone(),
        "reader".into(),
        crate::SessionId("task-sync-held-service".into()),
        None,
    );
    origin::ENUMERATIONS.with(|c| c.set(0));
    origin::DECODES.with(|c| c.set(0));
    service.one_read_connection(|| {
        for task in &tree.tasks {
            for _ in 0..2 {
                service
                    .work_authored_contract(&task.id.0.to_string(), at(6))
                    .unwrap();
            }
        }
        assert_eq!(origin::ENUMERATIONS.with(std::cell::Cell::get), 0);
        assert_eq!(origin::DECODES.with(std::cell::Cell::get), tree.tasks.len());
    });
    service.one_read_connection(|| {
        service
            .work_authored_contract(&tree.tasks[0].id.0.to_string(), at(7))
            .unwrap();
    });
    assert_eq!(origin::ENUMERATIONS.with(std::cell::Cell::get), 0);
    assert_eq!(
        origin::DECODES.with(std::cell::Cell::get),
        tree.tasks.len() + 1
    );
    assert_eq!(state(&store), before);
}

#[test]
fn task_sync_historical_import_actor_is_not_readmitted_as_a_live_session() {
    let mut tree = many_tasks();
    tree.tasks.truncate(1);
    let mut store = SqliteStore::open_in_memory().unwrap();
    import(&mut store, &tree);
    let (id, bytes): (String, Vec<u8>) = store
        .connection
        .query_row(
            "SELECT object_id, canonical_json FROM objects WHERE object_kind = ?1",
            [ORIGIN_KIND],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let mut historical: ImportOrigin = serde_json::from_slice(&bytes).unwrap();
    historical.actor.session_id = Some(crate::SessionId(
        crate::domain::TASK_SYNC_REPORTED_SESSION.into(),
    ));
    assert!(crate::storage::admit_live_actor_session(&historical.actor).is_err());
    let object = CanonicalObject::freeze(&historical).unwrap();
    store
        .connection
        .execute(
            "UPDATE objects SET canonical_json = ?1 WHERE object_id = ?2",
            params![object.bytes(), id],
        )
        .unwrap();
    let error = query::load_work_item(&store.connection, historical.task.id).unwrap_err();
    // The canonical and projected import attribution must still match exactly.
    assert!(error.to_string().contains("projection differs"));
    let json: Vec<u8> = store
        .connection
        .query_row(
            "SELECT item_json FROM work_items WHERE work_id = ?1",
            [historical.task.id.0.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    let mut projection: WorkItem = serde_json::from_slice(&json).unwrap();
    projection.created_by = linked_actor(
        &historical.actor,
        &ObjectId::from_stored(id.clone()).unwrap(),
    );
    store
        .connection
        .execute(
            "UPDATE work_items SET item_json = ?1 WHERE work_id = ?2",
            params![
                CanonicalObject::freeze(&projection).unwrap().bytes(),
                historical.task.id.0.to_string()
            ],
        )
        .unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
    assert_eq!(
        store.get_work_item(historical.task.id).unwrap().created_by,
        linked_actor(&historical.actor, &ObjectId::from_stored(id).unwrap())
    );
    let record: RestoredRecord = SqliteStore::get_canonical_object_on(
        &store.connection,
        &historical.record_id,
        "work_restored_record",
    )
    .unwrap()
    .unwrap()
    .decode()
    .unwrap();
    assert_eq!(
        record.schema_version,
        crate::WORK_GRAPH_SNAPSHOT_SCHEMA_VERSION
    );
    // Admission still refuses this same actor for a new import.
    let mut target = SqliteStore::open_in_memory().unwrap();
    let error = target
        .import_task_tree(
            &tree.project.project_id,
            &historical.actor,
            &tree,
            false,
            at(8),
        )
        .unwrap_err();
    assert!(error.to_string().contains("cannot name a live session"));
}

#[test]
fn task_sync_held_read_unwind_and_own_write_discard_cached_origins() {
    let tree = many_tasks();
    let mut store = SqliteStore::open_in_memory().unwrap();
    import(&mut store, &tree);
    origin::ENUMERATIONS.with(|c| c.set(0));
    origin::DECODES.with(|c| c.set(0));
    let unwind = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _scope = store.begin_held_read().unwrap();
        for _ in 0..2 {
            store.get_work_item(tree.tasks[0].id).unwrap();
        }
        assert_eq!(origin::ENUMERATIONS.with(std::cell::Cell::get), 0);
        assert_eq!(origin::DECODES.with(std::cell::Cell::get), 1);
        panic!("test unwind");
    }));
    assert!(unwind.is_err());
    store.end_held_read().unwrap();
    let transaction = store.begin_work_mutation().unwrap();
    query::load_work_item(&transaction, tree.tasks[0].id).unwrap();
    assert_eq!(origin::ENUMERATIONS.with(std::cell::Cell::get), 0);
    assert_eq!(origin::DECODES.with(std::cell::Cell::get), 2);
    let object = CanonicalObject::mint(&serde_json::json!({"value": "write"})).unwrap();
    SqliteStore::insert_object(&transaction, "test_unrelated", &object).unwrap();
    query::load_work_item(&transaction, tree.tasks[0].id).unwrap();
    assert_eq!(origin::ENUMERATIONS.with(std::cell::Cell::get), 0);
    assert_eq!(origin::DECODES.with(std::cell::Cell::get), 3);
    transaction.rollback().unwrap();
    store.get_work_item(tree.tasks[0].id).unwrap();
    assert_eq!(origin::ENUMERATIONS.with(std::cell::Cell::get), 0);
}

#[test]
fn task_sync_completion_summary_refuses_whitespace_and_valid_import_exports_graph() {
    let mut tree = candidate();
    let index = tree
        .tasks
        .iter()
        .position(|task| task.completion.is_some())
        .unwrap();
    for summary in [" result", "result ", "\nresult", "result\n"] {
        tree.tasks[index].completion.as_mut().unwrap().summary = summary.into();
        let mut store = SqliteStore::open_in_memory().unwrap();
        let before = state(&store);
        let error = store
            .import_task_tree(
                &tree.project.project_id,
                &actor("operator"),
                &tree,
                false,
                at(5),
            )
            .unwrap_err();
        assert!(error.to_string().contains("summary must be normalized"));
        assert_eq!(state(&store), before);
    }
    tree.tasks[index].completion.as_mut().unwrap().summary = "result".into();
    let mut store = SqliteStore::open_in_memory().unwrap();
    import(&mut store, &tree);
    store
        .save_work_graph_snapshot(
            &tree.project.project_id,
            &actor("saver"),
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(6),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
}

#[test]
fn task_sync_dry_run_and_apply_refuse_foreign_blocker_identity() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let tree = candidate();
    import(&mut store, &tree);
    let mut foreign = tree.clone();
    foreign.project.project_id = ProjectId("foreign".into());
    foreign.tasks.retain(|task| !task.blockers.is_empty());
    foreign.tasks[0].id = WorkId::new();
    foreign.tasks[0].parent_id = None;
    let before = state(&store);
    for dry_run in [true, false] {
        let error = store
            .import_task_tree(
                &foreign.project.project_id,
                &actor("operator"),
                &foreign,
                dry_run,
                at(8),
            )
            .unwrap_err();
        assert!(error.to_string().contains("blocker id already belongs"));
        assert_eq!(state(&store), before);
    }
}

#[test]
fn task_sync_reported_authors_remain_distinct_and_disposal_is_labelled_a_report() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("show.db");
    let mut tree = candidate();
    let completed = tree
        .tasks
        .iter()
        .find(|task| task.completion.is_some())
        .unwrap();
    let mut other = completed.clone();
    other.id = WorkId::new();
    other.completion.as_mut().unwrap().actor.actor_id = "different-reported-author".into();
    tree.tasks.push(other);
    tree.tasks.sort_by_key(|task| task.id.0);
    let mut store = SqliteStore::open(&database).unwrap();
    import(&mut store, &tree);
    drop(store);
    let verbs = crate::verbs::AgentVerbs::new(
        database.clone(),
        tree.project.project_id.clone(),
        "reader".into(),
        crate::SessionId("task-sync-disposal-reader".into()),
        None,
    );
    let shown = shown_tasks(&database, &tree);
    let mut authors = Vec::new();
    for (task, (text, value)) in tree.tasks.iter().zip(shown) {
        if task.completion.is_some() {
            let entry = value["restored_history"]["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|entry| entry["kind"] == "completed")
                .unwrap();
            let author = entry["by"].as_str().unwrap().to_owned();
            assert!(author.starts_with("peer-actor-"));
            assert!(text.contains(&author));
            authors.push(author);
        }
        if task.disposal_reason.is_some() {
            assert!(text.contains("disposal_reported"));
            assert!(
                value["restored_history"]["items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|entry| entry["kind"] == "disposal_reported")
            );
            let work_ref = super::super::super::planning::short_ref(task.id);
            let window = verbs
                .show_records(
                    &work_ref,
                    &crate::verbs::ShowInput {
                        history: true,
                        ..Default::default()
                    },
                    at(6),
                )
                .unwrap();
            let row = window.value["history"]["items"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["kind"] == "disposal_reported")
                .unwrap();
            assert!(window.text().contains("disposal_reported"));
            let detail = verbs
                .show_records(
                    &work_ref,
                    &crate::verbs::ShowInput {
                        note: Some(row["locator"].as_str().unwrap().into()),
                        ..Default::default()
                    },
                    at(6),
                )
                .unwrap();
            assert_eq!(detail.value["note"]["kind"], "disposal_reported");
            assert_eq!(detail.value["note"]["member"]["kind"], "disposed");
            assert!(detail.text().contains("disposal_reported"));
        }
    }
    assert_eq!(authors.len(), 2);
    assert_ne!(authors[0], authors[1]);
}

#[test]
fn task_sync_reported_marker_is_historical_only() {
    let marker = crate::SessionId(crate::domain::TASK_SYNC_REPORTED_SESSION.into());
    assert_eq!(
        marker.validate_admitted(),
        Err(crate::SessionIdAdmissionError::ReservedReported)
    );
    let mut store = SqliteStore::open_in_memory().unwrap();
    let mut request = root_request("marker", "marker", 0);
    request.actor.session_id = Some(marker);
    let before = state(&store);
    let error = store
        .create_work(&request, &crate::DevelopmentNoopRedactor)
        .unwrap_err();
    assert!(error.to_string().contains("cannot name a live session"));
    assert_eq!(state(&store), before);
    assert!(
        candidate().validate().is_ok(),
        "historical marker remains accepted"
    );
}

#[test]
fn task_sync_integrity_refuses_decodable_mismatched_origin_and_missing_origin() {
    let tree = candidate();
    let mut store = SqliteStore::open_in_memory().unwrap();
    import(&mut store, &tree);
    let (id, bytes): (String, Vec<u8>) = store
        .connection
        .query_row(
            "SELECT object_id, canonical_json FROM objects WHERE object_kind = ?1 LIMIT 1",
            [ORIGIN_KIND],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    let original: ImportOrigin = serde_json::from_slice(&bytes).unwrap();
    for mutation in ["project", "task", "record"] {
        let mut tampered = original.clone();
        match mutation {
            "project" => tampered.project_id = ProjectId("wrong".into()),
            "task" => tampered.task.id = WorkId::new(),
            _ => tampered.record_id = ObjectId::from_canonical_bytes(b"absent-record"),
        }
        let bytes = CanonicalObject::freeze(&tampered).unwrap();
        store
            .connection
            .execute(
                "UPDATE objects SET canonical_json = ?1 WHERE object_id = ?2",
                params![bytes.bytes(), id],
            )
            .unwrap();
        let report = store.verify_all().unwrap();
        assert!(!report.is_healthy(), "{mutation}");
        assert!(format!("{report:?}").contains("task_sync_import_origin"));
    }
    store
        .connection
        .execute("DELETE FROM objects WHERE object_id = ?1", [&id])
        .unwrap();
    let error = query::load_work_item(&store.connection, original.task.id).unwrap_err();
    assert!(error.to_string().contains("task import origin missing"));
}
