use super::{
    Connection, ControlWorkBinding, DEFAULT_WORK_CLAIM_TTL_SECONDS, DateTime, DecomposeWorkRequest,
    MAX_OPEN_WORK_DESCENDANTS, MAX_WORK_DEPTH, MAX_WORK_TTL_SECONDS, PlanningValidation, SessionId,
    StoreError, Transaction, Utc, WorkClaim, WorkClaimId, WorkClaimState, WorkId, WorkItem,
    WorkLifecycle, WorkPlanningAuthority, WorkRun, WorkRunId, WorkRunState,
    ancestors_admit_execution, any_canonical_claim_epoch_event, expire_handoff_offers,
    load_work_claim_optional, load_work_item, load_work_run, params, persist_claim,
    persist_work_run,
};
#[cfg(test)]
use super::{DESCENDANT_SCAN_COUNT, DESCENDANT_SCAN_VM_STEPS};

pub(crate) fn claim_expiry(
    now: DateTime<Utc>,
    ttl_seconds: i64,
) -> Result<DateTime<Utc>, StoreError> {
    if !(1..=MAX_WORK_TTL_SECONDS).contains(&ttl_seconds) {
        return Err(StoreError::InvalidWork(format!(
            "claim TTL must be from 1 through {MAX_WORK_TTL_SECONDS} seconds"
        )));
    }
    now.checked_add_signed(chrono::TimeDelta::seconds(ttl_seconds))
        .ok_or_else(|| StoreError::InvalidWork("claim expiry exceeds the supported clock".into()))
}

pub(in crate::storage::work) fn assert_revision(
    item: &WorkItem,
    expected: i64,
) -> Result<(), StoreError> {
    if item.revision == expected {
        Ok(())
    } else {
        Err(StoreError::WorkRevisionConflict {
            work: item.work_id,
            expected,
            current: item.revision,
        })
    }
}

pub(in crate::storage::work) fn assert_actor_session(
    actor: &crate::domain::ActorContext,
    expected: &SessionId,
) -> Result<(), StoreError> {
    // `expected` is a comparison identity, often a persisted claim holder.
    // Length-admit only the live actor session; do not retrofit stored ids.
    if let Some(session) = &actor.session_id {
        crate::storage::admit_session_id(session)?;
    }
    if actor.session_id.as_ref() == Some(expected) {
        Ok(())
    } else {
        Err(StoreError::InvalidWork(format!(
            "actor session {:?} does not match lifecycle holder {:?}",
            actor.session_id.as_ref().map(|session| &session.0),
            expected.0
        )))
    }
}

/// True selects peer-only creation; false retains ordinary planning authority;
/// an error refuses the plan. This probe never renews claims or sweeps offers.
pub(super) fn peer_decomposition_admitted(
    transaction: &Transaction<'_>,
    parent: &WorkItem,
    request: &DecomposeWorkRequest,
) -> Result<bool, StoreError> {
    if !matches!(request.authority, WorkPlanningAuthority::Project) {
        return Ok(false);
    }
    let Some(run_id) = parent.active_run_id else {
        return Ok(false);
    };
    let Some(claim) = load_work_claim_optional(transaction, run_id)? else {
        return Ok(false);
    };
    if claim.state != WorkClaimState::Active || claim.expires_at <= request.created_at {
        return Ok(false);
    }
    let session = request.actor.session_id.as_ref().ok_or_else(|| {
        StoreError::InvalidWork("peer proposals require an attributed session".into())
    })?;
    if session == &claim.holder {
        return Ok(false);
    }
    if !request.prerequisites.is_empty()
        || request
            .children
            .iter()
            .any(|child| child.child_requirement != crate::domain::ChildRequirement::Optional)
    {
        return Err(StoreError::WorkPeerDecompositionRefused {
            parent: parent.work_id,
        });
    }
    // Verify current authority without expiring offers, renewing the claim,
    // or appending anything to the parent's execution feed.
    validate_live_claim_on(
        transaction,
        parent.work_id,
        run_id,
        parent.revision,
        &claim.holder,
        claim.claim_id,
        claim.fence,
        request.created_at,
        true,
    )?;
    Ok(true)
}

pub(super) fn validate_planning_authority(
    transaction: &Transaction<'_>,
    item: &WorkItem,
    authority: &WorkPlanningAuthority,
    actor: &crate::domain::ActorContext,
    at: DateTime<Utc>,
) -> Result<(), StoreError> {
    crate::storage::work::task_sync::reconciliation::refuse_held_on(transaction, item.work_id)?;
    validate_planning_authority_body(transaction, item, authority, actor, at)
}

pub(super) fn validate_successor_planning_authority(
    checked: &super::super::task_sync::reconciliation::successor::CheckedPlanningSuccessor<
        '_,
        '_,
        '_,
    >,
    item: &WorkItem,
    authority: &WorkPlanningAuthority,
    actor: &crate::ActorContext,
    at: DateTime<Utc>,
) -> Result<(), StoreError> {
    checked.require_affected(item.work_id)?;
    if !matches!(authority, WorkPlanningAuthority::Project) {
        return Err(StoreError::InvalidWork(
            "a planning successor does not transfer claim authority".into(),
        ));
    }
    validate_planning_authority_body(checked.transaction(), item, authority, actor, at)
}

fn validate_planning_authority_body(
    transaction: &Transaction<'_>,
    item: &WorkItem,
    authority: &WorkPlanningAuthority,
    actor: &crate::ActorContext,
    at: DateTime<Utc>,
) -> Result<(), StoreError> {
    if let Some(run_id) = item.active_run_id {
        expire_handoff_offers(transaction, run_id, at, actor)?;
    }
    match authority {
        WorkPlanningAuthority::Project => {
            if let Some(run_id) = item.active_run_id
                && load_work_claim_optional(transaction, run_id)?.is_some_and(|claim| {
                    claim.state == WorkClaimState::Active && claim.expires_at > at
                })
            {
                return Err(StoreError::InvalidWork(
                    "project planning cannot revise work held by a live claim; use the holder's claim-bound planning context or wait for recovery"
                        .into(),
                ));
            }
        }
        WorkPlanningAuthority::Claim {
            run_id,
            holder,
            claim_id,
            claim_fence,
            ..
        } => {
            if actor.session_id.as_ref() != Some(holder) {
                return Err(StoreError::InvalidWork(
                    "planning claim holder must match the attributed actor session".into(),
                ));
            }
            validate_live_claim_on(
                transaction,
                item.work_id,
                *run_id,
                item.revision,
                holder,
                *claim_id,
                *claim_fence,
                at,
                false,
            )?;
        }
    }
    Ok(())
}

pub(super) fn validate_decomposition_budget(
    connection: &Connection,
    parent: &WorkItem,
    proposed_children: usize,
    validation: PlanningValidation,
) -> Result<(), StoreError> {
    let depth = work_depth(connection, parent.work_id)? + 1;
    if depth > i64::from(MAX_WORK_DEPTH) {
        return Err(StoreError::InvalidWork(format!(
            "decomposition exceeds the project hierarchy depth: at most {MAX_WORK_DEPTH} (root depth is zero)"
        )));
    }
    if matches!(validation, PlanningValidation::Immediate) {
        validate_root_descendant_budget(
            connection,
            &parent.project_id.0,
            parent.root_id,
            proposed_children,
        )?;
    }
    Ok(())
}

pub(super) fn validate_root_descendant_budget(
    connection: &Connection,
    project_id: &str,
    root_id: WorkId,
    proposed_children: usize,
) -> Result<(), StoreError> {
    let proposed = i64::try_from(proposed_children)
        .map_err(|_| StoreError::InvalidWork("decomposition size overflow".into()))?;
    let open_descendants = root_open_descendant_count(connection, project_id, root_id)?;
    if open_descendants + proposed > i64::from(MAX_OPEN_WORK_DESCENDANTS) {
        return Err(StoreError::InvalidWork(format!(
            "decomposition exceeds the root open-descendant budget: at most {MAX_OPEN_WORK_DESCENDANTS} open descendants per root ({} tasks including the root)",
            MAX_OPEN_WORK_DESCENDANTS + 1
        )));
    }
    Ok(())
}

/// The root's proposed and open descendants, counted by their `root_id`
/// through the project's lifecycle index rather than by walking `parent_id`.
///
/// Every descendant carries its root's id from creation and no supported
/// path moves it under another root, so for a valid stored tree this counts
/// the same rows the walk did, open work below a terminal ancestor included.
/// The work is bounded by the project's proposed and open rows: the index
/// range visits only those, and one row read each tests the root. Completed,
/// cancelled and superseded descendants are never visited, so the cost stays
/// flat as a root's closed history grows. `INDEXED BY` pins that access path;
/// the planner would otherwise be free to range over the root's whole history
/// through the root index instead.
pub(super) fn root_open_descendant_count(
    connection: &Connection,
    project_id: &str,
    root_id: WorkId,
) -> Result<i64, StoreError> {
    #[cfg(test)]
    DESCENDANT_SCAN_COUNT.with(|count| count.set(count.get() + 1));
    let mut statement = connection.prepare(
        "SELECT COUNT(*) FROM work_items INDEXED BY work_items_ready
         WHERE project_id = ?1
           AND lifecycle IN ('proposed', 'open')
           AND root_id = ?2
           AND work_id <> ?2",
    )?;
    let count = statement.query_row(params![project_id, root_id.0.to_string()], |row| {
        row.get::<_, i64>(0)
    })?;
    #[cfg(test)]
    DESCENDANT_SCAN_VM_STEPS.with(|steps| {
        steps.set(steps.get() + statement.get_status(rusqlite::StatementStatus::VmStep));
    });
    Ok(count)
}

/// The open descendants of a root created in the current transaction, counted
/// through the root index rather than the project's lifecycle range. Such a
/// root has no history yet: every row under its `root_id` was written by the
/// same plan, so the index range is exactly its new subtree, at most a plan's
/// worth of rows, and the count does not grow with the project's live rows
/// the way [`root_open_descendant_count`] does. Never use it for an existing
/// root, whose range would include its closed history.
pub(super) fn new_root_open_descendant_count(
    connection: &Connection,
    project_id: &str,
    root_id: WorkId,
) -> Result<i64, StoreError> {
    #[cfg(test)]
    DESCENDANT_SCAN_COUNT.with(|count| count.set(count.get() + 1));
    let mut statement = connection.prepare(
        "SELECT COUNT(*) FROM work_items INDEXED BY work_items_root
         WHERE project_id = ?1
           AND root_id = ?2
           AND work_id <> ?2
           AND lifecycle IN ('proposed', 'open')",
    )?;
    let count = statement.query_row(params![project_id, root_id.0.to_string()], |row| {
        row.get::<_, i64>(0)
    })?;
    #[cfg(test)]
    DESCENDANT_SCAN_VM_STEPS.with(|steps| {
        steps.set(steps.get() + statement.get_status(rusqlite::StatementStatus::VmStep));
    });
    Ok(count)
}

fn work_depth(connection: &Connection, work_id: WorkId) -> Result<i64, StoreError> {
    let mut depth = 0_i64;
    let mut current = load_work_item(connection, work_id)?;
    while let Some(parent_id) = current.parent_id {
        depth += 1;
        if depth > 1_024 {
            return Err(StoreError::InvalidWorkProjection(
                "work hierarchy depth exceeds the corruption guard".into(),
            ));
        }
        current = load_work_item(connection, parent_id)?;
    }
    Ok(depth)
}

pub(super) fn rebase_planning_claim(
    transaction: &Transaction<'_>,
    item: &WorkItem,
    authority: &WorkPlanningAuthority,
    at: DateTime<Utc>,
) -> Result<(Option<WorkClaim>, Option<WorkRun>), StoreError> {
    let WorkPlanningAuthority::Claim { run_id, .. } = authority else {
        return Ok((None, None));
    };
    let mut claim = load_work_claim_optional(transaction, *run_id)?
        .ok_or(StoreError::WorkClaimMismatch { work: item.work_id })?;
    let mut run = load_work_run(transaction, *run_id)?;
    claim.accepted_work_revision = item.revision;
    renew_holder_claim(transaction, &mut claim, at)?;
    run.revision += 1;
    run.updated_at = at;
    persist_work_run(transaction, &run, claim.fence)?;
    Ok((Some(claim), Some(run)))
}

pub(in crate::storage::work) fn renew_holder_claim(
    transaction: &Transaction<'_>,
    claim: &mut WorkClaim,
    at: DateTime<Utc>,
) -> Result<(), StoreError> {
    claim.expires_at = claim
        .expires_at
        .max(claim_expiry(at, DEFAULT_WORK_CLAIM_TTL_SECONDS)?);
    claim.revision += 1;
    persist_claim(transaction, claim)
}

#[allow(
    clippy::too_many_arguments,
    reason = "the exact authority basis is intentionally explicit at the storage boundary"
)]
pub(in crate::storage::work) fn validate_live_claim_on(
    connection: &Connection,
    work_id: WorkId,
    run_id: WorkRunId,
    expected_work_revision: i64,
    holder: &SessionId,
    claim_id: WorkClaimId,
    claim_fence: i64,
    now: DateTime<Utc>,
    allow_pending_handoff: bool,
) -> Result<(WorkItem, WorkRun, WorkClaim), StoreError> {
    let item = load_work_item(connection, work_id)?;
    validate_live_claim_for_item_on(
        connection,
        item,
        run_id,
        expected_work_revision,
        holder,
        claim_id,
        claim_fence,
        now,
        allow_pending_handoff,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "the exact authority basis is intentionally explicit at the storage boundary"
)]
pub(in crate::storage::work) fn validate_live_claim_for_item_on(
    connection: &Connection,
    item: WorkItem,
    run_id: WorkRunId,
    expected_work_revision: i64,
    holder: &SessionId,
    claim_id: WorkClaimId,
    claim_fence: i64,
    now: DateTime<Utc>,
    allow_pending_handoff: bool,
) -> Result<(WorkItem, WorkRun, WorkClaim), StoreError> {
    super::validate_live_claim_for_item_in_scope(
        connection,
        item,
        run_id,
        expected_work_revision,
        holder,
        claim_id,
        claim_fence,
        now,
        allow_pending_handoff,
        None,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "the live authority checks and optional read scope stay explicit"
)]
pub(in crate::storage::work) fn validate_live_claim_for_item_in_scope(
    connection: &Connection,
    item: WorkItem,
    run_id: WorkRunId,
    expected_work_revision: i64,
    holder: &SessionId,
    claim_id: WorkClaimId,
    claim_fence: i64,
    now: DateTime<Utc>,
    allow_pending_handoff: bool,
    scope: Option<&crate::storage::work::RootReadScope<'_>>,
) -> Result<(WorkItem, WorkRun, WorkClaim), StoreError> {
    let work_id = item.work_id;
    assert_revision(&item, expected_work_revision)?;
    if item.lifecycle != WorkLifecycle::Open || item.active_run_id != Some(run_id) {
        return Err(StoreError::WorkClaimMismatch { work: work_id });
    }
    let run = load_work_run(connection, run_id)?;
    if run.work_id != work_id
        || !matches!(run.state, WorkRunState::Claimed | WorkRunState::Active)
        || !ancestors_admit_execution(connection, &item)?
        || !crate::storage::work::completion::run_uses_active_root_execution_in_scope(
            connection, &item, &run, scope,
        )?
    {
        return Err(StoreError::WorkClaimMismatch { work: work_id });
    }
    let claim = load_work_claim_optional(connection, run_id)?
        .ok_or(StoreError::WorkClaimMismatch { work: work_id })?;
    let exact_authority_basis = claim.work_id == work_id
        && claim.run_id == run_id
        && claim.claim_id == claim_id
        && claim.accepted_work_revision == expected_work_revision
        && claim.fence == claim_fence
        && &claim.holder == holder
        && claim.state == WorkClaimState::Active;
    if exact_authority_basis && claim.expires_at <= now {
        return Err(StoreError::WorkClaimLapsed {
            work: work_id,
            expired_at: claim.expires_at,
        });
    }
    if !exact_authority_basis {
        return Err(StoreError::WorkClaimMismatch { work: work_id });
    }
    if !allow_pending_handoff
        && let Some(offer) =
            crate::storage::work::execution::live_handoff_on(connection, run_id, now)?
    {
        return Err(StoreError::WorkHandoffPending {
            offer_id: offer.offer_id,
        });
    }
    Ok((item, run, claim))
}

pub(in crate::storage) fn validate_control_work_binding_on(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
    session_id: &SessionId,
    binding: &ControlWorkBinding,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    validate_control_work_binding_in_scope(connection, project_id, session_id, binding, now, None)
}

pub(in crate::storage) fn validate_control_work_binding_in_scope(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
    session_id: &SessionId,
    binding: &ControlWorkBinding,
    now: DateTime<Utc>,
    scope: Option<&crate::storage::work::RootReadScope<'_>>,
) -> Result<(), StoreError> {
    if let Some(scope) = scope {
        scope.check_connection(connection)?;
    }
    let item = match load_work_item(connection, binding.work_id) {
        Ok(item) => item,
        Err(StoreError::WorkNotFound(_)) => {
            return Err(StoreError::WorkClaimMismatch {
                work: binding.work_id,
            });
        }
        Err(error) => return Err(error),
    };
    if &item.project_id != project_id
        || !control_work_binding_was_valid_on(connection, project_id, session_id, binding)?
    {
        return Err(StoreError::WorkClaimMismatch {
            work: binding.work_id,
        });
    }
    match validate_live_claim_for_item_in_scope(
        connection,
        load_work_item(connection, binding.work_id)?,
        binding.run_id,
        binding.work_revision,
        session_id,
        binding.claim_id,
        binding.claim_fence,
        now,
        false,
        scope,
    ) {
        Ok(_) => Ok(()),
        Err(
            StoreError::WorkRevisionConflict { .. }
            | StoreError::WorkClaimMismatch { .. }
            | StoreError::WorkClaimLapsed { .. }
            | StoreError::WorkHandoffPending { .. }
            | StoreError::InvalidWork(_),
        ) => Err(StoreError::ControlWorkBindingStale {
            work: binding.work_id,
        }),
        Err(error) => Err(error),
    }
}

fn control_work_binding_was_valid_on(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
    session_id: &SessionId,
    binding: &ControlWorkBinding,
) -> Result<bool, StoreError> {
    // Every event that can record this binding carries its claim, fence and
    // work revision, so only those are read.
    any_canonical_claim_epoch_event(
        connection,
        binding.work_id,
        binding.work_revision,
        binding.claim_id,
        binding.claim_fence,
        |event| {
            event.project_id == *project_id
                && event.work_id == binding.work_id
                && event.work.lifecycle == WorkLifecycle::Open
                && event.work.revision == binding.work_revision
                && event.work.active_run_id == Some(binding.run_id)
                && event.run.as_ref().is_some_and(|run| {
                    run.run_id == binding.run_id
                        && run.work_id == binding.work_id
                        && run.root_execution_id == binding.root_execution_id
                        && matches!(run.state, WorkRunState::Claimed | WorkRunState::Active)
                })
                && event.claim.as_ref().is_some_and(|claim| {
                    claim.work_id == binding.work_id
                        && claim.run_id == binding.run_id
                        && claim.claim_id == binding.claim_id
                        && claim.accepted_work_revision == binding.work_revision
                        && claim.fence == binding.claim_fence
                        && claim.holder == *session_id
                        && claim.state == WorkClaimState::Active
                        && claim.expires_at > event.created_at
                })
        },
    )
}
