//! A native verification record's typed facts reach the agent reads beside
//! its summary: the summary is the host's prose and never decides the result.

use super::*;
use crate::domain::{ExecutionOutcome, VerificationKind, VerificationResult};
use crate::storage::{HostCheck, verification_note_fixture};

/// Every row of every user table, in a stable order: a read must leave the
/// whole store as it was, not only its row counts.
pub(super) fn store_snapshot(database: &std::path::Path) -> Vec<(String, Vec<String>)> {
    let connection = rusqlite::Connection::open(database).expect("open");
    let tables = connection
        .prepare(
            "SELECT name FROM sqlite_master
             WHERE type = 'table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )
        .expect("tables")
        .query_map([], |row| row.get::<_, String>(0))
        .expect("table names")
        .collect::<Result<Vec<_>, _>>()
        .expect("read table names");
    tables
        .into_iter()
        .map(|table| {
            let mut statement = connection
                .prepare(&format!("SELECT * FROM \"{table}\""))
                .expect("select");
            let columns = statement.column_count();
            let mut rows = statement
                .query_map([], |row| {
                    (0..columns)
                        .map(|index| row.get::<_, rusqlite::types::Value>(index))
                        .collect::<Result<Vec<_>, _>>()
                        .map(|values| format!("{values:?}"))
                })
                .expect("rows")
                .collect::<Result<Vec<_>, _>>()
                .expect("read rows");
            rows.sort();
            (table, rows)
        })
        .collect()
}

/// The case the host reported: an unknown producer outcome whose summary says
/// the tests passed. Every read prints the typed facts, says in plain words
/// why the record cannot satisfy a passing check, keeps the summary as
/// written and the existing row fields as they were, and changes nothing.
#[test]
fn an_unknown_outcome_with_a_passing_summary_reads_as_indeterminate() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let database = directory.path().join("work.sqlite3");
    let summary = "57 of 57 tests passed";
    let (work_ref, record, claim_id) = verification_note_fixture(
        &database,
        "verification-rows",
        "runner",
        HostCheck {
            key: "unknown-outcome",
            kind: VerificationKind::Test,
            outcome: ExecutionOutcome::Unknown,
            result: VerificationResult::Indeterminate,
            summary,
        },
    );
    let verbs = AgentVerbs::new(
        database.clone(),
        ProjectId("verification-rows".into()),
        "runner".into(),
        SessionId("runner".into()),
        None,
    );
    let before = store_snapshot(&database);
    // Raw session, claim, grant and producer-observation ids stay on host
    // views; every read below is checked for them.
    let raw_ids = [
        "runner".to_owned(),
        claim_id,
        "grant-unknown-outcome".to_owned(),
        "check-unknown-outcome".to_owned(),
    ];
    let assert_no_raw_ids = |receipt: &Receipt, read: &str| {
        let json = receipt.value.to_string();
        for raw in &raw_ids {
            assert!(!receipt.text().contains(raw.as_str()), "{read} text: {raw}");
            assert!(!json.contains(raw.as_str()), "{read} JSON: {raw}");
        }
    };

    // The notes window.
    let window = verbs
        .show_records(
            &work_ref,
            &ShowInput {
                notes: true,
                ..ShowInput::default()
            },
            at(10),
        )
        .expect("notes window");
    let row = window.value["notes"]
        .as_array()
        .expect("rows")
        .iter()
        .find(|row| row["locator"].as_str() == Some(record.as_str()))
        .expect("the verification row")
        .clone();
    let facts = &row["verification"];
    assert_eq!(facts["result"], "indeterminate");
    assert_eq!(facts["check_kind"], "test");
    assert_eq!(facts["source_revision"], "A3");
    assert_eq!(facts["producer_outcome"], "unknown");
    let meaning = facts["meaning"].as_str().expect("plain words");
    assert!(
        meaning.contains("recorded the outcome as unknown")
            && meaning.contains("cannot satisfy a passing-check requirement"),
        "{meaning}"
    );
    // The existing fields are unchanged, and the summary stays the host's prose.
    for field in [
        "locator",
        "kind",
        "family",
        "by",
        "created_at",
        "body_bytes",
        "refs",
    ] {
        assert!(row.get(field).is_some(), "{field} is kept: {row}");
    }
    assert_eq!(row["summary"], summary);
    assert_eq!(row["kind"], "verification");
    let text = window.text();
    assert!(
        text.contains(
            "verification: indeterminate test on source revision A3; producer outcome unknown"
        ),
        "{text}"
    );
    assert!(text.contains(meaning), "{text}");
    assert_no_raw_ids(&window, "notes window");

    // The complete detail of the record.
    let detail = verbs
        .show_records(
            &work_ref,
            &ShowInput {
                note: Some(record.as_str().to_owned()),
                ..ShowInput::default()
            },
            at(11),
        )
        .expect("note detail");
    assert_eq!(detail.value["note"]["verification"], *facts);
    assert!(detail.text().contains(meaning), "{}", detail.text());
    assert_no_raw_ids(&detail, "note detail");

    // Ordinary show names the typed result of the latest note.
    let shown = verbs.show(&work_ref, at(12)).expect("show");
    let note = shown.value["notes"]
        .as_array()
        .expect("notes")
        .iter()
        .find(|note| note["kind"] == "verification")
        .expect("the verification note")
        .clone();
    assert_eq!(note["verification_result"], "indeterminate");
    assert_eq!(note["summary"], summary);
    assert!(
        shown.text().contains("latest verification (indeterminate)"),
        "{}",
        shown.text()
    );
    assert_no_raw_ids(&shown, "ordinary show");

    assert!(
        store_snapshot(&database) == before,
        "the reads changed no row of any table"
    );
}

/// A passing check reads as passed, with no indeterminate explanation.
#[test]
fn a_passing_check_reads_as_passed() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let database = directory.path().join("work.sqlite3");
    let (work_ref, record, _) = verification_note_fixture(
        &database,
        "verification-rows-passed",
        "runner",
        HostCheck {
            key: "passed",
            kind: VerificationKind::Build,
            outcome: ExecutionOutcome::Succeeded,
            result: VerificationResult::Passed,
            summary: "build finished",
        },
    );
    let verbs = AgentVerbs::new(
        database,
        ProjectId("verification-rows-passed".into()),
        "runner".into(),
        SessionId("runner".into()),
        None,
    );
    let detail = verbs
        .show_records(
            &work_ref,
            &ShowInput {
                note: Some(record.as_str().to_owned()),
                ..ShowInput::default()
            },
            at(10),
        )
        .expect("note detail");
    let facts = &detail.value["note"]["verification"];
    assert_eq!(facts["result"], "passed");
    assert_eq!(facts["check_kind"], "build");
    assert_eq!(facts["producer_outcome"], "succeeded");
    assert!(facts.get("meaning").is_none(), "{facts}");
}
