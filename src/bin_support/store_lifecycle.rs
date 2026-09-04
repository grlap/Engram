//! The operator `init`, `backup`, and `restore` words.

use std::{
    fs,
    path::{Path, PathBuf},
};

use anyhow::{Context, Result, bail};
use engram::domain::AssuranceLevel;
use engram::{
    ActorContext, ControlAssurance, ControlPolicy, DevelopmentNoopRedactor, HostPathPolicy,
    ProjectPolicyAuthorityDecision, SqliteStore,
};

use crate::{control_assurance_name, warn_if_action_gated};

use super::graph::engram_home_and_project_digest;

pub(crate) fn initialize(
    database: &Path,
    identity: Option<HostPathPolicy>,
    required_assurance: Option<ControlAssurance>,
    authorized_by: Option<String>,
    reason: Option<String>,
) -> Result<()> {
    if let Some(parent) = database.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let requested_attribution = authorized_by
        .as_deref()
        .zip(reason.as_deref())
        .map(|(actor_id, reason)| (actor_id.trim().to_owned(), reason.trim().to_owned()));
    let store = match (required_assurance, authorized_by, reason) {
        (None, None, None) => SqliteStore::open_with_host_path_identity(database, identity),
        (Some(required_assurance), Some(authorized_by), Some(reason)) => {
            SqliteStore::open_with_initial_control_assurance(
                database,
                identity,
                required_assurance,
                &ActorContext {
                    actor_id: authorized_by,
                    actor_kind: "host_operator".into(),
                    assurance: AssuranceLevel::Asserted,
                    run_id: None,
                    session_id: None,
                    source_tool: Some("cli:init".into()),
                    source_skill: None,
                    provenance_chain: Vec::new(),
                    reason: "authorize an explicit project bootstrap control policy".into(),
                },
                &reason,
                &DevelopmentNoopRedactor,
            )
        }
        _ => bail!("--required-assurance, --authorized-by, and --reason must be supplied together"),
    }
    .with_context(|| format!("failed to initialize {}", database.display()))?;
    let control = store.control_diagnostics()?;
    let bootstrap_attribution_recorded = if let Some((actor_id, reason)) = requested_attribution {
        let policy: ControlPolicy = store
            .get(&control.active_policy)?
            .context("active bootstrap policy object is missing")?;
        let authority: ProjectPolicyAuthorityDecision = store
            .get(&policy.authority)?
            .context("active bootstrap policy authority object is missing")?;
        authority.authorized_by.actor_id == actor_id && authority.reason == reason
    } else {
        false
    };
    if bootstrap_attribution_recorded {
        eprintln!(
            "WARNING: bootstrap policy administrator identity is asserted host context, not an authenticated identity"
        );
    }
    warn_if_action_gated(control.required_assurance);
    println!(
        "Initialized local Engram store at {} with policy {} (epoch {}, required {}, obligation rules {})",
        database.display(),
        control.active_policy,
        control.policy_epoch.0,
        control_assurance_name(control.required_assurance),
        control.obligation_rule_set,
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

/// Restores a verified backup as the project store. The backup is verified
/// read-only before anything is touched, staged beside the store, verified
/// again, and only then installed. Replacing an existing store first
/// checkpoints and truncates its write-ahead log under the writer lock so no
/// stale log can be applied to the restored file; no other Engram process
/// may use the store while it is replaced.
pub(crate) fn restore(database: &Path, from: &Path, replace: bool) -> Result<()> {
    let manifest = SqliteStore::verify_backup(from)
        .with_context(|| format!("backup {} is not a current Engram store", from.display()))?;
    if let (Ok(source), Ok(target)) = (fs::canonicalize(from), fs::canonicalize(database))
        && source == target
    {
        bail!("backup {} is the store itself", from.display());
    }
    let exists = database.exists();
    if exists && !replace {
        bail!(
            "store {} already exists; pass --replace to overwrite it with the backup",
            database.display()
        );
    }
    if let Some(parent) = database.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("failed to create {}", parent.display()))?;
    }
    let staged = PathBuf::from(format!(
        "{}.restore-{}.tmp",
        database.display(),
        std::process::id()
    ));
    let _ = fs::remove_file(&staged);
    fs::copy(from, &staged)
        .with_context(|| format!("failed to stage {} beside the store", from.display()))?;
    let install = (|| -> Result<()> {
        let staged_manifest = SqliteStore::verify_backup(&staged)
            .with_context(|| format!("staged copy {} failed verification", staged.display()))?;
        if staged_manifest.file_sha256 != manifest.file_sha256 {
            bail!("staged copy bytes differ from the backup");
        }
        if exists {
            // Fold the old log into the old file and truncate it under an
            // exclusive lock; a stale log must never meet the restored file.
            // A checkpoint that could not complete means another process still
            // holds the store, and replacement stops there.
            let old = rusqlite::Connection::open(database).with_context(|| {
                format!("failed to open {} for replacement", database.display())
            })?;
            old.execute_batch("PRAGMA locking_mode = EXCLUSIVE; BEGIN IMMEDIATE; COMMIT;")
                .context(
                    "failed to take the store being replaced; is another Engram process using it?",
                )?;
            let (busy, log_frames, checkpointed): (i64, i64, i64) = old
                .query_row("PRAGMA wal_checkpoint(TRUNCATE)", [], |row| {
                    Ok((row.get(0)?, row.get(1)?, row.get(2)?))
                })
                .context("failed to checkpoint the store being replaced")?;
            if busy != 0 || log_frames != checkpointed {
                bail!(
                    "another process still uses {}; stop it and retry (checkpoint busy={busy}, log={log_frames}, checkpointed={checkpointed})",
                    database.display()
                );
            }
            drop(old);
            for suffix in ["-wal", "-shm", "-journal"] {
                let sidecar = PathBuf::from(format!("{}{suffix}", database.display()));
                if sidecar.exists() {
                    fs::remove_file(&sidecar)
                        .with_context(|| format!("failed to remove {}", sidecar.display()))?;
                }
            }
            fs::rename(&staged, database)
                .with_context(|| format!("failed to install {} as the store", staged.display()))?;
        } else {
            // Nothing may be replaced without --replace, not even a store that
            // appeared while the copy was being staged.
            engram::install_store_copy_without_replacing(&staged, database)
                .with_context(|| format!("failed to install {} as the store", staged.display()))?;
        }
        Ok(())
    })();
    if let Err(error) = install {
        let _ = fs::remove_file(&staged);
        return Err(error);
    }
    let restored = SqliteStore::verify_backup(database)
        .with_context(|| format!("restored store {} failed verification", database.display()))?;
    if restored.file_sha256 != manifest.file_sha256 {
        bail!("restored store bytes differ from the backup");
    }
    println!("{}", serde_json::to_string_pretty(&restored)?);
    Ok(())
}
