//! Explicit-file migration commands never resolve the active project store.

use std::{path::PathBuf, process::ExitCode};

use anyhow::Result;
use clap::Subcommand;

#[derive(Debug, Subcommand)]
pub(crate) enum MigrationCommand {
    /// Convert a supported current or aggregate-root archive to a new offline store file.
    /// Preserves private data and same-host state; does not install or activate it.
    Import {
        #[arg(long)]
        archive: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// Export one coherent SQLite snapshot, including private and restricted data.
    /// This creates a migration archive, not an installed store or a graph snapshot.
    Export {
        /// Existing source database; may use a different Engram durable format.
        #[arg(long)]
        database: PathBuf,
        /// New archive file. Existing files are never replaced.
        #[arg(long)]
        out: PathBuf,
    },
    /// Check an export against its embedded manifest; this does not import it.
    Verify {
        #[arg(long)]
        archive: PathBuf,
    },
    /// Compare all exported categories and typed bytes to a fixed source snapshot.
    Compare {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        archive: PathBuf,
    },
    /// Reconstruct a scratch copy in the SOURCE format. Does not upgrade or install.
    Unpack {
        #[arg(long)]
        archive: PathBuf,
        #[arg(long)]
        out: PathBuf,
    },
    /// Backup, export, import and validate into a fixed operation directory.
    /// Does not move the live store. Requires attested coordinated downtime.
    Prepare {
        #[arg(long)]
        database: PathBuf,
        #[arg(long)]
        operation: PathBuf,
        #[arg(long)]
        old_executable: PathBuf,
        #[arg(long)]
        offline_confirmed: bool,
    },
    /// Inspect journal records and actual files in an operation directory.
    Status {
        #[arg(long)]
        operation: PathBuf,
    },
    /// Move the live store aside and publish the validated candidate.
    Activate {
        #[arg(long)]
        operation: PathBuf,
        #[arg(long)]
        offline_confirmed: bool,
    },
    /// Restore the retained original before finalize. Refused after finalize.
    Rollback {
        #[arg(long)]
        operation: PathBuf,
        #[arg(long)]
        offline_confirmed: bool,
    },
    /// Close automatic rollback forever after the candidate is live.
    Finalize {
        #[arg(long)]
        operation: PathBuf,
        #[arg(long)]
        offline_confirmed: bool,
    },
    /// Reconcile interrupted filesystem effects from actual files.
    Recover {
        #[arg(long)]
        operation: PathBuf,
        #[arg(long)]
        offline_confirmed: bool,
    },
}

pub(crate) fn run(command: &MigrationCommand) -> Result<ExitCode> {
    match command {
        MigrationCommand::Import { archive, out } => {
            eprintln!(
                "WARNING: offline same-host import includes private data and authority records. Do not activate this copy alongside its source or on another host."
            );
            let report = engram::storage::migration::import_archive(archive, out)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        MigrationCommand::Export { database, out } => {
            eprintln!(
                "WARNING: full export includes private scratch, restricted bodies and host authority records. Protect it like the original store. Export does not grant authority on another host."
            );
            let manifest = engram::storage::migration::export_store(database, out)?;
            println!("{}", serde_json::to_string_pretty(&manifest)?);
        }
        MigrationCommand::Verify { archive } => {
            let manifest = engram::storage::migration::verify_export(archive)?;
            println!("{}", serde_json::to_string_pretty(&manifest)?);
        }
        MigrationCommand::Compare { database, archive } => {
            let manifest = engram::storage::migration::compare_export_to_source(database, archive)?;
            println!("{}", serde_json::to_string_pretty(&manifest)?);
        }
        MigrationCommand::Unpack { archive, out } => {
            eprintln!(
                "WARNING: this creates a scratch database in its SOURCE format, with original private data and authority records. It does not upgrade, install, or authorize activating the copy."
            );
            let manifest = engram::storage::migration::restore_source_layout(archive, out)?;
            println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "operation": "unpack_source_format", "upgraded": false,
                    "installed": false, "manifest": manifest,
                }))?
            );
        }
        MigrationCommand::Prepare {
            database,
            operation,
            old_executable,
            offline_confirmed,
        } => {
            warn_offline();
            let report = engram::storage::migration::prepare_upgrade(
                &engram::storage::migration::UpgradePrepareRequest {
                    database: database.clone(),
                    operation: operation.clone(),
                    old_executable: old_executable.clone(),
                    offline_confirmed: *offline_confirmed,
                },
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        MigrationCommand::Status { operation } => {
            let report = engram::storage::migration::upgrade_status(operation)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        MigrationCommand::Activate {
            operation,
            offline_confirmed,
        } => {
            warn_offline();
            let report = engram::storage::migration::activate_upgrade(
                &engram::storage::migration::UpgradeOperationRequest {
                    operation: operation.clone(),
                    offline_confirmed: *offline_confirmed,
                },
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        MigrationCommand::Rollback {
            operation,
            offline_confirmed,
        } => {
            warn_offline();
            let report = engram::storage::migration::rollback_upgrade(
                &engram::storage::migration::UpgradeOperationRequest {
                    operation: operation.clone(),
                    offline_confirmed: *offline_confirmed,
                },
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        MigrationCommand::Finalize {
            operation,
            offline_confirmed,
        } => {
            warn_offline();
            let report = engram::storage::migration::finalize_upgrade(
                &engram::storage::migration::UpgradeOperationRequest {
                    operation: operation.clone(),
                    offline_confirmed: *offline_confirmed,
                },
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        MigrationCommand::Recover {
            operation,
            offline_confirmed,
        } => {
            warn_offline();
            let report = engram::storage::migration::recover_upgrade(
                &engram::storage::migration::UpgradeOperationRequest {
                    operation: operation.clone(),
                    offline_confirmed: *offline_confirmed,
                },
            )?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
    }
    Ok(ExitCode::SUCCESS)
}

fn warn_offline() {
    eprintln!(
        "WARNING: --offline-confirmed attests coordinated downtime. It is not a store lock and does not exclude other processes."
    );
}
