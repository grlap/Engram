//! A delivery page staged before the transfer: admitted, replayed at its
//! confirmed cursor, or refused by session.

use super::*;

/// A store whose session has a delivery page staged and not yet acknowledged.
fn staged_pending_delivery(database: &Path) -> (ProjectId, crate::SessionId) {
    let project = ProjectId("project-pending-transfer".into());
    let session = crate::SessionId("pending-session".into());
    let service = crate::work_service::LocalWorkService::new(
        database.to_path_buf(),
        project.clone(),
        "author".into(),
        session.clone(),
        None,
    );
    let at = |second: i64| {
        chrono::DateTime::parse_from_rfc3339("2026-09-17T10:00:00Z")
            .expect("time")
            .with_timezone(&Utc)
            + chrono::Duration::seconds(second)
    };
    let peer = crate::work_service::LocalWorkService::new(
        database.to_path_buf(),
        project.clone(),
        "peer".into(),
        crate::SessionId("peer-session".into()),
        None,
    );
    peer.work_propose(
        crate::work_service::WorkProposeInput::Root {
            acceptance_bindings: Vec::new(),
            evaluation_mode: None,
            external_ref: None,
            notes: Vec::new(),
            title: "Work another session proposed".into(),
            outcome: "the page carries a peer change".into(),
            acceptance: vec!["the peer change stays a peer change".into()],
            work_kind: None,
            priority: None,
            labels: Vec::new(),
            assigned_to: None,
            deferred_until: None,
            idempotency_key: "peer-root".into(),
        },
        at(1),
    )
    .expect("peer root");
    service
        .work_propose(
            crate::work_service::WorkProposeInput::Root {
                acceptance_bindings: Vec::new(),
                evaluation_mode: None,
                external_ref: None,
                notes: Vec::new(),
                title: "Carry a staged delivery".into(),
                outcome: "the page survives".into(),
                acceptance: vec!["the page replays".into()],
                work_kind: None,
                priority: None,
                labels: Vec::new(),
                assigned_to: None,
                deferred_until: None,
                idempotency_key: "pending-root".into(),
            },
            at(1),
        )
        .expect("root");
    service
        .work_next(20, crate::work_service::WorkNextQuery::default(), at(2))
        .expect("stage a delivery page");
    assert!(
        pending_cursor(database).is_some(),
        "the fixture needs a page staged but unacknowledged"
    );
    (project, session)
}

/// The confirmed cursor, the cursor a page is staged through, and its capability.
fn pending_state(path: &Path) -> (i64, i64, Option<String>) {
    Connection::open(path)
        .expect("open")
        .query_row(
            "SELECT project_cursor, tentative_project_cursor, tentative_delivery_token
             FROM work_session_state WHERE tentative_project_cursor IS NOT NULL",
            [],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .expect("a staged page")
}

/// The cursor a page is staged through, when one is pending.
fn pending_cursor(path: &Path) -> Option<i64> {
    Connection::open(path)
        .expect("open")
        .query_row(
            "SELECT tentative_project_cursor FROM work_session_state
             WHERE tentative_project_cursor IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .optional()
        .expect("staged cursor")
}

/// The session holding a staged page, and the page.
fn pending_payload(path: &Path) -> (String, Json) {
    let connection = Connection::open(path).expect("open");
    let (id, payload): (String, Vec<u8>) = connection
        .query_row(
            "SELECT session_id, tentative_delivery_payload
             FROM work_session_state WHERE tentative_project_cursor IS NOT NULL",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("a staged page");
    (
        id,
        serde_json::from_slice(&payload).expect("staged page json"),
    )
}

#[test]
fn a_staged_delivery_page_survives_the_transfer_and_replays_at_its_confirmed_cursor() {
    let directory = crate::test_support::temp_home().expect("directory");
    let source = directory.path().join("source.db");
    let (project, session) = staged_pending_delivery(&source);
    let (_, before) = pending_payload(&source);

    let file = directory.path().join("export.jsonl");
    export_json(&source, &file).expect("export");
    let target = directory.path().join("target.db");
    let imported = import_json(&file, &target).expect("import");
    assert_eq!(imported.checked_pending_deliveries, 1);
    let (_, after) = pending_payload(&target);
    assert_eq!(after, before, "the frozen payload is carried verbatim");

    assert_replays_without_acknowledging(&target, &project, &session, &before);
}

/// Replays the retained page the way a core retry does: at the cursor already
/// confirmed, with no acknowledgement capability, so the pending page is
/// re-read rather than cleared. Proves the page, its capability and the cursors
/// are exactly as they were afterwards.
fn assert_replays_without_acknowledging(
    database: &Path,
    project: &ProjectId,
    session: &crate::SessionId,
    expected: &Json,
) {
    let (confirmed, through, token) = pending_state(database);
    let service = crate::work_service::LocalWorkService::new(
        database.to_path_buf(),
        project.clone(),
        "author".into(),
        session.clone(),
        None,
    );
    let at = chrono::DateTime::parse_from_rfc3339("2026-09-17T11:00:00Z")
        .expect("time")
        .with_timezone(&Utc);
    let replayed = service
        .work_next_with_delivery_token(
            20,
            Some(confirmed),
            None,
            crate::work_service::WorkNextQuery::default(),
            at,
        )
        .expect("the retained page replays");
    let delivered = replayed.changes.as_ref().expect("the retained page");
    let staged = expected["changes"].as_array().expect("changes");
    assert_eq!(delivered.len(), staged.len(), "the exact retained entries");
    for (delivered, staged) in delivered.iter().zip(staged) {
        let delivered = serde_json::to_value(delivered).expect("change");
        assert_eq!(delivered["entry"], staged["entry"], "the exact feed entry");
        assert_eq!(
            delivered["from_current_session"].as_bool().unwrap_or(false),
            staged["from_current_session"].as_bool().unwrap_or(false),
            "the exact attribution"
        );
    }
    // Nothing was acknowledged: the same page, capability and cursors remain.
    let (still_confirmed, still_through, still_token) = pending_state(database);
    assert_eq!((still_confirmed, still_through), (confirmed, through));
    assert_eq!(still_token, token, "the delivery capability is unchanged");
    let (_, unchanged) = pending_payload(database);
    assert_eq!(&unchanged, expected, "the frozen page is unchanged");
}

#[test]
fn a_staged_page_that_omits_the_attribution_the_source_proves_is_refused() {
    let directory = crate::test_support::temp_home().expect("directory");
    let source = directory.path().join("source.db");
    staged_pending_delivery(&source);
    let (id, mut payload) = pending_payload(&source);
    // A page that leaves out the bit saying a change is the session's own reads
    // as claiming it is not, which the record contradicts. Nothing is supplied
    // on its behalf: the page is refused before publication, exactly as the
    // next retry would refuse it.
    let mut stripped = 0;
    for change in payload["changes"].as_array_mut().expect("changes") {
        if change
            .as_object_mut()
            .expect("change")
            .remove("from_current_session")
            .is_some_and(|bit| bit == Json::Bool(true))
        {
            stripped += 1;
        }
    }
    assert!(stripped > 0, "the fixture needs an own-session change");
    write_pending_payload(&source, &id, &payload);

    let file = directory.path().join("export.jsonl");
    export_json(&source, &file).expect("export");
    let target = directory.path().join("target.db");
    let error = import_json(&file, &target).expect_err("omitted attribution");
    assert!(
        matches!(&error, MigrationError::Refused(reason)
            if reason.contains("pending-session") && reason.contains("attribution differs")),
        "{error}"
    );
    assert!(!target.exists(), "nothing was published");
}

#[test]
fn a_staged_page_whose_attribution_the_source_denies_refuses_before_publication() {
    let directory = crate::test_support::temp_home().expect("directory");
    let source = directory.path().join("source.db");
    staged_pending_delivery(&source);
    let (id, mut payload) = pending_payload(&source);
    // A claim in the other direction: the page says a change another session
    // made is this session's own. Nothing may supply that, because the record
    // names the other session. An absent bit is indistinguishable from false,
    // so only this direction can be contradicted.
    let mut claimed = 0;
    for change in payload["changes"].as_array_mut().expect("changes") {
        let change = change.as_object_mut().expect("change");
        if change.get("from_current_session") != Some(&Json::Bool(true)) {
            change.insert("from_current_session".into(), Json::Bool(true));
            claimed += 1;
        }
    }
    assert!(claimed > 0, "the fixture needs a peer change");
    write_pending_payload(&source, &id, &payload);

    let file = directory.path().join("export.jsonl");
    export_json(&source, &file).expect("export");
    let target = directory.path().join("target.db");
    let error = import_json(&file, &target).expect_err("contradicted attribution");
    assert!(
        matches!(&error, MigrationError::Refused(reason)
            if reason.contains("pending-session") && reason.contains("attribution differs")),
        "{error}"
    );
    assert!(!target.exists());
}

fn write_pending_payload(path: &Path, id: &str, payload: &Json) {
    let bytes = serde_json_canonicalizer::to_vec(payload).expect("canonical page");
    let connection = Connection::open(path).expect("open");
    let changed = connection
        .execute(
            "UPDATE work_session_state SET tentative_delivery_payload = ?2
             WHERE session_id = ?1",
            rusqlite::params![id, bytes],
        )
        .expect("rewrite the staged page");
    assert_eq!(changed, 1);
}

#[test]
fn a_pending_delivery_the_file_left_incomplete_is_refused_by_session() {
    let directory = crate::test_support::temp_home().expect("directory");
    let source = directory.path().join("source.db");
    staged_pending_delivery(&source);
    let file = directory.path().join("export.jsonl");
    export_json(&source, &file).expect("export");
    let original = fs::read_to_string(&file).expect("file");
    let staged = |values: &Json| values["session_id"] == "pending-session";

    // The schema permits a row with only part of a pending delivery; the next
    // retry of that session refuses to read one. Either missing part must refuse
    // the import instead, so the published store never holds it.
    for column in ["tentative_delivery_payload", "tentative_delivery_token"] {
        fs::write(&file, &original).expect("restore file");
        with_row_cell(&file, "work_session_state", staged, column, Json::Null);
        let target = directory.path().join(format!("without-{column}.db"));
        let error = import_json(&file, &target).expect_err(column);
        assert!(
            matches!(&error, MigrationError::Refused(reason)
                if reason.contains("pending-session") && reason.contains("present together")),
            "{column}: {error}"
        );
        assert!(!target.exists(), "{column}: a store was published");
        assert_eq!(
            staging_leftovers(directory.path()),
            0,
            "{column}: a staging file was left"
        );
    }

    // Control: the same row as exported goes in, and is counted as checked.
    fs::write(&file, &original).expect("restore file");
    let target = directory.path().join("complete.db");
    let imported = import_json(&file, &target).expect("the complete row imports");
    assert_eq!(imported.checked_pending_deliveries, 1);
}

#[test]
fn a_malformed_staged_page_is_refused_without_printing_it() {
    let directory = crate::test_support::temp_home().expect("directory");
    let source = directory.path().join("source.db");
    staged_pending_delivery(&source);
    let (_, page) = pending_payload(&source);
    let file = directory.path().join("export.jsonl");
    export_json(&source, &file).expect("export");
    let original = fs::read_to_string(&file).expect("file");
    let staged = |values: &Json| values["session_id"] == "pending-session";
    // The page carries work titles and actor context. A page this build cannot
    // read is refused by session with the shape of the problem, never with
    // what the page holds.
    let sentinel = "a private title nobody else may read";
    let mut wrong_field = page.clone();
    wrong_field["changes"][0]["entry"]["position"] = Json::String(sentinel.into());
    // A page that decodes but names a record the feed does not hold at that
    // position fails in the verifier, whose reason is restated, not repeated:
    // the id it named must not come back either.
    let unknown_record = "f".repeat(32);
    let mut wrong_record = page.clone();
    wrong_record["changes"][0]["entry"]["object_id"] = Json::String(unknown_record.clone());
    let shapes = [
        (
            "a page that is one string",
            serde_json::json!({ "json": sentinel }),
            "not a delivery page",
            sentinel,
        ),
        (
            "a page with a field of the wrong type",
            serde_json::json!({ "json": wrong_field }),
            "not a delivery page",
            sentinel,
        ),
        (
            "bytes that are not JSON",
            serde_json::json!({ "text": format!("{{ not json {sentinel}") }),
            "not a delivery page",
            sentinel,
        ),
        (
            "a page naming a record the feed does not hold there",
            serde_json::json!({ "json": wrong_record }),
            "dense source interval",
            unknown_record.as_str(),
        ),
    ];
    for (label, value, expected, private) in shapes {
        fs::write(&file, &original).expect("restore file");
        with_row_cell(
            &file,
            "work_session_state",
            staged,
            "tentative_delivery_payload",
            value,
        );
        let target = directory.path().join("target.db");
        let error = import_json(&file, &target).expect_err(label);
        let text = error.to_string();
        assert!(
            matches!(&error, MigrationError::Refused(reason)
                if reason.contains("pending-session") && reason.contains(expected)),
            "{label}: {text}"
        );
        assert!(!text.contains(private), "{label} printed the page: {text}");
        assert!(!target.exists(), "{label}: a store was published");
    }
}
