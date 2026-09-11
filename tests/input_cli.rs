#[path = "../src/test_support.rs"]
mod test_support;

use serde_json::{Value, json};
use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

fn run(home: &Path, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_engram"))
        .arg("--home")
        .arg(home)
        .args(args)
        .output()
        .unwrap()
}

fn success(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

fn work(home: &Path, args: &[&str]) -> Output {
    let mut full = vec![
        "work",
        "--actor-id",
        "input-author",
        "--session-id",
        "input-session",
    ];
    full.extend_from_slice(args);
    run(home, &full)
}

#[test]
fn remember_text_flag_round_trips_the_same_key_body_and_revision_chain() {
    let temp = test_support::temp_home().unwrap();
    let home = temp.path();
    assert!(run(home, &["init"]).status.success());
    let body = "Full project note\nwith Unicode: żółw";
    let first = success(&work(
        home,
        &["remember", body, "--key", "input-note", "--json"],
    ));
    let replay = success(&work(
        home,
        &["remember", "--text", body, "--key", "input-note", "--json"],
    ));
    assert_eq!(first["key"], "input-note");
    assert_eq!(replay["key"], first["key"]);
    assert_eq!(replay["revision"], 1);
    assert_eq!(replay["duplicate"], true);
    let read = || success(&work(home, &["memories", "input-note", "--full", "--json"]));
    assert_eq!(read()["body"], body);
    let before = read();
    for args in [
        vec!["remember", "--text", "new", "old", "--key", "input-note"],
        vec!["remember", "old", "--text", "new", "--key", "input-note"],
    ] {
        let error = work(home, &args);
        assert!(!error.status.success());
        let stderr = String::from_utf8_lossy(&error.stderr);
        assert!(stderr.contains("cannot be used with"), "{stderr}");
        assert!(
            stderr.contains("--text") && stderr.contains("TEXT"),
            "{stderr}"
        );
        assert!(!stderr.contains("-- --text"));
    }
    assert!(
        !work(home, &["remember", "--key", "input-note"])
            .status
            .success()
    );
    assert_eq!(read(), before);
    let revised = success(&work(
        home,
        &[
            "remember",
            "--text",
            "Revision body",
            "--key",
            "input-note",
            "--revise",
            "--expected-revision",
            "1",
            "--json",
        ],
    ));
    assert_eq!(revised["revision"], 2);
    assert_eq!(read()["body"], "Revision body");
    assert_eq!(
        success(&work(
            home,
            &[
                "memories",
                "input-note",
                "--full",
                "--revision",
                "1",
                "--json"
            ]
        ))["body"],
        body
    );
    let help = work(home, &["remember", "--help"]);
    assert!(help.status.success());
    assert!(String::from_utf8_lossy(&help.stdout).contains("--text <TEXT>"));
}

fn file_pair(home: &Path, args: &[&str], input: &Value) -> Value {
    let path = home.join("request.json");
    let argument = format!("@{}", path.display());
    let mut full = args.to_vec();
    full.extend(["--input", &argument]);
    let plain = input.to_string();
    fs::write(&path, &plain).unwrap();
    let first = success(&run(home, &full));
    fs::write(&path, format!("\u{feff}{plain}")).unwrap();
    assert_eq!(
        success(&run(home, &full)),
        first,
        "marked replay must retain the original result"
    );
    first
}

#[test]
fn json_file_bom_reaches_all_core_mutation_readers_and_bounded_policy_reader() {
    let temp = test_support::temp_home().unwrap();
    let home = temp.path();
    assert!(run(home, &["init"]).status.success());
    let core = [
        "work",
        "--actor-id",
        "input-author",
        "--session-id",
        "input-session",
        "core",
    ];
    let mut args = core.to_vec();
    args.push("propose");
    let plan = file_pair(
        home,
        &args,
        &json!({"kind":"plan","plan":{
            "idempotency_key":"bom-plan", "tasks":[{"key":"root","title":"Root","outcome":"Delivered","acceptance":["Delivered"]}], "prerequisites":[]
        }}),
    );
    let reference = plan["tasks"][0]["short_ref"].as_str().unwrap();
    for (operation, input) in [
        (
            "update",
            json!({"kind":"claim","idempotency_key":"bom-claim"}),
        ),
        (
            "handoff",
            json!({"kind":"offer","to":"recipient","checkpoint_summary":"Ready for review","idempotency_key":"bom-offer"}),
        ),
        (
            "handoff",
            json!({"kind":"cancel","reason":"Keep execution","idempotency_key":"bom-cancel"}),
        ),
        (
            "complete",
            json!({"capture":{"summary":"Delivered"},"idempotency_key":"bom-complete"}),
        ),
    ] {
        let mut args = core.to_vec();
        args.extend([operation, "--work-ref", reference]);
        file_pair(home, &args, &input);
    }
    let shown = success(&work(home, &["show", reference, "--json"]));
    assert_eq!(shown["status"]["work"]["lifecycle"], "completed");
    file_pair(
        home,
        &[
            "control-policy",
            "set-obligation-rule-set",
            "--authorized-by",
            "input-operator",
            "--reason",
            "Explicit test policy",
            "--idempotency-key",
            "bom-policy",
        ],
        &json!({"schema_version":1,"rules":[]}),
    );
}
