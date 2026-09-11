use super::*;
use crate::work_service::{
    LocalWorkService, StagedWorkChangePage, WorkNextQuery, WorkNextSection, WorkProposeInput,
};

// A real current runtime page with a deliberately omitted own-session bit.
// This tests the audited replay boundary, not aggregate-root conversion.
fn fixture(
    explicit: Option<bool>,
    own: bool,
) -> (
    crate::test_support::TempHome,
    LocalWorkService,
    SqliteStore,
    CanonicalObject,
    ExportManifest,
    String,
) {
    let dir = crate::test_support::temp_home().expect("fixture");
    let path = dir.path().join("work.db");
    let service = LocalWorkService::new(
        path.clone(),
        ProjectId("p".into()),
        "author".into(),
        SessionId("s".into()),
        None,
    );
    let now = chrono::Utc::now();
    let creator = LocalWorkService::new(
        path.clone(),
        ProjectId("p".into()),
        "author".into(),
        SessionId(if own { "s" } else { "peer" }.into()),
        None,
    );
    creator
        .work_propose(
            WorkProposeInput::Root {
                title: "Attribution".into(),
                outcome: "Keep the original page".into(),
                acceptance: vec!["replay".into()],
                external_ref: None,
                notes: vec![],
                work_kind: None,
                priority: None,
                labels: vec![],
                assigned_to: None,
                deferred_until: None,
                idempotency_key: "root".into(),
            },
            now,
        )
        .expect("real work");
    service
        .work_next_with_delivery_token(
            20,
            None,
            None,
            WorkNextQuery {
                sections: vec![WorkNextSection::Changes],
                ..WorkNextQuery::default()
            },
            now,
        )
        .expect("stage");
    let store = SqliteStore::open(&path).expect("store");
    let payload = store
        .staged_work_session_delivery_payload(&ProjectId("p".into()), &SessionId("s".into()))
        .expect("payload")
        .expect("pending");
    let mut raw: serde_json::Value = payload.decode().expect("raw");
    assert_eq!(
        raw["changes"][0]["from_current_session"]
            .as_bool()
            .unwrap_or(false),
        own
    );
    match explicit {
        Some(value) => {
            raw["changes"][0]["from_current_session"] = value.into();
        }
        None => {
            raw["changes"][0]
                .as_object_mut()
                .expect("change")
                .remove("from_current_session");
        }
    }
    let payload = CanonicalObject::freeze(&raw).expect("historical shape");
    store.connection.execute("UPDATE work_session_state SET tentative_delivery_payload=?1,tentative_delivery_payload_hash=?2 WHERE session_id='s'",params![payload.bytes(),payload.hash().as_str()]).expect("fixture injection");
    let archive_path = dir.path().join("export.db");
    let manifest = super::super::export_store(&path, &archive_path).expect("raw export");
    let document = CanonicalObject::freeze(&manifest).expect("manifest");
    let source = document.hash().as_str().to_owned();
    store
        .connection
        .execute(
            "INSERT INTO migration_source_manifest VALUES (?1,'aggregate-root-v1',?2,?1)",
            params![source, document.bytes()],
        )
        .expect("source provenance");
    let archive = Connection::open(&archive_path).expect("archive");
    super::super::import_rows::CopyContext {
        archive: &archive,
        target: &store.connection,
        mapping: std::collections::HashMap::default(),
        heads: std::collections::HashMap::default(),
    }
    .retain_rows(&source)
    .expect("original rows");
    store.connection.execute("INSERT INTO migration_original_objects SELECT object_hash,object_kind,canonical_json,created_at,rowid FROM objects",[]).expect("originals");
    let objects: Vec<(String, String)> = store
        .connection
        .prepare("SELECT object_hash,object_kind FROM migration_original_objects")
        .expect("objects")
        .query_map([], |r| Ok((r.get(0)?, r.get(1)?)))
        .expect("rows")
        .collect::<Result<_, _>>()
        .expect("objects");
    for (hash, kind) in objects {
        let binding = CanonicalObject::freeze(&serde_json::json!({"profile":"aggregate-root-v1","source":hash,"target":hash,"kind":kind})).expect("binding");
        store
            .connection
            .execute(
                "INSERT INTO objects VALUES (?1,'migration_object_binding',?2,?3)",
                params![binding.hash().as_str(), binding.bytes(), now.to_rfc3339()],
            )
            .expect("canonical binding");
        store
            .connection
            .execute(
                "INSERT INTO migration_object_map VALUES (?1,?1,?2)",
                params![hash, binding.hash().as_str()],
            )
            .expect("mapping");
    }
    (dir, service, store, payload, manifest, source)
}

#[test]
fn migration_delivery_omitted_attribution_replays_without_rewriting_pending() {
    let (_dir, service, store, payload, manifest, source) = fixture(None, true);
    record_attribution(&store.connection, &manifest, &source).expect("audit");
    verify_attribution(&store.connection, &manifest, &source).expect("audit verified");
    let project = ProjectId("p".into());
    let session = SessionId("s".into());
    let state = store
        .work_session_state(&project, &session, chrono::Utc::now())
        .expect("state");
    let mut different_page: serde_json::Value = payload.decode().expect("new page value");
    different_page["omitted_count"] =
        (different_page["omitted_count"].as_u64().expect("count") + 1).into();
    let different_page = CanonicalObject::freeze(&different_page).expect("different page identity");
    assert!(
        crate::work_service::validate_migration_delivery(
            &store,
            &session,
            &project,
            state.project_cursor,
            state.tentative_project_cursor.expect("through"),
            &different_page
        )
        .is_err(),
        "another page cannot inherit this audit even with identical source entries"
    );
    let before = crate::storage::test_database_shape_snapshot(&store.connection).expect("before");
    crate::work_service::validate_migration_delivery(
        &store,
        &session,
        &project,
        state.project_cursor,
        state.tentative_project_cursor.expect("through"),
        &payload,
    )
    .expect("same runtime validator");
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&store.connection).expect("after"),
        before
    );
    // Repeating the already-confirmed acknowledgement leaves pending intact.
    // No acknowledgement would implicitly consume it before this decoder runs.
    let replay = service
        .work_next_with_delivery_token(
            20,
            Some(state.project_cursor),
            None,
            WorkNextQuery {
                sections: vec![WorkNextSection::Changes],
                ..WorkNextQuery::default()
            },
            chrono::Utc::now(),
        )
        .expect("ordinary pending replay");
    assert!(replay.changes.expect("changes")[0].from_current_session);
    assert_eq!(replay.delivery_token, state.tentative_delivery_token);
    assert_eq!(
        store
            .staged_work_session_delivery_payload(&project, &session)
            .expect("payload")
            .expect("pending")
            .bytes(),
        payload.bytes()
    );
    let decoded: StagedWorkChangePage = payload.decode().expect("original");
    assert!(
        !decoded.changes[0].from_current_session,
        "original bytes still omit the derived bit"
    );
    for fault in [
        "UPDATE migration_delivery_attribution SET audit_json=X'7b7d'",
        "UPDATE work_session_state SET tentative_delivery_token='different-token'",
        "DELETE FROM migration_source_manifest",
        "DELETE FROM migration_original_rows WHERE table_name='work_session_state'",
    ] {
        store
            .connection
            .execute_batch("PRAGMA foreign_keys=OFF; SAVEPOINT fault")
            .expect("fault boundary");
        store.connection.execute(fault, []).expect("inject");
        let before =
            crate::storage::test_database_shape_snapshot(&store.connection).expect("fault state");
        assert!(
            crate::work_service::validate_migration_delivery(
                &store,
                &session,
                &project,
                state.project_cursor,
                state.tentative_project_cursor.expect("through"),
                &payload
            )
            .is_err(),
            "{fault}"
        );
        assert_eq!(
            crate::storage::test_database_shape_snapshot(&store.connection).expect("read only"),
            before
        );
        store
            .connection
            .execute_batch("ROLLBACK TO fault; RELEASE fault; PRAGMA foreign_keys=ON")
            .expect("restore");
    }
    store
        .connection
        .execute("DELETE FROM migration_delivery_attribution", [])
        .expect("fault");
    assert!(verify_attribution(&store.connection, &manifest, &source).is_err());
    assert!(
        crate::work_service::validate_migration_delivery(
            &store,
            &session,
            &project,
            state.project_cursor,
            state.tentative_project_cursor.expect("through"),
            &payload
        )
        .is_err()
    );
    // Restore its audit, then acknowledge normally. A future page is not covered.
    record_attribution(&store.connection, &manifest, &source).expect("restore audit");
    let next = service
        .work_next(
            20,
            WorkNextQuery {
                sections: vec![WorkNextSection::Changes],
                ..WorkNextQuery::default()
            },
            chrono::Utc::now(),
        )
        .expect("normal implicit acknowledgement");
    assert_eq!(
        next.session.confirmed_project_cursor,
        state.tentative_project_cursor.expect("through")
    );
    assert!(next.changes.expect("following changes").is_empty());
    assert!(
        store
            .staged_work_session_delivery_payload(&project, &session)
            .expect("payload")
            .is_none()
    );
    verify_attribution(&store.connection, &manifest, &source)
        .expect("historical audit survives acknowledgement");
}

#[test]
fn migration_delivery_derivation_uses_exported_column_positions() {
    let (_dir, _service, store, _payload, manifest, source) = fixture(None, true);
    let table = session_table(&manifest).expect("session table");
    let (row_number, encoded): (i64, Vec<u8>) = store.connection.query_row(
        "SELECT row_number,cells FROM migration_original_rows WHERE source_id=?1 AND table_name='work_session_state'",
        [&source], |row| Ok((row.get(0)?, row.get(1)?)),
    ).expect("retained session row");
    let expected = derive(&store.connection, table, &source, row_number, &encoded)
        .expect("ordinary columns")
        .expect("derived attribution");
    let mut with_control = table.clone();
    let mut control = with_control.columns[0].clone();
    control.name = "virtual_control".into();
    control.hidden = 1;
    with_control.columns.insert(0, control);
    // Generated columns are encoded too; only virtual control columns disappear.
    with_control.columns[1].hidden = 2;
    with_control.columns[2].hidden = 3;
    for (position, column) in with_control.columns.iter_mut().enumerate() {
        column.position = i64::try_from(position).expect("position");
    }
    assert_eq!(
        derive(
            &store.connection,
            &with_control,
            &source,
            row_number,
            &encoded
        )
        .expect("same exported cells despite the control column")
        .as_ref(),
        Some(&expected)
    );
    assert!(
        table.rowid_alias.is_some(),
        "fixture exercises the rowid offset"
    );
    let cells = rows::decode(&encoded, table.columns.len() + 1).expect("source cells");
    let without_rowid = encode_cells(&store.connection, &cells[1..]);
    with_control.rowid_alias = None;
    assert_eq!(
        derive(
            &store.connection,
            &with_control,
            &source,
            row_number,
            &without_rowid
        )
        .expect("same attribution without a rowid cell"),
        Some(expected)
    );
}

fn encode_cells(connection: &Connection, cells: &[rows::Cell<'_>]) -> Vec<u8> {
    let sql = format!("SELECT {}", vec!["?"; cells.len()].join(","));
    let mut query = connection.prepare(&sql).expect("cell encoder");
    let mut selected = query
        .query(rusqlite::params_from_iter(cells.iter()))
        .expect("cells");
    rows::encode(
        selected.next().expect("row").expect("selected"),
        cells.len(),
    )
    .expect("encoded cells")
}

#[test]
fn migration_delivery_mutated_retained_row_refuses_rederivation() {
    let (_dir, _service, store, payload, manifest, source) = fixture(None, true);
    record_attribution(&store.connection, &manifest, &source).expect("record audit");
    verify_attribution(&store.connection, &manifest, &source).expect("valid audit");
    let project = ProjectId("p".into());
    let session = SessionId("s".into());
    let state = store
        .work_session_state(&project, &session, chrono::Utc::now())
        .expect("state");
    let validate = || {
        store.migrated_delivery_attribution(
            &project,
            &session,
            state.project_cursor,
            state.tentative_project_cursor.expect("through"),
            &payload,
        )
    };
    assert!(!validate().expect("valid replay").is_empty());
    let audit = lookup(&store.connection, &project, &session, payload.hash()).expect("audit");
    let table = session_table(&manifest).expect("table");
    let (row_number, encoded): (i64, Vec<u8>) = store.connection.query_row(
        "SELECT row_number,cells FROM migration_original_rows WHERE source_id=?1 AND table_name='work_session_state'",
        [&source], |row| Ok((row.get(0)?, row.get(1)?)),
    ).expect("retained session row");
    let count = table
        .columns
        .iter()
        .filter(|column| column.hidden != 1)
        .count()
        + usize::from(table.rowid_alias.is_some());
    let token_index = table
        .columns
        .iter()
        .filter(|column| column.hidden != 1)
        .position(|column| column.name == "tentative_delivery_token")
        .expect("token")
        + usize::from(table.rowid_alias.is_some());
    let mut cells = rows::decode(&encoded, count).expect("original cells");
    cells[token_index] = rows::Cell(ValueRef::Text(b"changed-retained-token"));
    let changed = encode_cells(&store.connection, &cells);
    assert_ne!(changed, encoded);
    assert_eq!(store.connection.execute(
        "UPDATE migration_original_rows SET cells=?1 WHERE source_id=?2 AND table_name='work_session_state' AND row_number=?3",
        params![changed, source, row_number],
    ).expect("mutate, do not delete, retained row"), 1);
    assert_ne!(
        derive(&store.connection, table, &source, row_number, &changed).expect("still decodable"),
        audit
    );
    assert_eq!(
        lookup(&store.connection, &project, &session, payload.hash()).expect("unchanged audit"),
        audit
    );
    let before =
        crate::storage::test_database_shape_snapshot(&store.connection).expect("fault state");
    for result in [
        verify_attribution(&store.connection, &manifest, &source),
        validate().map(|_| ()),
    ] {
        assert!(
            matches!(result, Err(StoreError::InvalidWorkProjection(reason))
            if reason == "migration delivery attribution: audit differs from original page and canonical source")
        );
    }
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&store.connection).expect("read only"),
        before
    );
    store.connection.execute(
        "UPDATE migration_original_rows SET cells=?1 WHERE source_id=?2 AND table_name='work_session_state' AND row_number=?3",
        params![encoded, source, row_number],
    ).expect("restore original row");
    verify_attribution(&store.connection, &manifest, &source).expect("restored audit");
    assert!(!validate().expect("restored replay").is_empty());
}

#[test]
fn migration_delivery_explicit_false_is_not_reinterpreted_and_true_needs_no_audit() {
    let (_dir, _service, store, _payload, manifest, source) = fixture(Some(false), true);
    let before = crate::storage::test_database_shape_snapshot(&store.connection).expect("before");
    assert!(
        matches!(record_attribution(&store.connection,&manifest,&source),Err(StoreError::InvalidWorkProjection(reason)) if reason.contains("explicit staged attribution"))
    );
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&store.connection).expect("after"),
        before
    );
    let (_dir, _service, store, _payload, manifest, source) = fixture(Some(true), true);
    record_attribution(&store.connection, &manifest, &source).expect("native true");
    let count: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM migration_delivery_attribution",
            [],
            |r| r.get(0),
        )
        .expect("count");
    assert_eq!(count, 0);
}

#[test]
fn migration_delivery_peer_false_and_absence_remain_false_and_explicit_true_refuses() {
    for explicit in [None, Some(false), Some(true)] {
        let (_dir, _service, store, payload, manifest, source) = fixture(explicit, false);
        let result = record_attribution(&store.connection, &manifest, &source);
        if explicit == Some(true) {
            assert!(result.is_err());
            continue;
        }
        result.expect("peer false");
        let project = ProjectId("p".into());
        let session = SessionId("s".into());
        let state = store
            .work_session_state(&project, &session, chrono::Utc::now())
            .expect("state");
        crate::work_service::validate_migration_delivery(
            &store,
            &session,
            &project,
            state.project_cursor,
            state.tentative_project_cursor.expect("through"),
            &payload,
        )
        .expect("native check preserved");
        let count: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM migration_delivery_attribution",
                [],
                |r| r.get(0),
            )
            .expect("audits");
        assert_eq!(count, 0);
    }
}
