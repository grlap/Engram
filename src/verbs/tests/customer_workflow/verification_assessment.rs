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

/// A host's real case over the agent words: a flagged change at R1, then a passed
/// test of R2 the host never reported as a change. The check's detail names
/// the change it must follow beside its own source, and done's refusal names
/// the same check, why it does not satisfy the bound criterion, the deciding
/// change, and the command that reads the check's detail; the cause's own
/// words are unchanged.
#[test]
fn a_stale_check_names_the_change_it_must_follow_in_show_and_done() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let database = directory.path().join("work.sqlite3");
    let project = "unreported-move";
    let (work_ref, check) = crate::storage::unreported_move_fixture(&database, project, "runner");
    let verbs = AgentVerbs::new(
        database,
        ProjectId(project.into()),
        "runner".into(),
        SessionId("runner".into()),
        None,
    );
    let detail = verbs
        .show_records(
            &work_ref,
            &ShowInput {
                note: Some(check.as_str().to_owned()),
                ..ShowInput::default()
            },
            at(100),
        )
        .expect("note detail");
    let rows = detail.value["note"]["assessment"]["rows"]
        .as_array()
        .expect("rows");
    let stale = rows
        .iter()
        .filter(|row| row["mismatch"] == "stale_source_revision")
        .collect::<Vec<_>>();
    assert_eq!(stale.len(), 2, "{rows:?}");
    for row in &stale {
        let source = &row["stale_source"];
        assert_eq!(source["decider"], "latest_change", "{row}");
        assert_eq!(source["workspace"], "workspace-old");
        assert_eq!(source["revision"], "R1");
        assert_eq!(source["source_changed"], true);
        assert_eq!(source["verification_workspace"], "workspace-new");
        assert_eq!(source["verification_revision"], "R2");
        assert!(source["position"].is_i64(), "{row}");
    }
    let text = detail.text();
    assert!(
        text.contains("decided by the run's latest source change at run position ")
            && text.contains(
                "a change, workspace workspace-old, revision R1; this check ran on revision R2 in workspace workspace-new"
            ),
        "{text}"
    );

    let refused = verbs
        .done(
            DoneInput {
                work_ref: Some(work_ref.clone()),
                summary: Some("delivered".into()),
                ..DoneInput::default()
            },
            // Inside the fixture's claim, which the store's own clock set.
            chrono::Utc
                .with_ymd_and_hms(2026, 8, 27, 1, 0, 10)
                .single()
                .expect("fixture time"),
        )
        .expect("a refusal is a receipt");
    let check_line = refused
        .reminders
        .iter()
        .find(|reminder| {
            reminder.starts_with("the newest passed test check after that obligation opened")
        })
        .unwrap_or_else(|| panic!("{:?}", refused.reminders));
    assert!(
        check_line.contains(&format!("({}, at run position ", check.as_str()))
            && check_line.contains("does not match it: stale source revision; decided by the run's latest source change")
            && check_line.contains("revision R1; this check ran on revision R2 in workspace workspace-new"),
        "{check_line}"
    );
    // The cause's own words come first, unchanged.
    assert!(
        refused
            .reminders
            .iter()
            .any(|reminder| reminder
                .starts_with(&format!("{work_ref} still owes Test for obligation "))),
        "{:?}",
        refused.reminders
    );
    let pointer = format!("engram work show {work_ref} --note {}", check.as_str());
    assert!(refused.next.contains(&pointer), "{:?}", refused.next);
    let value = serde_json::to_string(&refused.value).expect("JSON");
    assert!(
        value.contains("\"open_obligation_check\":{")
            && value.contains("\"decider\":\"latest_change\""),
        "{value}"
    );
}

/// The terminal line naming a deciding record escapes and bounds host text.
#[test]
fn the_deciding_record_line_is_one_terminal_safe_line() {
    use crate::domain::{StaleSourceDecider, StaleVerificationSource};
    let hostile = format!("ws\u{1b}[31m\nnext line{}", "y".repeat(300));
    for decider in [
        StaleSourceDecider::LatestChange,
        StaleSourceDecider::RootSighting,
        StaleSourceDecider::RootBinding,
    ] {
        let line =
            crate::verbs::verification_assessment::stale_source_line(&StaleVerificationSource {
                decider,
                position: 7,
                source_changed: Some(false),
                workspace: Some(hostile.clone()),
                revision: Some(hostile.clone()),
                root_generation: Some(3),
                verification_workspace: hostile.clone(),
                verification_revision: "R2".into(),
            });
        assert!(!line.contains(['\n', '\u{1b}']), "{line:?}");
        assert!(line.contains("bytes stored)"), "{line}");
        assert!(line.len() < 1_500, "{}", line.len());
    }
}
