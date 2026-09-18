//! Explicit-file migration commands never resolve the active project store.

use std::{path::PathBuf, process::ExitCode};

use anyhow::Result;
use clap::Subcommand;

#[derive(Debug, Subcommand)]
pub(crate) enum MigrationCommand {
    /// Write every row of a store to a JSON Lines file, private data included.
    /// The source is only read; it may be a store this build refuses to open.
    Export {
        /// Existing store database.
        #[arg(long)]
        database: PathBuf,
        /// New JSON Lines file. An existing file is never replaced.
        #[arg(long)]
        out: PathBuf,
    },
    /// Create a new store in the current format from an export file.
    /// Record ids are kept as they are. Does not install or activate the store.
    Import {
        /// File written by `migration export`.
        #[arg(long)]
        file: PathBuf,
        /// New store database. An existing file is never replaced.
        #[arg(long)]
        out: PathBuf,
    },
}

pub(crate) fn run(command: &MigrationCommand) -> Result<ExitCode> {
    match command {
        MigrationCommand::Export { database, out } => {
            eprintln!(
                "WARNING: the export holds private scratch, restricted bodies and host authority records. Protect it like the store itself."
            );
            let report = engram::storage::migration::export_json(database, out)?;
            if report.wal_bytes > 0 {
                eprintln!(
                    "WARNING: the source has a {}-byte write-ahead log beside it. The export read the committed frames it holds; a backup of the source is the database file together with its -wal and -shm files, and none of them may be left beside another database.",
                    report.wal_bytes
                );
            }
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
        MigrationCommand::Import { file, out } => {
            eprintln!(
                "WARNING: the imported store holds the same private data and authority records as its source. Do not run it alongside the source or on another host."
            );
            let report = engram::storage::migration::import_json(file, out)?;
            println!("{}", serde_json::to_string_pretty(&report)?);
        }
    }
    Ok(ExitCode::SUCCESS)
}
