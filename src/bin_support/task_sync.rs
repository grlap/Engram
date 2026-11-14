//! Thin planning interchange front door confined to the dedicated data checkout.

use super::attribution::resolve_shell_work_attribution;
use anyhow::{Context, Result};
use engram::{
    LocalWorkService, ProjectId, SessionId,
    storage::{TaskSyncImportResult, TaskSyncTaskStatus},
    task_sync::{directory, setup},
};
use std::path::{Path, PathBuf};
use std::process::ExitCode;

// A partial folder import keeps admitted changes but must not advance the
// caller's incoming-commit base as if every incoming document was consumed.
const PARTIAL_IMPORT_EXIT: u8 = 3;

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
    /// Execute one explicit ledger/data step; never dispatches network or merges.
    Step {
        /// Typed action as JSON or @FILE; retain it unchanged for exact retries.
        #[arg(long)]
        request: String,
    },
    /// Read selected family capture and coverage; never imports or publishes.
    Progress {
        #[arg(long)]
        binding: engram::ObjectId,
    },
    /// Create or lock the fixed .engram-data orphan worktree; no commit or network.
    DataInit {
        #[arg(long, default_value = "engram-data")]
        branch: String,
    },
    /// Retain an exact staged data commit; never advances the branch or pushes.
    DataCommit {
        #[arg(long, default_value = "engram-data")]
        branch: String,
        /// Exact commit selection as JSON or @FILE; keep it for replay.
        #[arg(long)]
        request: String,
    },
    /// Run an explicit cold custody audit; never enables ordinary managed writes.
    CustodyAudit {
        /// Exact typed audit request as JSON or @FILE; retain it for retries.
        #[arg(long)]
        request: String,
    },
    /// Read the logical producing origin; never enrolls or repairs continuity.
    Origin,
    /// Explicitly enroll this project-local store, or fork a named predecessor.
    OriginEnroll {
        #[arg(long)]
        operation: uuid::Uuid,
        #[arg(long)]
        predecessor: Option<engram::ObjectId>,
    },
    /// Create a new local successor after cloning, rewind or migration import.
    OriginRecover {
        #[arg(long)]
        operation: uuid::Uuid,
        #[arg(long)]
        predecessor: engram::ObjectId,
        #[arg(long, value_parser = ["db-only-clone", "same-origin-rewind", "migration-import"])]
        cause: String,
    },
    /// Release only an exact recovery intent proven uncommitted, keeping history.
    OriginRecoveryCancel {
        #[arg(long)]
        operation: uuid::Uuid,
        #[arg(long)]
        predecessor: engram::ObjectId,
        #[arg(long, value_parser = ["db-only-clone", "same-origin-rewind", "migration-import"])]
        cause: String,
    },
    /// Cancel an exact uncommitted enrollment intent, preserving all artifacts.
    OriginCancel {
        #[arg(long)]
        operation: uuid::Uuid,
        #[arg(long)]
        predecessor: Option<engram::ObjectId>,
    },
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
    /// Materialize selected preparation bytes in the data worktree; no commit or network.
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
    /// Publish .engram in the dedicated data worktree, or replay identical bytes.
    Export {
        directory: PathBuf,
        #[arg(long, default_value = "engram-data")]
        branch: String,
        /// Include eligible generic/status notes and report omissions.
        #[arg(long, requires = "origin_machine")]
        include_notes: bool,
        /// Include native activity under this asserted alias, also when notes are off.
        /// Imported notes and activity retain their original aliases.
        #[arg(long)]
        origin_machine: Option<String>,
        /// Include Internal project-memory histories under the memory profile.
        #[arg(long)]
        include_memories: bool,
        /// Explicit retained pre-export .engram folder for interrupted memory publication.
        #[arg(long, requires = "include_memories")]
        memory_recovery_basis: Option<PathBuf>,
        /// Compared inputs for an already authored common-prefix memory merge.
        #[arg(long, requires = "include_memories")]
        memory_resolution: Option<PathBuf>,
        /// Print the receipt as JSON, and a refusal as a JSON error with its code on stderr.
        #[arg(long)]
        json: bool,
    },
    /// Apply the dedicated data worktree's .engram against this host's own basis;
    /// an exact repeat replays its first receipt.
    Import {
        directory: PathBuf,
        #[arg(long, default_value = "engram-data")]
        branch: String,
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
        /// Consume the advertised memory component after task application.
        #[arg(long)]
        include_memories: bool,
        /// Per-key compared merge inputs or seed manifests as bounded JSON.
        #[arg(long, requires = "include_memories")]
        memory_resolution: Option<PathBuf>,
        /// Explicit one-time fallback for manifest-bound untagged old seeds.
        #[arg(long, requires = "memory_resolution")]
        mark_untagged_seeds: bool,
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

fn recovery_request(
    operation: uuid::Uuid,
    predecessor: engram::ObjectId,
    cause: &str,
) -> Result<engram::task_sync::origin::recovery::RecoveryRequest> {
    use engram::task_sync::origin::recovery::{RecoveryCause, RecoveryRequest};
    let cause = match cause {
        "db-only-clone" => RecoveryCause::DbOnlyClone,
        "same-origin-rewind" => RecoveryCause::SameOriginRewind,
        "migration-import" => RecoveryCause::MigrationImport,
        _ => anyhow::bail!("unsupported origin recovery cause"),
    };
    Ok(RecoveryRequest {
        operation,
        predecessor,
        cause,
    })
}

pub(crate) async fn run(
    database: PathBuf,
    project: ProjectId,
    project_file: &Path,
    args: TaskSyncArgs,
) -> Result<ExitCode> {
    Box::pin(engram::operation_time::observe(
        "task-sync command",
        run_observed(database, project, project_file, args),
    ))
    .await
}

async fn run_observed(
    database: PathBuf,
    project: ProjectId,
    project_file: &Path,
    args: TaskSyncArgs,
) -> Result<ExitCode> {
    if args.settings_action().is_some() {
        return run_settings(database, project, &args).map(|success| {
            if success {
                ExitCode::SUCCESS
            } else {
                ExitCode::FAILURE
            }
        });
    }
    let attribution = resolve_shell_work_attribution(args.actor_id, args.session_id)?;
    let selected_project = project.clone();
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
        return Ok(ExitCode::SUCCESS);
    }
    match args.operation {
        TaskSyncCommand::Step { request } => {
            let request = crate::parse_bounded_json_input::<engram::task_sync::driver::StepRequest>(
                &request,
                "task exchange step",
                2 * 1024 * 1024,
            )?;
            let receipt = Box::pin(service.task_sync_exchange_step(
                &request,
                engram::task_sync::exchange::RunLimits::default(),
                &engram::task_sync::exchange::Cancellation::default(),
            ))
            .await?;
            attribution.print_notices();
            println!("{}", serde_json::to_string_pretty(&receipt)?);
        }
        TaskSyncCommand::Progress { binding } => {
            let receipt = service.task_sync_family_frontiers(&binding, chrono::Utc::now())?;
            attribution.print_notices();
            println!("{}", serde_json::to_string_pretty(&receipt)?);
        }
        TaskSyncCommand::DataInit { branch } => {
            let receipt = service.initialize_task_sync_data_worktree(project_file, &branch)?;
            attribution.print_notices();
            println!("{}", serde_json::to_string_pretty(&receipt)?);
        }
        TaskSyncCommand::DataCommit { branch, request } => {
            service.require_task_sync_enabled()?;
            let request = crate::parse_bounded_json_input::<
                engram::task_sync::data_worktree::StagedCommitRequest,
            >(&request, "staged data commit request", 128 * 1024)?;
            let receipt = service.create_task_sync_data_commit(project_file, &branch, &request)?;
            attribution.print_notices();
            println!("{}", serde_json::to_string_pretty(&receipt)?);
        }
        TaskSyncCommand::CustodyAudit { request } => {
            // Admission precedes reading a request file, repository or SQLite.
            service.require_task_sync_enabled()?;
            let request = crate::parse_bounded_json_input::<
                engram::task_sync::delta::CustodyAuditRequest,
            >(&request, "custody audit request", 16 * 1024)?;
            attribution.print_notices();
            let receipt = if matches!(
                request.seed,
                engram::task_sync::delta::CustodySeedSelection::IncomingDataCommit { .. }
            ) {
                service
                    .audit_task_sync_incoming_data_commit(
                        &request,
                        engram::task_sync::exchange::RunLimits::default(),
                        &engram::task_sync::exchange::Cancellation::default(),
                    )
                    .await?
            } else {
                service.audit_task_sync_custody(&request, chrono::Utc::now())?
            };
            println!("{}", serde_json::to_string_pretty(&receipt)?);
        }
        TaskSyncCommand::OriginRecoveryCancel {
            operation,
            predecessor,
            cause,
        } => {
            let request = recovery_request(operation, predecessor, &cause)?;
            attribution.print_notices();
            let cancelled =
                service.cancel_task_sync_origin_recovery(&request, chrono::Utc::now())?;
            println!("{}", serde_json::json!({ "cancelled": cancelled }));
        }
        TaskSyncCommand::OriginRecover {
            operation,
            predecessor,
            cause,
        } => {
            let request = recovery_request(operation, predecessor, &cause)?;
            attribution.print_notices();
            let receipt = service.recover_task_sync_origin(&request, chrono::Utc::now())?;
            println!("{}", serde_json::to_string_pretty(&receipt)?);
        }
        TaskSyncCommand::OriginCancel {
            operation,
            predecessor,
        } => {
            attribution.print_notices();
            let cancelled = service.cancel_task_sync_origin_enrollment(
                &engram::task_sync::origin::EnrollRequest {
                    operation,
                    predecessor,
                },
                chrono::Utc::now(),
            )?;
            println!("{}", serde_json::json!({ "cancelled": cancelled }));
        }
        TaskSyncCommand::Origin => {
            println!(
                "{}",
                serde_json::to_string_pretty(&service.task_sync_origin(chrono::Utc::now())?)?
            );
        }
        TaskSyncCommand::OriginEnroll {
            operation,
            predecessor,
        } => {
            // The current command has its own attribution; an exact retry
            // returns the original immutable enrollment and its original actor.
            attribution.print_notices();
            let origin = service.enroll_task_sync_origin(
                &engram::task_sync::origin::EnrollRequest {
                    operation,
                    predecessor,
                },
                chrono::Utc::now(),
            )?;
            println!("{}", serde_json::to_string_pretty(&origin)?);
        }
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
            branch,
            include_notes,
            origin_machine,
            include_memories,
            memory_recovery_basis,
            memory_resolution,
            json,
        } => {
            let outcome = (|| -> Result<()> {
                service.require_task_sync_enabled()?;
                let profile = engram::task_sync::progress::ExportProfile {
                    include_notes,
                    origin_machine: origin_machine.clone(),
                };
                if let Some(alias) = profile.origin_machine.as_deref() {
                    engram::task_sync::notes::validate_machine_alias(alias)?;
                }
                let data = engram::task_sync::data_worktree::DataWorktree::observe_project(
                    project_file,
                    &selected_project,
                    &branch,
                )?;
                let path = data.select_folder(&path)?;
                let memory_basis = if include_memories {
                    let committed = engram::task_sync::memories::directory::committed(
                        &data,
                        &selected_project,
                    )?;
                    if let Some(retained) = &memory_recovery_basis {
                        anyhow::ensure!(
                            engram::task_sync::memories::directory::read_basis(retained)?
                                == committed,
                            "memory_export_basis_changed: recovery basis differs from data HEAD"
                        );
                    }
                    anyhow::ensure!(
                        path.exists() || committed.is_empty(),
                        "memory_export_component_missing: restore the committed memory component before export"
                    );
                    committed
                } else {
                    Default::default()
                };
                if let Some(retained) = &memory_recovery_basis {
                    directory::recover_memory_profile(&data, retained)?;
                }
                let resolutions = match memory_resolution {
                    Some(path) => engram::task_sync::memories::directory::read_options(&path)?,
                    None => Default::default(),
                };
                let (exported, memory_capture) = if include_memories {
                    let (tasks, memories) = service.export_task_sync_folder_with_memories(
                        &profile,
                        &memory_basis,
                        &resolutions,
                        chrono::Utc::now(),
                    )?;
                    (tasks, Some(memories))
                } else {
                    (
                        service.export_task_sync_folder(&profile, chrono::Utc::now())?,
                        None,
                    )
                };
                let written = if include_memories && path.exists() {
                    let existing = if memory_recovery_basis.is_some() {
                        engram::task_sync::memories::directory::read_tasks_for_recovery(&data)?
                    } else {
                        directory::read_resolved_data_folder_with_memories(&data)?
                    };
                    if existing.tree == exported.tree && existing.profile == exported.profile {
                        existing
                    } else {
                        directory::write_data_folder(&data, &exported.tree, &exported.profile)?
                    }
                } else {
                    directory::write_data_folder(&data, &exported.tree, &exported.profile)?
                };
                let memory_receipt = if include_memories {
                    let selected = memory_basis;
                    let captured = memory_capture.context("memory capture missing")?;
                    let mut complete = selected.clone();
                    complete.extend(captured.keys.clone());
                    directory::enable_memory_profile(&data)?;
                    if memory_recovery_basis.is_some() {
                        engram::task_sync::memories::directory::recover(
                            &data, &selected, &complete,
                        )?;
                    } else {
                        engram::task_sync::memories::directory::write(&data, &selected, &complete)?;
                    }
                    Some(
                        serde_json::json!({"exported_keys":captured.keys.len(),"withheld":captured.withheld}),
                    )
                } else {
                    None
                };
                attribution.print_notices();
                eprintln!("REDACTOR: development no-op; task planning fields are shared text");
                if json
                    || include_memories
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
                            "memories": memory_receipt,
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
            branch,
            basis,
            empty_basis,
            dry_run,
            include_memories,
            memory_resolution,
            mark_untagged_seeds,
            json,
        } => {
            let outcome = (|| -> Result<(TaskSyncImportResult, bool)> {
                service.require_task_sync_enabled()?;
                let data = engram::task_sync::data_worktree::DataWorktree::observe_project(
                    project_file,
                    &selected_project,
                    &branch,
                )?;
                data.select_folder(&path)?;
                let folder = if include_memories {
                    directory::read_resolved_data_folder_with_memories(&data)?
                } else {
                    directory::read_resolved_data_folder(&data)?
                };
                let memories = if include_memories {
                    engram::task_sync::memories::directory::read(&data)?
                } else {
                    Default::default()
                };
                let mut resolutions = match memory_resolution {
                    Some(path) => engram::task_sync::memories::directory::read_options(&path)?,
                    None => std::collections::BTreeMap::new(),
                };
                let basis = match (basis, empty_basis) {
                    (Some(file), false) => {
                        directory::DestinationBasis::Recorded(directory::read_folder_basis(&file)?)
                    }
                    (None, true) => directory::DestinationBasis::Empty,
                    _ => anyhow::bail!("select exactly one of --basis or --empty-basis"),
                };
                data.revalidate()?;
                for options in resolutions.values_mut() {
                    options.mark_untagged_seed = mark_untagged_seeds && options.seed.is_some();
                }
                let combined = service.import_task_sync_folder_with_memories(
                    &folder,
                    &basis,
                    &memories,
                    &resolutions,
                    dry_run,
                    chrono::Utc::now(),
                )?;
                let result = combined.tasks;
                let memory_results = combined.memories;
                let memory_partial = combined.memory_partial;
                attribution.print_notices();
                if include_memories {
                    println!(
                        "{}",
                        serde_json::to_string_pretty(
                            &serde_json::json!({"tasks":result,"memories":memory_results,"memory_partial":memory_partial})
                        )?
                    );
                } else {
                    println!("{}", serde_json::to_string_pretty(&result)?);
                }
                Ok((result, memory_partial))
            })();
            return match outcome {
                Ok((result, memory_partial)) => {
                    let task_exit = import_exit_code(&result, &path);
                    Ok(if memory_partial {
                        ExitCode::from(PARTIAL_IMPORT_EXIT)
                    } else {
                        task_exit
                    })
                }
                Err(error) => refusal_outcome(Err(error), json),
            };
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn import_exit_code(result: &TaskSyncImportResult, folder: &Path) -> ExitCode {
    let mut partial = false;
    for task in &result.tasks {
        let status = match task.status {
            TaskSyncTaskStatus::Applied => continue,
            TaskSyncTaskStatus::Held => "held",
            TaskSyncTaskStatus::Refused => "refused",
        };
        partial = true;
        let task_file = folder.join("tasks").join(format!("{}.json", task.task.0));
        let location = if task_file.is_file() {
            task_file.display().to_string()
        } else {
            format!("omitted task {}", task.task.0)
        };
        eprintln!(
            "{location}: {status} ({}: {})",
            task.reason.as_deref().unwrap_or("unknown"),
            task.detail.as_deref().unwrap_or("no detail")
        );
    }
    if partial {
        eprintln!(
            "Partial task import: preserve the incoming Git commit before the next export; a routine export now would overwrite its pending edits. Re-merge that commit against a fresh local export using the common commit, then import the resolved documents with the fresh basis. Do not replay the old documents over newer local edits or advance the incoming commit as the exchange base. Applied changes remain applied; --dry-run writes nothing."
        );
        ExitCode::from(PARTIAL_IMPORT_EXIT)
    } else {
        ExitCode::SUCCESS
    }
}

/// With `--json`, a store refusal is printed on stderr as the structured
/// error, carrying its stable code, and the command fails. A work-item
/// remedy never applies to these commands, so it is left out. Any other
/// error, and every error without `--json`, propagates as before.
fn refusal_outcome(outcome: Result<()>, json: bool) -> Result<ExitCode> {
    match outcome {
        Ok(()) => Ok(ExitCode::SUCCESS),
        Err(error) if json => {
            let Some(refusal) = error.downcast_ref::<engram::StoreError>() else {
                return Err(error);
            };
            let mut value = engram::store_error_value(refusal);
            if let Some(details) = value["error"]["details"].as_object_mut() {
                details.remove("remedy");
            }
            eprintln!("{}", serde_json::to_string_pretty(&value)?);
            Ok(ExitCode::FAILURE)
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

#[cfg(test)]
mod memory_tests {
    use super::*;
    use clap::Parser;

    #[test]
    fn memory_sync_cli_requires_explicit_profile_for_recovery_and_seed_marking() {
        let merged = crate::Cli::try_parse_from([
            "engram",
            "task-sync",
            "export",
            "folder",
            "--include-memories",
            "--memory-resolution",
            "merge.json",
        ])
        .unwrap();
        assert!(
            matches!(merged.command, crate::Command::TaskSync(args) if matches!(args.operation, TaskSyncCommand::Export { memory_resolution:Some(_), .. }))
        );
        assert!(
            crate::Cli::try_parse_from([
                "engram",
                "task-sync",
                "export",
                "folder",
                "--memory-resolution",
                "merge.json"
            ])
            .is_err()
        );
        let parsed = crate::Cli::try_parse_from([
            "engram",
            "task-sync",
            "export",
            "folder",
            "--include-memories",
            "--memory-recovery-basis",
            "retained",
        ])
        .unwrap();
        assert!(
            matches!(parsed.command, crate::Command::TaskSync(args) if matches!(args.operation, TaskSyncCommand::Export { include_memories:true, memory_recovery_basis:Some(_), .. }))
        );
        assert!(
            crate::Cli::try_parse_from([
                "engram",
                "task-sync",
                "export",
                "folder",
                "--memory-recovery-basis",
                "retained"
            ])
            .is_err()
        );
        let parsed = crate::Cli::try_parse_from([
            "engram",
            "task-sync",
            "import",
            "folder",
            "--empty-basis",
            "--include-memories",
            "--memory-resolution",
            "options.json",
            "--mark-untagged-seeds",
        ])
        .unwrap();
        assert!(
            matches!(parsed.command, crate::Command::TaskSync(args) if matches!(args.operation, TaskSyncCommand::Import { include_memories:true, mark_untagged_seeds:true, .. }))
        );
        assert!(
            crate::Cli::try_parse_from([
                "engram",
                "task-sync",
                "import",
                "folder",
                "--empty-basis",
                "--include-memories",
                "--mark-untagged-seeds"
            ])
            .is_err()
        );
    }
}
