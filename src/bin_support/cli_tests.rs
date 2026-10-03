//! CLI-surface unit tests: clap command construction, version/help output,
//! agent-word receipt classification, and flag parsing for the `work`
//! subcommands.

#[test]
fn authority_waiver_accepts_record_ids_and_reports_invalid_definition_ids() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("waiver-record-ids.db");
    let obligation_id = uuid::Uuid::new_v4();
    let waive = |expected_definition: String| {
        super::run_authority(
            &database,
            None,
            super::AuthorityCommand::WaiveObligation {
                obligation_id: obligation_id.to_string(),
                expected_definition,
                waived_by: "operator".into(),
                reason: "record-id parser regression".into(),
                idempotency_key: "record-id-parser".into(),
            },
        )
        .unwrap_err()
    };
    for valid in [
        engram::ObjectId::mint().to_string(),
        format!("{}{}", engram::ObjectId::mint(), engram::ObjectId::mint()),
    ] {
        let error = waive(valid);
        assert!(
            matches!(error.downcast_ref::<engram::StoreError>(),
                Some(engram::StoreError::InvalidWork(message))
                    if message == &format!("work obligation {obligation_id} does not exist")),
            "valid record id must reach obligation lookup: {error}"
        );
    }
    for invalid in [
        "A".repeat(32),
        "g".repeat(64),
        "a".repeat(31),
        String::new(),
    ] {
        assert_eq!(
            waive(invalid).to_string(),
            "invalid definition id: expected a lowercase hex record id"
        );
    }
}

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

#[test]
fn record_id_help_preserves_existing_cli_flags() {
    use clap::Parser;

    for (word, field, terminology) in [
        ("show", "--note", "record-id prefix"),
        ("evaluate", "--evidence", "full record id"),
    ] {
        let help = super::Cli::try_parse_from(["engram", "work", word, "--help"])
            .unwrap_err()
            .to_string();
        assert!(help.contains(field), "{help}");
        assert!(help.contains(terminology), "{help}");
        assert!(!help.contains("RECORD_HASH") && !help.contains("full hash"));
    }
    let help = super::Cli::try_parse_from([
        "engram",
        "control-policy",
        "set-required-assurance",
        "--help",
    ])
    .unwrap_err()
    .to_string();
    assert!(help.contains("--expected-policy-hash"), "{help}");
    assert!(!help.contains("--expected-policy-id"), "{help}");
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
    let help = Cli::try_parse_from(["engram", "graph", "save", "--help"])
        .unwrap_err()
        .to_string();
    assert!(help.contains("Secret-ref bodies are always carried verbatim"));
    assert!(help.contains("never dereferenced or validated as references"));
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
    // The phrase's escaped spaces are part of the representation the budget
    // measures: one occurrence adds ten bytes.
    let phrase =
        serde_json::json!({ "a": format!("{}database is locked", "x".repeat(limit - 36)) });
    let emitted = serialize_agent_receipt(&phrase).expect("serialize agent receipt");
    assert_eq!(emitted.len(), limit);
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&emitted).unwrap(),
        phrase
    );
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

#[test]
fn work_cli_memories_takes_the_context_generation_a_peek_prints() {
    let parsed = Cli::try_parse_from([
        "engram",
        "work",
        "memories",
        "--context-generation",
        "termal-7",
    ])
    .expect("parse the memories context generation");
    assert!(matches!(
        parsed.command,
        Command::Work { operation, .. }
            if matches!(
                *operation,
                WorkCommand::Memories { ref context_generation, ref query, .. }
                    if context_generation.as_deref() == Some("termal-7") && query.is_none()
            )
    ));
}

#[test]
fn work_cli_show_takes_the_observations_window_alone() {
    let parsed = Cli::try_parse_from([
        "engram",
        "work",
        "show",
        "w-000000000001",
        "--observations",
        "--after",
        "o1-00",
    ])
    .expect("parse the observations window and its continuation");
    assert!(matches!(
        parsed.command,
        Command::Work { operation, .. }
            if matches!(
                *operation,
                WorkCommand::Show { observations: true, ref after, .. } if after.as_deref() == Some("o1-00")
            )
    ));
    for other in [
        "--notes",
        "--history",
        "--full",
        "--evaluations",
        "--note=0123456789abcdef",
        "--evaluation=0123456789abcdef0123456789abcdef",
    ] {
        assert!(
            Cli::try_parse_from([
                "engram",
                "work",
                "show",
                "w-000000000001",
                "--observations",
                other,
            ])
            .is_err(),
            "{other}"
        );
    }
}

// A receipt, such as done's refusal naming the source observation that decided
// a stale evaluation, can carry host-recorded text. Written as the CLI writes
// it, it never spells the locked-store phrase, and every string still decodes
// to what was recorded. A recovery refusal on stderr is guarded the same way.
#[test]
fn receipts_and_recovery_refusals_never_spell_the_locked_store_phrase() {
    let value = serde_json::json!({
        "code": "acceptance_evaluation_stale",
        "recovery": {
            "deciding_observation": {
                "revision": "Database Is Locked",
                "workspace": "C:/work/database is locked",
            },
        },
    });
    let written = super::serialize_agent_receipt(&value).unwrap();
    assert!(
        !written.to_lowercase().contains("database is locked"),
        "{written}"
    );
    let decoded: serde_json::Value = serde_json::from_str(&written).unwrap();
    assert_eq!(decoded, value);

    let error = engram::storage::StoreError::WorkCompletionRecoveryRequired {
        work: engram::WorkId::new(),
        cause: engram::WorkCompletionRecoveryCause::AcceptanceEvaluationStale {
            reason: engram::AcceptanceStaleReason::Mutation,
        },
        context: Box::default(),
    };
    let text = serde_json::to_string_pretty(&value).unwrap();
    let guarded = super::refusal_stderr_text(&error, text);
    assert!(
        !guarded.to_lowercase().contains("database is locked"),
        "{guarded}"
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&guarded).unwrap(),
        value
    );

    // A bound check's refusal names the record that decided a stale check,
    // whose host-recorded text may spell the phrase too.
    let record = engram::ObjectId::from_canonical_bytes(b"a bound check");
    let bound = engram::storage::StoreError::WorkBoundVerificationRefused {
        work: engram::WorkId::new(),
        reason: "criterion 1 requires test verification".into(),
        cause: Box::new(engram::domain::WorkBoundVerificationCause {
            criterion: 1,
            requirement: engram::domain::VerificationRequirement {
                check_kind: engram::domain::VerificationKind::Test,
                check_fingerprint: None,
            },
            mismatch: engram::domain::VerificationEvidenceMismatch::StaleSourceRevision,
            verification: record.clone(),
            satisfied_by: record.clone(),
            producer_observation: record,
            result: engram::domain::VerificationResult::Passed,
            remedy: engram::domain::BoundVerificationRemedy::RunCurrentCheck,
            stale_source: Some(engram::domain::StaleVerificationSource {
                decider: engram::domain::StaleSourceDecider::LatestChange,
                position: 7,
                source_changed: Some(true),
                workspace: Some("C:/work/database is locked".into()),
                revision: Some("Database Is Locked".into()),
                root_generation: None,
                verification_workspace: "C:/work/DATABASE IS LOCKED".into(),
                verification_revision: "R2".into(),
            }),
        }),
    };
    let value = engram::store_error_value(&bound);
    let guarded = super::refusal_stderr_text(&bound, serde_json::to_string_pretty(&value).unwrap());
    assert!(
        !guarded.to_lowercase().contains("database is locked"),
        "{guarded}"
    );
    assert_eq!(
        serde_json::from_str::<serde_json::Value>(&guarded).unwrap(),
        value
    );
}

// Every refusal family that can carry host-recorded text is guarded on
// stderr, also those the core CLI path never reaches; any other error is
// written as it is, so a real lock error still reads as one.
#[test]
fn the_refusal_guard_covers_each_family_carrying_host_text_and_nothing_else() {
    let value = serde_json::json!({
        "error": {
            "details": {
                "deciding_observation": {
                    "revision": "Database Is Locked",
                    "workspace": "C:/work/database is locked",
                },
            },
        },
    });
    let text = serde_json::to_string_pretty(&value).unwrap();
    let work = engram::WorkId::new();
    let cause: engram::domain::AcceptanceEvaluationAdmissionCause =
        serde_json::from_value(serde_json::json!({
            "kind": "eligibility",
            "mismatch": "mode_disallowed",
            "requested_mode": "same_session",
            "task_mark": null,
            "admitted_modes": ["independent_session"],
            "remedy": "request_eligible_evaluation",
        }))
        .unwrap();
    let guarded = [
        engram::storage::StoreError::AcceptanceEvaluationBasisMoved {
            work,
            moved: engram::storage::EvaluationBasisMove::SourceChanged,
            reason: "the source changed after the evidence basis".into(),
            observation: None,
        },
        engram::storage::StoreError::AcceptanceEvaluationAdmissionRefused {
            work,
            reason: "mode same_session is not allowed by the project policy".into(),
            cause: Box::new(cause),
        },
    ];
    for error in &guarded {
        let written = super::refusal_stderr_text(error, text.clone());
        assert!(
            !written.to_lowercase().contains("database is locked"),
            "{error:?}: {written}"
        );
        assert_eq!(
            serde_json::from_str::<serde_json::Value>(&written).unwrap(),
            value,
            "{error:?}"
        );
    }
    let other = engram::storage::StoreError::InvalidWork("database is locked".into());
    assert_eq!(super::refusal_stderr_text(&other, text.clone()), text);
}
