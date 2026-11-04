//! Allowed next actions from the current work and claim facts.

use super::{
    AllowedNextContext, ReadyWork, WORK_UPDATE_CLAIM_ACTION, WORK_UPDATE_CLAIM_RECOVERY_ACTION,
    WorkAvailability, WorkClaimState, WorkLifecycle,
};

fn append_holder_execution_actions(
    allowed: &mut Vec<String>,
    completion_capture_ready: bool,
    completion_preflight_ready: bool,
    handoff_action: &str,
) {
    allowed.extend([
        "work_update:checkpoint".into(),
        "work_update:evidence".into(),
        "work_update:release".into(),
        handoff_action.into(),
    ]);
    if completion_capture_ready || completion_preflight_ready {
        allowed.push("work_complete".into());
    }
}

fn append_claim_actions(
    allowed: &mut Vec<String>,
    status: &ReadyWork,
    context: AllowedNextContext<'_>,
) {
    let AllowedNextContext {
        claim,
        handoffs,
        session,
        now,
        claim_recovery_required,
        completion_capture_ready,
        completion_preflight_ready,
        ..
    } = context;
    match claim {
        Some(claim)
            if claim.state == WorkClaimState::Active
                && claim.holder == *session
                && claim.expires_at > now =>
        {
            let pending = handoffs.iter().find(|offer| {
                status.work.active_run_id == Some(claim.run_id)
                    && offer.is_live_for_run(claim.run_id, now)
            });
            if let Some(offer) = pending {
                if offer.from == *session {
                    allowed.push("work_handoff:cancel".into());
                }
            } else {
                append_holder_execution_actions(
                    allowed,
                    completion_capture_ready,
                    completion_preflight_ready,
                    "work_handoff:offer",
                );
            }
        }
        Some(claim)
            if claim.state == WorkClaimState::Active
                && claim.holder == *session
                && claim.expires_at <= now
                && status.availability == WorkAvailability::Ready =>
        {
            allowed.push(WORK_UPDATE_CLAIM_ACTION.into());
        }
        Some(claim)
            if claim.state == WorkClaimState::Active
                && claim.holder == *session
                && claim.expires_at <= now => {}
        Some(claim)
            if claim.state == WorkClaimState::Active
                && claim.holder != *session
                && claim.expires_at > now => {}
        _ if status.availability == WorkAvailability::Ready => {
            if claim_recovery_required {
                allowed.push(WORK_UPDATE_CLAIM_RECOVERY_ACTION.into());
            } else {
                allowed.push(WORK_UPDATE_CLAIM_ACTION.into());
            }
        }
        _ => {}
    }
    if handoffs.iter().any(|offer| {
        status
            .work
            .active_run_id
            .is_some_and(|run_id| offer.is_live_for_run(run_id, now))
            && offer.to == *session
    }) {
        allowed.push("work_handoff:accept".into());
    }
}

pub(super) fn allowed_next(status: &ReadyWork, context: AllowedNextContext<'_>) -> Vec<String> {
    let mut allowed = vec!["work_focus".into()];
    if status.work.lifecycle == WorkLifecycle::Completed {
        allowed.extend([
            "work_update:gate".into(),
            "work_update:note".into(),
            "work_update:reopen".into(),
        ]);
        return allowed;
    }
    if status.work.lifecycle != WorkLifecycle::Open {
        return allowed;
    }
    let live_handoff = context.handoffs.iter().any(|offer| {
        status
            .work
            .active_run_id
            .is_some_and(|run_id| offer.is_live_for_run(run_id, context.now))
    });
    let own_live_claim = context.claim.is_some_and(|claim| {
        status.work.active_run_id == Some(claim.run_id)
            && claim.state == WorkClaimState::Active
            && claim.holder == *context.session
            && claim.expires_at > context.now
    });
    if !live_handoff || !own_live_claim {
        allowed.push("work_update:note".into());
    }
    let another_session_holds_live_claim = context.claim.is_some_and(|claim| {
        claim.state == WorkClaimState::Active
            && claim.holder != *context.session
            && claim.expires_at > context.now
    });
    if !another_session_holds_live_claim && !live_handoff {
        allowed.extend([
            "work_update:revise".into(),
            "work_update:block".into(),
            "work_update:unblock".into(),
            "work_update:add_prerequisite".into(),
            "work_update:remove_prerequisite".into(),
            crate::storage::DECOMPOSE_PROTOCOL_OPERATION.into(),
        ]);
        allowed.extend(["work_update:cancel".into(), "work_update:supersede".into()]);
    }
    if context.can_waive_required_child {
        allowed.push("work_update:waive_required_child".into());
    }
    append_claim_actions(&mut allowed, status, context);
    allowed.sort();
    allowed.dedup();
    allowed
}
