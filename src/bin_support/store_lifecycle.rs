//! The operator `init`, `backup`, and `restore` words.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result};
use engram::{HostPathPolicy, SqliteStore};

use super::graph::engram_home_and_project_digest;

pub(crate) fn initialize(database: &Path, identity: Option<HostPathPolicy>) -> Result<()> {
    if let Some(parent) = database.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let store = SqliteStore::open_with_host_path_identity(database, identity)
        .with_context(|| format!("failed to initialize {}", database.display()))?;
    let policy = store.control_diagnostics()?;
    println!(
        "Initialized local Engram store at {} with policy {} (epoch {}, obligation rules {})",
        database.display(),
        policy.active_policy,
        policy.policy_epoch.0,
        policy.obligation_rule_set
    );
    Ok(())
}

pub(crate) fn backup(database: &Path, out: Option<PathBuf>) -> Result<()> {
    let store = SqliteStore::open_unresolved(database)
        .with_context(|| format!("failed to open {}", database.display()))?;
    let out = if let Some(out) = out {
        out
    } else {
        {
            // <home>/projects/<digest>/engram.db → <home>/backups/<digest>/engram-<utc>.db
            let (home, digest) = engram_home_and_project_digest(database)?;
            home.join("backups").join(digest).join(format!(
                "engram-{}.db",
                chrono::Utc::now().format("%Y%m%dT%H%M%SZ")
            ))
        }
    };
    let manifest = store
        .backup_to(&out)
        .with_context(|| format!("failed to back up {}", database.display()))?;
    println!("{}", serde_json::to_string_pretty(&manifest)?);
    eprintln!(
        "WARNING: a backup is a complete store, host-private state and private scratch included; keep it where the store may be kept"
    );
    Ok(())
}

/// Restores a local file through the shared restore-occurrence lifecycle.
pub(crate) fn restore(
    home: &Path,
    project: &engram::ProjectId,
    database: &Path,
    from: &Path,
    replace: bool,
    abandon_pending: bool,
    abandoned_by: Option<&str>,
) -> Result<()> {
    let settings = super::backup::restore::RestoreSettings::new(
        super::backup::fetch::ReadSettings::new(super::backup::fetch::DEFAULT_READ_DEADLINE),
    );
    if abandon_pending {
        let abandoned = super::backup::restore::abandon_local(
            home,
            project,
            database,
            from,
            abandoned_by,
            &settings,
        )?;
        println!("{}", serde_json::to_string_pretty(&abandoned)?);
        return Ok(());
    }
    let restored =
        super::backup::restore::restore_local(home, project, database, from, replace, &settings)?;
    println!("{}", serde_json::to_string_pretty(&restored)?);
    Ok(())
}
