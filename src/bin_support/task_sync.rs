//! Thin explicit-directory planning interchange front door.

use super::attribution::resolve_shell_work_attribution;
use anyhow::Result;
use engram::{
    LocalWorkService, ProjectId, SessionId,
    task_sync::{directory, setup},
};
use std::path::{Path, PathBuf};

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
    /// Materialize selected preparation bytes in the main checkout; no commit or network.
    SetupMaterialize {
        repository: PathBuf,
        #[arg(long)]
        setup_operation: uuid::Uuid,
        #[arg(long)]
        prepared_operation: uuid::Uuid,
        #[arg(long)]
        progress_directory: PathBuf,
        #[arg(long)]
        candidate_directory: PathBuf,
    },
    /// Publish the Git folder (project.json with its export basis, and
    /// tasks/<uuid>.json) into an absent directory, or one holding the same bytes.
    Export {
        directory: PathBuf,
        /// Include eligible generic/status notes and report omissions.
        #[arg(long, requires = "origin_machine")]
        include_notes: bool,
        /// Include native activity under this asserted alias, also when notes are off.
        /// Imported notes and activity retain their original aliases.
        #[arg(long)]
        origin_machine: Option<String>,
        /// Print the receipt as JSON, and a refusal as a JSON error with its code on stderr.
        #[arg(long)]
        json: bool,
    },
    /// Apply a resolved Git folder against this host's own pre-merge basis;
    /// an exact repeat replays its first receipt.
    Import {
        directory: PathBuf,
        /// This host's own project.json from its export before the merge.
        #[arg(
            long,
            value_name = "PROJECT_JSON",
            conflicts_with = "empty_basis",
            required_unless_present = "empty_basis"
        )]
        basis: Option<PathBuf>,
        /// Import into a destination holding no shared planning yet; the import
        /// checks that it is empty.
        #[arg(long)]
        empty_basis: bool,
        /// Run the whole import, then roll it back.
        #[arg(long)]
        dry_run: bool,
        /// Print a refusal as a JSON error with its code on stderr.
        #[arg(long)]
        json: bool,
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
    let service = LocalWorkService::new_with_attribution(
        database,
        project,
        attribution.actor_id.clone(),
        SessionId(attribution.session_id.clone()),
        args.source_skill.clone(),
        args.actor_context.clone(),
        attribution.defaults,
    );
    if let TaskSyncCommand::SetupPrepare {
        repository,
        branch,
        visibility,
        readership,
        include_notes,
        origin_machine,
    } = args.operation
    {
        let receipt = service.prepare_task_sync_setup(
            project_file,
            &repository,
            &branch,
            setup::Assertions {
                visibility,
                readership,
                include_notes,
                origin_machine,
                profile: engram::task_sync::FORMAT.into(),
            },
        )?;
        attribution.print_notices();
        println!("{}", serde_json::to_string_pretty(&receipt)?);
        return Ok(true);
    }
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
            unreachable!("setup preparation handled before exchange dispatch")
        }
        TaskSyncCommand::SetupMaterialize {
            repository,
            setup_operation,
            prepared_operation,
            progress_directory,
            candidate_directory,
        } => {
            let receipt = service.materialize_task_sync_setup(
                project_file,
                &setup::MaterializeSelection {
                    repository: materialization_path(repository)?,
                    setup_operation,
                    prepared_operation,
                    progress_directory: materialization_path(progress_directory)?,
                    candidate_directory: materialization_path(candidate_directory)?,
                },
            )?;
            attribution.print_notices();
            println!("{}", serde_json::to_string_pretty(&receipt)?);
        }
        TaskSyncCommand::Export {
            directory: path,
            include_notes,
            origin_machine,
            json,
        } => {
            let outcome = (|| -> Result<()> {
                let exported = service.export_task_sync_folder(
                    &engram::task_sync::progress::ExportProfile {
                        include_notes,
                        origin_machine: origin_machine.clone(),
                    },
                    chrono::Utc::now(),
                )?;
                let written = directory::write_folder(&path, &exported.tree, &exported.profile)?;
                attribution.print_notices();
                eprintln!("REDACTOR: development no-op; task planning fields are shared text");
                if json
                    || include_notes
                    || origin_machine.is_some()
                    || !exported.activity_omissions.counts.is_empty()
                {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(&serde_json::json!({
                            "directory": path,
                            "notes_included": written.tree.project.notes_included,
                            "export_basis": written.export_basis,
                            "omissions": exported.omissions,
                            "activity_omissions": exported.activity_omissions,
                        }))?
                    );
                } else {
                    println!("{}", path.display());
                }
                Ok(())
            })();
            return refusal_outcome(outcome, json);
        }
        TaskSyncCommand::Import {
            directory: path,
            basis,
            empty_basis,
            dry_run,
            json,
        } => {
            let outcome = (|| -> Result<()> {
                service.require_task_sync_enabled()?;
                let folder = directory::read_resolved_folder(&path)?;
                let basis = match (basis, empty_basis) {
                    (Some(file), false) => {
                        directory::DestinationBasis::Recorded(directory::read_folder_basis(&file)?)
                    }
                    (None, true) => directory::DestinationBasis::Empty,
                    _ => anyhow::bail!("select exactly one of --basis or --empty-basis"),
                };
                let result = service.import_task_sync_folder(
                    &folder,
                    &basis,
                    dry_run,
                    chrono::Utc::now(),
                )?;
                attribution.print_notices();
                println!("{}", serde_json::to_string_pretty(&result)?);
                Ok(())
            })();
            return refusal_outcome(outcome, json);
        }
    }
    Ok(true)
}

/// With `--json`, a store refusal is printed on stderr as the structured
/// error, carrying its stable code, and the command fails. A work-item
/// remedy never applies to these commands, so it is left out. Any other
/// error, and every error without `--json`, propagates as before.
fn refusal_outcome(outcome: Result<()>, json: bool) -> Result<bool> {
    match outcome {
        Ok(()) => Ok(true),
        Err(error) if json => {
            let Some(refusal) = error.downcast_ref::<engram::StoreError>() else {
                return Err(error);
            };
            let mut value = engram::store_error_value(refusal);
            if let Some(details) = value["error"]["details"].as_object_mut() {
                details.remove("remedy");
            }
            eprintln!("{}", serde_json::to_string_pretty(&value)?);
            Ok(false)
        }
        Err(error) => Err(error),
    }
}

fn materialization_path(path: std::path::PathBuf) -> Result<std::path::PathBuf> {
    anyhow::ensure!(
        !path
            .components()
            .any(|component| component == std::path::Component::ParentDir),
        "materialization selection paths must not contain '..'; use the original absolute paths without parent components"
    );
    Ok(std::path::absolute(path)?)
}
