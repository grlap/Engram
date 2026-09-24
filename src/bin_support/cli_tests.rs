//! CLI-surface unit tests: clap command construction, version/help output,
//! agent-word receipt classification, and flag parsing for the `work`
//! subcommands.

#[test]
fn cli_command_construction_uses_only_package_version_metadata() {
    use clap::{CommandFactory, Parser};
    assert_eq!(
        super::Cli::command().get_version(),
        Some(env!("CARGO_PKG_VERSION"))
    );
    for flag in ["--version", "-V"] {
        let error = super::Cli::try_parse_from(["engram", flag]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayVersion);
        assert_eq!(
            error.to_string().trim(),
            format!("engram {}", env!("CARGO_PKG_VERSION"))
        );
    }
    // Only run_cli handles DisplayVersion with the extended diagnostic.
    // Constructing clap metadata for help/ordinary words stays cheap.
    let help = super::Cli::try_parse_from(["engram", "--help"]).unwrap_err();
    assert_eq!(help.kind(), clap::error::ErrorKind::DisplayHelp);
    assert!(super::Cli::try_parse_from(["engram", "work", "ls"]).is_ok());
}

use clap::CommandFactory;

use super::*;

#[test]
fn core_next_help_lists_discovery_sections() {
    let error = Cli::try_parse_from(["engram", "work", "core", "next", "--help"]).unwrap_err();
    assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);
    let help = error.to_string();
    assert!(help.contains("assigned,participated"), "{help}");
}

#[test]
fn core_mutation_help_teaches_the_shared_raw_input_ceiling() {
    for operation in ["propose", "update", "complete", "handoff"] {
        let error =
            Cli::try_parse_from(["engram", "work", "core", operation, "--help"]).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::DisplayHelp);
        let help = error.to_string();
        assert!(
            help.contains("2 MiB raw before decoding"),
            "{operation}: {help}"
        );
    }
}

#[test]
fn complete_cli_command_graph_fits_the_configured_parse_stack() {
    std::thread::Builder::new()
        .stack_size(CLI_STACK_BYTES)
        .spawn(|| Cli::command().debug_assert())
        .expect("spawn CLI command-graph test")
        .join()
        .expect("CLI command graph remains valid");
}

#[test]
fn policy_administration_has_no_reason_argument_or_help() {
    let commands = [
        vec![
            "init",
            "--required-assurance",
            "advisory",
            "--authorized-by",
            "operator",
        ],
        vec![
            "control-policy",
            "set-required-assurance",
            "advisory",
            "--authorized-by",
            "operator",
            "--idempotency-key",
            "key",
        ],
        vec![
            "control-policy",
            "set-obligation-rule-set",
            "--input",
            "{}",
            "--authorized-by",
            "operator",
            "--idempotency-key",
            "key",
        ],
        vec![
            "control-policy",
            "set-acceptance-evaluation",
            "--modes",
            "independent-session",
            "--authorized-by",
            "operator",
            "--idempotency-key",
            "key",
        ],
    ];
    for command in commands {
        let mut args = vec!["engram"];
        args.extend(command);
        assert!(Cli::try_parse_from(&args).is_ok(), "{args:?}");
        let mut help_args = args.clone();
        help_args.push("--help");
        let help = Cli::try_parse_from(help_args).unwrap_err();
        assert_eq!(help.kind(), clap::error::ErrorKind::DisplayHelp);
        assert!(!help.to_string().contains("--reason"), "{help}");
        args.extend(["--reason", "removed"]);
        let error = Cli::try_parse_from(args).unwrap_err();
        assert_eq!(error.kind(), clap::error::ErrorKind::UnknownArgument);
    }
}

#[test]
fn graph_save_is_operator_only_and_has_exclusive_destinations() {
    let parsed = Cli::try_parse_from([
        "engram",
        "graph",
        "--actor-id",
        "operator",
        "--session-id",
        "operator-session",
        "save",
        "--stdout",
        "--include-restricted",
        "--reason",
        "incident recovery",
    ])
    .expect("parse graph save");
    let Command::Graph { operation, .. } = parsed.command else {
        panic!("graph save did not parse as the operator graph command");
    };
    assert!(matches!(
        operation,
        GraphCommand::Save {
            stdout: true,
            include_restricted: true,
            reason: Some(ref reason),
            out: None,
        } if reason == "incident recovery"
    ));
    assert!(
        Cli::try_parse_from([
            "engram",
            "graph",
            "save",
            "--stdout",
            "--out",
            "snapshot.json",
        ])
        .is_err()
    );
    assert!(Cli::try_parse_from(["engram", "work", "graph", "save", "--stdout"]).is_err());
    assert!(
        Cli::try_parse_from([
            "engram",
            "graph",
            "save",
            "--stdout",
            "--include-restricted",
        ])
        .is_err()
    );
    assert!(
        Cli::try_parse_from([
            "engram",
            "graph",
            "save",
            "--stdout",
            "--reason",
            "not widening",
        ])
        .is_err()
    );
}

#[test]
fn agent_json_emission_uses_the_response_budget_representation() {
    let limit = engram::work_service::MAX_AGENT_WORK_RESPONSE_BYTES;
    let value = serde_json::json!({ "a": "x".repeat(limit - 8) });
    let emitted = serialize_agent_receipt(&value).expect("serialize agent receipt");
    assert_eq!(emitted.len(), limit);
    assert!(
        serde_json::to_string_pretty(&value)
            .expect("serialize pretty comparison")
            .len()
            > limit
    );
    assert!(!emitted.contains('\n'));
}

#[test]
fn every_agent_word_classifies_its_structured_receipt_exhaustively() {
    let cases: &[(&[&str], bool)] = &[
        (&["next"], false),
        (&["ls"], false),
        (&["show", "w-000000000001"], false),
        (&["add", "new work"], true),
        (&["claim", "w-000000000001"], true),
        (&["update", "--release"], true),
        (&["gate", "cargo-check"], true),
        (
            &[
                "evaluate",
                "w-000000000001",
                "--mode",
                "same-session",
                "--acceptance-basis",
                "1",
                "--evidence-basis",
                "4",
                "--verdict",
                "1=pass:judgment",
                "--rationale",
                "1=verified",
            ],
            true,
        ),
        (&["remember", "project observation"], true),
        (&["memories"], false),
        (&["forget", "project-observation"], true),
        (&["note", "progress"], true),
        (&["done"], true),
        (&["handoff", "--to", "peer-session"], true),
    ];

    let command = Cli::command();
    let work = command
        .find_subcommand("work")
        .expect("work command exists");
    let agent_word_count = work
        .get_subcommands()
        .filter(|subcommand| subcommand.get_name() != "core")
        .count();
    assert_eq!(cases.len(), agent_word_count);

    for &(args, expected) in cases {
        let mut command = vec!["engram", "work"];
        command.extend_from_slice(args);
        let parsed = Cli::try_parse_from(command).expect("parse agent word");
        let Command::Work { operation, .. } = parsed.command else {
            panic!("agent word did not parse as work");
        };
        assert_eq!(
            operation.returns_mutation_receipt(),
            expected,
            "unexpected receipt classification for {args:?}"
        );
    }
}

#[test]
fn work_cli_parses_cut_a_update_flags_and_gate() {
    let prerequisite = Cli::try_parse_from([
        "engram",
        "work",
        "--actor-id",
        "agent",
        "--session-id",
        "session",
        "update",
        "w-000000000001",
        "--after",
        "w-000000000002",
    ])
    .expect("parse prerequisite flag");
    assert!(matches!(
        prerequisite.command,
        Command::Work { operation, .. }
            if matches!(*operation, WorkCommand::Update(ref args) if args.after.is_some())
    ));

    let supersede = Cli::try_parse_from([
        "engram",
        "work",
        "--actor-id",
        "agent",
        "--session-id",
        "session",
        "update",
        "w-000000000001",
        "--supersede-with",
        "w-000000000002",
        "--reason",
        "duplicate",
    ])
    .expect("parse supersession flag");
    assert!(matches!(
        supersede.command,
        Command::Work { operation, .. }
            if matches!(*operation, WorkCommand::Update(ref args) if args.supersede_with.is_some() && args.reason.is_some())
    ));

    let gate = Cli::try_parse_from([
        "engram",
        "work",
        "--actor-id",
        "agent",
        "--session-id",
        "session",
        "gate",
        "--work-ref",
        "w-000000000001",
        "cargo-test",
        "--failed",
        "one::test",
        "--ref",
        "target/test.log",
    ])
    .expect("parse gate word");
    assert!(matches!(
        gate.command,
        Command::Work { operation, .. }
            if matches!(&*operation, WorkCommand::Gate { work_ref: Some(work_ref), failed, evidence_ref: Some(_), .. } if work_ref == "w-000000000001" && failed == &["one::test"])
    ));

    let failures_before_name = Cli::try_parse_from([
        "engram",
        "work",
        "--actor-id",
        "agent",
        "--session-id",
        "session",
        "gate",
        "--failed",
        "cargo fmt --check",
        "--failed",
        "doc-links",
        "quality-gates",
    ])
    .expect("repeatable failure labels do not consume the gate name");
    assert!(matches!(
        failures_before_name.command,
        Command::Work { operation, .. }
            if matches!(
                &*operation,
                WorkCommand::Gate { name, failed, .. }
                    if name == "quality-gates"
                        && failed == &["cargo fmt --check", "doc-links"]
            )
    ));
}

#[test]
fn phoenix_note_cli_accepts_positional_target_and_describes_observations() {
    let parsed = Cli::try_parse_from([
        "engram",
        "work",
        "note",
        "w-000000000001",
        "review finding",
        "--ref",
        "review:detail",
    ])
    .unwrap();
    assert!(matches!(parsed.command, Command::Work { operation, .. }
        if matches!(&*operation, WorkCommand::Note { args, refs, .. }
            if args == &["w-000000000001", "review finding"] && refs == &["review:detail"])));
    let help = Cli::try_parse_from(["engram", "work", "note", "--help"]).unwrap_err();
    assert_eq!(help.kind(), clap::error::ErrorKind::DisplayHelp);
    let text = help.to_string();
    assert!(text.contains("observation without a claim"));
    assert!(text.contains("[REF] TEXT"));
    assert!(!text.contains("--work-ref"));
}

#[test]
fn work_cli_parses_required_child_waiver() {
    let waive = Cli::try_parse_from([
        "engram",
        "work",
        "--actor-id",
        "agent",
        "--session-id",
        "session",
        "update",
        "w-000000000001",
        "--waive",
        "w-000000000002",
        "--reason",
        "explicitly accepted omission",
    ])
    .expect("parse required-child waiver flag");
    assert!(matches!(
        waive.command,
        Command::Work { operation, .. }
            if matches!(*operation, WorkCommand::Update(ref args) if args.waive.is_some() && args.reason.is_some())
    ));
}
