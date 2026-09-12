use super::{
    AuthorityCommand, Command, ControlAssuranceArg, ControlPolicyCommand, CoreWorkCommand,
    GraphCommand, ImportCommand, WorkCommand, command_resolves_host_path_identity,
};
use clap::Parser;
use std::path::PathBuf;

fn work(operation: WorkCommand) -> Command {
    Command::Work {
        actor_id: None,
        session_id: None,
        actor_context: None,
        source_skill: None,
        json: false,
        operation: Box::new(operation),
    }
}

fn next(peek: bool) -> WorkCommand {
    WorkCommand::Next {
        peek,
        limit: 20,
        verbose: false,
        context_generation: None,
    }
}

fn core_next() -> WorkCommand {
    WorkCommand::Core {
        operation: Box::new(CoreWorkCommand::Next {
            limit: 20,
            acknowledge_through: None,
            acknowledge_token: None,
            sections: Vec::new(),
            search: None,
            lifecycles: Vec::new(),
            availabilities: Vec::new(),
            blocked_only: false,
            assigned_to: None,
            label: None,
            catalog_after: None,
            context_generation: None,
        }),
    }
}

#[test]
fn host_path_identity_resolution_is_exhaustive_over_command_variants() {
    let rows = [
        (
            "init",
            true,
            Command::Init {
                required_assurance: None,
                authorized_by: None,
                reason: None,
            },
        ),
        (
            "doctor",
            true,
            Command::Doctor {
                json: false,
                recover_policy: false,
                repair_projections: false,
            },
        ),
        (
            "control",
            true,
            Command::Control {
                actor_id: "host".into(),
                session_id: "session".into(),
                source_skill: None,
            },
        ),
        (
            "authority",
            true,
            Command::Authority {
                operation: AuthorityCommand::WaiveObligation {
                    obligation_id: "ob".into(),
                    expected_definition: "def".into(),
                    waived_by: "host".into(),
                    reason: "reason".into(),
                    idempotency_key: "key".into(),
                },
            },
        ),
        (
            "control-policy",
            true,
            Command::ControlPolicy {
                operation: ControlPolicyCommand::SetRequiredAssurance {
                    level: ControlAssuranceArg::Advisory,
                    authorized_by: "host".into(),
                    reason: "reason".into(),
                    idempotency_key: "key".into(),
                    expected_policy_hash: None,
                },
            },
        ),
        (
            "migration",
            false,
            Command::Migration {
                operation: crate::bin_support::migration::MigrationCommand::Verify {
                    archive: PathBuf::from("archive"),
                },
            },
        ),
        (
            "import",
            false,
            Command::Import {
                actor_id: None,
                session_id: None,
                actor_context: None,
                operation: ImportCommand::Lookup {
                    adapter: "file".into(),
                    reference: "ref".into(),
                },
            },
        ),
        ("work next", false, work(next(false))),
        ("work next peek", false, work(next(true))),
        (
            "work ls",
            false,
            work(WorkCommand::Ls {
                search: None,
                blocked: false,
                ready: false,
                mine: false,
                all: false,
                label: None,
                under: None,
                optional: false,
                required: false,
                after: None,
                limit: 20,
                verbose: false,
            }),
        ),
        ("work core next", false, work(core_next())),
        (
            "mcp",
            false,
            Command::Mcp {
                actor_id: "agent".into(),
                session_id: "session".into(),
                actor_context: None,
                source_skill: None,
            },
        ),
        (
            "graph",
            false,
            Command::Graph {
                actor_id: None,
                session_id: None,
                actor_context: None,
                source_skill: None,
                operation: GraphCommand::Save {
                    out: None,
                    stdout: true,
                    include_restricted: false,
                    reason: None,
                },
            },
        ),
        ("backup", false, Command::Backup { out: None }),
        (
            "restore",
            false,
            Command::Restore {
                from: PathBuf::from("backup.db"),
                replace: false,
            },
        ),
    ];
    for (label, expected, command) in rows {
        assert_eq!(
            command_resolves_host_path_identity(&command),
            expected,
            "{label}"
        );
    }
}

#[test]
fn host_path_policy_help_names_path_bearing_host_commands() {
    let error = super::Cli::try_parse_from(["engram", "--help"]).unwrap_err();
    let help = error.to_string();
    assert!(help.contains("init, doctor, control"), "{help}");
    assert!(help.contains("do not probe"), "{help}");
}
