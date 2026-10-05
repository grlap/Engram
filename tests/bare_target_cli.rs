//! Through the CLI, a bare `done` or `evaluate` from a session holding more
//! than one live claim is refused and records nothing; the bare `note`,
//! `gate` and `handoff`, the words given an explicit ref, and a bare word
//! with one live claim act as before.

#[path = "../src/test_support.rs"]
mod test_support;

use engram::{ProjectId, project_database_path};
use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

const PROJECT: &str = "bare-target-cli";

fn engram(root: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_engram"))
        .current_dir(root)
        .env_remove("ENGRAM_HOME")
        .arg("--home")
        .arg(root.join("home"))
        .arg("--project-file")
        .arg(root.join(".engram-project"))
        .args(args)
        .output()
        .expect("run engram")
}

/// One agent word from one session, asking for JSON.
fn work(root: &Path, args: &[&str]) -> Output {
    let mut full = vec![
        "work",
        "--actor-id",
        "cli-agent",
        "--session-id",
        "bare-target-cli",
    ];
    full.extend_from_slice(args);
    full.push("--json");
    engram(root, &full)
}

fn acted(output: &Output, word: &str) -> Value {
    assert!(
        output.status.success(),
        "{word}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("JSON receipt")
}

/// A refused word's JSON error, which the CLI prints on standard error.
fn refused(output: &Output, word: &str) -> Value {
    assert_eq!(output.status.code(), Some(1), "{word}: {output:?}");
    let value: Value = serde_json::from_slice(&output.stderr).unwrap_or_else(|error| {
        panic!(
            "{word}: {error}: {}",
            String::from_utf8_lossy(&output.stderr)
        )
    });
    value["error"].clone()
}

fn add(root: &Path, title: &str) -> String {
    let accept = format!("{title} is delivered");
    acted(&work(root, &["add", title, "--accept", &accept]), "add")["work"]["short_ref"]
        .as_str()
        .expect("added ref")
        .to_owned()
}

/// Every row of the work feed and object tables, so a refusal can be shown
/// to record nothing.
fn recorded(root: &Path) -> (i64, i64) {
    let database = project_database_path(&root.join("home"), &ProjectId(PROJECT.into()));
    let connection = rusqlite::Connection::open(database).expect("store");
    let count = |table: &str| {
        connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("count rows")
    };
    (count("work_feed_entries"), count("objects"))
}

/// An evaluation the project's policy does not admit, so it reaches the
/// same admission refusal whichever item it lands on.
fn evaluate(root: &Path, work_ref: Option<&str>) -> Output {
    let mut args = vec!["evaluate"];
    args.extend(work_ref);
    args.extend_from_slice(&[
        "--mode",
        "same_session",
        "--acceptance-basis",
        "1",
        "--evidence-basis",
        "1",
        "--verdict",
        "1=fail:judgment",
        "--rationale",
        "1=not delivered yet",
    ]);
    work(root, &args)
}

#[test]
fn bare_done_and_evaluate_refuse_through_the_cli_while_several_claims_are_live() {
    let directory = test_support::temp_home().expect("temporary directory");
    let root = directory.path();
    fs::write(root.join(".engram-project"), format!("{PROJECT}\n")).expect("project file");
    let init = engram(root, &["init"]);
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    let first = add(root, "First held work");
    let second = add(root, "Second held work");
    for work_ref in [&first, &second] {
        acted(&work(root, &["claim", work_ref]), "claim");
    }
    let mut held = vec![first.clone(), second.clone()];
    held.sort();
    let before = recorded(root);

    // Several live claims: both words are refused, naming every held item.
    for (word, output, command) in [
        (
            "done",
            work(root, &["done", "delivered"]),
            "engram work done {} \"…\"",
        ),
        (
            "evaluate",
            evaluate(root, None),
            "engram work evaluate {} …",
        ),
    ] {
        let error = refused(&output, word);
        assert_eq!(error["code"], "work_bare_target_ambiguous", "{error}");
        assert_eq!(error["details"]["operation"], word, "{error}");
        assert_eq!(error["details"]["focused_ref"], json!(second), "{error}");
        assert_eq!(error["details"]["held_refs"], json!(held), "{error}");
        assert_eq!(error["details"]["more"], 0, "{error}");
        let explicit: Vec<String> = held
            .iter()
            .map(|work_ref| command.replace("{}", work_ref))
            .collect();
        assert_eq!(error["next"], json!(explicit), "{error}");
    }
    assert_eq!(
        recorded(root),
        before,
        "a refused bare word recorded something"
    );

    // Bare note, gate and handoff act on the held focus as before.
    let noted = acted(&work(root, &["note", "a finding on the focus"]), "note");
    assert_eq!(noted["work"]["short_ref"], json!(second), "{noted}");
    let gated = acted(&work(root, &["gate", "unit"]), "gate");
    assert_eq!(gated["work"]["short_ref"], json!(second), "{gated}");
    for args in [
        &["handoff", "--to", "another-session"][..],
        &["handoff", "--cancel", "kept it"][..],
    ] {
        let handed = acted(&work(root, args), "handoff");
        assert!(handed.to_string().contains(&second), "{handed}");
    }

    // Naming the item acts as before: evaluate reaches the admission the
    // project's policy refuses, and done completes the named item.
    let admission = refused(&evaluate(root, Some(&second)), "explicit evaluate");
    assert_eq!(
        admission["code"], "acceptance_evaluation_refused",
        "{admission}"
    );
    let done = acted(
        &work(root, &["done", &second, "delivered the second item"]),
        "explicit done",
    );
    assert_eq!(done["work"]["short_ref"], json!(second), "{done}");

    // One live claim left: bare evaluate gets the same admission refusal,
    // and bare done completes it.
    acted(&work(root, &["claim", &first]), "claim");
    let bare = refused(&evaluate(root, None), "bare evaluate");
    assert_eq!(bare["code"], admission["code"], "{bare}");
    let completed = acted(
        &work(root, &["done", "delivered the first item"]),
        "bare done",
    );
    assert_eq!(completed["work"]["short_ref"], json!(first), "{completed}");
}

/// Five live claims with the focus last in ref order: the CLI refusal names
/// the first two and the focus, counts the other two and offers a command for
/// each named item only.
#[test]
fn a_bare_done_through_the_cli_beside_many_claims_names_three_and_counts_the_rest() {
    let directory = test_support::temp_home().expect("temporary directory");
    let root = directory.path();
    fs::write(root.join(".engram-project"), format!("{PROJECT}\n")).expect("project file");
    let init = engram(root, &["init"]);
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );
    let mut held: Vec<String> = (0..5)
        .map(|index| add(root, &format!("Held work {index}")))
        .collect();
    for work_ref in &held {
        acted(&work(root, &["claim", work_ref]), "claim");
    }
    held.sort();
    let focus = held[4].clone();
    acted(&work(root, &["claim", &focus]), "claim the focus");
    let before = recorded(root);

    let error = refused(&work(root, &["done", "delivered"]), "done");
    assert_eq!(error["code"], "work_bare_target_ambiguous", "{error}");
    let shown = vec![held[0].clone(), held[1].clone(), focus.clone()];
    assert_eq!(error["details"]["held_refs"], json!(shown), "{error}");
    assert_eq!(error["details"]["more"], 2, "{error}");
    assert_eq!(error["details"]["focused_ref"], json!(focus), "{error}");
    let explicit: Vec<String> = shown
        .iter()
        .map(|work_ref| format!("engram work done {work_ref} \"…\""))
        .collect();
    assert_eq!(error["next"], json!(explicit), "{error}");
    let message = error["message"].as_str().expect("message");
    assert!(
        message.contains("holds 5 live claims") && message.contains("and 2 more"),
        "{message}"
    );
    assert_eq!(
        recorded(root),
        before,
        "a refused bare done recorded something"
    );
}
