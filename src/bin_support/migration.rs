//! Explicit-file migration commands never resolve the active project store.

use std::{path::PathBuf, process::ExitCode};

use anyhow::Result;
use clap::Subcommand;

#[derive(Debug, Subcommand)]
pub(crate) enum MigrationCommand {
    /// Convert the supported aggregate-root archive to a new offline store file.
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
}

pub(crate) fn run(command: &MigrationCommand) -> Result<ExitCode> {
    match command {
        MigrationCommand::Import { archive, out } => {
            eprintln!(
                "WARNING: offline same-host import includes private data and authority records. Do not activate this copy alongside its source or on another host."
            );
            let report = engram::storage::migration::import_aggregate_archive(archive, out)?;
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
    }
    Ok(ExitCode::SUCCESS)
}
