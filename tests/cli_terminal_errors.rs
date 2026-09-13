#[path = "../src/test_support.rs"]
mod test_support;

use std::{
    fs,
    path::Path,
    process::{Command, Output},
};

use serde_json::{Value, json};

const HOSTILE_PATH: &str =
    "miss\u{0001}\u{009b}\u{202e}\u{034f}\u{fe0f}\n\t\rerror:\nnext:\n  forged.json";
const HOSTILE_REF: &str = "w-ff\u{0001}\u{009b}\u{202e}\u{034f}\u{fe0f}ff";
const HOSTILE_TITLE: &str = "persist\u{0001}\u{202e}\u{034f}\u{fe0f}title";
const HOSTILE_SCALARS: &[char] = &[
    '\u{0001}', '\u{009b}', '\u{202e}', '\u{034f}', '\u{fe0f}', '\t', '\r',
];

fn engram() -> Command {
    Command::new(env!("CARGO_BIN_EXE_engram"))
}

fn run(home: &Path, args: &[&str]) -> Output {
    engram()
        .arg("--home")
        .arg(home)
        .args(args)
        .output()
        .expect("run engram")
}

fn work(home: &Path, args: &[&str]) -> Output {
    let mut full = vec![
        "work",
        "--actor-id",
        "cli-error",
        "--session-id",
        "cli-error-session",
    ];
    full.extend_from_slice(args);
    run(home, &full)
}

fn init(home: &Path) {
    let output = run(home, &["init"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn cwd_project_id() -> String {
    fs::read_to_string(".engram-project")
        .expect("read cwd project file")
        .trim()
        .to_owned()
}

fn catalog_total(home: &Path) -> i64 {
    let output = work(home, &["ls", "--json"]);
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let value: Value = serde_json::from_slice(&output.stdout).unwrap();
    value["total"].as_i64().expect("ls total")
}

fn stderr_text(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn is_forbidden_on_rendered_line(character: char) -> bool {
    let scalar = character as u32;
    (scalar <= 0x1f || (0x7f..=0x9f).contains(&scalar))
        || matches!(
            character,
            '\u{00ad}'
                | '\u{034f}'
                | '\u{061c}'
                | '\u{180e}'
                | '\u{2028}'
                | '\u{2029}'
                | '\u{feff}'
                | '\u{200b}'..='\u{200f}'
                | '\u{202a}'..='\u{202e}'
                | '\u{2060}'..='\u{206f}'
                | '\u{fe00}'..='\u{fe0f}'
        )
}

fn assert_rendered_stderr_lines(stderr: &str) {
    for line in stderr.lines() {
        assert!(
            !line.chars().any(is_forbidden_on_rendered_line),
            "forbidden scalar on stderr line: {line:?}"
        );
        for scalar in HOSTILE_SCALARS {
            assert!(!line.contains(*scalar), "raw {scalar:?} on {line:?}");
        }
    }
}

fn assert_failure_exit_1(output: &Output) {
    assert_eq!(output.status.code(), Some(1), "{output:?}");
}

fn missing_ref_reason(work_ref: &str) -> String {
    format!(
        "work reference {work_ref:?} does not exist in project {:?}",
        cwd_project_id()
    )
}

#[test]
fn clap_help_parser_and_custom_version_stay_outside_the_error_renderer() {
    let help = engram().arg("--help").output().unwrap();
    assert!(help.status.success(), "{}", stderr_text(&help));
    let stdout = String::from_utf8_lossy(&help.stdout);
    assert!(stdout.contains("Usage:"), "{stdout}");
    assert!(
        stdout.lines().count() > 1,
        "help must remain multi-line: {stdout}"
    );

    let parser = engram()
        .args(["work", "ls", "--not-a-real-flag"])
        .output()
        .unwrap();
    assert!(!parser.status.success());
    let stderr = stderr_text(&parser);
    assert!(
        stderr.contains("unexpected") || stderr.contains("Usage:"),
        "{stderr}"
    );
    assert!(
        stderr.lines().count() > 1,
        "clap parser diagnostics must not go through the single-line error renderer: {stderr}"
    );

    // Custom DisplayVersion branch prints build identity; it is not clap's
    // version writer and not the error renderer.
    let version = engram().arg("--version").output().unwrap();
    assert!(version.status.success(), "{}", stderr_text(&version));
    let version_text = String::from_utf8_lossy(&version.stdout);
    assert!(version_text.starts_with("engram "), "{version_text:?}");
    assert!(
        !String::from_utf8_lossy(&version.stderr).contains("error: "),
        "{}",
        stderr_text(&version)
    );
}

#[test]
fn generic_missing_hostile_input_path_is_framed_and_exits_1() {
    let temp = test_support::temp_home().unwrap();
    let home = temp.path();
    init(home);
    let total_before = catalog_total(home);
    let input = format!("@{HOSTILE_PATH}");
    let output = work(home, &["core", "propose", "--input", &input]);
    assert_failure_exit_1(&output);
    let stderr = stderr_text(&output);
    assert_rendered_stderr_lines(&stderr);
    assert_eq!(
        stderr.lines().count(),
        2,
        "own renderer lines are exactly open-context then IO cause: {stderr:?}"
    );
    let error_lines: Vec<&str> = stderr
        .lines()
        .filter(|line| line.starts_with("error: "))
        .collect();
    assert_eq!(
        error_lines.len(),
        2,
        "own lines are open-context then IO cause: {stderr}"
    );
    let raw_outer = format!("failed to open work propose JSON input {HOSTILE_PATH}");
    assert!(
        error_lines[0].starts_with("error: failed to open work propose JSON input "),
        "outer cause first: {stderr}"
    );
    assert!(
        error_lines[0].contains("miss") && error_lines[0].contains("forged.json"),
        "{stderr}"
    );
    assert_eq!(
        error_lines[0],
        format!("error: {}", engram::terminal_error_line(&raw_outer)),
        "outer cause must be the exact framed open-context Display"
    );
    let io_oracle = fs::File::open(HOSTILE_PATH)
        .expect_err("hostile path must not exist")
        .to_string();
    assert!(
        error_lines[1].starts_with("error: "),
        "inner cause second: {stderr}"
    );
    if io_oracle.chars().any(is_forbidden_on_rendered_line) {
        assert!(
            error_lines[1].contains("os error") || error_lines[1].contains("cannot"),
            "inner cause must still be the IO display after framing: {} vs {io_oracle}",
            error_lines[1]
        );
        assert_ne!(
            error_lines[1],
            format!("error: {io_oracle}"),
            "inner IO Display carried forbidden scalars; the renderer must not reprint them raw"
        );
    } else {
        let collapsed = io_oracle.split_whitespace().collect::<Vec<_>>().join(" ");
        assert_eq!(
            error_lines[1],
            format!("error: {collapsed}"),
            "inner cause must be the File::open Display oracle, not another engram run"
        );
    }
    assert!(!stderr.contains("Usage:"));
    assert_eq!(
        catalog_total(home),
        total_before,
        "bounded catalog total after a generic open refusal"
    );
}

#[test]
fn typed_hostile_ref_text_and_json_use_a_contract_oracle() {
    let temp = test_support::temp_home().unwrap();
    let home = temp.path();
    init(home);
    let total_before = catalog_total(home);
    let reason = missing_ref_reason(HOSTILE_REF);
    let message = format!("local work input is invalid: {reason}");

    let text = work(home, &["show", HOSTILE_REF]);
    assert_failure_exit_1(&text);
    let stderr = stderr_text(&text);
    let lines: Vec<&str> = stderr.lines().collect();
    assert!(lines[0].starts_with("error: "), "{stderr}");
    assert!(lines[0].contains("does not exist"), "{stderr}");
    assert!(
        !lines[0].contains(HOSTILE_REF),
        "text renderer must not reprint the raw hostile ref; Debug-escaped refs are not the discriminator: {stderr}"
    );
    assert_rendered_stderr_lines(&stderr);
    assert!(stderr.contains("next:\n  engram work ls"), "{stderr}");

    let json = work(home, &["show", "--json", HOSTILE_REF]);
    assert_failure_exit_1(&json);
    let parsed: Value = serde_json::from_slice(&json.stderr).unwrap();
    let expected = json!({
        "error": {
            "code": "work_invalid",
            "message": message,
            "details": {
                "reason": reason,
                "remedy": "run next, then show the affected item and follow next",
            },
            "reminders": ["no such item"],
            "next": ["engram work ls"],
        }
    });
    assert_eq!(parsed, expected);
    assert_eq!(catalog_total(home), total_before);
}

#[test]
fn json_show_preserves_exact_persisted_hostile_title() {
    let temp = test_support::temp_home().unwrap();
    let home = temp.path();
    init(home);
    let total_before = catalog_total(home);
    let added = work(home, &["add", HOSTILE_TITLE, "--json"]);
    assert!(
        added.status.success(),
        "{}",
        String::from_utf8_lossy(&added.stderr)
    );
    let created: Value = serde_json::from_slice(&added.stdout).unwrap();
    assert_eq!(created["work"]["title"].as_str().unwrap(), HOSTILE_TITLE);
    let work_ref = created["work"]["short_ref"].as_str().expect("short_ref");
    let shown = work(home, &["show", work_ref, "--json"]);
    assert!(
        shown.status.success(),
        "{}",
        String::from_utf8_lossy(&shown.stderr)
    );
    let view: Value = serde_json::from_slice(&shown.stdout).unwrap();
    assert_eq!(
        view["status"]["work"]["title"].as_str().unwrap(),
        HOSTILE_TITLE
    );
    assert_eq!(catalog_total(home), total_before + 1);
}
