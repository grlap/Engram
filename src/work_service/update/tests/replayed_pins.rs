//! Stored protocol results that carry an obligation page, whose items each
//! hold a verification requirement, are decoded again when an exact retry
//! replays them: an ordinary update, a note, a gate and a completion. Each
//! replay refuses a requirement that names an environment, with a value or
//! as null, rather than reading past it, and names the member.

use super::*;

/// Rewrites the newest stored result of `operation` so that the requirement
/// of its first obligation page item names `environment`. Replay compares the
/// attempt's result bytes with the stored result object, so both receive the
/// same canonical bytes.
fn name_an_environment_in_the_newest_result(
    database: &std::path::Path,
    operation: &str,
    environment: serde_json::Value,
) {
    let connection = rusqlite::Connection::open(database).expect("store connection");
    let (result_id, stored): (String, Vec<u8>) = connection
        .query_row(
            "SELECT result_id, result_json FROM work_protocol_attempts
             WHERE operation = ?1 AND result_id IS NOT NULL
             ORDER BY initiated_at_ms DESC LIMIT 1",
            [operation],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("the stored protocol result");
    let mut result: serde_json::Value = serde_json::from_slice(&stored).expect("result json");
    let requirement = result
        .pointer_mut("/obligation_page/items/0/requirement")
        .unwrap_or_else(|| panic!("{operation} stores an obligation page with a requirement"));
    requirement["required_environment"] = environment;
    let bytes = serde_json_canonicalizer::to_vec(&result).expect("canonical result");
    connection
        .execute(
            "UPDATE objects SET canonical_json = ?1 WHERE object_id = ?2",
            rusqlite::params![bytes, result_id],
        )
        .expect("store a result object that names an environment");
    connection
        .execute(
            "UPDATE work_protocol_attempts SET result_json = ?1 WHERE result_id = ?2",
            rusqlite::params![bytes, result_id],
        )
        .expect("store the attempt's matching result bytes");
}

fn environments() -> [serde_json::Value; 2] {
    [
        serde_json::json!(ObjectId::from_canonical_bytes(b"environment")),
        serde_json::Value::Null,
    ]
}

fn refused_by_name(error: &StoreError, entry: &str) {
    let error = error.to_string();
    assert!(
        error.contains("unknown field `required_environment`"),
        "{entry}: {error}"
    );
}

/// A source change opens the stock obligation, so every result below carries
/// an obligation page with one requirement. An exact retry replays each
/// result unchanged; once the stored result names an environment, the same
/// retry is refused.
#[test]
fn replayed_results_whose_obligation_page_names_an_environment_are_refused_by_name() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("work.sqlite3");
    let service = LocalWorkService::new(
        database.clone(),
        ProjectId("replayed-page-environment".into()),
        "agent".into(),
        SessionId("agent".into()),
        Some("protocol-test".into()),
    );
    let root = proposed_root(
        service
            .work_propose(root_input("Changed work", "changed-root"), at(0))
            .expect("root"),
    );
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(3_600),
                recovery_reason: None,
                idempotency_key: "changed-claim".into(),
            },
            at(1),
        )
        .expect("claim");
    SqliteStore::open(&database)
        .expect("fixture store")
        .append_source_change_fixture(root.work_id, "changed", at(2), "changed-revision");

    // An ordinary update, keyed explicitly.
    let checkpoint = WorkUpdateInput::Checkpoint {
        summary: "found the cause".into(),
        evidence: None,
        idempotency_key: "changed-checkpoint".into(),
    };
    let first = service
        .work_update(checkpoint.clone(), at(3))
        .expect("checkpoint");
    assert_eq!(first.obligation_page.items.len(), 1);
    let replayed = service
        .work_update(checkpoint.clone(), at(4))
        .expect("the checkpoint replays");
    assert_eq!(
        serde_json::to_value(&replayed).expect("replay JSON"),
        serde_json::to_value(&first).expect("first JSON")
    );
    for environment in environments() {
        name_an_environment_in_the_newest_result(&database, "work_update:checkpoint", environment);
        let error = service
            .work_update(checkpoint.clone(), at(5))
            .expect_err("a replayed update naming an environment is refused");
        refused_by_name(&error, "checkpoint");
    }

    // A note.
    let note = service
        .work_note_on(Some(&root.short_ref), "a durable finding", &[], at(6))
        .expect("note");
    assert_eq!(note.obligation_page.items.len(), 1);
    let replayed = service
        .work_note_on(Some(&root.short_ref), "a durable finding", &[], at(7))
        .expect("the note replays");
    assert_eq!(
        serde_json::to_value(&replayed).expect("replay JSON"),
        serde_json::to_value(&note).expect("first JSON")
    );
    for environment in environments() {
        name_an_environment_in_the_newest_result(&database, "work_update:note", environment);
        let error = service
            .work_note_on(Some(&root.short_ref), "a durable finding", &[], at(8))
            .expect_err("a replayed note naming an environment is refused");
        refused_by_name(&error, "note");
    }

    // A gate.
    let gate = service
        .work_gate("cargo-test", &[], None, at(9))
        .expect("gate");
    assert_eq!(gate.obligation_page.items.len(), 1);
    let replayed = service
        .work_gate("cargo-test", &[], None, at(10))
        .expect("the gate replays");
    assert_eq!(
        serde_json::to_value(&replayed).expect("replay JSON"),
        serde_json::to_value(&gate).expect("first JSON")
    );
    for environment in environments() {
        name_an_environment_in_the_newest_result(&database, "work_update:gate", environment);
        let error = service
            .work_gate("cargo-test", &[], None, at(11))
            .expect_err("a replayed gate naming an environment is refused");
        refused_by_name(&error, "gate");
    }

    // A completion, whose receipt lists the waived untested change.
    let completion = completion_input("delivered the change", "changed-completion");
    let completed = service
        .work_complete(completion.clone(), at(12))
        .expect("complete");
    let WorkCompleteResult::Completed(receipt) = &completed else {
        panic!("an untested change must not refuse completion");
    };
    assert_eq!(receipt.obligation_page.items.len(), 1);
    let replayed = service
        .work_complete(completion.clone(), at(13))
        .expect("the completion replays");
    assert_eq!(
        serde_json::to_value(&replayed).expect("replay JSON"),
        serde_json::to_value(&completed).expect("first JSON")
    );
    // A stored receipt is decoded as the receipt its seal claims, so the
    // member is named here too.
    for environment in environments() {
        name_an_environment_in_the_newest_result(&database, "work_complete", environment);
        let error = service
            .work_complete(completion.clone(), at(14))
            .expect_err("a replayed completion naming an environment is refused");
        assert!(matches!(error, StoreError::Json(_)), "{error:?}");
        refused_by_name(&error, "completion");
    }
}
