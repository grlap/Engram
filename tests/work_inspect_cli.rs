#[path = "../src/test_support.rs"]
mod test_support;

use serde_json::{Value, json};
use std::{
    path::Path,
    process::{Command, Output},
};

fn core(home: &Path, session: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_engram"))
        .arg("--home")
        .arg(home)
        .args([
            "work",
            "--actor-id",
            "host-test",
            "--session-id",
            session,
            "core",
        ])
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
    serde_json::from_slice(&output.stdout).expect("JSON output")
}

fn propose_root(home: &Path, key: &str) -> String {
    let input = json!({
        "kind": "root",
        "title": format!("Inspect target {key}"),
        "outcome": format!("Outcome {key}"),
        "acceptance": [format!("criterion {key}")],
        "idempotency_key": key,
    });
    let receipt = success(&core(
        home,
        "holder",
        &["propose", "--input", &input.to_string()],
    ));
    receipt["work"]["short_ref"]
        .as_str()
        .expect("root short ref")
        .to_owned()
}

#[test]
fn core_inspect_returns_the_holders_binding_without_moving_focus() {
    let directory = test_support::temp_home().expect("temp");
    let home = directory.path();
    let init = Command::new(env!("CARGO_BIN_EXE_engram"))
        .arg("--home")
        .arg(home)
        .arg("init")
        .output()
        .expect("init");
    assert!(
        init.status.success(),
        "{}",
        String::from_utf8_lossy(&init.stderr)
    );

    let claimed = propose_root(home, "claimed");
    let elsewhere = propose_root(home, "elsewhere");
    success(&core(home, "holder", &["focus", &claimed]));
    let claim = json!({"kind": "claim", "ttl_seconds": 300, "idempotency_key": "claim"});
    let receipt = success(&core(
        home,
        "holder",
        &["update", "--input", &claim.to_string()],
    ));
    let binding = receipt["receipt"]["control_binding"].clone();
    assert!(binding.is_object(), "claim receipt carries the binding");
    success(&core(home, "holder", &["focus", &elsewhere]));

    let inspected = success(&core(home, "holder", &["inspect", &claimed]));
    assert_eq!(inspected["status"]["work"]["short_ref"], claimed.as_str());
    assert_eq!(inspected["control_binding"], binding);

    // The holder's focus stayed where the agent left it.
    let next = success(&core(home, "holder", &["next", "--sections", "focus"]));
    assert_eq!(
        next["focus"]["status"]["work"]["short_ref"],
        elsewhere.as_str()
    );

    // Another session reads the item and gets an explicit null binding, not
    // a missing key.
    let peer = success(&core(home, "peer", &["inspect", &claimed]));
    assert_eq!(peer["status"]["work"]["short_ref"], claimed.as_str());
    assert_eq!(
        peer.as_object()
            .expect("inspect JSON object")
            .get("control_binding"),
        Some(&Value::Null)
    );
}
