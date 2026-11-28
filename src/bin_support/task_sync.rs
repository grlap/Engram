//! Thin planning interchange front door confined to the dedicated data checkout.

use super::attribution::{ShellWorkAttribution, resolve_shell_work_attribution};
use anyhow::{Context, Result};
use engram::{
    LocalWorkService, ProjectId, SessionId,
    storage::{TaskSyncImportResult, TaskSyncTaskStatus},
    task_sync::{directory, memories::ImportOutcome, setup},
    terminal_error_line,
    work_service::FolderSyncOptions,
};
use std::process::ExitCode;
use std::{
    collections::BTreeMap,
    path::{Path, PathBuf},
};

// A partial folder import keeps admitted changes but must not advance the
// caller's incoming-commit base as if every incoming document was consumed.
const PARTIAL_IMPORT_EXIT: u8 = 3;
const REDACTOR_NOTICE: &str = "REDACTOR: development no-op; task planning fields are shared text";

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
    /// Run a full two-way exchange, the task-data equivalent of git pull plus git push.
    ///
    /// Pushes to the configured remote, which may be public. Use --no-push to
    /// prepare local commits and review them for secrets and private data.
    Sync {
        #[arg(long, default_value = "origin")]
        remote: String,
        #[arg(long, default_value = "engram-data")]
        branch: String,
        /// This host's export alias. Required for its first notes-enabled export.
        /// Eligible machine activity can be shared under this alias even with notes off.
        #[arg(long)]
        origin_machine: Option<String>,
        /// Opt an enrolled data branch into sharing project memories.
        /// Sync includes memories automatically once the branch carries them.
        #[arg(long)]
        include_memories: bool,
        /// Prepare local commits for disclosure review without pushing.
        #[arg(long)]
        no_push: bool,
        #[arg(long)]
        json: bool,
    },
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
        /// Include `Internal` project-memory histories under the memory profile.
        #[arg(long)]
        include_memories: bool,
        /// Explicit retained pre-export .engram folder for interrupted memory publication.
        #[arg(long, requires = "include_memories")]
        memory_recovery_basis: Option<PathBuf>,
        /// Compared inputs for an already authored common-prefix memory merge.
        #[arg(long, requires = "include_memories")]
        memory_resolution: Option<PathBuf>,
        /// Explicitly retire unseen selected revisions of this locally forgotten key.
        #[arg(long, requires = "include_memories", value_name = "KEY")]
        retire_unseen: Vec<String>,
        /// Write the complete folder and record it afresh, instead of only the
        /// tasks changed since the last recorded export or import.
        #[arg(long)]
        rebuild: bool,
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
        /// This host's own project.json from its export before the merge, for
        /// the complete route: the first import of a store, or `--rebuild`.
        /// An incremental import reads its basis from the recorded folder head.
        #[arg(long, value_name = "PROJECT_JSON", conflicts_with = "empty_basis")]
        basis: Option<PathBuf>,
        /// Import into a destination holding no shared planning yet; the import
        /// checks that it is empty.
        #[arg(long)]
        empty_basis: bool,
        /// Import the complete folder against `--basis` and record it afresh,
        /// instead of only the paths changed since the last recorded import.
        #[arg(long)]
        rebuild: bool,
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
    home: &Path,
    args: TaskSyncArgs,
) -> Result<ExitCode> {
    Box::pin(engram::operation_time::observe(
        "task-sync command",
        run_observed(database, project, project_file, home, args),
    ))
    .await
}

async fn run_observed(
    database: PathBuf,
    project: ProjectId,
    project_file: &Path,
    home: &Path,
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
    )
    .with_task_sync_cli_selection(&std::path::absolute(home)?, project_file);
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
        TaskSyncCommand::Sync {
            remote,
            branch,
            origin_machine,
            include_memories,
            no_push,
            json,
        } => {
            eprintln!("{REDACTOR_NOTICE}");
            let receipt = Box::pin(service.sync_task_folder(
                project_file,
                &FolderSyncOptions {
                    remote: &remote,
                    branch: &branch,
                    origin_machine: origin_machine.as_deref(),
                    no_push,
                    include_memories,
                },
                print_memory_retirement,
            ))
            .await?;
            for withheld in &receipt.withheld_memories {
                eprintln!("Memory key {} withheld: {}", withheld.key, withheld.reason);
            }
            attribution.print_notices();
            if json {
                println!("{}", serde_json::to_string_pretty(&receipt)?);
            } else if no_push && receipt.remote_head.as_deref() != Some(receipt.local_head.as_str())
            {
                println!(
                    "Project task data prepared at {}; push skipped",
                    receipt.local_head
                );
            } else {
                println!("Project task data synchronized at {}", receipt.local_head);
            }
        }
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
        operation @ (TaskSyncCommand::Export { .. } | TaskSyncCommand::Import { .. }) => {
            return run_folder_exchange(
                &service,
                project_file,
                &selected_project,
                &attribution,
                operation,
            );
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn print_memory_retirement(outcome: &ImportOutcome) {
    let key = terminal_error_line(&outcome.key);
    if outcome.retired_local_revisions.is_empty() {
        eprintln!("Retired project memory `{key}`");
    } else {
        let revisions = outcome
            .retired_local_revisions
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", ");
        eprintln!("Retired project memory `{key}` with local revisions {revisions}");
    }
}

fn run_folder_exchange(
    service: &LocalWorkService,
    project_file: &Path,
    selected_project: &ProjectId,
    attribution: &ShellWorkAttribution,
    operation: TaskSyncCommand,
) -> Result<ExitCode> {
    match operation {
        TaskSyncCommand::Export {
            directory: path,
            branch,
            include_notes,
            origin_machine,
            include_memories,
            memory_recovery_basis,
            memory_resolution,
            retire_unseen,
            rebuild,
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
                    selected_project,
                    &branch,
                )?;
                let path = data.select_admitted_folder(&path)?;
                let memory_lock = include_memories
                    .then(|| engram::task_sync::memories::directory::publication_lock(&data))
                    .transpose()?;
                let memory_basis = if include_memories {
                    let committed =
                        engram::task_sync::memories::directory::committed(&data, selected_project)?;
                    anyhow::ensure!(
                        path.exists() || committed.is_empty(),
                        engram::task_sync::TaskSyncRefusal::MemoryExportComponentMissing
                            .invalid("restore the committed memory component before export")
                    );
                    committed
                } else {
                    BTreeMap::default()
                };
                let mut resolutions = match memory_resolution {
                    Some(path) => engram::task_sync::memories::directory::read_options(&path)?,
                    None => BTreeMap::default(),
                };
                for options in resolutions.values_mut() {
                    options.retire_unseen = false;
                }
                for key in retire_unseen {
                    resolutions.entry(key).or_default().retire_unseen = true;
                }
                // The memory component is captured on the task plan's own
                // snapshot, and every memory admission check runs on it
                // before any file is written; the task export then counts
                // the component's bytes against the folder's size limit.
                let mut admitted = None;
                let memory_export = memory_lock
                    .as_ref()
                    .map(|held| {
                        engram::work_service::FolderMemoryExport::publication(
                            &data,
                            selected_project,
                            &memory_basis,
                            held,
                            &resolutions,
                            memory_recovery_basis.as_deref(),
                            &mut admitted,
                        )
                    })
                    .transpose()?;
                let receipt = service.export_task_sync_data_folder(
                    &data,
                    &profile,
                    rebuild,
                    memory_export,
                    chrono::Utc::now(),
                )?;
                let memory_receipt = if include_memories {
                    let captured = admitted.context("memory capture missing")?;
                    for withheld in &captured.withheld {
                        eprintln!("Memory key {} withheld: {}", withheld.key, withheld.reason);
                    }
                    Some(
                        serde_json::json!({"exported_keys":captured.keys.len(),"withheld":captured.withheld,"retired_unseen":captured.retired_unseen}),
                    )
                } else {
                    None
                };
                attribution.print_notices();
                eprintln!("{REDACTOR_NOTICE}");
                report_superseded(&receipt.superseded);
                if json
                    || include_memories
                    || include_notes
                    || origin_machine.is_some()
                    || !receipt.activity_omissions.counts.is_empty()
                {
                    let mut value = serde_json::to_value(&receipt)?;
                    value["directory"] = serde_json::to_value(&path)?;
                    if let Some(memories) = memory_receipt {
                        value["memories"] = memories;
                    }
                    value["next"] = serde_json::json!([
                        "commit the exported folder in the data worktree before merging or importing"
                    ]);
                    println!("{}", serde_json::to_string_pretty(&value)?);
                } else {
                    println!("{}", path.display());
                }
                Ok(())
            })();
            refusal_outcome(outcome, json)
        }
        TaskSyncCommand::Import {
            directory: path,
            branch,
            basis,
            empty_basis,
            rebuild,
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
                    selected_project,
                    &branch,
                )?;
                data.select_folder(&path)?;
                let memory_lock = include_memories
                    .then(|| engram::task_sync::memories::directory::publication_lock(&data))
                    .transpose()?;
                let (memories, memory_bytes) = match &memory_lock {
                    Some(held) => {
                        engram::task_sync::memories::directory::read_locked_with_bytes(&data, held)?
                    }
                    None => (BTreeMap::default(), 0),
                };
                let mut resolutions = match memory_resolution {
                    Some(path) => engram::task_sync::memories::directory::read_options(&path)?,
                    None => std::collections::BTreeMap::new(),
                };
                for options in resolutions.values_mut() {
                    options.mark_untagged_seed = mark_untagged_seeds && options.seed.is_some();
                }
                let basis = destination_basis(basis, empty_basis)?;
                data.revalidate()?;
                let receipt = service.import_task_sync_data_folder(
                    &data,
                    basis.as_ref(),
                    rebuild,
                    dry_run,
                    memory_lock
                        .as_ref()
                        .map(|fence| engram::work_service::MemoryImport {
                            fence,
                            selected: &memories,
                            options: &resolutions,
                            bytes: memory_bytes,
                            on_retirement: Some(&print_memory_retirement),
                        })
                        .as_ref(),
                    chrono::Utc::now(),
                )?;
                attribution.print_notices();
                println!("{}", serde_json::to_string_pretty(&receipt)?);
                Ok((receipt.result, receipt.memory_partial))
            })();
            match outcome {
                Ok((result, memory_partial)) => {
                    let task_exit = import_exit_code(&result, &path);
                    Ok(if memory_partial {
                        ExitCode::from(PARTIAL_IMPORT_EXIT)
                    } else {
                        task_exit
                    })
                }
                Err(error) => refusal_outcome(Err(error), json),
            }
        }
        _ => anyhow::bail!("folder exchange requires an export or import operation"),
    }
}

/// Name the held edits a rebuild replaced, so that loss is never silent.
fn report_superseded(superseded: &[engram::WorkId]) {
    if !superseded.is_empty() {
        eprintln!(
            "This rebuild wrote this host's version over another host's held edit of: {}",
            superseded
                .iter()
                .map(|id| id.0.to_string())
                .collect::<Vec<_>>()
                .join(", ")
        );
    }
}

/// The destination basis an import selected: this host's own export, an
/// empty store, or none for an import that reads its recorded folder head.
fn destination_basis(
    basis: Option<PathBuf>,
    empty_basis: bool,
) -> Result<Option<directory::DestinationBasis>> {
    Ok(match (basis, empty_basis) {
        (Some(file), false) => Some(directory::DestinationBasis::Recorded(Box::new(
            directory::read_folder_basis(&file)?,
        ))),
        (None, true) => Some(directory::DestinationBasis::Empty),
        (None, false) => None,
        (Some(_), true) => anyhow::bail!("select at most one of --basis or --empty-basis"),
    })
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
            "Partial task import: each held task keeps the incoming bytes in the folder, and an ordinary export leaves them in place. Resolve why it was held here, for example by releasing the claim, then import again: every import reads the held tasks again at the commit it imports. For a task held as local_planning_changed, follow the recovery instructions next to that task. Applied changes remain applied; --dry-run writes nothing."
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
        assert!(
            crate::Cli::try_parse_from([
                "engram",
                "task-sync",
                "export",
                "folder",
                "--retire-unseen",
                "rule",
            ])
            .is_err()
        );
        let retirement = crate::Cli::try_parse_from([
            "engram",
            "task-sync",
            "export",
            "folder",
            "--include-memories",
            "--retire-unseen",
            "rule",
        ])
        .unwrap();
        assert!(matches!(retirement.command, crate::Command::TaskSync(args)
            if matches!(args.operation, TaskSyncCommand::Export { ref retire_unseen, .. } if retire_unseen == &["rule"])));
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
