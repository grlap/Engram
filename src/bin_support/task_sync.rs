//! Thin explicit-directory planning interchange front door.

use super::attribution::resolve_shell_work_attribution;
use anyhow::Result;
use engram::{LocalWorkService, ProjectId, SessionId, task_sync::directory};
use std::path::PathBuf;

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

pub(crate) fn run(database: PathBuf, project: ProjectId, args: TaskSyncArgs) -> Result<()> {
    let attribution = resolve_shell_work_attribution(args.actor_id, args.session_id)?;
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
    Ok(())
}
