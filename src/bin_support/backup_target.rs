//! The operator `backup target` words: set, show and clear a project's
//! backup targets. They read and write only the records under the Engram
//! home and never open the store.

use std::path::{Path, PathBuf};

use anyhow::{Result, anyhow};
use chrono::Utc;
use engram::{
    ProjectId,
    backup::{
        CopyKind,
        target::{
            AdapterKind, DEFAULT_KEEP, DEFAULT_WINDOW_HOURS, TargetError, TargetRequest,
            TargetView, clear_target, set_target, show_targets,
        },
    },
};
use serde_json::json;

use super::graph::engram_home_and_project_digest;

/// The words that manage backup records rather than write a copy.
#[derive(clap::Subcommand, Debug)]
pub(crate) enum BackupCommand {
    /// Configure, show or clear where this project's copies go.
    #[command(subcommand)]
    Target(TargetCommand),
}

/// The target words.
#[derive(clap::Subcommand, Debug)]
pub(crate) enum TargetCommand {
    /// Record a target for one copy kind, replacing any earlier one; what was
    /// recorded for an earlier target stays as history. Copies nothing.
    Set {
        #[arg(long, value_enum)]
        kind: KindArg,
        #[arg(long, value_enum)]
        adapter: AdapterArg,
        /// The absolute directory that receives the copies.
        #[arg(long, value_name = "PATH")]
        dir: PathBuf,
        /// The operator who authorizes this destination to hold everything
        /// the kind carries.
        #[arg(long, value_name = "NAME")]
        disclosure_authorized_by: String,
        /// The operator who states that the destination leaves this machine;
        /// required for a directory target.
        #[arg(long, value_name = "NAME")]
        off_host_asserted_by: Option<String>,
        /// Hours a copy keeps the kind qualified.
        #[arg(long, value_name = "N", default_value_t = DEFAULT_WINDOW_HOURS)]
        window_hours: u32,
        /// How many copies the target keeps.
        #[arg(long, value_name = "N", default_value_t = DEFAULT_KEEP)]
        keep: u32,
    },
    /// Print this project's targets with both of the operator's statements.
    Show {
        #[arg(long)]
        json: bool,
    },
    /// Remove the target and recorded state of one kind.
    Clear {
        #[arg(long, value_enum)]
        kind: KindArg,
    },
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub(crate) enum KindArg {
    Store,
}

impl From<KindArg> for CopyKind {
    fn from(kind: KindArg) -> Self {
        match kind {
            KindArg::Store => Self::Store,
        }
    }
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
pub(crate) enum AdapterArg {
    Directory,
}

impl From<AdapterArg> for AdapterKind {
    fn from(adapter: AdapterArg) -> Self {
        match adapter {
            AdapterArg::Directory => Self::Directory,
        }
    }
}

pub(crate) fn run(database: &Path, project: &ProjectId, command: BackupCommand) -> Result<()> {
    let (home, _) = engram_home_and_project_digest(database)?;
    let BackupCommand::Target(command) = command;
    match command {
        TargetCommand::Set {
            kind,
            adapter,
            dir,
            disclosure_authorized_by,
            off_host_asserted_by,
            window_hours,
            keep,
        } => {
            let request = TargetRequest {
                kind: kind.into(),
                adapter: adapter.into(),
                dir,
                disclosure_authorized_by,
                off_host_asserted_by,
                window_hours,
                keep,
            };
            let view =
                set_target(home, project, &request, Utc::now()).map_err(|error| refusal(&error))?;
            println!("backup target set");
            print_view(&view);
        }
        TargetCommand::Show { json } => {
            let views = show_targets(home, project).map_err(|error| refusal(&error))?;
            if json {
                let targets = views
                    .iter()
                    .map(|view| {
                        let mut value = json!(view.config);
                        value["identity"] = json!(view.identity);
                        value["off_host"] = json!(off_host(view));
                        value["state_recorded"] = json!(view.state_recorded);
                        value
                    })
                    .collect::<Vec<_>>();
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "project": project.0,
                        "targets": targets,
                    }))?
                );
            } else if views.is_empty() {
                println!("No backup target configured for this project.");
            } else {
                for view in &views {
                    print_view(view);
                }
            }
        }
        TargetCommand::Clear { kind } => {
            let kind = CopyKind::from(kind);
            if clear_target(home, project, kind).map_err(|error| refusal(&error))? {
                println!("backup target cleared: {}", kind.as_str());
            } else {
                println!("No {} backup target was configured.", kind.as_str());
            }
        }
    }
    Ok(())
}

/// How far the destination is known to leave the machine.
fn off_host(view: &TargetView) -> &'static str {
    match view.config.adapter {
        AdapterKind::Directory => "off-host asserted; not verified",
    }
}

fn print_view(view: &TargetView) {
    let config = &view.config;
    println!(
        "{}: {} {}",
        config.kind.as_str(),
        config.adapter.as_str(),
        config.dir
    );
    println!("  identity: {}", view.identity.as_str());
    println!(
        "  window: {} h; keep: {} copies",
        config.window_hours, config.keep
    );
    println!(
        "  disclosure authorized by {} at {} (asserted)",
        config.disclosure_authorized.by,
        config.disclosure_authorized.at.to_rfc3339()
    );
    match &config.off_host_asserted {
        Some(statement) => println!(
            "  {}: stated by {} at {}",
            off_host(view),
            statement.by,
            statement.at.to_rfc3339()
        ),
        None => println!("  {}", off_host(view)),
    }
    if !view.state_recorded {
        println!("  no recorded state");
    }
}

fn refusal(error: &TargetError) -> anyhow::Error {
    anyhow!("{}: {error}", error.code())
}
