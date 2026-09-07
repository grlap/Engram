use super::*;

fn read_modes(locator: &str) -> Vec<crate::verbs::ShowInput> {
    vec![
        crate::verbs::ShowInput::default(),
        crate::verbs::ShowInput {
            notes: true,
            ..Default::default()
        },
        crate::verbs::ShowInput {
            notes: true,
            gates: true,
            ..Default::default()
        },
        crate::verbs::ShowInput {
            history: true,
            ..Default::default()
        },
        crate::verbs::ShowInput {
            note: Some(locator.into()),
            ..Default::default()
        },
    ]
}

#[test]
fn read_contention_explicit_reads_preserve_focus_and_staged_delivery_under_writer() {
    let (_directory, reader, path, project) = fixture();
    let held = add(&reader, "Held item", None, false, 0);
    reader
        .claim(
            ClaimInput {
                work_ref: held.clone(),
                ttl_seconds: None,
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
    let target = add(&peer, "Requested committed item", None, false, 2);
    note(&peer, &target, "Committed note", 3);
    let locator = peer.show_with_notes(&target, true, at(4)).unwrap().value["notes"][0]["locator"]
        .as_str()
        .unwrap()
        .to_owned();
    reader
        .service
        .work_next(
            20,
            crate::work_service::WorkNextQuery {
                sections: vec![crate::work_service::WorkNextSection::Changes],
                ..Default::default()
            },
            at(5),
        )
        .unwrap();
    let inspect = rusqlite::Connection::open(&path).unwrap();
    let store = SqliteStore::open(&path).unwrap();
    let before_session = store
        .work_session_state(&project, &SessionId("agent".into()), at(6))
        .unwrap();
    assert!(before_session.tentative_project_cursor.is_some());
    let held_id = store.resolve_work_ref(&project, &held).unwrap().work_id;
    let held_claim = store.current_work_claim(held_id).unwrap();
    assert!(held_claim.is_some());
    let before = crate::storage::test_database_shape_snapshot(&inspect).unwrap();
    let mut writer = rusqlite::Connection::open(&path).unwrap();
    let transaction = writer
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    assert_eq!(transaction.execute("UPDATE work_session_state SET updated_at_ms = updated_at_ms + 1 WHERE session_id = 'peer'", []).unwrap(), 1);
    assert_writer_outlives_busy_timeout(&inspect);
    for input in read_modes(&locator) {
        let receipt = reader.show_records(&target, &input, at(6)).unwrap();
        let reference = if input.note.is_some() {
            &receipt.value["work_ref"]
        } else {
            &receipt.value["status"]["work"]["short_ref"]
        };
        assert_eq!(reference, &json!(target));
        if input.note.is_some() {
            assert_eq!(receipt.value["note"]["summary"], "Committed note");
        } else {
            assert_eq!(
                receipt.value["status"]["work"]["title"],
                "Requested committed item"
            );
        }
        assert!(!receipt.next.is_empty());
        assert!(receipt.next.iter().all(|command| command.contains(&target)));
        assert!(!writer_is_unlocked(&inspect));
        assert_eq!(
            crate::storage::test_database_shape_snapshot(&inspect).unwrap(),
            before
        );
    }
    transaction.rollback().unwrap();
    assert_eq!(
        store
            .work_session_state(&project, &SessionId("agent".into()), at(6))
            .unwrap(),
        before_session
    );
    let written = reader
        .note(
            &NoteInput {
                work_ref: Some(target.clone()),
                text: "Explicit observation after read".into(),
                status: false,
                refs: vec![],
            },
            at(7),
        )
        .unwrap();
    assert_eq!(written.value["work"]["short_ref"], target);
    assert!(written.text().contains("observation, no run credit"));
    assert_eq!(store.current_work_claim(held_id).unwrap(), held_claim);
    assert_eq!(
        store.resolve_work_ref(&project, &held).unwrap().lifecycle,
        WorkLifecycle::Open
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

fn assert_writer_outlives_busy_timeout(connection: &rusqlite::Connection) {
    // A real competing writer times out while the owner's transaction stays
    // open. Read the connection's normal busy budget; do not fake lock errors
    // or replace the held writer with a sleep.
    let timeout: u32 = connection
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .unwrap();
    assert!(timeout > 0);
    let started = std::time::Instant::now();
    let busy = connection.execute_batch("BEGIN IMMEDIATE").unwrap_err();
    assert_eq!(
        busy.sqlite_error_code(),
        Some(rusqlite::ErrorCode::DatabaseBusy)
    );
    assert!(started.elapsed() >= std::time::Duration::from_millis(u64::from(timeout)));
}

fn writer_is_unlocked(connection: &rusqlite::Connection) -> bool {
    connection.busy_timeout(std::time::Duration::ZERO).unwrap();
    match connection.execute_batch("BEGIN IMMEDIATE") {
        Ok(()) => {
            connection.execute_batch("ROLLBACK").unwrap();
            true
        }
        Err(error) => {
            assert_eq!(
                error.sqlite_error_code(),
                Some(rusqlite::ErrorCode::DatabaseBusy)
            );
            false
        }
    }
}

#[test]
fn read_contention_fresh_process_read_defers_session_registration() {
    let (_directory, owner, path, project) = fixture();
    let target = add(&owner, "Fresh reader target", None, false, 0);
    note(&owner, &target, "Committed detail", 1);
    owner
        .service
        .remember_project_memory(
            "Read-only project memory".into(),
            Some("read-only-memory".into()),
            at(1),
        )
        .unwrap();
    let locator = owner.show_with_notes(&target, true, at(2)).unwrap().value["notes"][0]["locator"]
        .as_str()
        .unwrap()
        .to_owned();
    let inspect = rusqlite::Connection::open(&path).unwrap();
    for input in read_modes(&locator) {
        let before = crate::storage::test_database_shape_snapshot(&inspect).unwrap();
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
            project.clone(),
            "fresh".into(),
            session.clone(),
            None,
            None,
            crate::work_service::WorkAttributionDefaults {
                actor: None,
                session: true,
            },
        );
        let mut writer = rusqlite::Connection::open(&path).unwrap();
        let transaction = writer
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
            .unwrap();
        let receipt = reader.show_records(&target, &input, at(3)).unwrap();
        assert!(receipt.next.iter().any(|command| command.contains(&target)));
        assert_eq!(
            reader.ls(&LsInput::default(), at(3)).unwrap().value["total"],
            1
        );
        assert_eq!(
            reader.search("Fresh reader", None, at(3)).unwrap().value["total"],
            1
        );
        assert_eq!(
            reader
                .service
                .project_memories(None, None, at(3))
                .unwrap()
                .memories
                .len(),
            1
        );
        reader
            .service
            .project_memory_full("read-only-memory", at(3))
            .unwrap();
        assert_eq!(
            crate::storage::test_database_shape_snapshot(&inspect).unwrap(),
            before
        );
        transaction.rollback().unwrap();
        // The same process can later write with normal attributed admission;
        // the read has not marked its deferred initialization as completed.
        let receipt = reader
            .note(
                &NoteInput {
                    work_ref: Some(target.clone()),
                    text: "First actual write".into(),
                    status: false,
                    refs: vec![],
                },
                at(4),
            )
            .unwrap();
        assert_eq!(receipt.value["work"]["short_ref"], target);
        let registered: i64 = inspect
            .query_row(
                "SELECT COUNT(*) FROM work_session_state WHERE session_id = ?1",
                [&session.0],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(registered, 1);
    }
}

#[test]
fn read_contention_reading_another_item_does_not_steer_a_bare_note() {
    let (_directory, verbs, path, project) = fixture();
    let other = add(&verbs, "Read only", None, false, 0);
    note(&verbs, &other, "Read me", 1);
    let locator = verbs.show_with_notes(&other, true, at(2)).unwrap().value["notes"][0]["locator"]
        .as_str()
        .unwrap()
        .to_owned();
    let held = add(&verbs, "Keep execution here", None, false, 3);
    verbs
        .claim(
            ClaimInput {
                work_ref: held.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(4),
        )
        .unwrap();
    for (index, input) in read_modes(&locator).into_iter().enumerate() {
        let now = 5 + i64::try_from(index).unwrap();
        verbs.show_records(&other, &input, at(now)).unwrap();
        let receipt = verbs
            .note(
                &NoteInput {
                    work_ref: None,
                    text: format!("Still working here {index}"),
                    status: false,
                    refs: vec![],
                },
                at(now),
            )
            .unwrap();
        assert_eq!(receipt.value["work"]["short_ref"], held);
        assert!(!receipt.text().contains("observation, no run credit"));
    }
    let store = SqliteStore::open(&path).unwrap();
    assert_eq!(
        store
            .work_session_state(&project, &SessionId("agent".into()), at(10))
            .unwrap()
            .focused_work_id,
        Some(store.resolve_work_ref(&project, &held).unwrap().work_id)
    );
    assert!(store.verify_all().unwrap().is_healthy());
}
