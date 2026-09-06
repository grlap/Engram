use super::*;
use crate::storage::{WorkRecordContent, WorkRecordKind};

#[test]
fn record_windows_live_member_guards_refuse_wrong_bindings_and_missing_feed_positions() {
    let (directory, verbs, path, project) = fixture();
    let work = add(&verbs, "Guarded record membership", None, false, 0);
    note(&verbs, &work, "Guarded native note", 1);
    let store = SqliteStore::open(&path).unwrap();
    let id = store.resolve_work_ref(&project, &work).unwrap().work_id;
    let other_id = crate::WorkId::new();
    let other_project = ProjectId("not-the-record-project".into());
    for kind in [WorkRecordKind::Notes, WorkRecordKind::History] {
        assert!(matches!(store.work_record_index(&other_project, id, kind),
            Err(StoreError::InvalidWorkProjection(reason)) if reason == "record window cannot cross projects"));
        let index = store.work_record_index(&project, id, kind).unwrap();
        assert!(!index.is_empty());
        let expected = if kind == WorkRecordKind::Notes {
            "note differs from its work binding"
        } else {
            "history event belongs to another work item"
        };
        assert!(
            matches!(store.work_record_content(&project, other_id, &index[0]),
            Err(StoreError::InvalidWorkProjection(reason)) if reason == expected)
        );
        if kind == WorkRecordKind::History {
            assert!(
                matches!(store.work_record_content(&other_project, id, &index[0]),
                Err(StoreError::InvalidWorkProjection(reason)) if reason == expected)
            );
        }
    }
    let index = store
        .work_record_index(&project, id, WorkRecordKind::Notes)
        .unwrap();
    let hash = index[0].address.hash.as_str();
    let connection = rusqlite::Connection::open(&path).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&connection);
    connection.execute("CREATE TEMP TABLE removed_record_position AS SELECT * FROM work_feed_entries WHERE feed_kind = 'project' AND object_hash = ?1", [hash]).unwrap();
    assert_eq!(
        connection
            .execute(
                "DELETE FROM work_feed_entries WHERE feed_kind = 'project' AND object_hash = ?1",
                [hash]
            )
            .unwrap(),
        1
    );
    assert!(
        matches!(store.work_record_index(&project, id, WorkRecordKind::Notes),
        Err(StoreError::InvalidWorkProjection(reason)) if reason == "record is missing its project-feed position")
    );
    connection
        .execute(
            "INSERT INTO work_feed_entries SELECT * FROM removed_record_position",
            [],
        )
        .unwrap();
    connection
        .execute("DROP TABLE removed_record_position", [])
        .unwrap();
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&connection),
        before
    );
    assert!(store.verify_all().unwrap().is_healthy());

    let (_restored, store, _) = super::super::review::load(
        directory.path(),
        &super::super::review::snapshot(&path, &project, &work),
    );
    let index = store
        .work_record_index(&project, id, WorkRecordKind::Notes)
        .unwrap();
    assert_eq!(index.len(), 1);
    assert!(index[0].address.member.is_some());
    for (project, id) in [(&project, other_id), (&other_project, id)] {
        assert!(matches!(store.work_record_content(project, id, &index[0]),
            Err(StoreError::InvalidWorkProjection(reason)) if reason == "inherited member belongs to another work item"));
    }
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn record_windows_keep_relative_actor_labels_in_all_modes() {
    let (_directory, verbs, path, project) = fixture();
    let work = add(&verbs, "Relative record authors", None, false, 0);
    let authors = ["agent", "private-peer-principal"];
    let readers = authors.map(|actor| {
        AgentVerbs::new_with_attribution(
            path.clone(),
            project.clone(),
            actor.into(),
            SessionId(actor.into()),
            None,
            Some(format!("host-context-{}", usize::from(actor != "agent"))),
            crate::WorkAttributionDefaults::default(),
        )
    });
    for (index, writer) in readers.iter().enumerate() {
        note(
            writer,
            &work,
            &format!("Authored note {index}"),
            i64::try_from(index).unwrap() * 2 + 1,
        );
        writer
            .update(
                UpdateInput {
                    work_ref: Some(work.clone()),
                    action: serde_json::from_value(
                        json!({"action":"revise", "title":format!("Revision {index}")}),
                    )
                    .unwrap(),
                },
                at(i64::try_from(index).unwrap() * 2 + 2),
            )
            .unwrap();
    }
    let notes = window(&readers[0], &work, false, None, 5);
    let history = window(&readers[0], &work, true, None, 5);
    for receipt in [&notes, &history] {
        let rows = if receipt.value.get("notes_window").is_some() {
            receipt.value["notes"].as_array().unwrap()
        } else {
            receipt.value["history"]["items"].as_array().unwrap()
        };
        assert!(rows.iter().any(|row| row["by"] == "you (host-context-0)"));
        assert!(
            rows.iter()
                .any(|row| row["by"] == "another actor (host-context-1)")
        );
        assert!(!receipt.text().contains(authors[1]));
        assert!(
            !serde_json::to_string(&receipt.value)
                .unwrap()
                .contains(authors[1])
        );
    }
    for (index, row) in notes.value["notes"].as_array().unwrap().iter().enumerate() {
        if index == 0 {
            assert_eq!(row["actor_session_id"], authors[0]);
        } else {
            assert!(row.get("actor_session_id").is_none());
        }
        let detail = readers[0]
            .show_records(
                &work,
                &ShowInput {
                    note: Some(row["locator"].as_str().unwrap().into()),
                    ..ShowInput::default()
                },
                at(5),
            )
            .unwrap();
        assert_eq!(detail.value["note"]["by"], row["by"]);
        assert_eq!(
            detail.value["note"].get("actor_session_id"),
            row.get("actor_session_id")
        );
        assert_eq!(
            detail.value["note"]["summary"],
            format!("Authored note {index}")
        );
        assert!(!detail.text().contains(authors[1]));
        assert!(
            !serde_json::to_string(&detail.value)
                .unwrap()
                .contains(authors[1])
        );
    }
}

#[test]
fn record_windows_history_note_summaries_name_complete_detail() {
    let (directory, verbs, path, project) = fixture();
    let work = add(&verbs, "Inherited history size", None, false, 0);
    let body = format!("{} END", "ü history ".repeat(800));
    note(&verbs, &work, &body, 1);
    let (restored, _store, _) = super::super::review::load(
        directory.path(),
        &super::super::review::snapshot(&path, &project, &work),
    );
    let history = window(&restored, &work, true, None, 102);
    let row = history.value["history"]["items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["locator"].as_str().unwrap().ends_with(":1"))
        .unwrap();
    assert_eq!(row["body_bytes"], body.len());
    assert_eq!(row["summary_truncated"], true);
    assert!(row["summary"].as_str().unwrap().len() < body.len());
    assert_eq!(
        row["detail"],
        format!(
            "engram work show {work} --note {}",
            row["locator"].as_str().unwrap()
        )
    );
    assert!(history.text().contains("summary shortened"));
    assert!(history.text().contains(row["detail"].as_str().unwrap()));
    let detail = restored
        .show_records(
            &work,
            &ShowInput {
                note: Some(row["locator"].as_str().unwrap().into()),
                ..ShowInput::default()
            },
            at(102),
        )
        .unwrap();
    assert_eq!(detail.value["note"]["summary"], body);
    assert_eq!(detail.value["note"]["body_bytes"], body.len());
}

#[test]
fn record_windows_decode_each_restored_generation_once() {
    let (directory, verbs, path, project) = fixture();
    let work = add(&verbs, "Shared inherited decoding", None, false, 0);
    for index in 1..=8 {
        note(
            &verbs,
            &work,
            &format!("Member {index}: {}", "body ".repeat(5000)),
            index,
        );
    }
    let (_restored, store, _) = super::super::review::load(
        directory.path(),
        &super::super::review::snapshot(&path, &project, &work),
    );
    let id = store.resolve_work_ref(&project, &work).unwrap().work_id;
    store
        .work_read_snapshot(|store| {
            let index = store.work_record_index(&project, id, WorkRecordKind::Notes)?;
            assert_eq!(index.len(), 8);
            crate::canonical::reset_canonical_decode_count();
            for entry in &index {
                let WorkRecordContent::Note(note) =
                    store.work_record_content(&project, id, entry)?
                else {
                    panic!("note member")
                };
                assert!(note.summary.len() > 20_000);
            }
            assert_eq!(
                crate::canonical::canonical_decode_count(),
                0,
                "verified index members must not re-decode the restored record"
            );
            Ok(())
        })
        .unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
}
