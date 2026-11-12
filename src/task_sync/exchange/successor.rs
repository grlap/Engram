//! A successor consumes the original source and its own exact local recipe.

use crate::task_sync::{
    delta::{DeltaCandidatePayload, PlanningSuccessorRequest},
    ledger::{Binding, CompletedExchange},
    retention::successor,
};
use crate::{ObjectId, Redactor, storage::SqliteStore};
use anyhow::Result;

/// Finish deferred planning through a new request after normal local execution.
/// The original operation always replays its original deferred outcome. A
/// completed successor replays before any files are opened; an unfinished one
/// reuses its exact retained construction and native identities. This performs
/// no transport, code-worktree change or host-runtime coordination.
///
/// # Errors
/// Refuses changed requests/construction, private uncertainty, live local work,
/// stale source or destination bases, or any atomic native planning failure.
pub fn apply_planning_successor<R: Redactor>(
    store: &mut SqliteStore,
    binding_id: &ObjectId,
    binding: &Binding,
    request: &PlanningSuccessorRequest,
    payload: &DeltaCandidatePayload,
    now: chrono::DateTime<chrono::Utc>,
    redactor: &R,
) -> Result<CompletedExchange> {
    if let Some(original) = store.completed_task_sync_planning_successor(binding_id, request)? {
        return Ok(original);
    }
    let retained = match successor::read(binding_id, binding, request)? {
        Some(original) => {
            anyhow::ensure!(
                &original.payload == payload,
                "successor retry payload differs from retained construction"
            );
            successor::retain(binding, &original)?;
            original
        }
        None => store
            .retain_task_sync_planning_successor(binding_id, request, payload, now, redactor)?,
    };
    Ok(store.apply_task_sync_planning_successor(binding, &retained, redactor)?)
}
