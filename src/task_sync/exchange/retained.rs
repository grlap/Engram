//! Pin an existing exact local commit under an operation-specific private ref.
//! These operations cannot update a branch, index or remote.

use std::{
    path::{Path, PathBuf},
    time::Instant,
};

use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};

use super::{Cancellation, RunLimits, execute, remaining};
use crate::{
    ObjectId,
    task_sync::{
        git,
        ledger::{RetainedCommit, oid},
        retention::{commit_selection, reserve_commit},
    },
};

/// Retain an already-created local commit without recreating its metadata.
/// All commands share one bounded deadline and use the confined local runner.
///
/// # Errors
/// Refuses invalid objects, a conflicting private ref, cancellation or limits.
/// A failed publication is preserved; no branch or remote result is implied.
pub async fn retain_commit(
    repository: &Path,
    operation: uuid::Uuid,
    commit: &str,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<RetainedCommit> {
    ensure!(
        !operation.is_nil() && repository.is_absolute(),
        "invalid commit retention selection"
    );
    oid(commit)?;
    let started = Instant::now();
    let git_directory = git_directory(repository, remaining(limits, started)?, cancel).await?;
    let reference = format!("refs/engram/task-sync/exchange/{operation}/commit");
    let retained = inspect(
        repository,
        commit,
        &reference,
        remaining(limits, started)?,
        cancel,
    )
    .await?;
    if let Some(selected) = commit_selection(&git_directory, operation)? {
        ensure!(
            selected == retained,
            "retained commit selection differs; preserve it"
        );
        verify_retained_commit(repository, &selected, remaining(limits, started)?, cancel).await?;
        return Ok(selected);
    }
    let current = local(
        repository,
        &[
            "for-each-ref",
            "--format=%(objectname) %(symref)",
            &reference,
        ],
        remaining(limits, started)?,
        cancel,
    )
    .await?;
    ensure!(
        current.is_empty(),
        "unrecorded private commit ref exists; preserve it"
    );
    reserve_commit(&git_directory, operation, &retained)?;
    let absent = "0".repeat(commit.len());
    local(
        repository,
        &["update-ref", "--no-deref", &reference, commit, &absent],
        remaining(limits, started)?,
        cancel,
    )
    .await?;
    verify_retained_commit(repository, &retained, remaining(limits, started)?, cancel).await?;
    Ok(retained)
}

/// Verify the exact retained commit and ref. Missing state is never recreated.
///
/// # Errors
/// Refuses a missing/moved ref, object changes and bounded process failures.
pub async fn verify_retained_commit(
    repository: &Path,
    retained: &RetainedCommit,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<()> {
    retained.validate()?;
    ensure!(repository.is_absolute(), "repository path must be absolute");
    let started = Instant::now();
    let operation = retained
        .retention_ref
        .strip_prefix("refs/engram/task-sync/exchange/")
        .and_then(|rest| rest.strip_suffix("/commit"))
        .context("invalid retention operation ref")?;
    let operation = uuid::Uuid::parse_str(operation)?;
    ensure!(
        retained.retention_ref == format!("refs/engram/task-sync/exchange/{operation}/commit"),
        "noncanonical retention operation ref"
    );
    let git_directory = git_directory(repository, remaining(limits, started)?, cancel).await?;
    ensure!(
        commit_selection(&git_directory, operation)?.as_ref() == Some(retained),
        "retained commit selection is missing or changed"
    );
    let current = local(
        repository,
        &[
            "for-each-ref",
            "--format=%(objectname) %(symref)",
            &retained.retention_ref,
        ],
        remaining(limits, started)?,
        cancel,
    )
    .await?;
    ensure!(
        current == format!("{} \n", retained.oid).as_bytes(),
        "retained commit ref is missing or changed; preserve uncertainty"
    );
    ensure!(
        inspect(
            repository,
            &retained.oid,
            &retained.retention_ref,
            remaining(limits, started)?,
            cancel
        )
        .await?
            == *retained,
        "retained commit bytes differ"
    );
    Ok(())
}

async fn inspect(
    repository: &Path,
    commit: &str,
    reference: &str,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<RetainedCommit> {
    let started = Instant::now();
    ensure!(
        local(
            repository,
            &["cat-file", "-t", commit],
            remaining(limits, started)?,
            cancel
        )
        .await?
            == b"commit\n",
        "selected object is not an exact commit"
    );
    let bytes = local(
        repository,
        &["cat-file", "commit", commit],
        remaining(limits, started)?,
        cancel,
    )
    .await?;
    let headers = bytes
        .windows(2)
        .position(|pair| pair == b"\n\n")
        .context("commit header boundary missing")?;
    let mut lines = bytes[..headers].split(|byte| *byte == b'\n');
    let tree = std::str::from_utf8(lines.next().context("commit tree missing")?)?
        .strip_prefix("tree ")
        .context("commit tree header missing")?
        .to_owned();
    let mut parents = Vec::new();
    for line in lines {
        if let Some(parent) = line.strip_prefix(b"parent ") {
            parents.push(std::str::from_utf8(parent)?.to_owned());
        }
    }
    let digest = Sha256::digest(&bytes);
    let retained = RetainedCommit {
        oid: commit.into(),
        tree,
        parents,
        retention_ref: reference.into(),
        raw_bytes_fingerprint: format!("{digest:x}")
            .parse::<ObjectId>()
            .map_err(anyhow::Error::msg)?,
    };
    retained.validate()?;
    Ok(retained)
}

pub(super) async fn local(
    repository: &Path,
    args: &[&str],
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<Vec<u8>> {
    Ok(execute(git::command(repository, args), limits, cancel)
        .await?
        .stdout)
}

pub(super) async fn git_directory(
    repository: &Path,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<PathBuf> {
    let bytes = local(
        repository,
        &["rev-parse", "--absolute-git-dir"],
        limits,
        cancel,
    )
    .await?;
    let path = std::str::from_utf8(&bytes)?
        .strip_suffix('\n')
        .context("invalid Git admin path output")?;
    ensure!(
        !path.chars().any(char::is_control),
        "invalid Git admin path"
    );
    let path = PathBuf::from(path);
    ensure!(path.is_absolute(), "Git admin path is not absolute");
    Ok(path)
}

#[cfg(all(test, windows))]
mod tests;
