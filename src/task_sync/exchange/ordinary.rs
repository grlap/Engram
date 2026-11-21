//! Ordinary Git observation and exchange for the folder command.

use super::{
    Cancellation, ExchangeOperation, ExchangeTarget, RunError, RunLimits, attempt, command,
    execute, process, remaining, run_selected,
};
use anyhow::Result;
use std::{path::Path, time::Instant};

pub(crate) async fn observe(
    target: &ExchangeTarget,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<Option<String>> {
    let result = run_selected(target, &ExchangeOperation::Observe, limits, cancel).await?;
    attempt::selected_tip(&result.stdout, &target.selection.reference)
}

pub(crate) async fn fetch(
    target: &ExchangeTarget,
    commit: &str,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<()> {
    execute(command::fetch_tip(target, commit)?, limits, cancel).await?;
    Ok(())
}

pub(crate) async fn push(
    target: &ExchangeTarget,
    commit: &str,
    observed: Option<&str>,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<(), RunError> {
    let started = Instant::now();
    // The exact lease enforces compare-and-swap. An independent ancestry
    // check prevents the lease from authorizing a non-fast-forward update.
    if let Some(observed) = observed
        && !is_ancestor(
            &target.selection.repository,
            observed,
            commit,
            limits,
            cancel,
        )
        .await?
    {
        return Err(RunError::NonFastForward);
    }
    let limits = remaining(limits, started)?;
    limits.validate()?;
    if cancel.is_cancelled() {
        return Err(RunError::Cancelled);
    }
    process::execute_push(
        command::push_at_observed_tip(target, commit, observed)?,
        limits,
        cancel,
        commit,
        &target.selection.reference,
        true,
    )
    .await?;
    Ok(())
}

pub(crate) async fn is_ancestor(
    repository: &Path,
    ancestor: &str,
    descendant: &str,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<bool, RunError> {
    if !command::object_id(ancestor) || !command::object_id(descendant) {
        return Err(RunError::InvalidOperation);
    }
    limits.validate()?;
    if cancel.is_cancelled() {
        return Err(RunError::Cancelled);
    }
    let command = local_command(
        repository,
        &["merge-base", "--is-ancestor", ancestor, descendant],
    );
    process::execute_predicate(command, limits, cancel).await
}

pub(crate) async fn local(
    repository: &Path,
    args: &[&str],
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<()> {
    execute(local_command(repository, args), limits, cancel).await?;
    Ok(())
}

/// Return whether Git completed with its merge-conflict exit code. The
/// caller still has to verify the selected merge and its unmerged paths.
pub(crate) async fn merge(
    repository: &Path,
    args: &[&str],
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<bool, RunError> {
    limits.validate()?;
    if cancel.is_cancelled() {
        return Err(RunError::Cancelled);
    }
    Ok(!process::execute_predicate(local_command(repository, args), limits, cancel).await?)
}

fn local_command(repository: &Path, args: &[&str]) -> std::process::Command {
    let mut git = command::local(repository, args);
    for name in [
        "GIT_AUTHOR_NAME",
        "GIT_AUTHOR_EMAIL",
        "GIT_COMMITTER_NAME",
        "GIT_COMMITTER_EMAIL",
    ] {
        git.env_remove(name);
    }
    git
}
