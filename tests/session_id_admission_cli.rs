#[path = "../src/test_support.rs"]
mod test_support;

use serde_json::Value;
use std::{
    path::Path,
    process::{Command, Output},
};

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

fn work(home: &Path, session: &str, args: &[&str]) -> Output {
    command()
        .arg("--home")
        .arg(home)
        .args(["work", "--actor-id", "cli-author", "--session-id", session])
        .args(args)
        .output()
        .expect("work CLI")
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn home_has_a_store(home: &Path) -> bool {
    fn walk(path: &Path) -> std::io::Result<bool> {
        for entry in std::fs::read_dir(path)? {
            let entry = entry?;
            let child = entry.path();
            if child.is_dir() {
                if walk(&child)? {
                    return Ok(true);
                }
                continue;
            }
            if entry.file_name() == "engram.db"
                || child
                    .extension()
                    .is_some_and(|ext| ext == "db" || ext == "sqlite3")
            {
                return Ok(true);
            }
        }
        Ok(false)
    }
    walk(home).expect("fixture home must be inspectable")
}

#[test]
fn work_cli_oversized_session_reports_a_bounded_error_without_echoing_the_id() {
    let directory = test_support::temp_home().expect("temp");
    let home = directory.path();
    assert!(run(home, &["init"]).status.success());
    let giant = "s".repeat(65);
    let output = work(home, &giant, &["next", "--peek", "--json"]);
    assert!(!output.status.success(), "{}", stderr_text(&output));
    let stderr = stderr_text(&output);
    assert!(stderr.contains("session id exceeds 64 UTF-8 bytes"));
    assert!(!stderr.contains(&giant));
}

#[test]
fn work_cli_oversized_session_does_not_create_a_store_on_an_empty_home() {
    let directory = test_support::temp_home().expect("temp");
    let home = directory.path();
    assert!(!home_has_a_store(home), "fixture home must start empty");
    let giant = "s".repeat(65);
    let output = work(home, &giant, &["next", "--peek", "--json"]);
    assert!(!output.status.success(), "{}", stderr_text(&output));
    let stderr = stderr_text(&output);
    assert!(stderr.contains("session id exceeds 64 UTF-8 bytes"));
    assert!(!stderr.contains(&giant));
    assert!(
        !home_has_a_store(home),
        "oversized session must not create engram.db under an empty home"
    );
}

#[test]
fn work_cli_help_teaches_the_64_byte_session_bound() {
    let help = command().args(["work", "--help"]).output().expect("help");
    assert!(help.status.success(), "{}", stderr_text(&help));
    let text = format!(
        "{}{}",
        String::from_utf8_lossy(&help.stdout),
        stderr_text(&help)
    );
    assert!(
        text.contains("64 UTF-8"),
        "work --help must name the live session bound:\n{text}"
    );
    let handoff = command()
        .args(["work", "handoff", "--help"])
        .output()
        .expect("handoff help");
    assert!(handoff.status.success(), "{}", stderr_text(&handoff));
    let handoff_text = format!(
        "{}{}",
        String::from_utf8_lossy(&handoff.stdout),
        stderr_text(&handoff)
    );
    assert!(
        handoff_text.contains("64 UTF-8"),
        "handoff --help must name the recipient bound:\n{handoff_text}"
    );
}

#[test]
fn work_cli_preserves_an_exact_64_byte_session() {
    let directory = test_support::temp_home().expect("temp");
    let home = directory.path();
    assert!(run(home, &["init"]).status.success());
    let session = "s".repeat(64);
    let output = work(home, &session, &["next", "--peek", "--verbose", "--json"]);
    assert!(output.status.success(), "{}", stderr_text(&output));
    let value: Value = serde_json::from_slice(&output.stdout).expect("JSON");
    assert_eq!(value["session"]["session_id"], session);
}

#[test]
fn work_cli_admits_uuid_and_termal_session_ids() {
    let directory = test_support::temp_home().expect("temp");
    let home = directory.path();
    assert!(run(home, &["init"]).status.success());
    for session in ["01234567-89ab-cdef-0123-456789abcdef", "session-6670"] {
        let output = work(home, session, &["next", "--peek", "--verbose", "--json"]);
        assert!(
            output.status.success(),
            "{session}: {}",
            stderr_text(&output)
        );
        let value: Value = serde_json::from_slice(&output.stdout).expect("JSON");
        assert_eq!(value["session"]["session_id"], session);
    }
}
