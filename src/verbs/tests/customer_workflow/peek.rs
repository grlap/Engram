use super::*;

fn peek_input(verbose: bool) -> NextInput {
    NextInput {
        peek: true,
        verbose,
        ..NextInput::default()
    }
}

#[test]
fn peek_residual_wal_writer_process() {
    let Some(path) = std::env::var_os("ENGRAM_PEEK_WAL_FIXTURE") else {
        return;
    };
    let path = std::path::PathBuf::from(path);
    let _store = SqliteStore::open(&path).unwrap();
    let writer = AgentVerbs::new(
        path,
        ProjectId("peek-residual-wal".into()),
        "writer".into(),
        SessionId("writer".into()),
        None,
    );
    add(&writer, "Committed only in residual WAL", None, false, 0);
    // Exit without connection destructors: no last-close checkpoint/cleanup.
    // This helper runs only in the exact filtered child owned by the test below.
    std::process::exit(0);
}

#[test]
fn peek_reads_residual_wal_after_all_connections_exit_with_or_without_shm() {
    let home = crate::test_support::temp_home().unwrap();
    for remove_shm in [false, true] {
        let path = home.path().join(format!("residual-{remove_shm}.db"));
        let output = std::process::Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "verbs::tests::customer_workflow::peek::peek_residual_wal_writer_process",
            ])
            .env("ENGRAM_PEEK_WAL_FIXTURE", &path)
            .output()
            .unwrap();
        assert!(output.status.success(), "{output:?}");
        let wal = std::path::PathBuf::from(format!("{}-wal", path.display()));
        let shm = std::path::PathBuf::from(format!("{}-shm", path.display()));
        let database_before = std::fs::read(&path).unwrap();
        let wal_before = std::fs::read(&wal).unwrap();
        let marker = b"Committed only in residual WAL";
        assert!(
            !database_before
                .windows(marker.len())
                .any(|bytes| bytes == marker)
        );
        assert!(
            wal_before
                .windows(marker.len())
                .any(|bytes| bytes == marker)
        );
        assert!(wal_before.len() > 32, "must retain uncheckpointed frames");
        assert!(shm.exists());
        if remove_shm {
            // Exactly this child-created fixture sidecar, never a live store.
            std::fs::remove_file(&shm).unwrap();
        }
        let reader = AgentVerbs::new(
            path.clone(),
            ProjectId("peek-residual-wal".into()),
            "reader".into(),
            SessionId("reader".into()),
            None,
        );
        let receipt = reader.next(&peek_input(false), at(1)).unwrap();
        assert!(receipt.text().contains("Committed only in residual WAL"));
        assert_peek(&receipt, true);
        assert_eq!(std::fs::read(&path).unwrap(), database_before);
        assert_eq!(std::fs::read(&wal).unwrap(), wal_before);
    }
}

fn assert_peek(receipt: &Receipt, changed: bool) {
    assert_eq!(receipt.value["peek"]["delivery_advanced"], false);
    assert_eq!(receipt.value["memories"]["changed"], changed);
    assert_eq!(receipt.value["memories_detail"], "engram work memories");
    assert!(
        receipt
            .next
            .iter()
            .any(|command| command == "engram work memories")
    );
    assert!(receipt.text().contains("delivery: not advanced"));
    assert!(
        receipt
            .text()
            .contains("not whether notes were read or applied")
    );
    assert!(receipt.value.get("delivered_through").is_none());
    assert!(receipt.value.get("delivery_token").is_none());
}

#[test]
fn peek_bounds_both_renderers_without_shedding_disclosures() {
    let (_home, reader, path, project) = fixture();
    let peer = AgentVerbs::new(
        path.clone(),
        project,
        "peer".into(),
        SessionId("peer".into()),
        None,
    );
    for index in 0..40 {
        add(
            &peer,
            &format!("Ready {index} {}", "large planning title ".repeat(20)),
            None,
            false,
            index,
        );
    }
    let inspect = rusqlite::Connection::open(&path).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&inspect).unwrap();
    for verbose in [false, true] {
        let receipt = reader
            .next(
                &NextInput {
                    limit: Some(100),
                    ..peek_input(verbose)
                },
                at(50),
            )
            .unwrap();
        assert_peek(&receipt, true);
        assert!(receipt.text().len() < MAX_AGENT_WORK_RESPONSE_BYTES);
        assert!(
            serde_json::to_vec_pretty(&receipt.value).unwrap().len()
                < MAX_AGENT_WORK_RESPONSE_BYTES
        );
        assert_eq!(
            crate::storage::test_database_shape_snapshot(&inspect).unwrap(),
            before
        );
    }
}

#[test]
fn peek_rich_focus_remains_bounded_including_verbose_metadata() {
    let (_home, reader, service, root) = super::budgets::rich_focus(8);
    service.select_work(&root, at(100)).unwrap();
    for verbose in [false, true] {
        let receipt = reader.next(&peek_input(verbose), at(101)).unwrap();
        assert_peek(&receipt, true);
        assert!(receipt.text().len() < MAX_AGENT_WORK_RESPONSE_BYTES);
        assert!(
            serde_json::to_vec_pretty(&receipt.value).unwrap().len()
                < MAX_AGENT_WORK_RESPONSE_BYTES
        );
    }
}

#[test]
fn peek_local_scan_is_bounded_and_repeats_without_false_backlog_counts() {
    let (_home, reader, path, project) = fixture();
    for index in 0..12 {
        add(&reader, &format!("Own entry {index}"), None, false, index);
    }
    let peer = AgentVerbs::new(
        path.clone(),
        project,
        "peer".into(),
        SessionId("peer".into()),
        None,
    );
    add(&peer, "Peer beyond local scan", None, false, 20);
    let inspect = rusqlite::Connection::open(&path).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&inspect).unwrap();
    for _ in 0..2 {
        let view = reader
            .service
            .work_next_peek_for_agent(
                1,
                1,
                false,
                WorkNextQuery {
                    sections: vec![WorkNextSection::Changes],
                    ..Default::default()
                },
                at(21),
                |changes| {
                    !crate::verbs::collapsed_changes(changes, reader.service.display_identity())
                        .is_empty()
                },
            )
            .unwrap();
        assert!(view.peek.unwrap().more_changes_available);
        assert_eq!(
            view.changes
                .unwrap()
                .last()
                .unwrap()
                .entry
                .position
                .position,
            8
        );
        assert_eq!(
            crate::storage::test_database_shape_snapshot(&inspect).unwrap(),
            before
        );
    }
}

#[test]
fn peek_respects_changes_and_memories_section_selection() {
    let (_home, reader, path, _) = fixture();
    add(&reader, "Selection fixture", None, false, 0);
    reader
        .service
        .remember_project_memory(
            "Selected memory".into(),
            Some("selection".into()),
            false,
            None,
            at(1),
        )
        .unwrap();
    let inspect = rusqlite::Connection::open(&path).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&inspect).unwrap();
    for sections in [
        vec![WorkNextSection::Focus],
        vec![WorkNextSection::Changes],
        vec![WorkNextSection::Memories],
        vec![],
    ] {
        let changes_selected = sections.is_empty() || sections.contains(&WorkNextSection::Changes);
        let memories_selected =
            sections.is_empty() || sections.contains(&WorkNextSection::Memories);
        let scans = std::cell::Cell::new(0);
        let view = reader
            .service
            .work_next_peek_for_agent(
                20,
                20,
                false,
                WorkNextQuery {
                    sections,
                    ..Default::default()
                },
                at(2),
                |_| {
                    scans.set(scans.get() + 1);
                    true
                },
            )
            .unwrap();
        assert_eq!(view.changes.is_some(), changes_selected);
        assert_eq!(view.memories.is_some(), memories_selected);
        if !changes_selected {
            assert_eq!(scans.get(), 0);
            assert!(!view.peek.unwrap().more_changes_available);
        }
        assert_eq!(
            crate::storage::test_database_shape_snapshot(&inspect).unwrap(),
            before
        );
    }
}

#[test]
fn peek_preserves_whole_store_and_pending_delivery_under_writer() {
    let (_home, reader, path, project) = fixture();
    let held = add(&reader, "Held orientation", None, false, 0);
    reader
        .claim(
            ClaimInput {
                work_ref: held,
                ttl_seconds: Some(3600),
                recover: None,
            },
            at(1),
        )
        .unwrap();
    let peer = AgentVerbs::new(
        path.clone(),
        project.clone(),
        "peer".into(),
        SessionId("peer".into()),
        None,
    );
    let ready = add(&peer, "Ready peer change", None, false, 2);
    peer.service
        .remember_project_memory(
            "Check the current rule".into(),
            Some("orientation".into()),
            false,
            None,
            at(3),
        )
        .unwrap();
    // A real pending page must survive looking, including exact payload/token.
    reader
        .service
        .work_next(
            20,
            crate::WorkNextQuery {
                sections: vec![crate::WorkNextSection::Changes],
                ..Default::default()
            },
            at(4),
        )
        .unwrap();
    let inspect = rusqlite::Connection::open(&path).unwrap();
    let store = SqliteStore::open(&path).unwrap();
    let session = store
        .work_session_state(&project, &SessionId("agent".into()), at(5))
        .unwrap();
    assert!(session.tentative_project_cursor.is_some());
    let before = crate::storage::test_database_shape_snapshot(&inspect).unwrap();
    let mut writer = rusqlite::Connection::open(&path).unwrap();
    let tx = writer
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    for verbose in [false, true, false] {
        let receipt = reader.next(&peek_input(verbose), at(5)).unwrap();
        assert_peek(&receipt, true);
        assert!(receipt.text().contains(&ready));
        assert!(receipt.text().contains("Ready peer change"));
        assert_eq!(
            crate::storage::test_database_shape_snapshot(&inspect).unwrap(),
            before
        );
        assert_eq!(
            store
                .work_session_state(&project, &SessionId("agent".into()), at(5))
                .unwrap(),
            session
        );
        assert!(!super::read_contention::writer_is_unlocked(&inspect));
    }
    tx.rollback().unwrap();
    reader
        .service
        .project_memory_full("orientation", None, at(6))
        .unwrap();
    assert_peek(&reader.next(&peek_input(false), at(6)).unwrap(), true);
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&inspect).unwrap(),
        before
    );
    reader.next(&NextInput::default(), at(7)).unwrap();
    assert_peek(&reader.next(&peek_input(false), at(8)).unwrap(), false);
    let after = store
        .work_session_state(&project, &SessionId("agent".into()), at(8))
        .unwrap();
    assert_eq!(
        after.project_cursor,
        session.tentative_project_cursor.unwrap()
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn peek_fresh_process_does_not_register_and_later_next_still_does() {
    let (_home, owner, path, project) = fixture();
    add(&owner, "Ready for a fresh reader", None, false, 0);
    let timestamp = uuid::Timestamp::from_unix(
        uuid::NoContext,
        u64::try_from(at(0).timestamp()).unwrap(),
        0,
    );
    let session = SessionId(format!(
        "local-process-v1-42-{}",
        uuid::Uuid::new_v7(timestamp)
    ));
    let reader = AgentVerbs::new_with_attribution(
        path.clone(),
        project,
        "fresh".into(),
        session.clone(),
        None,
        None,
        crate::WorkAttributionDefaults {
            actor: None,
            session: true,
        },
    );
    let inspect = rusqlite::Connection::open(&path).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&inspect).unwrap();
    let mut writer = rusqlite::Connection::open(&path).unwrap();
    let tx = writer
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    for verbose in [false, true] {
        assert_peek(&reader.next(&peek_input(verbose), at(1)).unwrap(), true);
    }
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&inspect).unwrap(),
        before
    );
    tx.rollback().unwrap();
    reader.next(&NextInput::default(), at(2)).unwrap();
    let registered: i64 = inspect
        .query_row(
            "SELECT COUNT(*) FROM work_session_state WHERE session_id = ?1",
            [&session.0],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(registered, 1);
}

#[test]
fn peek_never_creates_or_initializes_a_missing_or_empty_store() {
    let (_home, reader, path, _) = fixture();
    assert!(!path.exists());
    assert!(matches!(
        reader.next(&peek_input(false), at(0)).unwrap_err().error,
        StoreError::StoreNotInitialized
    ));
    assert!(!path.exists());
    let empty = rusqlite::Connection::open(&path).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&empty).unwrap();
    assert!(matches!(
        reader.next(&peek_input(false), at(0)).unwrap_err().error,
        StoreError::StoreNotInitialized
    ));
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&empty).unwrap(),
        before
    );
    let mode: String = empty
        .query_row("PRAGMA journal_mode", [], |row| row.get(0))
        .unwrap();
    assert_eq!(mode, "delete");
}

#[test]
fn peek_refuses_damaged_schema_without_repair_and_validates_before_open() {
    let (_home, reader, path, _) = fixture();
    let invalid = NextInput {
        context_generation: Some("bad\ngeneration".into()),
        ..peek_input(false)
    };
    assert!(matches!(
        reader.next(&invalid, at(0)),
        Err(VerbError {
            error: StoreError::InvalidProjectMemory(_),
            ..
        })
    ));
    assert!(!path.exists());
    add(&reader, "Established store", None, false, 1);
    let inspect = rusqlite::Connection::open(&path).unwrap();
    inspect
        .execute_batch("DROP INDEX objects_memory_assertion_version")
        .unwrap();
    let before = crate::storage::test_database_shape_snapshot(&inspect).unwrap();
    assert!(matches!(
        reader.next(&peek_input(false), at(2)),
        Err(VerbError {
            error: StoreError::InvalidControlProjection(_),
            ..
        })
    ));
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&inspect).unwrap(),
        before
    );
    let index_count: i64 = inspect
        .query_row(
            "SELECT COUNT(*) FROM sqlite_master WHERE name = 'objects_memory_assertion_version'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(index_count, 0);
}

#[test]
fn peek_memory_revision_retirement_and_generation_repeat_until_advertised() {
    let (_home, reader, path, _) = fixture();
    reader
        .service
        .remember_project_memory("First rule".into(), Some("rule".into()), false, None, at(0))
        .unwrap();
    reader.next(&NextInput::default(), at(1)).unwrap();
    assert_peek(&reader.next(&peek_input(false), at(2)).unwrap(), false);
    reader
        .service
        .remember_project_memory(
            "Revised rule".into(),
            Some("rule".into()),
            true,
            Some(1),
            at(3),
        )
        .unwrap();
    let inspect = rusqlite::Connection::open(&path).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&inspect).unwrap();
    for time in [4, 5] {
        assert_peek(&reader.next(&peek_input(false), at(time)).unwrap(), true);
    }
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&inspect).unwrap(),
        before
    );
    reader.next(&NextInput::default(), at(6)).unwrap();
    reader
        .service
        .forget_project_memory("rule".into(), at(7))
        .unwrap();
    let retired = reader.next(&peek_input(false), at(8)).unwrap();
    assert_peek(&retired, true);
    assert_eq!(retired.value["memories"]["count"], 0);
    reader.next(&NextInput::default(), at(9)).unwrap();
    let input = NextInput {
        context_generation: Some("compacted".into()),
        ..peek_input(false)
    };
    let before = crate::storage::test_database_shape_snapshot(&inspect).unwrap();
    for time in [10, 11] {
        assert_peek(&reader.next(&input, at(time)).unwrap(), true);
    }
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&inspect).unwrap(),
        before
    );
    reader
        .next(
            &NextInput {
                peek: false,
                ..input.clone()
            },
            at(12),
        )
        .unwrap();
    assert_peek(&reader.next(&input, at(13)).unwrap(), false);
}
