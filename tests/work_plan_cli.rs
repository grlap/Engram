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
        .expect("CLI fixture")
}

fn success(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("JSON receipt")
}

fn propose(home: &Path, input: &str) -> Output {
    run(
        home,
        &[
            "work",
            "--actor-id",
            "plan-author",
            "--session-id",
            "plan-session",
            "core",
            "propose",
            "--input",
            input,
        ],
    )
}

#[test]
fn atomic_plan_cli_creates_whole_graph_and_replays_the_complete_mapping() {
    let directory = test_support::temp_home().expect("temp");
    let home = directory.path();
    assert!(run(home, &["init"]).status.success());
    let input = json!({"kind":"plan", "plan": {
        "idempotency_key":"cli-plan",
        "tasks":[
            {"key":"root","title":"Root","outcome":"Whole result","acceptance":["whole"]},
            {"key":"child","parent_key":"root","title":"Child","outcome":"Part","acceptance":["part"]},
            {"key":"leaf","parent_key":"child","title":"Leaf","outcome":"Detail","acceptance":["detail"]},
            {"key":"prerequisite","title":"Prerequisite","outcome":"Ready","acceptance":["ready"]}
        ],
        "prerequisites":[{"work_key":"leaf","prerequisite":{"kind":"local","value":"prerequisite"}}]
    }});
    let path = home.join("plan.json");
    fs::write(&path, serde_json::to_vec_pretty(&input).unwrap()).unwrap();
    let argument = format!("@{}", path.display());
    let receipt = success(&propose(home, &argument));
    assert_eq!(receipt["kind"], "plan");
    let rows = receipt["tasks"].as_array().expect("mapping");
    assert_eq!(rows.len(), 4);
    let keys: Vec<_> = rows
        .iter()
        .map(|row| row["key"].as_str().unwrap())
        .collect();
    assert_eq!(keys, ["root", "child", "leaf", "prerequisite"]);
    assert_eq!(success(&propose(home, &argument)), receipt);
    let leaf = success(&run(
        home,
        &[
            "work",
            "show",
            rows[2]["short_ref"].as_str().unwrap(),
            "--json",
        ],
    ));
    assert!(
        serde_json::to_string(&leaf)
            .unwrap()
            .contains(rows[3]["short_ref"].as_str().unwrap())
    );
    let listing = || success(&run(home, &["work", "ls", "--all", "--json"]));
    let before = listing();
    let mut conflict = input.clone();
    conflict["plan"]["tasks"][0]["outcome"] = json!("Different intent");
    let refused = propose(home, &conflict.to_string());
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("idempotency"));
    assert_eq!(listing(), before);
    let mut invalid = input;
    invalid["plan"]["idempotency_key"] = json!("invalid-graph");
    invalid["plan"]["tasks"][0]["parent_key"] = json!("leaf");
    let refused = propose(home, &invalid.to_string());
    assert!(!refused.status.success());
    assert!(String::from_utf8_lossy(&refused.stderr).contains("cycle"));
    assert_eq!(listing(), before);
    let doctor = run(home, &["doctor"]);
    assert!(
        doctor.status.success(),
        "{}",
        String::from_utf8_lossy(&doctor.stderr)
    );
}
