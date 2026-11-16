//! Listing work is bounded by the page, while explicit full reads retain history.

use super::*;

const PROJECT: &str = "memory-list-cost";
const SESSION: &str = "memory-list-reader";

fn fixture(revisions: usize) -> SqliteStore {
    fixture_with_target(revisions, false)
}

fn fixture_with_target(revisions: usize, local_target: bool) -> SqliteStore {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let target = local_target.then(|| retiring::create_item(&mut store, PROJECT, "listing target"));
    for key in 0..23 {
        let key = format!("list-{key:02}");
        let mut request = project_memory_request(PROJECT, SESSION, Some(&key), "", 1000);
        if let Some(target) = &target {
            request.retiring_target = crate::domain::ProjectMemoryRetiringTargetChange::Set {
                target: crate::domain::ProjectMemoryRetiringTargetInput::Local {
                    work_ref: target.short_ref.clone(),
                },
            };
        }
        for revision in 1..=revisions {
            request.body = format!("needle current line\nrevision {revision}");
            request.revise = revision > 1;
            store
                .remember_project_memory(&request, &DevelopmentNoopRedactor)
                .unwrap();
        }
    }
    store
}

fn page(
    store: &SqliteStore,
    query: Option<&str>,
    after: Option<&str>,
    limit: usize,
) -> ProjectMemoryList {
    store
        .project_memories_at_cut(
            &ProjectId(PROJECT.into()),
            &SessionId(SESSION.into()),
            &actor(SESSION),
            query,
            after,
            false,
            limit,
        )
        .unwrap()
        .0
}

#[test]
fn memory_listing_decodes_only_the_page_independent_of_revision_count() {
    let mut measurements = Vec::new();
    for revisions in [2, 24] {
        let store = fixture(revisions);
        for limit in [1, 5, 20] {
            for query in [None, Some("needle")] {
                crate::canonical::reset_canonical_decode_count();
                let list = page(&store, query, None, limit);
                let decoded = crate::canonical::canonical_decode_count();
                assert_eq!(list.memories.len(), limit);
                assert!(
                    list.memories
                        .iter()
                        .all(|row| row.revision == revisions as u64)
                );
                measurements.push((revisions, limit, query, decoded));
            }
        }
    }
    eprintln!("memory listing (revisions, page, query, decoded): {measurements:?}");
    for (revisions, limit, query, decoded) in &measurements {
        assert!(
            *decoded <= 2 * (limit + 1),
            "{revisions} revisions, page {limit}, query {query:?}: decoded {decoded}"
        );
    }
    for (short, long) in measurements[..6].iter().zip(&measurements[6..]) {
        assert_eq!(
            short.3, long.3,
            "growing history must not grow listing work"
        );
    }
}

#[test]
fn memory_listing_local_target_validation_is_page_bounded_and_preserves_state() {
    let mut measurements = Vec::new();
    for revisions in [2, 24] {
        let store = fixture_with_target(revisions, true);
        let expected = history_row(&store, "list-00");
        assert!(expected.retiring_state.is_some());
        for limit in [1, 5, 20] {
            for query in [None, Some("needle")] {
                crate::canonical::reset_canonical_decode_count();
                let list = page(&store, query, None, limit);
                let decoded = crate::canonical::canonical_decode_count();
                assert_eq!(list.memories.len(), limit);
                assert!(list.memories.iter().all(|row| {
                    row.revision == revisions as u64
                        && row.retiring_target == expected.retiring_target
                        && row.retiring_state == expected.retiring_state
                }));
                // Two memory objects plus the native target's latest WorkEvent
                // per candidate, including the one-row pagination lookahead.
                assert!(
                    decoded <= 3 * (limit + 1),
                    "decoded {decoded}, page {limit}"
                );
                measurements.push((revisions, limit, query, decoded));
            }
        }
    }
    eprintln!("local-target listing (revisions, page, query, decoded): {measurements:?}");
    for (short, long) in measurements[..6].iter().zip(&measurements[6..]) {
        assert_eq!(
            short.3, long.3,
            "target validation never replays memory history"
        );
    }
}

// The old listing's history-based envelope, independent of the new metadata.
fn history_row(store: &SqliteStore, key: &str) -> ProjectMemoryListRow {
    let full = store
        .project_memory_full(
            &ProjectId(PROJECT.into()),
            &SessionId(SESSION.into()),
            &actor(SESSION),
            key,
            None,
        )
        .unwrap();
    ProjectMemoryListRow {
        key: full.key,
        revision: full.revision,
        first_line: project_memory_first_line(&full.body),
        remembered_at: full.remembered_at,
        actor_id: full.actor_id,
        actor_context: full.actor_context,
        retiring_target: full.retiring_target,
        retiring_state: full.retiring_state,
        retiring_target_dropped: full.retiring_target_dropped,
        workaround: full.workaround,
    }
}

#[test]
fn memory_listing_matches_history_envelopes_and_pagination() {
    let mut store = fixture(3);
    let mut request = project_memory_request(
        PROJECT,
        SESSION,
        Some("list-00"),
        &format!("\n \t\nneedle {}\nother text", "ż".repeat(200)),
        1001,
    );
    request.revise = true;
    request.retiring_target = crate::domain::ProjectMemoryRetiringTargetChange::Set {
        target: crate::domain::ProjectMemoryRetiringTargetInput::External {
            project: "upstream".into(),
            reference: "fix".into(),
        },
    };
    store
        .remember_project_memory(&request, &DevelopmentNoopRedactor)
        .unwrap();
    // Model old/imported writers dropping a target without an explicit clear.
    for at in [1002, 1003] {
        retiring::insert_raw_version(&mut store, PROJECT, "list-00", "needle dropped", None, at);
    }
    store
        .forget_project_memory(
            &ForgetProjectMemoryRequest {
                project_id: ProjectId(PROJECT.into()),
                session_id: SessionId(SESSION.into()),
                key: "list-22".into(),
                actor: actor(SESSION),
                created_at: Utc.timestamp_millis_opt(1004).unwrap(),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();

    let expected = (0..22)
        .map(|key| history_row(&store, &format!("list-{key:02}")))
        .collect::<Vec<_>>();
    assert_eq!(
        expected[0]
            .retiring_target_dropped
            .as_ref()
            .unwrap()
            .revision,
        5
    );
    let mut collected = Vec::new();
    let mut after = None;
    loop {
        let result = page(&store, None, after.as_deref(), 5);
        assert_eq!(result.omitted_count, 0);
        assert_eq!(result.exhausted, result.next_after.is_none());
        collected.extend(result.memories);
        after = result.next_after;
        if after.is_none() {
            break;
        }
    }
    assert_eq!(
        serde_json::to_vec(&collected).unwrap(),
        serde_json::to_vec(&expected).unwrap()
    );
    let filtered = page(&store, Some("list-01"), None, 5);
    assert_eq!(filtered.memories, vec![history_row(&store, "list-01")]);
    assert!(filtered.exhausted);
    assert_eq!(filtered.omitted_count, 0);
    assert_eq!(filtered.next_after, None);

    request.retiring_target = crate::domain::ProjectMemoryRetiringTargetChange::Clear;
    request.created_at = Utc.timestamp_millis_opt(1005).unwrap();
    store
        .remember_project_memory(&request, &DevelopmentNoopRedactor)
        .unwrap();
    assert_eq!(
        page(&store, None, None, 1).memories,
        vec![history_row(&store, "list-00")]
    );
    assert!(
        page(&store, None, None, 1).memories[0]
            .first_line
            .ends_with('…')
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn memory_listing_refuses_missing_or_stale_metadata_without_history_fallback() {
    for damage in [
        "DELETE FROM project_memory_list_metadata WHERE key='list-00'",
        "UPDATE project_memory_list_metadata SET assertion_id='missing' WHERE key='list-00'",
        "UPDATE project_memory_list_metadata SET dropped_target_json='bad json' WHERE key='list-00'",
    ] {
        let store = fixture(8);
        let expected = page(&store, None, None, 1);
        store.connection.execute(damage, []).unwrap();
        crate::canonical::reset_canonical_decode_count();
        let refused = store
            .project_memories_at_cut(
                &ProjectId(PROJECT.into()),
                &SessionId(SESSION.into()),
                &actor(SESSION),
                None,
                None,
                false,
                1,
            )
            .unwrap_err();
        assert!(
            matches!(refused, StoreError::InvalidMemoryProjection(ref message)
            if message.contains("doctor --repair-projections"))
        );
        assert!(crate::canonical::canonical_decode_count() <= 4);
        assert!(!store.verify_all().unwrap().is_healthy());
        super::super::listing::rebuild(&store.connection).unwrap();
        assert_eq!(page(&store, None, None, 1), expected);
        assert!(store.verify_all().unwrap().is_healthy());
    }
}

#[test]
fn memory_listing_and_doctor_preserve_operational_metadata_read_errors() {
    use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

    let store = fixture(2);
    let expected = page(&store, None, None, 1);
    store
        .connection
        .authorizer(Some(|context: AuthContext<'_>| {
            if matches!(
                context.action,
                AuthAction::Read {
                    table_name: "project_memory_list_metadata",
                    ..
                }
            ) {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }))
        .unwrap();
    let list_error = store
        .project_memories_at_cut(
            &ProjectId(PROJECT.into()),
            &SessionId(SESSION.into()),
            &actor(SESSION),
            None,
            None,
            false,
            1,
        )
        .unwrap_err();
    let doctor_error = store.verify_all().unwrap_err();
    for error in [list_error, doctor_error] {
        assert!(matches!(&error, StoreError::Sqlite(sqlite)
            if sqlite.sqlite_error_code() == Some(rusqlite::ErrorCode::AuthorizationForStatementDenied)));
        assert_eq!(
            crate::storage::store_open_refusal_kind(&error),
            crate::storage::StoreOpenRefusalKind::Permission
        );
    }
    store
        .connection
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .unwrap();
    assert_eq!(page(&store, None, None, 1), expected);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn memory_listing_metadata_write_is_atomic_with_the_canonical_head() {
    let mut store = fixture(2);
    store
        .connection
        .execute_batch(
            "CREATE TRIGGER refuse_list_metadata BEFORE INSERT ON project_memory_list_metadata
         BEGIN SELECT RAISE(ABORT, 'injected metadata write failure'); END;",
        )
        .unwrap();
    let before = crate::storage::test_database_shape_snapshot(&store.connection);
    let mut request = project_memory_request(PROJECT, SESSION, Some("list-00"), "new body", 1001);
    request.revise = true;
    assert!(
        store
            .remember_project_memory(&request, &DevelopmentNoopRedactor)
            .is_err()
    );
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&store.connection),
        before
    );
    assert_eq!(page(&store, None, None, 1).memories[0].revision, 2);
}

#[test]
fn memory_listing_metadata_narrow_cutover_preserves_other_rows_and_tombstones() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("memory-list.db");
    let mut store = SqliteStore::open(&database).unwrap();
    for key in ["active", "retired"] {
        let mut request = project_memory_request(PROJECT, SESSION, Some(key), "first", 1000);
        store
            .remember_project_memory(&request, &DevelopmentNoopRedactor)
            .unwrap();
        request.revise = true;
        request.body = "second".into();
        store
            .remember_project_memory(&request, &DevelopmentNoopRedactor)
            .unwrap();
    }
    store
        .forget_project_memory(
            &ForgetProjectMemoryRequest {
                project_id: ProjectId(PROJECT.into()),
                session_id: SessionId(SESSION.into()),
                key: "retired".into(),
                actor: actor(SESSION),
                created_at: Utc.timestamp_millis_opt(1001).unwrap(),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let expected = page(&store, None, None, 20);
    store
        .connection
        .execute_batch("DROP TABLE project_memory_list_metadata")
        .unwrap();
    let before = crate::storage::test_database_shape_snapshot(&store.connection);
    drop(store);
    assert!(
        SqliteStore::open(&database).is_err(),
        "ordinary open never repairs"
    );
    assert_eq!(
        SqliteStore::build_project_memory_list_metadata(&database).unwrap(),
        2
    );
    assert!(SqliteStore::build_project_memory_list_metadata(&database).is_err());
    let store = SqliteStore::open(&database).unwrap();
    assert_eq!(page(&store, None, None, 20), expected);
    assert!(matches!(
        store.project_memory_full(
            &ProjectId(PROJECT.into()),
            &SessionId(SESSION.into()),
            &actor(SESSION),
            "retired",
            None,
        ),
        Err(StoreError::ProjectMemoryRetired(_))
    ));
    store
        .connection
        .execute_batch("DROP TABLE project_memory_list_metadata")
        .unwrap();
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&store.connection),
        before
    );
    drop(store);
    assert!(
        SqliteStore::repair_rebuildable_projections(&database)
            .unwrap()
            .is_healthy()
    );
    let mut store = SqliteStore::open(&database).unwrap();
    store.rebuild_memory_index().unwrap();
    assert_eq!(page(&store, None, None, 20), expected);
    assert!(store.verify_all().unwrap().is_healthy());
    let retired_revision: i64 = store
        .connection
        .query_row(
            "SELECT revision FROM project_memory_list_metadata WHERE key='retired'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        retired_revision, 2,
        "forget adds an assertion, not a revision"
    );
}

#[test]
fn memory_listing_metadata_narrow_cutover_rolls_back_on_bad_heads_or_other_schema_damage() {
    for bad_head in [false, true] {
        let directory = crate::test_support::temp_home().unwrap();
        let database = directory.path().join("memory-list.db");
        let mut store = SqliteStore::open(&database).unwrap();
        store
            .remember_project_memory(
                &project_memory_request(PROJECT, SESSION, Some("rule"), "body", 1000),
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        store
            .connection
            .execute_batch("DROP TABLE project_memory_list_metadata")
            .unwrap();
        if bad_head {
            store
                .connection
                .execute("DELETE FROM memory_heads", [])
                .unwrap();
        } else {
            store
                .connection
                .execute_batch("DROP INDEX memory_heads_scope")
                .unwrap();
        }
        let before = crate::storage::test_database_shape_snapshot(&store.connection);
        drop(store);
        assert!(SqliteStore::build_project_memory_list_metadata(&database).is_err());
        let connection = rusqlite::Connection::open(&database).unwrap();
        assert_eq!(
            crate::storage::test_database_shape_snapshot(&connection),
            before
        );
    }
}
