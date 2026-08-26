//! Minimal Engram operator CLI for initializing and checking the local store.

use std::{
    env, fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use engram::{McpServer, ProjectId, SessionId, SqliteStore, project_database_path};
use rmcp::{ServiceExt, transport::stdio};

#[derive(Debug, Parser)]
#[command(name = "engram", version, about)]
struct Cli {
    /// Stable project-id file shared by every worktree.
    #[arg(long, default_value = ".engram-project")]
    project_file: PathBuf,
    /// Host-local Engram data directory (or set `ENGRAM_HOME`).
    #[arg(long)]
    home: Option<PathBuf>,
    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Create or migrate the local Engram database.
    Init,
    /// Verify every immutable object in the local database.
    Doctor,
    /// Serve the coding-agent memory tools over MCP stdio.
    Mcp {
        /// Actor identity asserted by the host integration.
        #[arg(long)]
        actor_id: String,
        /// Durable runtime session identity used for task binding and privacy.
        #[arg(long)]
        session_id: String,
        /// Skill instruction that supplied this actor context, when available.
        #[arg(long)]
        source_skill: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let (project_id, database) = resolve_project(&cli.project_file, cli.home)?;
    match cli.command {
        Command::Init => initialize(&database),
        Command::Doctor => doctor(&database),
        Command::Mcp {
            actor_id,
            session_id,
            source_skill,
        } => serve_mcp(database, project_id, actor_id, session_id, source_skill).await,
    }
}

fn resolve_project(project_file: &Path, home: Option<PathBuf>) -> Result<(ProjectId, PathBuf)> {
    let project_id = fs::read_to_string(project_file)
        .with_context(|| format!("failed to read {}", project_file.display()))?;
    let project_id = project_id.trim();
    if project_id.is_empty() {
        bail!("project id in {} is empty", project_file.display());
    }
    let home = home.or_else(|| env::var_os("ENGRAM_HOME").map(PathBuf::from));
    let home = home.context("pass --home or set ENGRAM_HOME")?;
    let project_id = ProjectId(project_id.to_owned());
    let database = project_database_path(&home, &project_id);
    Ok((project_id, database))
}

fn initialize(database: &Path) -> Result<()> {
    if let Some(parent) = database.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    SqliteStore::open(database)
        .with_context(|| format!("failed to initialize {}", database.display()))?;
    println!("Initialized local Engram store at {}", database.display());
    Ok(())
}

fn doctor(database: &Path) -> Result<()> {
    let store = SqliteStore::open(database)
        .with_context(|| format!("failed to open {}", database.display()))?;
    let report = store.verify_all()?;
    if !report.is_healthy() {
        bail!(
            "integrity check failed for {} object(s)",
            report.invalid_objects.len()
        );
    }
    println!(
        "Engram store is healthy ({} immutable object(s) checked)",
        report.checked_objects
    );
    eprintln!("WARNING: development no-op redactor is active; no secret or PII protection");
    Ok(())
}

async fn serve_mcp(
    database: PathBuf,
    project_id: ProjectId,
    actor_id: String,
    session_id: String,
    source_skill: Option<String>,
) -> Result<()> {
    let server = McpServer::new(
        database,
        project_id,
        actor_id,
        SessionId(session_id),
        source_skill,
    )
    .serve(stdio())
    .await
    .context("failed to start Engram MCP stdio server")?;
    server
        .waiting()
        .await
        .context("Engram MCP stdio server stopped with an error")?;
    Ok(())
}
