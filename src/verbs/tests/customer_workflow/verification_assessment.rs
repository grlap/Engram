//! A verification record's detail shows its reconstructed obligation
//! assessment a page at a time; ordinary reads carry no such list.

use super::verification_rows::store_snapshot;
use super::*;
use crate::storage::assessed_verification_fixture;

const LABEL: &str = "under the current matching rules";

fn after_token(command: &str) -> String {
    command
        .split_once(" --after ")
        .map(|(_, token)| token.to_owned())
        .expect("a continuation command")
}

/// Ten candidates: the detail shows eight with exact counts and a
/// continuation, which shows the other two; the reads change nothing, name no
/// raw id, and a continuation refuses once the run has moved.
#[test]
fn ten_candidate_obligations_page_eight_then_two_and_refuse_when_stale() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let database = directory.path().join("work.sqlite3");
    let project = "verification-assessment";
    let (work_ref, records) = assessed_verification_fixture(&database, project, "runner", 9, 2);
    let record = &records[0];
    let verbs = AgentVerbs::new(
        database.clone(),
        ProjectId(project.into()),
        "runner".into(),
        SessionId("runner".into()),
        None,
    );
    let before = store_snapshot(&database);
    let detail_input = |after: Option<String>| ShowInput {
        note: Some(record.as_str().to_owned()),
        after,
        ..ShowInput::default()
    };

    let first = verbs
        .show_records(&work_ref, &detail_input(None), at(100))
        .expect("note detail");
    let block = &first.value["note"]["assessment"];
    assert_eq!(block["total"], 10, "{block}");
    assert_eq!(block["shown"], 8);
    assert_eq!(block["earlier"], 0);
    assert_eq!(block["omitted"], 2);
    let label = block["label"].as_str().expect("label");
    assert!(
        label.starts_with("reconstructed at record position ") && label.ends_with(LABEL),
        "{label}"
    );
    let rows = block["rows"].as_array().expect("rows");
    assert_eq!(rows.len(), 8);
    for row in rows {
        assert_eq!(row["status"], "matches", "{row}");
        assert_eq!(row["recorded"], "satisfied_by_this_record", "{row}");
        assert_eq!(row["check_kind"], "test");
        assert!(row.get("obligation_id").is_none(), "no raw ids: {row}");
    }
    assert_eq!(
        rows.iter().filter(|row| row["criterion"] == 1).count(),
        1,
        "the bound criterion's rule is one of the candidates"
    );
    let continuation = block["continuation"].as_str().expect("continuation");
    assert!(
        continuation.starts_with(&format!(
            "engram work show {work_ref} --note {} --after ",
            record.as_str()
        )),
        "{continuation}"
    );
    let text = first.text();
    assert!(text.contains(continuation), "{text}");
    // The continuation names the record's full id, so it stays on the block.
    assert!(
        first
            .next
            .iter()
            .chain(&first.reminders)
            .all(|command| !command.contains(record.as_str())),
        "{:?}",
        first.next
    );
    assert!(text.contains(label), "{text}");
    assert!(
        text.contains(
            "10 obligations of check kind test on the run; 8 shown, 0 earlier, 2 omitted"
        ),
        "{text}"
    );
    assert!(text.contains("matches at that position; recorded: satisfied by this record"));

    let second = verbs
        .show_records(
            &work_ref,
            &detail_input(Some(after_token(continuation))),
            at(101),
        )
        .expect("assessment continuation");
    let block = &second.value["assessment"];
    assert_eq!(block["total"], 10, "{block}");
    assert_eq!(block["shown"], 2);
    assert_eq!(block["earlier"], 8);
    assert_eq!(block["omitted"], 8);
    assert!(block.get("continuation").is_none());
    assert_eq!(block["label"], label);
    assert!(
        second.value.get("note").is_none(),
        "a continuation carries the assessment alone"
    );

    // Ordinary show and next carry no assessment list.
    let shown = verbs.show(&work_ref, at(102)).expect("show");
    let next = verbs
        .next(
            &NextInput {
                peek: true,
                ..NextInput::default()
            },
            at(103),
        )
        .expect("next");
    let notes = verbs
        .show_records(
            &work_ref,
            &ShowInput {
                notes: true,
                ..ShowInput::default()
            },
            at(104),
        )
        .expect("notes window");
    for receipt in [&shown, &next, &notes] {
        assert!(!receipt.text().contains(LABEL), "{}", receipt.text());
        assert!(!receipt.value.to_string().contains("\"assessment\""));
    }

    assert!(
        store_snapshot(&database) == before,
        "the reads changed no row of any table"
    );

    // A continuation belongs to its record, and a token that does not decode
    // is refused; each refusal points at a fresh detail read.
    let refusal = |note: &str, after: &str, expected: &str| {
        let refused = verbs
            .show_records(
                &work_ref,
                &ShowInput {
                    note: Some(note.to_owned()),
                    after: Some(after.to_owned()),
                    ..ShowInput::default()
                },
                at(105),
            )
            .expect_err("a refused continuation");
        assert!(refused.to_string().contains(expected), "{refused}");
        assert_eq!(
            refused.guidance().next,
            vec![format!("engram work show '{work_ref}' --note '{note}'")],
            "a refusal starts again from the detail"
        );
    };
    refusal(
        records[1].as_str(),
        &after_token(continuation),
        "continuation belongs to another item, project or record",
    );
    refusal(record.as_str(), "v1-garbage", "invalid assessment cursor");

    // The run moves on: the continuation refuses rather than page a
    // different list.
    {
        let mut store = crate::storage::SqliteStore::open(&database).expect("store");
        let work = store
            .resolve_work_ref(&ProjectId(project.into()), &work_ref)
            .expect("work");
        store.append_source_change_fixture(work.work_id, "later", at(50), "R-later");
    }
    let refused = verbs
        .show_records(
            &work_ref,
            &detail_input(Some(after_token(continuation))),
            at(106),
        )
        .expect_err("a stale continuation refuses");
    assert!(
        refused
            .to_string()
            .contains("the run changed since this page was read"),
        "{refused}"
    );
}

/// `--after` continues only the assessment of the record it was issued for:
/// a real continuation of a verification record is refused on a plain note,
/// and the refusal points at that note's detail.
#[test]
fn after_refuses_on_a_note_without_an_assessment() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let database = directory.path().join("work.sqlite3");
    let project = "verification-assessment-note";
    let (work_ref, records) = assessed_verification_fixture(&database, project, "runner", 9, 1);
    let verbs = AgentVerbs::new(
        database,
        ProjectId(project.into()),
        "runner".into(),
        SessionId("runner".into()),
        None,
    );
    verbs
        .note(
            &NoteInput {
                work_ref: Some(work_ref.clone()),
                text: "a plain note".into(),
                status: false,
                refs: Vec::new(),
            },
            at(100),
        )
        .expect("note");
    let window = verbs
        .show_records(
            &work_ref,
            &ShowInput {
                notes: true,
                ..ShowInput::default()
            },
            at(101),
        )
        .expect("notes window");
    let plain = window.value["notes"]
        .as_array()
        .expect("rows")
        .iter()
        .find(|row| row["kind"] != "verification" && row["summary"] == "a plain note")
        .expect("the plain note")["locator"]
        .as_str()
        .expect("locator")
        .to_owned();
    let detail = verbs
        .show_records(
            &work_ref,
            &ShowInput {
                note: Some(plain.clone()),
                ..ShowInput::default()
            },
            at(102),
        )
        .expect("plain detail");
    assert!(detail.value["note"].get("assessment").is_none());
    let continuation = verbs
        .show_records(
            &work_ref,
            &ShowInput {
                note: Some(records[0].as_str().to_owned()),
                ..ShowInput::default()
            },
            at(103),
        )
        .expect("verification detail")
        .value["note"]["assessment"]["continuation"]
        .as_str()
        .map(after_token)
        .expect("a continuation of the verification record");
    let refused = verbs
        .show_records(
            &work_ref,
            &ShowInput {
                note: Some(plain.clone()),
                after: Some(continuation),
                ..ShowInput::default()
            },
            at(104),
        )
        .expect_err("after on a plain note refuses");
    assert!(
        refused
            .to_string()
            .contains("continuation belongs to another item, project or record"),
        "{refused}"
    );
    assert_eq!(
        refused.guidance().next,
        vec![format!("engram work show '{work_ref}' --note '{plain}'")]
    );
}
