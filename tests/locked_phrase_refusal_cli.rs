//! A host retries a command whose output says "database is locked" as if the
//! store were locked. Host-recorded text can spell that phrase, so the real
//! binary must never write it in a refusal that carries such text: neither
//! `work evaluate --json` on stderr nor the core completion refusal on stdout.
//! Every string still decodes to exactly what the host recorded.

#[path = "../src/test_support.rs"]
mod test_support;

use std::{
    fmt::Write as _,
    io::{BufRead, BufReader, Write},
    path::Path,
    process::{Child, ChildStdin, ChildStdout, Command, Output, Stdio},
};

use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const PHRASE: &str = "database is locked";
const LOCKED_WORKSPACE: &str = "C:/work/Database Is Locked";
const RUNNER: &str = "locked-runner";
const JUDGE: &str = "locked-judge";

fn engram(home: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_engram"));
    command.arg("--home").arg(home);
    command
}

fn work(home: &Path, session: &str, args: &[&str]) -> Output {
    engram(home)
        .args(["work", "--actor-id", session, "--session-id", session])
        .args(args)
        .output()
        .expect("run engram work")
}

fn core(home: &Path, session: &str, operation: &str, input: &Value) -> Output {
    work(
        home,
        session,
        &["core", operation, "--input", &input.to_string()],
    )
}

fn succeeded(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("JSON stdout")
}

fn fingerprint(value: &str) -> String {
    Sha256::digest(value.as_bytes())
        .iter()
        .fold(String::new(), |mut hex, byte| {
            let _ = write!(hex, "{byte:02x}");
            hex
        })
}

/// Whether any object in `value` holds `expected` under `key`.
fn holds(value: &Value, key: &str, expected: &str) -> bool {
    match value {
        Value::Object(map) => {
            map.get(key).and_then(Value::as_str) == Some(expected)
                || map.values().any(|child| holds(child, key, expected))
        }
        Value::Array(items) => items.iter().any(|child| holds(child, key, expected)),
        _ => false,
    }
}

/// The host's control channel for one session, one JSON request per line.
struct Control {
    child: Child,
    stdin: Option<ChildStdin>,
    stdout: BufReader<ChildStdout>,
}

impl Control {
    fn spawn(home: &Path, session: &str) -> Self {
        let mut child = engram(home)
            .args(["control", "--actor-id", session, "--session-id", session])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn engram control");
        let stdin = child.stdin.take();
        let stdout = BufReader::new(child.stdout.take().expect("control stdout"));
        Self {
            child,
            stdin,
            stdout,
        }
    }

    fn ok(&mut self, request: &Value) -> Value {
        let stdin = self.stdin.as_mut().expect("open control stdin");
        writeln!(stdin, "{request}").expect("write control request");
        stdin.flush().expect("flush control request");
        let mut line = String::new();
        self.stdout
            .read_line(&mut line)
            .expect("read control response");
        let response: Value = serde_json::from_str(&line).expect("JSON control response");
        assert_eq!(response["status"], "ok", "{request}: {response}");
        response["result"].clone()
    }

    /// One turn: evaluate, begin, then checkpoint with `observations`.
    fn turn(
        &mut self,
        routing_token: &Value,
        name: &str,
        effects: &[&str],
        resources: &[Value],
        observations: &[Value],
    ) {
        let granted = self.ok(&json!({
            "operation": "turn_evaluate",
            "routing_token": routing_token,
            "idempotency_key": format!("{name}-evaluate"),
            "intent_fingerprint": fingerprint(name),
            "purpose": "ordinary",
            "requested_effects": effects,
            "resource_intents": resources,
        }));
        assert_eq!(granted["decision"], "grant", "{granted}");
        let grant = &granted["grant"];
        let tokens: Vec<Value> = grant["delivery"]["page"]["delivery_token"]
            .as_str()
            .map(|token| vec![json!(token)])
            .unwrap_or_default();
        let begun = self.ok(&json!({
            "operation": "turn_begin",
            "routing_token": routing_token,
            "grant_id": grant["grant_id"],
            "delivery_tokens": tokens,
            "idempotency_key": format!("{name}-begin"),
        }));
        assert_eq!(begun["decision"], "begin", "{begun}");
        let checkpointed = self.ok(&json!({
            "operation": "turn_checkpoint",
            "routing_token": routing_token,
            "grant_id": grant["grant_id"],
            "next_intent": "continue",
            "observations": observations,
            "idempotency_key": format!("{name}-checkpoint"),
        }));
        assert_eq!(checkpointed["decision"], "checkpointed", "{checkpointed}");
    }
}

impl Drop for Control {
    fn drop(&mut self) {
        drop(self.stdin.take());
        let _ = self.child.wait();
    }
}

fn evaluate(home: &Path, work_ref: &str, bases: (i64, i64), note: &str, attempt: &str) -> Output {
    work(
        home,
        JUDGE,
        &[
            "evaluate",
            work_ref,
            "--mode",
            "independent_session",
            "--acceptance-basis",
            &bases.0.to_string(),
            "--evidence-basis",
            &bases.1.to_string(),
            "--verdict",
            "1=pass:judgment",
            "--rationale",
            "1=the runner's note shows the outcome",
            "--evidence",
            &format!("1={note}"),
            "--source-fingerprint",
            "content-revision-2",
            "--attempt",
            attempt,
            "--json",
        ],
    )
}

#[test]
fn refusals_carrying_host_recorded_text_never_spell_the_locked_store_phrase() {
    let directory = test_support::temp_home().expect("repository-local home");
    let home = directory.path();
    assert!(engram(home).arg("init").output().unwrap().status.success());
    let policy = engram(home)
        .args([
            "control-policy",
            "set-acceptance-evaluation",
            "--modes",
            "independent-session",
            "--authorized-by",
            "locked-operator",
            "--idempotency-key",
            "locked-policy",
        ])
        .output()
        .unwrap();
    assert!(
        policy.status.success(),
        "{}",
        String::from_utf8_lossy(&policy.stderr)
    );

    let proposed = succeeded(&core(
        home,
        RUNNER,
        "propose",
        &json!({
            "kind": "root",
            "title": "Keep the locked-store phrase out of refusals",
            "outcome": "Refusals decode to the recorded text",
            "acceptance": ["the outcome holds"],
            "work_kind": "chore",
            "idempotency_key": "locked-root",
        }),
    ));
    let work_ref = proposed["work"]["short_ref"].as_str().unwrap().to_owned();
    let project =
        succeeded(&engram(home).args(["doctor", "--json"]).output().unwrap())["project_id"].clone();
    assert!(project.is_string(), "{project}");
    let claimed = succeeded(&core(
        home,
        RUNNER,
        "update",
        &json!({ "kind": "claim", "ttl_seconds": 3600, "idempotency_key": "locked-claim" }),
    ));
    let binding = claimed["receipt"]["control_binding"].clone();
    assert!(binding.is_object(), "{claimed}");
    let noted = succeeded(&work(
        home,
        RUNNER,
        &[
            "note",
            &work_ref,
            "the outcome holds as the runner observed",
            "--json",
        ],
    ));
    let note = noted["evidence"]
        .as_str()
        .unwrap_or_else(|| panic!("note locator: {noted}"))
        .to_owned();

    let mut control = Control::spawn(home, RUNNER);
    let bound = control.ok(&json!({
        "operation": "session_bind",
        "external_ref": "local-work:locked-phrase",
        "title": "Host control for the locked-phrase refusals",
        "assurance": "turn_gated",
        "mediated_effects": ["observe", "mutate_local"],
        "work_binding": binding,
        "capability_map_revision": 1,
        "idempotency_key": "locked-bind",
    }));
    let routing = bound["routing_token"].clone();
    control.turn(&routing, "locked-sync", &["observe"], &[], &[]);

    // The judge evaluates at this cut, declaring the revision it judged.
    let shown = succeeded(&work(home, RUNNER, &["show", &work_ref, "--json"]));
    let bases = (
        shown["acceptance_basis"]
            .as_i64()
            .expect("acceptance basis"),
        shown["evidence_basis"].as_i64().expect("evidence basis"),
    );
    succeeded(&evaluate(home, &work_ref, bases, &note, "locked-first"));

    // The host then sees the source at a revision and in a workspace whose
    // recorded text spells the phrase.
    control.turn(
        &routing,
        "locked-sighting",
        &["mutate_local"],
        &[json!({
            "kind": "path",
            "project_id": project,
            "segments": ["src", "lib.rs"],
            "coverage": "exact",
        })],
        &[json!({
            "observation_id": "locked-sighting",
            "action_fingerprint": fingerprint("locked-sighting"),
            "effect": "mutate_local",
            "outcome": "succeeded",
            "source_changed": false,
            "source_basis": { "workspace_id": LOCKED_WORKSPACE, "source_revision": PHRASE },
            "observed_at": chrono::Utc::now().to_rfc3339(),
        })],
    );
    drop(control);

    // Evaluating again at the old cut is refused because the basis moved;
    // the JSON refusal on stderr names that sighting.
    let moved = evaluate(home, &work_ref, bases, &note, "locked-again");
    assert_eq!(moved.status.code(), Some(1));
    let stderr = String::from_utf8(moved.stderr).expect("UTF-8 stderr");
    assert!(!stderr.to_lowercase().contains(PHRASE), "{stderr}");
    let refusal: Value = serde_json::from_str(&stderr).expect("JSON stderr");
    let deciding = &refusal["error"]["details"]["deciding_observation"];
    assert_eq!(deciding["revision"], PHRASE, "{refusal}");
    assert_eq!(deciding["workspace"], LOCKED_WORKSPACE, "{refusal}");

    // The core completion refusal names it too, on stdout.
    let refused = core(
        home,
        RUNNER,
        "complete",
        &json!({ "idempotency_key": "locked-complete" }),
    );
    assert_eq!(refused.status.code(), Some(1));
    let stdout = String::from_utf8(refused.stdout).expect("UTF-8 stdout");
    assert!(!stdout.to_lowercase().contains(PHRASE), "{stdout}");
    let receipt: Value = serde_json::from_str(&stdout).expect("JSON stdout");
    assert!(holds(&receipt, "revision", PHRASE), "{receipt}");
    assert!(holds(&receipt, "workspace", LOCKED_WORKSPACE), "{receipt}");
}
