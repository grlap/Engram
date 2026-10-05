//! A verification record's detail shows its reconstructed obligation
//! assessment a page at a time; ordinary reads carry no such list.

use super::verification_rows::store_snapshot;
use super::*;
use crate::storage::{assessed_verification_fixture, closed_then_live_verification_fixture};

const LABEL: &str = "under the current matching rules";

fn after_token(command: &str) -> String {
    command
        .split_once(" --after ")
        .map(|(_, token)| token.to_owned())
        .expect("a continuation command")
}

/// Ten candidates, none closed: the detail's summary counts and shows all ten
/// and names the full history, whose pages show eight with exact counts and a
/// continuation to the other two; the reads change nothing, name no raw id,
/// and a continuation refuses once the run has moved.
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

    let summary = verbs
        .show_records(&work_ref, &detail_input(None), at(100))
        .expect("note detail");
    let block = &summary.value["note"]["assessment"];
    assert_eq!(block["view"], "summary", "{block}");
    assert_eq!(block["total"], 10);
    assert_eq!(block["counts"], json!([{"status": "matches", "count": 10}]));
    assert_eq!(block["must_show_total"], 10);
    assert_eq!(block["must_show"].as_array().expect("must_show").len(), 10);
    assert!(
        block.get("rows").is_none(),
        "rows belong to the history view"
    );
    assert!(block.get("continuation").is_none());
    let history = block["history"].as_str().expect("history command");
    assert!(summary.text().contains(history));

    let first = verbs
        .show_records(
            &work_ref,
            &detail_input(Some(after_token(history))),
            at(100),
        )
        .expect("history first page");
    let block = &first.value["assessment"];
    assert_eq!(block["view"], "history", "{block}");
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
    // The continuations name the record's full id, so they stay on the block.
    for receipt in [&summary, &first] {
        assert!(
            receipt
                .next
                .iter()
                .chain(&receipt.reminders)
                .all(|command| !command.contains(record.as_str())),
            "{:?}",
            receipt.next
        );
    }
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
    refusal(record.as_str(), "v2-garbage", "invalid assessment cursor");
    // A token of the earlier format, whose pages had no summary, refuses.
    let earlier_format = after_token(continuation).replacen("v2-", "v1-", 1);
    refusal(
        record.as_str(),
        &earlier_format,
        "invalid assessment cursor",
    );

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

fn runner_verbs(database: &std::path::Path, project: &str) -> AgentVerbs {
    AgentVerbs::new(
        database.to_path_buf(),
        ProjectId(project.into()),
        "runner".into(),
        SessionId("runner".into()),
        None,
    )
}

fn note_detail(
    verbs: &AgentVerbs,
    work_ref: &str,
    record: &ObjectId,
    after: Option<String>,
    second: i64,
) -> Result<Receipt, VerbError> {
    verbs.show_records(
        work_ref,
        &ShowInput {
            note: Some(record.as_str().to_owned()),
            after,
            ..ShowInput::default()
        },
        at(second),
    )
}

fn is_already_closed(row: &Value) -> bool {
    row["status"] == "left_out" && row["left_out"] == "already_closed"
}

/// The measured shape: many candidates already closed at the record, with
/// the two that matter last in trigger order. The first read counts every
/// candidate and shows those two in full, with no continuation and no closed
/// row, in a fraction of the bytes the exhaustive pages took.
#[test]
fn a_record_with_many_closed_candidates_shows_its_live_ones_first() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let database = directory.path().join("work.sqlite3");
    let project = "assessment-measured";
    let (work_ref, [_, record]) =
        closed_then_live_verification_fixture(&database, project, "runner", 46, 2, "R45");
    let verbs = runner_verbs(&database, project);
    let detail = note_detail(&verbs, &work_ref, &record, None, 100).expect("note detail");
    let block = &detail.value["note"]["assessment"];
    assert_eq!(block["view"], "summary", "{block}");
    assert_eq!(block["total"], 49, "{block}");
    let counts = block["counts"].as_array().expect("counts");
    assert!(
        counts.contains(&json!({"status": "left_out", "reason": "already_closed", "count": 47})),
        "{block}"
    );
    assert_eq!(
        counts
            .iter()
            .map(|count| count["count"].as_u64().expect("count"))
            .sum::<u64>(),
        49,
        "the counts cover every candidate"
    );
    assert_eq!(block["must_show_total"], 2, "{block}");
    assert_eq!(block["must_show_earlier"], 0);
    assert_eq!(block["must_show_remaining"], 0);
    assert!(block.get("continuation").is_none(), "{block}");
    assert!(
        block.get("rows").is_none(),
        "rows belong to the history view"
    );
    let shown = block["must_show"].as_array().expect("must_show");
    assert_eq!(shown.len(), 2, "{block}");
    assert!(shown.iter().all(|row| !is_already_closed(row)), "{block}");
    assert!(
        shown[0]["trigger_position"].as_u64() < shown[1]["trigger_position"].as_u64(),
        "trigger order: {block}"
    );
    assert!(
        block["record_position"].as_u64().is_some() && block["cut_position"].as_u64().is_some(),
        "{block}"
    );
    let bytes = serde_json::to_vec(block).expect("json").len();
    assert!(bytes < 3 * 1024, "the summary stays small: {bytes} bytes");

    for row in shown {
        assert_eq!(
            (&row["status"], &row["mismatch"], &row["recorded"]),
            (
                &json!("mismatch"),
                &json!("stale_source_revision"),
                &json!("open")
            ),
            "{row}"
        );
    }

    let text = detail.text();
    assert!(
        text.contains(&format!(
            "49 obligations of check kind test on the run at cut position {} (reconstructed at record position {}",
            block["cut_position"], block["record_position"]
        )),
        "{text}"
    );
    assert!(
        text.contains("47 left out (already closed), 2 do not match (stale source revision)"),
        "{text}"
    );
    assert!(
        text.contains("2 to act on (matching, mismatching or still open), shown in full:"),
        "{text}"
    );
    assert!(
        !text.contains("left out before matching: already closed"),
        "{text}"
    );
    assert!(!text.contains("more must-show rows"), "{text}");
    let history = block["history"].as_str().expect("history command");
    assert!(text.contains(&format!("full history: {history}")), "{text}");

    // The full history at the same cut still lists every candidate, the
    // closed ones first.
    let first =
        note_detail(&verbs, &work_ref, &record, Some(after_token(history)), 101).expect("history");
    let page = &first.value["assessment"];
    assert_eq!(page["view"], "history", "{page}");
    assert_eq!(page["total"], 49);
    assert_eq!(page["cut_position"], block["cut_position"]);
    assert!(
        page["rows"]
            .as_array()
            .expect("rows")
            .iter()
            .all(is_already_closed),
        "{page}"
    );
}

/// The summary shows in full only what a reader must act on. Later changes
/// that a later check satisfied are left out as not yet defined at the
/// first check, with their obligations ended by another record: they appear
/// only in the counts. A later change still open is shown, though it too is
/// left out before matching. The history still lists every candidate.
#[test]
fn ended_left_out_rows_are_counted_and_open_ones_shown() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let database = directory.path().join("work.sqlite3");
    let project = "assessment-act-on";
    let (work_ref, record) = crate::storage::later_ended_and_open_verification_fixture(
        &database, project, "runner", 2, 3,
    );
    let verbs = runner_verbs(&database, project);
    let detail = note_detail(&verbs, &work_ref, &record, None, 100).expect("note detail");
    let block = &detail.value["note"]["assessment"];
    assert_eq!(block["view"], "summary", "{block}");
    let total = block["total"].as_u64().expect("total");
    let counts = block["counts"].as_array().expect("counts");
    assert_eq!(
        counts
            .iter()
            .map(|count| count["count"].as_u64().expect("count"))
            .sum::<u64>(),
        total,
        "the counts cover every candidate: {block}"
    );
    assert!(
        counts.contains(&json!({"status": "left_out", "reason": "not_yet_defined", "count": 4})),
        "three ended later changes and the open one are not yet defined: {block}"
    );
    let shown = block["must_show"].as_array().expect("must_show");
    assert_eq!(block["must_show_total"], json!(shown.len()), "{block}");
    // Every shown row is one to act on.
    for row in shown {
        assert!(
            row["status"] != "left_out" || row["recorded"] == "open",
            "a left-out row is shown only while its obligation is open: {row}"
        );
    }
    // The open later change is shown; the three ended ones are not.
    let left_out_shown = shown
        .iter()
        .filter(|row| row["status"] == "left_out")
        .collect::<Vec<_>>();
    assert_eq!(left_out_shown.len(), 1, "{block}");
    assert_eq!(left_out_shown[0]["recorded"], "open");
    assert_eq!(left_out_shown[0]["left_out"], "not_yet_defined");
    assert_eq!(
        u64::try_from(shown.len()).expect("small") + 3,
        total,
        "only the three ended left-out rows are counted without being shown: {block}"
    );

    // The full history at the same cut still lists every candidate.
    let history = block["history"].as_str().expect("history command");
    let mut listed = 0;
    let mut after = Some(after_token(history));
    while let Some(token) = after {
        let page = note_detail(&verbs, &work_ref, &record, Some(token), 101).expect("history");
        listed += page.value["assessment"]["shown"].as_u64().expect("shown");
        after = page.value["assessment"]["continuation"]
            .as_str()
            .map(after_token);
    }
    assert_eq!(listed, total, "the history lists every candidate");
}

/// Every candidate already closed: the summary is the counts alone, says no
/// row needs showing, and still names the full history, which pages the
/// closed rows; once the run moves, that history refuses.
#[test]
fn an_all_closed_record_shows_counts_and_the_history_command() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let database = directory.path().join("work.sqlite3");
    let project = "assessment-all-closed";
    let (work_ref, records) = assessed_verification_fixture(&database, project, "runner", 9, 2);
    let record = &records[1];
    let verbs = runner_verbs(&database, project);
    let detail = note_detail(&verbs, &work_ref, record, None, 100).expect("note detail");
    let block = &detail.value["note"]["assessment"];
    assert_eq!(block["view"], "summary", "{block}");
    assert_eq!(
        block["counts"],
        json!([{"status": "left_out", "reason": "already_closed", "count": 10}]),
        "{block}"
    );
    assert_eq!(block["must_show_total"], 0);
    assert_eq!(block["must_show"], json!([]));
    assert!(block.get("continuation").is_none(), "{block}");
    let text = detail.text();
    assert!(
        text.contains(
            "none matches, mismatches or is still open at this record; the counts cover every one"
        ),
        "{text}"
    );
    let history = block["history"]
        .as_str()
        .expect("history command")
        .to_owned();
    let page =
        note_detail(&verbs, &work_ref, record, Some(after_token(&history)), 101).expect("history");
    let page = &page.value["assessment"];
    assert_eq!(
        (page["shown"].as_u64(), page["omitted"].as_u64()),
        (Some(8), Some(2))
    );
    assert!(
        page["rows"]
            .as_array()
            .expect("rows")
            .iter()
            .all(is_already_closed),
        "{page}"
    );

    {
        let mut store = crate::storage::SqliteStore::open(&database).expect("store");
        let work = store
            .resolve_work_ref(&ProjectId(project.into()), &work_ref)
            .expect("work");
        store.append_source_change_fixture(work.work_id, "later", at(50), "R-later");
    }
    let refused = note_detail(&verbs, &work_ref, record, Some(after_token(&history)), 102)
        .expect_err("a stale history refuses");
    assert!(
        refused
            .to_string()
            .contains("the run changed since this page was read"),
        "{refused}"
    );
}

/// More must-show rows than the budget holds: each summary page shows whole
/// rows within it, says how many remain and gives the continuation, and the
/// continuations reach every must-show row in trigger order without a closed
/// row; every page names the same full history.
#[test]
fn must_show_rows_over_the_budget_page_before_any_closed_row() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let database = directory.path().join("work.sqlite3");
    let project = "assessment-over-budget";
    let (work_ref, [_, record]) =
        closed_then_live_verification_fixture(&database, project, "runner", 5, 60, "L59");
    let verbs = runner_verbs(&database, project);
    let mut after = None;
    let mut seen = Vec::new();
    let mut history = None;
    let mut pages: i64 = 0;
    loop {
        let receipt =
            note_detail(&verbs, &work_ref, &record, after.clone(), 100 + pages).expect("page");
        let block = if after.is_none() {
            receipt.value["note"]["assessment"].clone()
        } else {
            assert!(receipt.value.get("note").is_none());
            receipt.value["assessment"].clone()
        };
        pages += 1;
        assert_eq!(block["view"], "summary", "{block}");
        assert_eq!(block["total"], 66, "{block}");
        assert_eq!(block["must_show_total"], 60);
        assert_eq!(block["must_show_earlier"], json!(seen.len()));
        let rows = block["must_show"].as_array().expect("must_show").clone();
        assert!(!rows.is_empty(), "{block}");
        let row_bytes: usize = rows
            .iter()
            .map(|row| serde_json::to_vec(row).expect("json").len() + 1)
            .sum();
        assert!(
            row_bytes <= crate::verbs::verification_assessment::MUST_SHOW_BUDGET,
            "{row_bytes}"
        );
        assert!(rows.iter().all(|row| !is_already_closed(row)), "{block}");
        let command = block["history"].as_str().expect("history").to_owned();
        assert_eq!(*history.get_or_insert_with(|| command.clone()), command);
        seen.extend(rows);
        let remaining = 60 - seen.len();
        assert_eq!(block["must_show_remaining"], json!(remaining));
        let text = receipt.text();
        let Some(continuation) = block.get("continuation").and_then(Value::as_str) else {
            assert_eq!(remaining, 0, "{block}");
            assert!(!text.contains("more must-show rows"), "{text}");
            break;
        };
        assert!(remaining > 0);
        assert!(
            text.contains(&format!(
                "more must-show rows: {remaining}; next: {continuation}"
            )),
            "{text}"
        );
        after = Some(after_token(continuation));
    }
    assert!(pages > 1, "sixty rows exceed one page");
    let positions = seen
        .iter()
        .map(|row| row["trigger_position"].as_u64().expect("position"))
        .collect::<Vec<_>>();
    assert!(
        positions.windows(2).all(|pair| pair[0] < pair[1]),
        "trigger order, no repeats: {positions:?}"
    );

    // A summary continuation refuses once the run moves, like the history.
    let first = note_detail(&verbs, &work_ref, &record, None, 200).expect("first page");
    let continuation = first.value["note"]["assessment"]["continuation"]
        .as_str()
        .map(after_token)
        .expect("continuation");
    {
        let mut store = crate::storage::SqliteStore::open(&database).expect("store");
        let work = store
            .resolve_work_ref(&ProjectId(project.into()), &work_ref)
            .expect("work");
        store.append_source_change_fixture(work.work_id, "later", at(150), "R-later");
    }
    let refused = note_detail(&verbs, &work_ref, &record, Some(continuation), 201)
        .expect_err("a stale summary continuation refuses");
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
        .value["note"]["assessment"]["history"]
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
    let rows = detail.value["note"]["assessment"]["must_show"]
        .as_array()
        .expect("must-show rows");
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
