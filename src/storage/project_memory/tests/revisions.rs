use super::*;

fn assert_revision_refusals_and_creation_replay(
    store: &mut SqliteStore,
    first: &RememberProjectMemoryRequest,
    third: &RememberProjectMemoryRequest,
) {
    let unchanged = crate::storage::test_database_shape_snapshot(&store.connection);
    for missing in [0, 4, u64::MAX] {
        assert!(
            matches!(store.project_memory_full(&first.project_id, &first.session_id, &first.actor, "rule", Some(missing)),
            Err(StoreError::ProjectMemoryRevisionNotFound { revision, current: 3, .. }) if revision == missing)
        );
    }
    let mut invalid_basis = third.clone();
    invalid_basis.expected_revision = Some(0);
    assert!(matches!(
        store.remember_project_memory(&invalid_basis, &DevelopmentNoopRedactor),
        Err(StoreError::InvalidProjectMemory(_))
    ));
    let mut missing_key = third.clone();
    missing_key.key = Some("absent".into());
    assert!(
        matches!(store.remember_project_memory(&missing_key, &DevelopmentNoopRedactor), Err(StoreError::ProjectMemoryNotFound(key)) if key == "absent")
    );
    let creation_replay = store
        .remember_project_memory(first, &DevelopmentNoopRedactor)
        .unwrap();
    assert!(creation_replay.duplicate);
    assert_eq!(
        (creation_replay.revision, creation_replay.replaced_revision),
        (1, None)
    );
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&store.connection),
        unchanged
    );
}

#[test]
fn memory_history_decoding_is_linear_without_duplicate_read_walks() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let mut request =
        project_memory_request("decode-history", "author", Some("rule"), "body", 1000);
    for revision in 1..=12 {
        request.body = format!("body {revision}");
        request.revise = revision > 1;
        store
            .remember_project_memory(&request, &DevelopmentNoopRedactor)
            .unwrap();
    }
    crate::canonical::reset_canonical_decode_count();
    store
        .project_memories(
            &request.project_id,
            &request.session_id,
            &request.actor,
            None,
            None,
        )
        .unwrap();
    let list = crate::canonical::canonical_decode_count();
    crate::canonical::reset_canonical_decode_count();
    store
        .project_memory_full(
            &request.project_id,
            &request.session_id,
            &request.actor,
            "rule",
            Some(1),
        )
        .unwrap();
    let full = crate::canonical::canonical_decode_count();
    crate::canonical::reset_canonical_decode_count();
    assert!(
        store
            .remember_project_memory(&request, &DevelopmentNoopRedactor)
            .unwrap()
            .duplicate
    );
    let replay = crate::canonical::canonical_decode_count();
    crate::canonical::reset_canonical_decode_count();
    store.rebuild_memory_index().unwrap();
    let rebuild = crate::canonical::canonical_decode_count();
    eprintln!(
        "memory history decodes: list={list}, full={full}, replay={replay}, rebuild={rebuild}"
    );
    // Two canonical objects per version, plus bounded head/admission overhead.
    assert!(list <= 2 * 12 + 8, "list decoded {list}");
    assert!(full <= 2 * 12 + 8, "full decoded {full}");
    assert!(replay <= 2 * 12 + 8, "replay decoded {replay}");
    assert!(rebuild <= 8 * 12 + 16, "rebuild decoded {rebuild}");
    request.body = "appended body".into();
    crate::canonical::reset_canonical_decode_count();
    store
        .remember_project_memory(&request, &DevelopmentNoopRedactor)
        .unwrap();
    let append = crate::canonical::canonical_decode_count();
    crate::canonical::reset_canonical_decode_count();
    store
        .forget_project_memory(
            &ForgetProjectMemoryRequest {
                project_id: request.project_id.clone(),
                session_id: request.session_id.clone(),
                key: "rule".into(),
                actor: request.actor.clone(),
                created_at: request.created_at,
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let forget = crate::canonical::canonical_decode_count();
    eprintln!("memory mutation decodes: append={append}, forget={forget}");
    // Mutations validate both their old chain and their new canonical state.
    assert!(append <= 4 * 13 + 8, "append decoded {append}");
    assert!(forget <= 4 * 13 + 8, "forget decoded {forget}");
}

#[test]
fn populated_memory_repair_restores_missing_indexes_and_rolls_back_invalid_heads() {
    for index in [
        "objects_project_memory_key",
        "objects_memory_assertion_version",
    ] {
        for retired in [false, true] {
            for damaged in [false, true] {
                let directory = crate::test_support::temp_home().unwrap();
                let database = directory.path().join("repair.db");
                let mut store = SqliteStore::open(&database).unwrap();
                let mut request =
                    project_memory_request("repair-history", "author", Some("rule"), "first", 1000);
                store
                    .remember_project_memory(&request, &DevelopmentNoopRedactor)
                    .unwrap();
                request.revise = true;
                request.body = "second".into();
                store
                    .remember_project_memory(&request, &DevelopmentNoopRedactor)
                    .unwrap();
                request.body = "current".into();
                store
                    .remember_project_memory(&request, &DevelopmentNoopRedactor)
                    .unwrap();
                if retired {
                    store
                        .forget_project_memory(
                            &ForgetProjectMemoryRequest {
                                project_id: request.project_id.clone(),
                                session_id: request.session_id.clone(),
                                key: "rule".into(),
                                actor: request.actor.clone(),
                                created_at: request.created_at,
                            },
                            &DevelopmentNoopRedactor,
                        )
                        .unwrap();
                }
                let history =
                    project_memory_history_on(&store.connection, &request.project_id, "rule")
                        .unwrap();
                let identities = history
                    .iter()
                    .map(|entry| entry.version_hash.clone())
                    .collect::<Vec<_>>();
                if damaged {
                    store
                        .connection
                        .execute("UPDATE memory_heads SET body = 'not canonical'", [])
                        .unwrap();
                }
                store
                    .connection
                    .execute_batch(&format!("DROP INDEX {index}"))
                    .unwrap();
                let before = crate::storage::test_database_shape_snapshot(&store.connection);
                drop(store);
                assert!(SqliteStore::open(&database).is_err());
                let result = SqliteStore::repair_rebuildable_projections(&database);
                if damaged {
                    assert!(
                        matches!(result, Err(StoreError::InvalidMemoryProjection(_))),
                        "{result:?}"
                    );
                    let inspected = Connection::open(&database).unwrap();
                    assert_eq!(
                        crate::storage::test_database_shape_snapshot(&inspected),
                        before
                    );
                    assert!(
                        !inspected
                            .query_row(
                                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE name = ?1)",
                                [index],
                                |row| row.get::<_, bool>(0)
                            )
                            .unwrap()
                    );
                } else {
                    assert!(
                        result
                            .unwrap_or_else(|error| panic!("{index}, retired={retired}: {error}"))
                            .is_healthy()
                    );
                    let reopened = SqliteStore::open(&database).unwrap();
                    let after = project_memory_history_on(
                        &reopened.connection,
                        &request.project_id,
                        "rule",
                    )
                    .unwrap();
                    assert_eq!(
                        after
                            .iter()
                            .map(|entry| entry.version_hash.clone())
                            .collect::<Vec<_>>(),
                        identities
                    );
                    assert_eq!(
                        after
                            .iter()
                            .map(|entry| entry.version.body.as_str())
                            .collect::<Vec<_>>(),
                        ["first", "second", "current"]
                    );
                    for revision in 1..=3 {
                        let full = reopened.project_memory_full(
                            &request.project_id,
                            &request.session_id,
                            &request.actor,
                            "rule",
                            Some(revision),
                        );
                        if retired {
                            assert!(matches!(full, Err(StoreError::ProjectMemoryRetired(_))));
                        } else {
                            let full = full.unwrap();
                            assert_eq!(full.revision, revision);
                            assert_eq!(full.current_revision, 3);
                            assert_eq!(
                                full.body,
                                after[usize::try_from(revision - 1).unwrap()].version.body
                            );
                        }
                    }
                }
            }
        }
    }
}

#[test]
fn concurrent_memory_revisions_with_expected_basis_have_one_typed_winner() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("race.db");
    let mut source = SqliteStore::open(&database).unwrap();
    let first = project_memory_request(
        "revision-race",
        "original-author",
        Some("rule"),
        "original",
        1000,
    );
    source
        .remember_project_memory(&first, &DevelopmentNoopRedactor)
        .unwrap();
    let barrier = Arc::new(Barrier::new(3));
    let spawn = |session: &'static str| {
        let database = database.clone();
        let barrier = Arc::clone(&barrier);
        std::thread::spawn(move || {
            let mut store = SqliteStore::open(database).unwrap();
            let mut request =
                project_memory_request("revision-race", session, Some("rule"), session, 2000);
            request.revise = true;
            request.expected_revision = Some(1);
            barrier.wait();
            store.remember_project_memory(&request, &DevelopmentNoopRedactor)
        })
    };
    let first = spawn("first-contender");
    let second = spawn("second-contender");
    barrier.wait();
    let results = [first.join().unwrap(), second.join().unwrap()];
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(
                result,
                Err(StoreError::ProjectMemoryRevisionConflict {
                    expected: 1,
                    current: 2,
                    ..
                })
            ))
            .count(),
        1
    );
    assert_eq!(
        project_memory_history_on(
            &source.connection,
            &ProjectId("revision-race".into()),
            "rule"
        )
        .unwrap()
        .len(),
        2
    );
}

#[test]
fn invalid_memory_revision_parent_refuses_reads_and_doctor_does_not_accept_the_head() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let first =
        project_memory_request("revision-invalid", "author", Some("rule"), "original", 1000);
    store
        .remember_project_memory(&first, &DevelopmentNoopRedactor)
        .unwrap();
    let foreign = project_memory_request(
        "revision-invalid",
        "author",
        Some("other"),
        "foreign root",
        1000,
    );
    store
        .remember_project_memory(&foreign, &DevelopmentNoopRedactor)
        .unwrap();
    let wrong_parent = lookup_project_memory_on(&store.connection, &first.project_id, "other")
        .unwrap()
        .unwrap();
    let bad = prepare_project_memory(&first, "rule", Some(&wrong_parent)).unwrap();
    SqliteStore::insert_object(&store.connection, "memory_version", &bad.version_object).unwrap();
    SqliteStore::insert_object(
        &store.connection,
        "memory_assertion_event",
        &bad.assertion_object,
    )
    .unwrap();
    let before = crate::storage::test_database_shape_snapshot(&store.connection);
    assert!(matches!(
        store.project_memory_full(
            &first.project_id,
            &first.session_id,
            &first.actor,
            "rule",
            None
        ),
        Err(StoreError::InvalidMemoryProjection(_))
    ));
    assert!(!store.verify_all().unwrap().invalid_objects.is_empty());
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&store.connection),
        before
    );
}

#[test]
fn memory_revisions_retain_attribution_replay_basis_and_rebuild_the_current_head() {
    let directory = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(directory.path().join("memory.db")).unwrap();
    let first = project_memory_request(
        "revision-project",
        "first-author",
        Some("rule"),
        "original",
        1000,
    );
    let created = store
        .remember_project_memory(&first, &DevelopmentNoopRedactor)
        .unwrap();
    assert_eq!(created.revision, 1);
    assert_eq!(created.replaced_revision, None);
    let mut revision = project_memory_request(
        "revision-project",
        "second-author",
        Some("rule"),
        "corrected",
        1000,
    );
    revision.revise = true;
    revision.expected_revision = Some(1);
    let revised = store
        .remember_project_memory(&revision, &DevelopmentNoopRedactor)
        .unwrap();
    assert_eq!((revised.revision, revised.replaced_revision), (2, Some(1)));
    let before = store.connection.total_changes();
    assert!(
        store
            .remember_project_memory(&revision, &DevelopmentNoopRedactor)
            .unwrap()
            .duplicate
    );
    assert_eq!(store.connection.total_changes(), before);
    let mut third = revision.clone();
    third.body = "current truth".into();
    third.session_id = SessionId("third-author".into());
    third.actor = actor("third-author");
    assert!(matches!(
        store.remember_project_memory(&third, &DevelopmentNoopRedactor),
        Err(StoreError::ProjectMemoryRevisionConflict {
            expected: 1,
            current: 2,
            ..
        })
    ));
    assert_eq!(store.connection.total_changes(), before);
    third.expected_revision = None;
    let receipt = store
        .remember_project_memory(&third, &DevelopmentNoopRedactor)
        .unwrap();
    assert_eq!((receipt.revision, receipt.replaced_revision), (3, Some(2)));
    assert_revision_refusals_and_creation_replay(&mut store, &first, &third);
    let mut overflowing_basis = third.clone();
    overflowing_basis.expected_revision = Some(u64::MAX);
    assert!(
        matches!(
            store.remember_project_memory(&overflowing_basis, &DevelopmentNoopRedactor),
            Err(StoreError::ProjectMemoryRevisionConflict {
                expected: u64::MAX,
                current: 3,
                ..
            })
        ),
        "an overflowing explicit basis must never fall back to basis-free replay"
    );
    let before = store.connection.total_changes();
    let replay = store
        .remember_project_memory(&revision, &DevelopmentNoopRedactor)
        .unwrap();
    assert!(replay.duplicate);
    assert_eq!(
        replay.revision, 2,
        "explicit-basis replay recovers the original revision even after a later append"
    );
    assert_eq!(store.connection.total_changes(), before);
    for (index, expected) in [&first, &revision, &third].into_iter().enumerate() {
        let full = store
            .project_memory_full(
                &first.project_id,
                &first.session_id,
                &first.actor,
                "rule",
                Some(index as u64 + 1),
            )
            .unwrap();
        assert_eq!(full.body, expected.body);
        assert_eq!(full.session_id, Some(expected.session_id.clone()));
        assert_eq!(full.current_revision, 3);
    }
    let full = store
        .project_memory_full(
            &first.project_id,
            &first.session_id,
            &first.actor,
            "rule",
            None,
        )
        .unwrap();
    assert_eq!(full.body, third.body);
    assert_eq!(
        project_memory_state_on(&store.connection, &first.project_id).unwrap(),
        (1, 3)
    );
    assert_eq!(
        derived_project_memory_state_on(&store.connection, &first.project_id).unwrap(),
        (1, 3)
    );
    let report = store.verify_all().unwrap();
    assert!(
        report.invalid_objects.is_empty(),
        "{:?}",
        report.invalid_objects
    );
    store.rebuild_memory_index().unwrap();
    assert_eq!(
        store
            .project_memory_full(
                &first.project_id,
                &first.session_id,
                &first.actor,
                "rule",
                None
            )
            .unwrap(),
        full
    );
    assert!(store.verify_all().unwrap().invalid_objects.is_empty());
    let rows = store
        .project_memories(
            &first.project_id,
            &first.session_id,
            &first.actor,
            Some("current"),
            None,
        )
        .unwrap();
    assert_eq!(rows.memories.len(), 1);
    assert_eq!(rows.memories[0].revision, 3);
    assert!(
        store
            .project_memories(
                &first.project_id,
                &first.session_id,
                &first.actor,
                Some("original"),
                None
            )
            .unwrap()
            .memories
            .is_empty()
    );
    store
        .forget_project_memory(
            &ForgetProjectMemoryRequest {
                project_id: first.project_id.clone(),
                session_id: first.session_id.clone(),
                key: "rule".into(),
                actor: first.actor.clone(),
                created_at: first.created_at,
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    assert!(matches!(
        store.remember_project_memory(&revision, &DevelopmentNoopRedactor),
        Err(StoreError::ProjectMemoryRetired(_))
    ));
    assert!(matches!(
        store.project_memory_full(
            &first.project_id,
            &first.session_id,
            &first.actor,
            "rule",
            Some(1)
        ),
        Err(StoreError::ProjectMemoryRetired(_))
    ));
    assert_eq!(
        project_memory_history_on(&store.connection, &first.project_id, "rule")
            .unwrap()
            .len(),
        3
    );
    store.rebuild_memory_index().unwrap();
    assert_eq!(
        project_memory_state_on(&store.connection, &first.project_id).unwrap(),
        (0, 4)
    );
}
