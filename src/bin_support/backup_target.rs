//! The operator `backup target` words, which set, show and clear a project's
//! backup targets and read and write only the records under the Engram home,
//! and `backup push`, which brings each configured kind's copy up to date.

use std::{
    path::{Path, PathBuf},
    time::Duration,
};

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

use super::{
    backup::push::{
        DEFAULT_CAPTURE_DEADLINE, DEFAULT_TRANSPORT_DEADLINE, KindReport, Outcome, PushSettings,
        push,
    },
    graph::engram_home_and_project_digest,
};

/// The words that manage backup targets and push copies to them.
#[derive(clap::Subcommand, Debug)]
pub(crate) enum BackupCommand {
    /// Configure, show or clear where this project's copies go.
    #[command(subcommand)]
    Target(TargetCommand),
    /// Capture the store and bring the copy at each configured target up to
    /// date. Exits 0 when nothing is configured or another push is running,
    /// and 1 when a push failed.
    Push {
        /// Push only this kind; every configured kind by default.
        #[arg(long, value_enum)]
        kind: Option<KindArg>,
        #[arg(long)]
        json: bool,
        /// Seconds the capture may take.
        #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_CAPTURE_DEADLINE.as_secs())]
        capture_deadline_secs: u64,
        /// Seconds the requests to the target may take together.
        #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_TRANSPORT_DEADLINE.as_secs())]
        transport_deadline_secs: u64,
    },
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

/// Runs a backup word. Returns whether it succeeded; a push that failed has
/// already printed its report.
pub(crate) fn run(database: &Path, project: &ProjectId, command: BackupCommand) -> Result<bool> {
    let (home, _) = engram_home_and_project_digest(database)?;
    let command = match command {
        BackupCommand::Target(command) => command,
        BackupCommand::Push {
            kind,
            json,
            capture_deadline_secs,
            transport_deadline_secs,
        } => {
            let settings = PushSettings::new(
                Duration::from_secs(capture_deadline_secs),
                Duration::from_secs(transport_deadline_secs),
            );
            let kinds = kind.map_or_else(|| CopyKind::ALL.to_vec(), |kind| vec![kind.into()]);
            let mut reports = Vec::new();
            let mut abandoned = None;
            for kind in kinds {
                let run = push(home, project, kind, &settings);
                reports.push(run.report);
                if run.abandoned.is_some() {
                    // No further kind is pushed while a worker may still run.
                    abandoned = run.abandoned;
                    break;
                }
            }
            print_push(project, &reports, json)?;
            if let Some(abandoned) = abandoned {
                // A request to the target passed its deadline and may still be
                // running. Ending the process is what stops it, and the push
                // lock is held until then.
                let _held = abandoned;
                std::process::exit(1);
            }
            return Ok(reports
                .iter()
                .all(|report| report.outcome != Outcome::Failed));
        }
    };
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
    Ok(true)
}

fn print_push(project: &ProjectId, reports: &[KindReport], json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "project": project.0,
                "kinds": reports,
            }))?
        );
        return Ok(());
    }
    for report in reports {
        let kind = report.kind.as_str();
        match report.outcome {
            Outcome::NotConfigured => {
                println!("No {kind} backup target is configured for this project.");
            }
            Outcome::Busy => println!("{kind}: another push is running; nothing was done."),
            Outcome::Uploaded | Outcome::Unchanged => {
                let verb = if report.outcome == Outcome::Uploaded {
                    "copy put and read back"
                } else {
                    "unchanged; the newest copy was confirmed"
                };
                let copy = report
                    .receipt
                    .as_ref()
                    .map_or("", |receipt| receipt.manifest.copy.as_str());
                println!("{kind}: {verb}: {copy}");
            }
            Outcome::Failed => println!(
                "{kind}: push failed: {}: {}",
                report.code.as_deref().unwrap_or("backup_io"),
                report.message.as_deref().unwrap_or("")
            ),
        }
        for (label, copy) in [
            ("recovered pending copy", &report.recovered),
            ("dropped pending attempt", &report.dropped),
            ("set aside attempt for another target", &report.set_aside),
            ("attempt left pending", &report.pending),
        ] {
            if let Some(copy) = copy {
                println!("  {label}: {copy}");
            }
        }
        for warning in &report.warnings {
            println!("  warning: {warning}");
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
