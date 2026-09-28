//! The registry is the one list persistence admits and the doctor reads by.

use std::collections::HashSet;
use std::path::Path;

use rusqlite::params;
use serde_json::{Value as Json, json};

use super::super::test_support::{DevelopmentNoopRedactor, at, root_request};
use super::{CORE_RECEIPTS, PROTOCOL_RESULTS, Receipt};
use crate::domain::{ChildRequirement, SessionId, WorkRevisionPatch};
use crate::storage::{BeginWorkProtocolAttempt, SqliteStore, StoreError};
use crate::work_service::{
    LocalWorkService, WorkChildInput, WorkItemSummary, WorkProposeInput, WorkProposeResult,
    WorkUpdateInput,
};
use crate::{CanonicalObject, ObjectId, ProjectId};

fn invalid_work_records(database: &Path) -> Vec<String> {
    SqliteStore::open(database)
        .expect("doctor store")
        .verify_all()
        .expect("doctor")
        .invalid_work_records
}

/// Each table names an operation once, so one name has one decoder.
#[test]
fn every_operation_is_registered_once() {
    for table in [CORE_RECEIPTS, PROTOCOL_RESULTS] {
        let mut names = HashSet::new();
        for (name, _) in table {
            assert!(names.insert(*name), "{name} is registered twice");
        }
    }
}

/// A receipt is stored only under a registered name the build still runs:
/// an unregistered or a retired name is refused before anything is written,
/// for a core receipt and for an ambient protocol attempt alike.
#[test]
fn storing_under_an_unregistered_or_retired_name_is_refused() {
    let directory = crate::test_support::temp_home().expect("directory");
    let database = directory.path().join("store.db");
    let mut store = SqliteStore::open(&database).expect("store");
    assert!(
        CORE_RECEIPTS
            .iter()
            .any(|(name, receipt)| *name == "complete_work_recovery"
                && matches!(receipt, Receipt::Retired)),
        "the retired core operation stays registered as retired"
    );
    let transaction = store.connection.transaction().expect("transaction");
    for operation in ["invented_operation", "complete_work_recovery"] {
        let refused = super::super::planning::persist_operation_result(
            &transaction,
            operation,
            "key",
            &ObjectId::from_canonical_bytes(b"request"),
            &json!({}),
        );
        assert!(
            matches!(&refused, Err(StoreError::InvalidWorkProjection(detail)) if detail.contains(operation)),
            "{operation}: {refused:?}"
        );
    }
    for operation in ["work_update:invented", "invented"] {
        let refused = super::super::session::begin_work_protocol_attempt_on(
            &transaction,
            &BeginWorkProtocolAttempt {
                project_id: &ProjectId("receipts".into()),
                session_id: &SessionId("session".into()),
                operation,
                idempotency_key: "key",
                intent: &json!({"intent": 1}),
                basis: &json!({"basis": 1}),
                now: at(0),
            },
        );
        assert!(
            matches!(&refused, Err(StoreError::InvalidWorkProjection(detail)) if detail.contains(operation)),
            "{operation}: {refused:?}"
        );
    }
    let written: i64 = transaction
        .query_row(
            "SELECT (SELECT COUNT(*) FROM work_operation_results)
                  + (SELECT COUNT(*) FROM work_protocol_attempts)",
            [],
            |row| row.get(0),
        )
        .expect("count");
    assert_eq!(written, 0, "a refused name writes nothing");
}

/// The doctor decodes every stored core receipt as the type its operation
/// stores. It reports each one that does not decode, or whose operation is
/// registered nowhere, by operation and key with a reason that names the
/// shape and never a value, and it goes on past a bad row. A retired
/// operation's receipt is read as stored and never reported.
#[test]
fn the_doctor_decodes_every_stored_receipt_as_its_operation_stores_it() {
    let directory = crate::test_support::temp_home().expect("directory");
    let database = directory.path().join("store.db");
    let mut store = SqliteStore::open(&database).expect("store");
    store
        .create_work(
            &root_request("receipts", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let healthy = store.verify_all().expect("doctor");
    assert!(healthy.is_healthy(), "{:?}", healthy.invalid_work_records);
    let rows: [(&str, &str, &[u8]); 6] = [
        (
            "checkpoint_work",
            "wrong-type",
            br#""private-receipt-value""#,
        ),
        ("claim_work", "missing", b"{}"),
        (
            "complete_work_recovery",
            "retired",
            b"private receipt, old shape",
        ),
        ("create_work", "malformed", b"{"),
        ("invented_operation", "unknown", b"{}"),
        ("record_work_note", "truncated", br#"{"evidence":"#),
    ];
    for (operation, key, receipt) in rows {
        store
            .connection
            .execute(
                "INSERT INTO work_operation_results (
                     operation, idempotency_key, request_hash, result_json
                 ) VALUES (?1, ?2, ?3, ?4)",
                params![
                    operation,
                    key,
                    ObjectId::from_canonical_bytes(key.as_bytes()).as_str(),
                    receipt
                ],
            )
            .expect("stored receipt");
    }
    let report = store.verify_all().expect("doctor");
    let reported = report.invalid_work_records;
    assert_eq!(
        reported,
        [
            "work_operation_result:checkpoint_work:wrong-type:a field of the wrong type or value at line 0 column 0",
            "work_operation_result:claim_work:missing:missing field `claim_id` at line 1 column 2",
            "work_operation_result:create_work:malformed:truncated JSON at line 1 column 1",
            "work_operation_result:invented_operation:unknown:an operation this build does not register",
            "work_operation_result:record_work_note:truncated:truncated JSON at line 1 column 12",
        ],
        "each bad row is reported once, in order, and the retired row is not"
    );
    assert!(
        reported
            .iter()
            .all(|label| !label.contains("private") && !label.contains("old shape")),
        "{reported:?}"
    );
}

/// The ambient attempt a service call leaves under `operation`, with its
/// stored result read back as JSON.
struct StoredResult {
    label: String,
    result_id: String,
    result: Json,
}

fn stored_result(database: &Path, operation: &str) -> StoredResult {
    let connection = rusqlite::Connection::open(database).expect("connection");
    let (project, session, key, result_id, bytes) = connection
        .query_row(
            "SELECT project_id, session_id, idempotency_key, result_id, result_json
             FROM work_protocol_attempts WHERE operation = ?1",
            [operation],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, Vec<u8>>(4)?,
                ))
            },
        )
        .expect("one completed attempt");
    StoredResult {
        label: format!("work_protocol_attempt:{project}:{session}:{operation}:{key}"),
        result_id,
        result: serde_json::from_slice(&bytes).expect("stored result"),
    }
}

/// Replaces the attempt's stored result and the canonical object it names
/// with the same canonical bytes, so the attempt still matches its object
/// and only the result's shape is wrong.
fn rewrite_result(database: &Path, operation: &str, stored: &StoredResult, result: &Json) {
    let canonical = serde_json_canonicalizer::to_vec(result).expect("canonical result");
    let connection = rusqlite::Connection::open(database).expect("connection");
    assert_eq!(
        connection
            .execute(
                "UPDATE work_protocol_attempts SET result_json = ?1 WHERE operation = ?2",
                params![canonical, operation],
            )
            .expect("rewrite the attempt"),
        1
    );
    assert_eq!(
        connection
            .execute(
                "UPDATE objects SET canonical_json = ?1 WHERE object_id = ?2",
                params![canonical, stored.result_id],
            )
            .expect("rewrite the object"),
        1
    );
}

fn restore_result(database: &Path, operation: &str, stored: &StoredResult) {
    rewrite_result(database, operation, stored, &stored.result);
}

fn ambient_service(database: &Path) -> LocalWorkService {
    LocalWorkService::new(
        database.to_path_buf(),
        ProjectId("ambient-receipts".into()),
        "agent".into(),
        SessionId("ambient-session".into()),
        Some("receipt-test".into()),
    )
}

fn propose_root(service: &LocalWorkService, key: &str, second: i64) -> WorkItemSummary {
    let proposed = service
        .work_propose(
            WorkProposeInput::Root {
                acceptance_bindings: Vec::new(),
                evaluation_mode: None,
                external_ref: None,
                notes: Vec::new(),
                title: format!("Read receipts back ({key})"),
                outcome: "Every receipt decodes".into(),
                acceptance: vec!["the doctor decodes them".into()],
                work_kind: None,
                priority: None,
                labels: Vec::new(),
                assigned_to: None,
                deferred_until: None,
                idempotency_key: key.into(),
            },
            at(second),
        )
        .expect("root");
    let WorkProposeResult::Root { work, .. } = proposed else {
        panic!("expected a root");
    };
    work
}

/// Operations the rest of the suite runs only directly on the store also
/// store, through the service, what the registry reads back: a removed
/// prerequisite and a keyed claim of a parent's next ready child, under
/// their core and their protocol names.
#[test]
fn a_removed_prerequisite_and_a_keyed_next_ready_claim_are_read_back() {
    let directory = crate::test_support::temp_home().expect("directory");
    let database = directory.path().join("service.db");
    let service = ambient_service(&database);
    let dependant = propose_root(&service, "dependant", 0);
    let parent = propose_root(&service, "parent", 1);
    for (second, input) in [
        (
            2,
            WorkUpdateInput::AddPrerequisite {
                prerequisite: parent.short_ref.clone(),
                idempotency_key: "add".into(),
            },
        ),
        (
            3,
            WorkUpdateInput::RemovePrerequisite {
                prerequisite: parent.short_ref.clone(),
                idempotency_key: "remove".into(),
            },
        ),
    ] {
        service
            .work_update_on(Some(&dependant.short_ref), input, at(second))
            .expect("prerequisite update");
    }
    service
        .work_focus(&parent.short_ref, at(4))
        .expect("focus the parent");
    service
        .work_propose(
            WorkProposeInput::Decompose {
                children: vec![WorkChildInput {
                    acceptance_bindings: Vec::new(),
                    evaluation_mode: None,
                    external_ref: None,
                    notes: Vec::new(),
                    key: "child".into(),
                    title: "Ready child".into(),
                    outcome: "Claimed as the next ready child".into(),
                    acceptance: vec!["claimed".into()],
                    requirement: Some(ChildRequirement::Required),
                    kind: None,
                    priority: None,
                    labels: Vec::new(),
                    assigned_to: None,
                    deferred_until: None,
                }],
                prerequisites: Vec::new(),
                idempotency_key: "decompose".into(),
            },
            at(5),
        )
        .expect("decompose");
    service
        .work_update_on(
            Some(&parent.short_ref),
            WorkUpdateInput::ClaimNextReady {
                ttl_seconds: None,
                recovery_reason: None,
                idempotency_key: "next-ready".into(),
            },
            at(6),
        )
        .expect("claim the next ready child");
    let connection = rusqlite::Connection::open(&database).expect("connection");
    let stored = connection
        .prepare(
            "SELECT operation FROM work_operation_results
             UNION SELECT operation FROM work_protocol_attempts WHERE result_json IS NOT NULL",
        )
        .expect("names")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("names")
        .collect::<Result<HashSet<_>, _>>()
        .expect("names");
    for operation in [
        "remove_work_prerequisite",
        "work_update:remove_prerequisite",
        "claim_next_ready_child",
        "work_update:claim_next_ready",
    ] {
        assert!(stored.contains(operation), "{operation}: {stored:?}");
    }
    assert_eq!(invalid_work_records(&database), Vec::<String>::new());
}

/// The doctor decodes each ambient result as the type its operation
/// stores, not only its binding. A note result is read as a note, so one
/// that lost its evidence member is reported while an update result, which
/// has none, is not; a root proposal that reads as another kind is
/// reported; an update result without its receipt is reported with that
/// member named, though its binding check fails too; and a pending attempt
/// is reported only when its operation is registered nowhere. Attempt and
/// object are changed together, so each report comes from decoding and not
/// from their byte comparison.
#[test]
fn the_doctor_decodes_each_ambient_result_as_its_operation_stores_it() {
    let directory = crate::test_support::temp_home().expect("directory");
    let database = directory.path().join("service.db");
    let service = ambient_service(&database);
    let work = propose_root(&service, "root", 0);
    service
        .work_update_on(
            Some(&work.short_ref),
            WorkUpdateInput::Revise {
                patch: WorkRevisionPatch {
                    title: Some("Read every receipt back".into()),
                    ..WorkRevisionPatch::default()
                },
                idempotency_key: "revise".into(),
            },
            at(1),
        )
        .expect("revise");
    service
        .work_note_on(Some(&work.short_ref), "a note to read back", &[], at(2))
        .expect("note");
    assert_eq!(invalid_work_records(&database), Vec::<String>::new());

    let note = stored_result(&database, "work_update:note");
    let mut without_evidence = note.result.clone();
    without_evidence
        .as_object_mut()
        .expect("note result")
        .remove("evidence")
        .expect("a note result carries its evidence");
    rewrite_result(&database, "work_update:note", &note, &without_evidence);
    let reported = invalid_work_records(&database);
    assert_eq!(reported.len(), 1, "{reported:?}");
    assert!(
        reported[0].starts_with(&format!(
            "{}:missing field `evidence` at line 1 column ",
            note.label
        )),
        "{reported:?}"
    );
    restore_result(&database, "work_update:note", &note);

    let proposal = stored_result(&database, "work_propose:root");
    let mut decomposition = proposal.result.clone();
    decomposition["kind"] = json!("decomposition");
    decomposition["parent"] = proposal.result["work"].clone();
    decomposition["child_count"] = json!(0);
    decomposition["children"] = json!([]);
    rewrite_result(&database, "work_propose:root", &proposal, &decomposition);
    assert_eq!(
        invalid_work_records(&database),
        [format!("{}:a proposal of another kind", proposal.label)]
    );
    restore_result(&database, "work_propose:root", &proposal);

    // Without its receipt the result also fails the binding check, which
    // reads the work id from the receipt; the report still names the member.
    let revise = stored_result(&database, "work_update:revise");
    let mut without_receipt = revise.result.clone();
    without_receipt
        .as_object_mut()
        .expect("update result")
        .remove("receipt")
        .expect("an update result carries its receipt");
    rewrite_result(&database, "work_update:revise", &revise, &without_receipt);
    let reported = invalid_work_records(&database);
    assert_eq!(reported.len(), 1, "{reported:?}");
    assert!(
        reported[0].starts_with(&format!(
            "{}:missing field `receipt` at line 1 column ",
            revise.label
        )),
        "{reported:?}"
    );
    restore_result(&database, "work_update:revise", &revise);
    assert_eq!(invalid_work_records(&database), Vec::<String>::new());

    let basis = CanonicalObject::freeze(&json!({"pending": true})).expect("basis");
    let connection = rusqlite::Connection::open(&database).expect("connection");
    for operation in ["work_update:revise", "work_update:invented"] {
        connection
            .execute(
                "INSERT INTO work_protocol_attempts (
                     project_id, session_id, operation, idempotency_key, request_hash,
                     basis_hash, basis_json, initiated_at_ms, result_id, result_json
                 ) VALUES ('ambient-receipts', 'ambient-session', ?1, 'pending', ?2, ?3, ?4, 0, NULL, NULL)",
                params![
                    operation,
                    ObjectId::from_canonical_bytes(operation.as_bytes()).as_str(),
                    basis.key().as_str(),
                    basis.bytes()
                ],
            )
            .expect("pending attempt");
    }
    drop(connection);
    assert_eq!(
        invalid_work_records(&database),
        [
            "work_protocol_attempt:ambient-receipts:ambient-session:work_update:invented:pending:an operation this build does not register"
        ]
    );
}
