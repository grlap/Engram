#[path = "../src/test_support.rs"]
mod test_support;

use serde_json::Value;
use std::{
    path::Path,
    process::{Command, Output},
};

const MAX_AGENT_WORK_RESPONSE_BYTES: usize = 12 * 1024;

fn command() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_engram"));
    command.env_remove("ENGRAM_SESSION_ID");
    command.env_remove("ENGRAM_ACTOR_ID");
    command.env_remove("ENGRAM_ACTOR_CONTEXT");
    command
}

fn run(home: &Path, args: &[&str]) -> Output {
    command()
        .arg("--home")
        .arg(home)
        .args(args)
        .output()
        .expect("CLI fixture")
}

fn work(home: &Path, session: Option<&str>, args: &[String]) -> Output {
    let mut command = command();
    command
        .arg("--home")
        .arg(home)
        .arg("work")
        .arg("--actor-id")
        .arg("cli-author");
    if let Some(session) = session {
        command.arg("--session-id").arg(session);
    }
    command.args(args).output().expect("work CLI")
}

fn cli_json_stdout(output: &Output) -> Value {
    assert_eq!(
        output.stdout.last().copied(),
        Some(b'\n'),
        "CLI JSON stdout must end with one LF: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let payload = &output.stdout[..output.stdout.len() - 1];
    assert!(
        !payload.contains(&b'\n'),
        "CLI JSON payload must contain no other LF"
    );
    let parsed: Value = serde_json::from_slice(payload).expect("JSON receipt");
    let compact = serde_json::to_vec(&parsed).expect("compact JSON");
    assert_eq!(
        payload.len(),
        compact.len(),
        "CLI JSON stdout minus the trailing LF must equal compact JSON"
    );
    let mut framed = compact;
    framed.push(b'\n');
    assert_eq!(
        output.stdout, framed,
        "CLI JSON stdout must be compact receipt bytes plus one trailing LF"
    );
    assert!(
        output.stdout.len() <= MAX_AGENT_WORK_RESPONSE_BYTES,
        "CLI JSON stdout inclusive of the transport LF must be <= {MAX_AGENT_WORK_RESPONSE_BYTES}"
    );
    parsed
}

fn success_json(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    cli_json_stdout(output)
}

fn compact_len(value: &Value) -> usize {
    serde_json::to_vec(value).expect("compact JSON").len()
}

fn add_args(title: &str, acceptance: usize, under: Option<&str>, optional: bool) -> Vec<String> {
    let mut args = vec!["add".into(), title.into(), "--json".into()];
    for index in 0..acceptance {
        args.push("--accept".into());
        args.push(format!("c{index:03}"));
    }
    if let Some(parent) = under {
        args.push("--under".into());
        args.push(parent.to_owned());
        if optional {
            args.push("--optional".into());
        }
    }
    args
}

#[test]
fn cli_json_stdout_frames_compact_receipt_plus_one_trailing_lf() {
    let directory = test_support::temp_home().expect("temp");
    let home = directory.path();
    assert!(run(home, &["init"]).status.success());

    let defaulted_add = success_json(&work(
        home,
        None,
        &["add".into(), "Defaulted session".into(), "--json".into()],
    ));
    let defaulted_session = defaulted_add["effective_session_id"]
        .as_str()
        .expect("process-default mutation must emit effective_session_id through run_work")
        .to_owned();
    assert!(defaulted_session.starts_with("local-process-v1-"));
    assert!(compact_len(&defaulted_add) < MAX_AGENT_WORK_RESPONSE_BYTES);

    let explicit_add = success_json(&work(
        home,
        Some("explicit-session"),
        &["add".into(), "Explicit session".into(), "--json".into()],
    ));
    assert!(explicit_add.get("effective_session_id").is_none());

    let next = success_json(&work(
        home,
        None,
        &["next".into(), "--peek".into(), "--json".into()],
    ));
    assert!(next.get("effective_session_id").is_none());

    let shown = success_json(&work(
        home,
        None,
        &[
            "show".into(),
            explicit_add["work"]["short_ref"]
                .as_str()
                .expect("explicit ref")
                .to_owned(),
            "--json".into(),
        ],
    ));
    assert!(shown.get("effective_session_id").is_none());

    let owed_parent = success_json(&work(
        home,
        Some("explicit-session"),
        &add_args("Owed parent", 1, None, false),
    ));
    let owed_ref = owed_parent["work"]["short_ref"]
        .as_str()
        .expect("owed ref")
        .to_owned();
    success_json(&work(
        home,
        Some("explicit-session"),
        &add_args("Required child", 0, Some(&owed_ref), false),
    ));
    success_json(&work(
        home,
        Some("explicit-session"),
        &["claim".into(), owed_ref.clone(), "--json".into()],
    ));
    success_json(&work(
        home,
        Some("explicit-session"),
        &[
            "note".into(),
            owed_ref.clone(),
            "cannot seal yet".into(),
            "--json".into(),
        ],
    ));
    let owed = work(
        home,
        Some("explicit-session"),
        &[
            "done".into(),
            owed_ref,
            "still owed".into(),
            "--json".into(),
        ],
    );
    assert_eq!(
        owed.status.code(),
        Some(2),
        "{}",
        String::from_utf8_lossy(&owed.stderr)
    );
    let owed_receipt = cli_json_stdout(&owed);
    assert!(owed_receipt.get("effective_session_id").is_none());
}
