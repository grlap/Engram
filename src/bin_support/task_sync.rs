//! Thin explicit-directory planning interchange front door.

use super::attribution::resolve_shell_work_attribution;
use anyhow::Result;
use engram::{LocalWorkService, ProjectId, SessionId, task_sync::directory};
use std::path::{Path, PathBuf};

mod setup;

#[derive(Debug, clap::Args)]
pub(crate) struct TaskSyncArgs {
    #[arg(long, env = "ENGRAM_ACTOR_ID", global = true)]
    actor_id: Option<String>,
    #[arg(long, env = "ENGRAM_SESSION_ID", global = true)]
    session_id: Option<String>,
    #[arg(long, env = "ENGRAM_ACTOR_CONTEXT", global = true)]
    actor_context: Option<String>,
    #[arg(long, global = true)]
    source_skill: Option<String>,
    #[command(subcommand)]
    operation: TaskSyncCommand,
}

#[derive(Debug, clap::Subcommand)]
enum TaskSyncCommand {
    /// Observe this project's host-local opt-in without opening SQLite.
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Explicitly opt in; this does not materialize or export shared files.
    Enable {
        #[arg(long)]
        json: bool,
        #[arg(long)]
        user_instruction: Option<String>,
    },
    /// Opt out without removing shared files.
    Disable {
        #[arg(long)]
        json: bool,
    },
    /// Prepare a complete candidate, serializing only changed tasks, with local progress.
    Prepare {
        directory: PathBuf,
        #[arg(long)]
        progress_directory: PathBuf,
        #[arg(long, requires = "origin_machine")]
        include_notes: bool,
        /// Include native activity under this asserted alias, also when notes are off.
        #[arg(long)]
        origin_machine: Option<String>,
        /// Discard a valid completed baseline; existing output is preserved.
        #[arg(long)]
        rebaseline: bool,
    },
    /// Prepare private setup intent and proposed edits; does not enable exchange.
    SetupPrepare {
        repository: PathBuf,
        #[arg(long)]
        branch: String,
        #[arg(long, value_parser = ["public", "private"])]
        visibility: String,
        #[arg(long)]
        readership: String,
        #[arg(long, requires = "origin_machine")]
        include_notes: bool,
        #[arg(long)]
        origin_machine: Option<String>,
    },
    /// Write deterministic project.json and tasks/<uuid>.json into an explicit directory.
    Export {
        directory: PathBuf,
        /// Include eligible generic/status notes and report omissions.
        #[arg(long, requires = "origin_machine")]
        include_notes: bool,
        /// Include native activity under this asserted alias, also when notes are off.
        /// Imported notes and activity retain their original aliases.
        #[arg(long)]
        origin_machine: Option<String>,
    },
    /// Import a complete candidate into an empty task graph; exact repeats are no-ops.
    Import {
        directory: PathBuf,
        #[arg(long)]
        dry_run: bool,
    },
}

impl TaskSyncArgs {
    pub(crate) fn settings_action(
        &self,
    ) -> Option<(engram::task_sync::settings::Action, bool, Option<&str>)> {
        use engram::task_sync::settings::Action;
        match &self.operation {
            TaskSyncCommand::Status { json } => Some((Action::Status, *json, None)),
            TaskSyncCommand::Enable {
                json,
                user_instruction,
            } => Some((Action::Enable, *json, user_instruction.as_deref())),
            TaskSyncCommand::Disable { json } => Some((Action::Disable, *json, None)),
            _ => None,
        }
    }
}

pub(crate) fn settings_resolution_error(args: &TaskSyncArgs) -> Result<bool> {
    let Some((action, json, _)) = args.settings_action() else {
        return Ok(false);
    };
    let error = engram::task_sync::settings::Error::new(
        None,
        action,
        "project_selection_failed",
        "select a valid project marker and Engram home",
        None,
    );
    settings_error(&error, json)?;
    Ok(true)
}

fn settings_error(error: &engram::task_sync::settings::Error, json: bool) -> Result<()> {
    if json {
        println!("{}", serde_json::to_string(error)?);
    } else {
        eprintln!("{error}");
    }
    Ok(())
}

fn run_settings(database: PathBuf, project: ProjectId, args: &TaskSyncArgs) -> Result<bool> {
    use engram::task_sync::settings::{Action, Error, Request};
    let (action, json, instruction) = args.settings_action().expect("settings command selected");
    let attribution = if action == Action::Status {
        None
    } else if let Ok(attribution) =
        resolve_shell_work_attribution(args.actor_id.clone(), args.session_id.clone())
    {
        Some(attribution)
    } else {
        let error = Error::new(
            Some(&project),
            action,
            "invalid_attribution",
            "current actor or session attribution is invalid",
            Some(engram::task_sync::settings::read(&database, &project)),
        );
        settings_error(&error, json)?;
        return Ok(false);
    };
    let service = LocalWorkService::new_with_attribution(
        database,
        project,
        attribution
            .as_ref()
            .map_or_else(String::new, |value| value.actor_id.clone()),
        SessionId(
            attribution
                .as_ref()
                .map_or_else(String::new, |value| value.session_id.clone()),
        ),
        args.source_skill.clone(),
        args.actor_context.clone(),
        attribution
            .as_ref()
            .map_or_else(engram::WorkAttributionDefaults::default, |value| {
                value.defaults
            }),
    );
    let outcome = service.task_sync_settings(
        &Request {
            action,
            user_instruction: instruction.map(str::to_owned),
        },
        chrono::Utc::now(),
    );
    if let Some(attribution) = attribution {
        attribution.print_notices();
    }
    match outcome {
        Ok(status) => {
            if json {
                println!("{}", serde_json::to_string(&status)?);
            } else {
                println!("{}\n{}", status.reason, status.effect);
            }
            Ok(true)
        }
        Err(error) => {
            settings_error(&error, json)?;
            Ok(false)
        }
    }
}

pub(crate) fn run(
    database: PathBuf,
    project: ProjectId,
    project_file: &Path,
    args: TaskSyncArgs,
) -> Result<bool> {
    if args.settings_action().is_some() {
        return run_settings(database, project, &args);
    }
    let attribution = resolve_shell_work_attribution(args.actor_id, args.session_id)?;
    if let TaskSyncCommand::SetupPrepare {
        repository,
        branch,
        visibility,
        readership,
        include_notes,
        origin_machine,
    } = args.operation
    {
        let receipt = setup::prepare(
            project_file,
            &project,
            &repository,
            &branch,
            setup::Assertions {
                visibility,
                readership,
                include_notes,
                origin_machine,
                profile: engram::task_sync::FORMAT.into(),
            },
            setup::Attribution {
                actor_id: attribution.actor_id.clone(),
                session_id: attribution.session_id.clone(),
                actor_context: args.actor_context,
                source_skill: args.source_skill,
            },
        )?;
        attribution.print_notices();
        println!("{}", serde_json::to_string_pretty(&receipt)?);
        return Ok(true);
    }
    let service = LocalWorkService::new_with_attribution(
        database,
        project,
        attribution.actor_id.clone(),
        SessionId(attribution.session_id.clone()),
        args.source_skill,
        args.actor_context,
        attribution.defaults,
    );
    match args.operation {
        TaskSyncCommand::Status { .. }
        | TaskSyncCommand::Enable { .. }
        | TaskSyncCommand::Disable { .. } => unreachable!("settings handled before store access"),
        TaskSyncCommand::Prepare {
            directory,
            progress_directory,
            include_notes,
            origin_machine,
            rebaseline,
        } => {
            let profile = engram::task_sync::progress::ExportProfile {
                include_notes,
                origin_machine,
            };
            let result = service.prepare_task_tree(
                &directory,
                &progress_directory,
                &profile,
                rebaseline,
                chrono::Utc::now(),
            )?;
            attribution.print_notices();
            eprintln!("REDACTOR: development no-op; task planning fields are shared text");
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
        TaskSyncCommand::SetupPrepare { .. } => {
            unreachable!("setup handled before service creation")
        }
        TaskSyncCommand::Export {
            directory: path,
            include_notes,
            origin_machine,
        } => {
            let exported = service.export_task_tree_with_options(
                &engram::task_sync::TaskSyncExportOptions {
                    include_notes,
                    origin_machine: origin_machine.clone(),
                },
                chrono::Utc::now(),
            )?;
            directory::write(&path, &exported.tree)?;
            attribution.print_notices();
            eprintln!("REDACTOR: development no-op; task planning fields are shared text");
            if include_notes
                || origin_machine.is_some()
                || !exported.activity_omissions.counts.is_empty()
            {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&serde_json::json!({
                        "directory": path, "notes_included": exported.tree.project.notes_included,
                        "omissions": exported.omissions, "activity_omissions": exported.activity_omissions,
                    }))?
                );
            } else {
                println!("{}", path.display());
            }
        }
        TaskSyncCommand::Import {
            directory: path,
            dry_run,
        } => {
            let tree = directory::read(&path)?;
            let result = service.import_task_tree(&tree, dry_run, chrono::Utc::now())?;
            attribution.print_notices();
            println!("{}", serde_json::to_string_pretty(&result)?);
        }
    }
    Ok(true)
}
