//! The operator `backup target` words, which set, show and clear a project's
//! backup targets and read and write only the records under the Engram home;
//! `backup push`, which brings each configured kind's copy up to date; and
//! `backup list` and `backup fetch`, which read the copies a target holds;
//! and `backup restore`, which installs one onto a home without a store.

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
    backup::check::{
        CheckOutcome, CheckReport, CheckSettings, DEFAULT_CHECK_DEADLINE, check_targets,
    },
    backup::fetch::{
        DEFAULT_READ_DEADLINE, Fetched, Listing, ReadFailure, ReadSettings, fetch, list,
    },
    backup::push::{
        DEFAULT_CAPTURE_DEADLINE, DEFAULT_TRANSPORT_DEADLINE, KindReport, Outcome, PushSettings,
        push,
    },
    backup::restore::{RestoreSettings, Restored, report_text, restore},
    graph::engram_home_and_project_digest,
};

/// The words that manage backup targets and push copies to them.
#[derive(clap::Subcommand, Debug)]
pub(crate) enum BackupCommand {
    /// Configure, show or clear where this project's copies go.
    #[command(subcommand)]
    Target(TargetCommand),
    /// Report the backup mode, with what backs it, and each kind's recorded
    /// evidence. Reads only local files and the store unless asked to check
    /// the targets.
    Status {
        #[arg(long)]
        json: bool,
        /// Ask each target to confirm the newest copy, recording a
        /// confirmation or a missing copy under the push lock.
        #[arg(long)]
        check_target: bool,
        /// Seconds a target check may take, at least 1.
        #[arg(
            long,
            value_name = "SECONDS",
            default_value_t = DEFAULT_CHECK_DEADLINE.as_secs(),
            value_parser = clap::value_parser!(u64).range(1..),
            requires = "check_target"
        )]
        check_deadline_secs: u64,
    },
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
    /// Print the manifests of the copies the configured target holds for
    /// this project. Opens no store.
    List {
        #[arg(long, value_enum, default_value = "store")]
        kind: KindArg,
        #[arg(long)]
        json: bool,
        /// Seconds the requests to the target may take together.
        #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_READ_DEADLINE.as_secs())]
        deadline_secs: u64,
    },
    /// Write one copy, decoded to no more than the length its manifest
    /// declares and checked against that manifest, to a new file. Checks
    /// first that the local disk has room for the stored file and the
    /// uncompressed copy. A hidden staging file that the move to the new file
    /// left behind is named in a warning. Opens no store.
    Fetch {
        /// The copy's name, as `backup list` prints it.
        copy: String,
        /// The file to write; it must not exist.
        #[arg(long, value_name = "FILE")]
        out: PathBuf,
        #[arg(long, value_enum, default_value = "store")]
        kind: KindArg,
        #[arg(long)]
        json: bool,
        /// Seconds the requests to the target may take together.
        #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_READ_DEADLINE.as_secs())]
        deadline_secs: u64,
    },
    /// Install one `store` copy from the configured target onto this home,
    /// which must hold no store for the project. Changes no row of the copy.
    Restore {
        /// The copy's name, as `backup list` prints it.
        copy: String,
        /// The operator who states that the origin store will never run
        /// again; recorded as asserted context.
        #[arg(long, value_name = "NAME")]
        origin_retired_by: Option<String>,
        #[arg(long)]
        json: bool,
        /// Seconds the requests to the target may take together.
        #[arg(long, value_name = "SECONDS", default_value_t = DEFAULT_READ_DEADLINE.as_secs())]
        deadline_secs: u64,
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
        BackupCommand::Status {
            json,
            check_target,
            check_deadline_secs,
        } => {
            // The check goes first, so the status shows what it recorded.
            let checks = check_target.then(|| {
                check_targets(
                    home,
                    project,
                    &CheckSettings::new(Duration::from_secs(check_deadline_secs)),
                    Utc::now(),
                )
            });
            let status = engram::backup::status::backup_status(home, project, database);
            let reports = checks.as_ref().map(|run| run.reports.as_slice());
            print_status(&status, reports, json)?;
            if checks.is_some_and(|run| run.abandoned.is_some()) {
                // A check passed its deadline inside the operating system;
                // ending the process is what stops it. It recorded nothing.
                std::process::exit(0);
            }
            return Ok(true);
        }
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
            let succeeded = reports
                .iter()
                .all(|report| report.outcome != Outcome::Failed);
            if let Some(abandoned) = abandoned {
                // A request to the target passed its deadline and may still be
                // running. Ending the process is what stops it, and the push
                // lock is held until then.
                let _held = abandoned;
                std::process::exit(i32::from(!succeeded));
            }
            return Ok(succeeded);
        }
        BackupCommand::List {
            kind,
            json,
            deadline_secs,
        } => {
            let run = list(
                home,
                project,
                kind.into(),
                &ReadSettings::new(Duration::from_secs(deadline_secs)),
            );
            let listing = finish(run.outcome, run.abandoned.is_some(), json)?;
            print_listing(kind.into(), &listing, json)?;
            return Ok(true);
        }
        BackupCommand::Fetch {
            copy,
            out,
            kind,
            json,
            deadline_secs,
        } => {
            let run = fetch(
                home,
                project,
                kind.into(),
                &copy,
                &out,
                &ReadSettings::new(Duration::from_secs(deadline_secs)),
            );
            let fetched = finish(run.outcome, run.abandoned.is_some(), json)?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&fetch_receipt(&fetched))?
                );
            } else {
                print!("{}", fetch_text(&fetched));
            }
            return Ok(true);
        }
        BackupCommand::Restore {
            copy,
            origin_retired_by,
            json,
            deadline_secs,
        } => {
            let run = restore(
                home,
                project,
                database,
                &copy,
                origin_retired_by.as_deref(),
                &RestoreSettings::new(ReadSettings::new(Duration::from_secs(deadline_secs))),
            );
            let restored = finish(run.outcome, run.abandoned.is_some(), json)?;
            print_restored(&restored, json)?;
            return Ok(true);
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

fn print_status(
    status: &engram::backup::status::BackupStatus,
    checks: Option<&[CheckReport]>,
    json: bool,
) -> Result<()> {
    if json {
        let mut value = serde_json::to_value(status)?;
        if let Some(checks) = checks {
            value["checks"] = serde_json::to_value(checks)?;
        }
        println!("{}", serde_json::to_string_pretty(&value)?);
        return Ok(());
    }
    print!("{}", engram::backup::status::render_status(status));
    for check in checks.unwrap_or_default() {
        let found = match check.outcome {
            CheckOutcome::Confirmed => "confirmed".to_owned(),
            CheckOutcome::NothingToCheck => "nothing to check".to_owned(),
            _ => format!(
                "{}: {}",
                check.code.unwrap_or("backup_target_unconfirmed"),
                check.reason.as_deref().unwrap_or("")
            ),
        };
        let recorded = if check.recorded {
            "recorded".to_owned()
        } else if let Some(why) = &check.not_recorded {
            format!("not recorded: {why}")
        } else {
            "nothing recorded".to_owned()
        };
        let copy = check
            .copy
            .as_deref()
            .map_or_else(String::new, |copy| format!(" ({copy})"));
        println!("check {}{copy}: {found}; {recorded}", check.kind.as_str());
    }
    Ok(())
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
        for copy in &report.removed {
            println!("  removed beyond the retention count: {copy}");
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

/// The schema of the `--json` receipts of `backup list` and `backup fetch`.
const READ_SCHEMA_VERSION: u32 = 1;

/// The outcome of a list or fetch. A refusal is also printed as a JSON
/// object with its code under `--json`. A worker left running past its
/// deadline is stopped by ending the process, after the refusal is printed.
fn finish<T>(
    outcome: std::result::Result<T, ReadFailure>,
    abandoned: bool,
    json: bool,
) -> Result<T> {
    match outcome {
        Ok(value) => Ok(value),
        Err(failure) => {
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({
                        "schema_version": READ_SCHEMA_VERSION,
                        "code": failure.code,
                        "message": failure.message,
                    }))?
                );
            }
            if abandoned {
                eprintln!("error: {}: {}", failure.code, failure.message);
                std::process::exit(1);
            }
            Err(anyhow!("{}: {}", failure.code, failure.message))
        }
    }
}

fn print_restored(restored: &Restored, json: bool) -> Result<()> {
    let authority = &restored.report.authority;
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "schema_version": READ_SCHEMA_VERSION,
                "copy": restored.copy,
                "store": restored.store.display().to_string(),
                "bytes": restored.bytes,
                "sha256": restored.sha256,
                "origin_host": restored.origin_host,
                "origin_retired_by": restored.origin_retired_by,
                "completed_interrupted": restored.completed_interrupted,
                "kept_record": restored.kept_record.as_ref().map(|path| path.display().to_string()),
                "authority": authority,
            }))?
        );
        return Ok(());
    }
    print!("{}", report_text(restored));
    Ok(())
}

/// The `--json` receipt of a fetch.
pub(crate) fn fetch_receipt(fetched: &Fetched) -> serde_json::Value {
    let manifest = &fetched.manifest;
    json!({
        "schema_version": READ_SCHEMA_VERSION,
        "copy": manifest.copy,
        "out": fetched.out.display().to_string(),
        "bytes": manifest.capture.bytes,
        "sha256": manifest.capture.sha256,
        "manifest": manifest,
        "warnings": fetched.warnings,
    })
}

/// The text a fetch prints: one line, then one per warning.
pub(crate) fn fetch_text(fetched: &Fetched) -> String {
    let manifest = &fetched.manifest;
    let mut lines = vec![format!(
        "fetched {} to {}: {} bytes, sha256 {}",
        manifest.copy,
        fetched.out.display(),
        manifest.capture.bytes,
        manifest.capture.sha256
    )];
    lines.extend(
        fetched
            .warnings
            .iter()
            .map(|warning| format!("warning: {warning}")),
    );
    lines.into_iter().map(|line| line + "\n").collect()
}

fn print_listing(kind: CopyKind, listing: &Listing, json: bool) -> Result<()> {
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "schema_version": READ_SCHEMA_VERSION,
                "kind": kind.as_str(),
                "copies": listing.manifests,
                "unreadable": listing.unreadable,
                "foreign": listing.foreign,
            }))?
        );
        return Ok(());
    }
    if listing.manifests.is_empty() {
        println!(
            "The target holds no {} copy of this project.",
            kind.as_str()
        );
    }
    for manifest in &listing.manifests {
        let capture = &manifest.capture;
        println!(
            "{}: captured {}, {} bytes ({} stored), sha256 {}, cut work feed {} memory {}",
            manifest.copy,
            capture.capture_started_at.to_rfc3339(),
            capture.bytes,
            manifest.stored_bytes,
            capture.sha256,
            capture.cut.work_feed,
            capture.cut.project_memory
        );
        println!(
            "  format {}, build {}, source {}, host {}",
            capture.format_identity.as_str(),
            capture
                .build_fingerprint
                .as_ref()
                .map_or("unknown", engram::ObjectId::as_str),
            capture.source_revision.as_deref().unwrap_or("unknown"),
            capture.host_name.as_deref().unwrap_or("unknown")
        );
    }
    for name in &listing.unreadable {
        println!("unreadable manifest: {name}");
    }
    for copy in &listing.foreign {
        println!("manifest of another project: {copy}");
    }
    Ok(())
}

fn refusal(error: &TargetError) -> anyhow::Error {
    anyhow!("{}: {error}", error.code())
}
