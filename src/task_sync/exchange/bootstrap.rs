//! Explicit cold source seeding through the real Git selection observer.

use super::{
    Cancellation, RunLimits,
    admission::{GitSelection, observe_git_selection},
    remaining,
};
use crate::{
    ActorContext,
    storage::SqliteStore,
    task_sync::delta::{CustodyAuditReceipt, CustodyAuditRequest, CustodySeedSelection},
};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use std::time::Instant;

/// Seed incoming membership from an exact retained folder-envelope commit.
/// This performs no fetch, publication or planning application. Completed
/// requests replay before observing today's Git configuration or branch.
///
/// # Errors
/// Refuses another seed family, changed Git selection, missing pins, unsafe
/// data history, stale local custody and every refusal of the audit owner.
pub async fn audit_incoming_data_commit(
    store: &mut SqliteStore,
    request: &CustodyAuditRequest,
    actor: &ActorContext,
    now: DateTime<Utc>,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<CustodyAuditReceipt> {
    let started = std::time::Instant::now();
    limits.validate_request()?;
    let _warning =
        crate::operation_time::Warning::start("task exchange audit incoming data commit");
    let limits = crate::task_sync::exchange::remaining(limits, started)?;
    audit_incoming_data_commit_remaining(store, request, actor, now, limits, cancel).await
}

// The public ingress admitted the full budget; internal calls spend its remainder.
pub(crate) async fn audit_incoming_data_commit_remaining(
    store: &mut SqliteStore,
    request: &CustodyAuditRequest,
    actor: &ActorContext,
    now: DateTime<Utc>,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<CustodyAuditReceipt> {
    if store.has_task_sync_custody_audit(request)? {
        return Ok(store.audit_task_sync_custody(request, actor, now)?);
    }
    let CustodySeedSelection::IncomingDataCommit { commit } = &request.seed else {
        anyhow::bail!("incoming data audit requires its explicit data-commit selection");
    };
    let binding = store
        .task_sync_binding(&request.binding)?
        .context("incoming data audit binding is missing")?;
    request.validate(&binding)?;
    let started = Instant::now();
    let data = crate::task_sync::data_worktree::DataWorktree::from_binding_within(
        &binding,
        crate::task_sync::data_worktree::Budget::within(started, limits.deadline)?,
    )?;
    let selected = GitSelection {
        expected_head: data.head.as_deref(),
        commit,
        common_commit: None,
        published_head: false,
    };
    observe_git_selection(&binding, selected, limits, started, cancel).await?;
    let snapshot = crate::task_sync::git::read_task_snapshot_bounded(
        &binding.repository.main_checkout,
        &binding.project,
        &commit.oid,
        remaining(limits, started)?.deadline,
    )?;
    ensure!(
        snapshot.envelope.profile == binding.profile,
        "incoming data audit disclosure profile differs"
    );
    observe_git_selection(
        &binding,
        selected,
        remaining(limits, started)?,
        Instant::now(),
        cancel,
    )
    .await?;
    Ok(store.audit_task_sync_custody(request, actor, now)?)
}
