use super::*;

fn policy(args: &[&str]) -> Option<bool> {
    let matches = Cli::command().try_get_matches_from(args).unwrap();
    let cli = Cli::from_arg_matches(&matches).unwrap();
    cli_warning_output(&matches, cli.warn_slow)
}

#[test]
fn json_output_never_enables_slow_warning_text() {
    for args in [
        vec!["engram", "source", "--root", "fixture", "--json"],
        vec!["engram", "task-sync", "status", "--json"],
        vec!["engram", "--warn-slow", "task-sync", "status", "--json"],
        vec!["engram", "task-sync", "status", "--json", "--warn-slow"],
        vec!["engram", "--warn-slow", "work", "--json", "add", "fixture"],
        vec!["engram", "--warn-slow", "doctor", "--json"],
        vec!["engram", "--warn-slow", "readiness", "--json"],
        vec!["engram", "--warn-slow", "backup", "status", "--json"],
    ] {
        assert_eq!(policy(&args), Some(false), "{args:?}");
    }
}

#[test]
fn human_warning_is_explicit_and_text_is_not_an_output_flag() {
    assert_eq!(policy(&["engram", "task-sync", "status"]), Some(false));
    assert_eq!(
        policy(&["engram", "task-sync", "status", "--warn-slow"]),
        Some(true)
    );
    assert_eq!(
        policy(&["engram", "--warn-slow", "work", "add", "--", "--json"]),
        Some(true)
    );
}

#[test]
fn long_lived_transports_keep_their_warning_policy() {
    assert_eq!(
        policy(&[
            "engram",
            "mcp",
            "--actor-id",
            "fixture",
            "--session-id",
            "fixture",
        ]),
        None
    );
    assert_eq!(
        policy(&[
            "engram",
            "control",
            "--work-session",
            "--actor-id",
            "fixture",
            "--session-id",
            "fixture",
        ]),
        None
    );
}
