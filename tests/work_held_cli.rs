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
        "title": format!("Held target {key}"),
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
fn core_held_lists_the_sessions_claims_with_their_bindings() {
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

    // Hosts pass the global --json after the core command, as with inspect.
    let held = success(&core(home, "holder", &["held", "--json"]));
    assert_eq!(held["total"], 1);
    assert_eq!(held["omitted"], 0);
    let rows = held["items"].as_array().expect("items");
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["short_ref"], claimed.as_str());
    assert_eq!(rows[0]["control_binding"], binding);
    assert_eq!(rows[0]["claim_id"], binding["claim_id"]);
    assert_eq!(rows[0]["claim_fence"], binding["claim_fence"]);
    assert_eq!(rows[0]["focused"], false);
    assert!(rows[0]["claimed_at"].is_string());
    assert!(rows[0]["expires_at"].is_string());

    // The holder's focus stayed where the agent left it.
    let next = success(&core(home, "holder", &["next", "--sections", "focus"]));
    assert_eq!(
        next["focus"]["status"]["work"]["short_ref"],
        elsewhere.as_str()
    );

    // A session holding nothing gets an empty list and explicit nulls.
    let peer = success(&core(home, "peer", &["held"]));
    assert_eq!(peer["items"], json!([]));
    assert_eq!(
        (peer["total"].clone(), peer["omitted"].clone()),
        (json!(0), json!(0))
    );
    assert_eq!(
        peer.as_object()
            .expect("held JSON object")
            .get("focused_work_id"),
        Some(&Value::Null)
    );
}
