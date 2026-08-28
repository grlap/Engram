//! Deterministic behavioral-control evaluation.
//!
//! [`observe_turn`] remains a shadow evidence path. The same pure rules also
//! support the host-private persisted lifecycle, whose storage transaction is
//! responsible for minting and consuming authority.

use std::collections::{BTreeMap, BTreeSet, HashSet};

use chrono::TimeDelta;
use serde::Serialize;

use crate::{
    CanonicalObject, ObjectHash,
    domain::{
        ActionBeginDecision, ActionBeginSnapshot, ActionGrantBasis, CONTROL_SCHEMA_VERSION,
        ChangeCursor, ContextPacket, ControlAssurance, ControlDirective, ControlHealth,
        ControlRefusalCode, DirectiveSatisfaction, DirectiveTarget, EffectClass,
        ExecutionObservation, IssuedTurnGrant, LeaseBasis, LeaseKind, LeaseMode,
        ObservedActionBeginDecision, ObservedTurnDecision, PacketSafety, ParticipantMembership,
        ProjectPolicyEpoch, SessionPhase, TaskDelta, TaskState, TurnBeginDecision,
        TurnBeginSnapshot, TurnCheckpointDecision, TurnCheckpointSnapshot, TurnDecision,
        TurnEvaluationInput, TurnGrantBasis, TurnGrantState, TurnPurpose, VerificationEvidence,
        VerificationEvidenceMismatch, VerificationResult, WorkEvidenceKind,
    },
    storage::StoreError,
};

const MAX_SHADOW_GRANT_TTL_SECONDS: i64 = 300;

/// Immutable inputs for matching typed verification evidence at one exact
/// dense run-feed cut.
pub struct VerificationEvidenceMatchInput<'a> {
    pub candidate_kind: WorkEvidenceKind,
    pub evidence: Option<&'a VerificationEvidence>,
    pub producer: Option<&'a ExecutionObservation>,
    pub latest_mutation: &'a ExecutionObservation,
    pub evidence_position: i64,
    pub latest_mutation_position: i64,
    pub required_check_fingerprint: &'a ObjectHash,
}

/// Applies the anti-stale verification rule without performing I/O.
///
/// `source_revision` is a host-computed fingerprint of complete workspace
/// content (committed state plus dirty-tree content). Workspace identity is
/// retained for audit but deliberately does not participate in equality: a
/// peer worktree may verify the same exact content fingerprint.
///
/// # Errors
///
/// Returns the first typed mismatch that prevents the candidate from
/// satisfying the exact verification requirement at this run-feed cut.
pub fn match_verification_evidence(
    input: &VerificationEvidenceMatchInput<'_>,
) -> Result<(), VerificationEvidenceMismatch> {
    if input.candidate_kind != WorkEvidenceKind::Verification {
        return Err(VerificationEvidenceMismatch::WrongKind);
    }
    let evidence = input
        .evidence
        .ok_or(VerificationEvidenceMismatch::WrongKind)?;
    let producer = input
        .producer
        .ok_or(VerificationEvidenceMismatch::InvalidProducer)?;
    let latest_basis = input
        .latest_mutation
        .source_basis
        .as_ref()
        .ok_or(VerificationEvidenceMismatch::InvalidProducer)?;
    let latest_observed_at = input
        .latest_mutation
        .observed_at
        .ok_or(VerificationEvidenceMismatch::InvalidProducer)?;
    let same_run = evidence.project_id == input.latest_mutation.project_id
        && evidence.binding.root_execution_id == input.latest_mutation.binding.root_execution_id
        && evidence.binding.work_id == input.latest_mutation.binding.work_id
        && evidence.binding.run_id == input.latest_mutation.binding.run_id
        && producer.project_id == evidence.project_id
        && producer.binding == evidence.binding
        && producer.session_id == evidence.session_id;
    if !same_run {
        return Err(VerificationEvidenceMismatch::WrongRun);
    }
    if !input.latest_mutation.source_changed
        || evidence.source_basis.source_revision != latest_basis.source_revision
    {
        return Err(VerificationEvidenceMismatch::StaleSourceRevision);
    }
    if evidence.check_fingerprint != producer.action_fingerprint
        || &evidence.check_fingerprint != input.required_check_fingerprint
    {
        return Err(VerificationEvidenceMismatch::CheckFingerprintMismatch);
    }
    if evidence.result != VerificationResult::Passed {
        return Err(VerificationEvidenceMismatch::ResultNotPassed);
    }
    if input.evidence_position <= input.latest_mutation_position {
        return Err(VerificationEvidenceMismatch::NotAfterMutation);
    }
    let actor_matches = evidence.actor.session_id.as_ref() == Some(&evidence.session_id)
        && evidence.actor.run_id.as_deref() == Some(evidence.binding.run_id.0.to_string().as_str())
        && producer.actor.session_id.as_ref() == Some(&producer.session_id)
        && producer.actor.run_id.as_deref() == Some(producer.binding.run_id.0.to_string().as_str());
    let times_are_monotone = evidence.completed_at >= latest_observed_at
        && evidence.completed_at <= evidence.recorded_at
        && producer.observed_at == Some(evidence.completed_at)
        && producer.recorded_at >= evidence.completed_at;
    if !actor_matches || !times_are_monotone {
        return Err(VerificationEvidenceMismatch::InvalidTime);
    }
    Ok(())
}

/// Minimum host assurance that may mediate one material effect class.
///
/// Project policy may raise this floor, but cannot lower it. V1 keeps
/// observation and communication available to advisory hosts while every
/// mutation, external side effect, and lifecycle transition requires a host
/// that actually gates turns.
#[must_use]
pub(crate) const fn minimum_assurance_for_effect(effect: EffectClass) -> ControlAssurance {
    match effect {
        EffectClass::Observe | EffectClass::Communicate => ControlAssurance::Advisory,
        EffectClass::Coordinate
        | EffectClass::MutateLocal
        | EffectClass::MutateShared
        | EffectClass::ExternalSideEffect
        | EffectClass::Lifecycle => ControlAssurance::TurnGated,
    }
}

/// Declared host effects capped by what its assurance can honestly mediate.
#[must_use]
pub(crate) fn effective_mediated_effects(
    assurance: ControlAssurance,
    declared: &[EffectClass],
) -> Vec<EffectClass> {
    declared
        .iter()
        .copied()
        .filter(|effect| assurance.covers(minimum_assurance_for_effect(*effect)))
        .collect()
}

/// Immutable inputs for one lease-boundary policy decision.
pub(crate) struct LeasePolicyInput<'a> {
    pub request_key: &'a str,
    pub host_assurance: ControlAssurance,
    pub declared_mediated_effects: &'a [EffectClass],
    pub project_required_assurance: ControlAssurance,
    pub policy_effects: &'a [EffectClass],
    pub session_policy_epoch: ProjectPolicyEpoch,
    pub active_policy_epoch: ProjectPolicyEpoch,
    pub effect: EffectClass,
}

/// Pure lease-boundary refusal plus whether the persisted session may adopt
/// the current project-policy epoch after recording that refusal.
pub(crate) struct LeasePolicyRefusal {
    pub directive: ControlDirective,
    pub adopt_project_policy_epoch: bool,
}

/// Applies the same project, intrinsic-effect, mediation, capability, and
/// epoch ladder used by turn admission without performing I/O.
pub(crate) fn evaluate_lease_policy(
    input: &LeasePolicyInput<'_>,
) -> Result<Vec<EffectClass>, LeasePolicyRefusal> {
    let effective =
        effective_mediated_effects(input.host_assurance, input.declared_mediated_effects);
    let refuse = |code, effect, required_assurance, adopt_project_policy_epoch| {
        Err(LeasePolicyRefusal {
            directive: control_directive(
                input.request_key,
                code,
                effect,
                required_assurance,
                Some(input.declared_mediated_effects),
                Some(&effective),
            ),
            adopt_project_policy_epoch,
        })
    };
    if !input
        .host_assurance
        .covers(input.project_required_assurance)
    {
        return refuse(
            ControlRefusalCode::ControlAssuranceInsufficient,
            None,
            Some(input.project_required_assurance),
            false,
        );
    }
    let effect_required = minimum_assurance_for_effect(input.effect);
    if !input.host_assurance.covers(effect_required) {
        return refuse(
            ControlRefusalCode::ControlAssuranceInsufficient,
            Some(input.effect),
            Some(effect_required),
            false,
        );
    }
    if !effective.contains(&input.effect) {
        return refuse(
            ControlRefusalCode::ControlAssuranceInsufficient,
            Some(input.effect),
            Some(effect_required),
            false,
        );
    }
    if !input.policy_effects.contains(&input.effect) {
        return refuse(
            ControlRefusalCode::CapabilityNotPermitted,
            Some(input.effect),
            None,
            false,
        );
    }
    if input.session_policy_epoch != input.active_policy_epoch {
        return refuse(ControlRefusalCode::PolicyEpochChanged, None, None, true);
    }
    Ok(effective)
}

#[derive(Serialize)]
struct ControlDeliveryContent<'a> {
    context: Option<&'a ContextPacket>,
    delta: &'a TaskDelta,
}

pub(crate) fn delivery_content_digest(
    context: Option<&ContextPacket>,
    delta: &TaskDelta,
) -> Result<ObjectHash, StoreError> {
    Ok(
        CanonicalObject::freeze(&ControlDeliveryContent { context, delta })?
            .hash()
            .clone(),
    )
}

/// Evaluates one turn from explicitly supplied state without performing I/O.
///
/// The result is shadow evidence only. A future host-private transport must
/// persist and activate a grant transactionally before this can authorize a
/// model turn.
#[must_use]
pub fn observe_turn(input: &TurnEvaluationInput) -> ObservedTurnDecision {
    ObservedTurnDecision {
        control_schema_version: CONTROL_SCHEMA_VERSION,
        request_key: input.intent.idempotency_key.clone(),
        observed_at: input.evaluated_at,
        decision: evaluate_turn(input),
    }
}

/// Rechecks the complete action-grant basis at the execution boundary.
///
/// Like [`observe_turn`], this is shadow-only. It proves deterministic
/// comparison semantics without consuming a grant or authorizing a side
/// effect.
#[must_use]
pub fn observe_action_begin(
    grant: &ActionGrantBasis,
    snapshot: &ActionBeginSnapshot,
) -> ObservedActionBeginDecision {
    let decision = match action_begin_refusal(grant, snapshot) {
        Some(code) => ActionBeginDecision::Refuse { code },
        None => ActionBeginDecision::Begin {
            grant_id: grant.grant_id.clone(),
        },
    };
    ObservedActionBeginDecision {
        control_schema_version: CONTROL_SCHEMA_VERSION,
        grant_id: grant.grant_id.clone(),
        observed_at: snapshot.observed_at,
        decision,
    }
}

/// Rechecks a persisted turn grant immediately before prompt dispatch.
///
/// This function performs no I/O. The storage layer must evaluate it and
/// consume an issued grant in the same transaction as the tentative delivery
/// and session revision update.
#[must_use]
#[allow(
    clippy::too_many_lines,
    reason = "begin-time validation keeps the complete fail-closed decision order visible"
)]
pub fn evaluate_turn_begin(
    grant: &IssuedTurnGrant,
    snapshot: &TurnBeginSnapshot,
) -> TurnBeginDecision {
    let basis = &grant.basis;
    if grant.control_schema_version != CONTROL_SCHEMA_VERSION
        || snapshot.control_schema_version != CONTROL_SCHEMA_VERSION
    {
        return TurnBeginDecision::Refuse {
            code: ControlRefusalCode::UnknownControlSchema,
        };
    }
    if grant.grant_id.trim().is_empty()
        || grant.request_key.trim().is_empty()
        || basis.session_id != snapshot.session_id
        || basis.task_id != snapshot.task_id
        || !matches!(snapshot.grant_state, TurnGrantState::Issued)
        || !matches!(snapshot.phase, SessionPhase::TurnOpen)
    {
        return TurnBeginDecision::Refuse {
            code: ControlRefusalCode::GrantScopeMismatch,
        };
    }
    if basis.work_binding != snapshot.work_binding || !snapshot.work_binding_current {
        return TurnBeginDecision::Refuse {
            code: ControlRefusalCode::StaleFence,
        };
    }
    if !matches!(
        snapshot.participant_membership,
        ParticipantMembership::Member
    ) {
        return TurnBeginDecision::Refuse {
            code: ControlRefusalCode::TaskAccessDenied,
        };
    }
    if !snapshot
        .task_state
        .is_some_and(|task_state| match grant.basis.purpose {
            TurnPurpose::Ordinary => matches!(task_state, TaskState::Active),
            TurnPurpose::Recovery => matches!(
                task_state,
                TaskState::Active | TaskState::Quiescing | TaskState::FinalizationPending
            ),
            TurnPurpose::Finalizer => matches!(task_state, TaskState::FinalizationPending),
        })
    {
        return TurnBeginDecision::Refuse {
            code: ControlRefusalCode::LifecycleHold,
        };
    }
    if snapshot.observed_at >= basis.expires_at {
        return TurnBeginDecision::Refuse {
            code: ControlRefusalCode::GrantExpired,
        };
    }
    if snapshot.current_epochs.project_policy != basis.project_policy_epoch {
        return TurnBeginDecision::Refuse {
            code: ControlRefusalCode::PolicyEpochChanged,
        };
    }
    if snapshot.current_epochs.task_admission != basis.task_admission_epoch {
        return TurnBeginDecision::Refuse {
            code: ControlRefusalCode::TaskAdmissionEpochChanged,
        };
    }
    if !snapshot.context_current {
        return TurnBeginDecision::Refuse {
            code: ControlRefusalCode::DeltaRequired,
        };
    }
    let expected_head = basis
        .inline_delivery
        .as_ref()
        .map_or(basis.delivery_cursor, |page| page.head_cursor);
    if snapshot.current_head != expected_head {
        return TurnBeginDecision::Refuse {
            code: ControlRefusalCode::DeltaRequired,
        };
    }
    if snapshot.capability_map_revision != basis.capability_map_revision {
        return TurnBeginDecision::Refuse {
            code: ControlRefusalCode::GrantScopeMismatch,
        };
    }
    let Some(current_leases) = lease_map(&snapshot.leases) else {
        return TurnBeginDecision::Refuse {
            code: ControlRefusalCode::StaleFence,
        };
    };
    if basis.leases.iter().any(|granted| {
        current_leases
            .get(granted.lease_id.as_str())
            .is_none_or(|current| {
                current.holder != basis.session_id
                    || current.kind != granted.kind
                    || current.mode != granted.mode
                    || current.subject != granted.subject
                    || current.fence != granted.fence
                    || current.expires_at <= snapshot.observed_at
            })
    }) {
        return TurnBeginDecision::Refuse {
            code: ControlRefusalCode::StaleFence,
        };
    }

    let expected_tokens: Vec<_> = basis
        .inline_delivery
        .iter()
        .map(|delivery| delivery.delivery_token.as_str())
        .collect();
    if expected_tokens
        != snapshot
            .delivery_tokens
            .iter()
            .map(String::as_str)
            .collect::<Vec<_>>()
        || !delivery_matches_grant(grant)
    {
        return TurnBeginDecision::Refuse {
            code: ControlRefusalCode::DeliveryInvalid,
        };
    }

    TurnBeginDecision::Begin
}

/// Checks whether a begun turn can transition to a durable checkpoint.
///
/// Checkpoint deliberately does not recheck expiry, policy epochs, or lease
/// fences: begin already admitted the turn, and checkpoint must preserve its
/// durable progress while closing that authority. Any next turn is evaluated
/// against fresh epochs and fences.
#[must_use]
pub fn evaluate_turn_checkpoint(
    grant: &IssuedTurnGrant,
    snapshot: &TurnCheckpointSnapshot,
) -> TurnCheckpointDecision {
    if grant.control_schema_version != CONTROL_SCHEMA_VERSION
        || snapshot.control_schema_version != CONTROL_SCHEMA_VERSION
    {
        return TurnCheckpointDecision::Refuse {
            code: ControlRefusalCode::UnknownControlSchema,
        };
    }
    if grant.grant_id.trim().is_empty()
        || grant.basis.session_id != snapshot.session_id
        || grant.basis.task_id != snapshot.task_id
        || grant.basis.work_binding != snapshot.work_binding
        || !matches!(snapshot.phase, SessionPhase::TurnOpen)
        || !matches!(snapshot.grant_state, TurnGrantState::Begun)
    {
        return TurnCheckpointDecision::Refuse {
            code: ControlRefusalCode::GrantScopeMismatch,
        };
    }
    TurnCheckpointDecision::Checkpoint
}

pub(crate) fn delivery_matches_grant(grant: &IssuedTurnGrant) -> bool {
    match (&grant.basis.inline_delivery, &grant.delivery) {
        (None, None) => true,
        (Some(page), Some(delivery)) => {
            page == &delivery.page
                && if page.has_more {
                    delivery.context.is_none()
                } else {
                    delivery.context.as_ref().is_some_and(|context| {
                        context.header.task_id == Some(grant.basis.task_id)
                            && context.header.event_cursor == page.to_cursor
                    })
                }
                && delivery.delta.task_id == grant.basis.task_id
                && delivery_delta_matches(page, &delivery.delta)
                && delivery_content_digest(delivery.context.as_ref(), &delivery.delta)
                    .is_ok_and(|digest| digest == page.content_digest)
        }
        (None, Some(_)) | (Some(_), None) => false,
    }
}

fn delivery_delta_matches(page: &crate::domain::DeliveryPage, delta: &TaskDelta) -> bool {
    let Ok(expected_count) = usize::try_from(page.to_cursor.0 - page.from_cursor.0) else {
        return false;
    };
    page.has_more == (page.to_cursor < page.head_cursor)
        && delta.after == page.from_cursor
        && delta.cursor == page.to_cursor
        && delta.changes.len() == expected_count
        && delta.changes.iter().enumerate().all(|(offset, change)| {
            i64::try_from(offset).is_ok_and(|offset| {
                change.cursor.0 == page.from_cursor.0 + offset + 1
                    && CanonicalObject::freeze(&change.object)
                        .is_ok_and(|object| object.hash() == &change.object_hash)
            })
        })
}

fn action_begin_refusal(
    grant: &ActionGrantBasis,
    snapshot: &ActionBeginSnapshot,
) -> Option<ControlRefusalCode> {
    if grant.control_schema_version != CONTROL_SCHEMA_VERSION
        || snapshot.control_schema_version != CONTROL_SCHEMA_VERSION
    {
        return Some(ControlRefusalCode::UnknownControlSchema);
    }
    action_identity_refusal(grant, snapshot)
        .or_else(|| action_freshness_refusal(grant, snapshot))
        .or_else(|| action_authority_refusal(grant, snapshot))
        .or_else(|| action_resolution_refusal(grant, snapshot))
}

fn action_identity_refusal(
    grant: &ActionGrantBasis,
    snapshot: &ActionBeginSnapshot,
) -> Option<ControlRefusalCode> {
    if !matches!(
        snapshot.grant_state,
        crate::domain::ActionGrantState::Available
    ) || !matches!(
        snapshot.parent_turn_state,
        crate::domain::ParentTurnState::Open
    ) || snapshot.parent_turn_id != grant.parent_turn_id
    {
        return Some(ControlRefusalCode::GrantScopeMismatch);
    }
    if grant.grant_id.trim().is_empty()
        || grant.parent_turn_id.trim().is_empty()
        || grant.session_id.0.trim().is_empty()
        || grant.epochs.project_policy.0 < 0
        || grant.epochs.task_admission.0 < 0
        || snapshot.current_epochs.project_policy.0 < 0
        || snapshot.current_epochs.task_admission.0 < 0
        || grant.blocking_watermark.0 < 0
        || snapshot.acknowledged_blocking_watermark.0 < 0
    {
        return Some(ControlRefusalCode::GrantScopeMismatch);
    }
    if snapshot.session_id != grant.session_id
        || snapshot.task_id != grant.task_id
        || snapshot.turn_purpose != grant.turn_purpose
        || snapshot.effect != grant.effect
        || snapshot.resource_subjects != grant.resource_subjects
        || snapshot.request_fingerprint != grant.request_fingerprint
    {
        return Some(ControlRefusalCode::GrantScopeMismatch);
    }
    if snapshot.observed_at >= grant.expires_at {
        return Some(ControlRefusalCode::GrantExpired);
    }

    None
}

fn action_freshness_refusal(
    grant: &ActionGrantBasis,
    snapshot: &ActionBeginSnapshot,
) -> Option<ControlRefusalCode> {
    if snapshot.current_epochs.project_policy != grant.epochs.project_policy {
        return Some(ControlRefusalCode::PolicyEpochChanged);
    }
    if snapshot.current_epochs.task_admission != grant.epochs.task_admission {
        return Some(ControlRefusalCode::TaskAdmissionEpochChanged);
    }
    if snapshot.acknowledged_blocking_watermark < grant.blocking_watermark {
        return Some(ControlRefusalCode::DeltaRequired);
    }
    if !action_phase_matches(grant.turn_purpose, snapshot.phase, snapshot.task_state) {
        return Some(ControlRefusalCode::LifecycleHold);
    }
    if snapshot.capability_map_revision != grant.capability_map_revision
        || grant.capability_map_revision < 0
        || !effect_fits_purpose(grant.turn_purpose, grant.effect)
        || grant.resource_subjects.is_empty()
        || !grant
            .resource_subjects
            .iter()
            .all(crate::domain::ResourceSubject::has_valid_shape)
    {
        return Some(ControlRefusalCode::GrantScopeMismatch);
    }

    None
}

fn action_authority_refusal(
    grant: &ActionGrantBasis,
    snapshot: &ActionBeginSnapshot,
) -> Option<ControlRefusalCode> {
    if grant.authority_references.is_empty()
        || grant
            .authority_references
            .iter()
            .any(|reference| reference.trim().is_empty())
        || !matches!(
            snapshot.authority_state,
            crate::domain::AuthorityState::Valid
        )
        || !same_unique_strings(&snapshot.authority_references, &grant.authority_references)
    {
        return Some(ControlRefusalCode::MissingAuthority);
    }
    let (Some(snapshot_fences), Some(grant_fences)) =
        (lease_map(&snapshot.leases), lease_map(&grant.leases))
    else {
        return Some(ControlRefusalCode::StaleFence);
    };
    for (lease_id, grant_lease) in grant_fences {
        let Some(current_lease) = snapshot_fences.get(lease_id) else {
            return Some(ControlRefusalCode::StaleFence);
        };
        if current_lease.holder != grant.session_id
            || grant_lease.holder != grant.session_id
            || grant_lease.lease_id.trim().is_empty()
            || current_lease.kind != grant_lease.kind
            || current_lease.mode != grant_lease.mode
            || current_lease.subject != grant_lease.subject
            || current_lease.fence != grant_lease.fence
            || current_lease.fence < 0
            || !current_lease.subject.has_valid_shape()
            || !grant_lease.subject.has_valid_shape()
            || current_lease.expires_at <= snapshot.observed_at
            || grant_lease.expires_at <= snapshot.observed_at
        {
            return Some(ControlRefusalCode::StaleFence);
        }
    }
    if !leases_cover_action(grant, snapshot) {
        return Some(ControlRefusalCode::LeaseRequired);
    }

    None
}

fn leases_cover_action(grant: &ActionGrantBasis, snapshot: &ActionBeginSnapshot) -> bool {
    let required_kind = match grant.effect {
        EffectClass::MutateLocal | EffectClass::MutateShared => Some(LeaseKind::Execution),
        EffectClass::Lifecycle => Some(LeaseKind::Coordination),
        EffectClass::Observe
        | EffectClass::Communicate
        | EffectClass::Coordinate
        | EffectClass::ExternalSideEffect => None,
    };
    let Some(required_kind) = required_kind else {
        return true;
    };

    grant.resource_subjects.iter().all(|resource| {
        grant.leases.iter().any(|granted_lease| {
            snapshot.leases.iter().any(|current_lease| {
                current_lease.lease_id == granted_lease.lease_id
                    && current_lease.holder == grant.session_id
                    && matches!(current_lease.mode, LeaseMode::Exclusive)
                    && current_lease.kind == required_kind
                    && current_lease.subject.covers(resource)
            })
        })
    })
}

fn action_resolution_refusal(
    grant: &ActionGrantBasis,
    snapshot: &ActionBeginSnapshot,
) -> Option<ControlRefusalCode> {
    let has_path_subject = grant
        .resource_subjects
        .iter()
        .any(crate::domain::ResourceSubject::is_path);
    if has_path_subject {
        if !matches!(
            snapshot.resolution_assurance,
            crate::domain::ResolutionAssurance::PinnedThroughInvocation
        ) {
            return Some(ControlRefusalCode::ControlAssuranceInsufficient);
        }
        if grant.resolution_binding_digest.is_none()
            || snapshot.resolution_binding_digest != grant.resolution_binding_digest
        {
            return Some(ControlRefusalCode::ResourceRemapped);
        }
    } else if grant.resolution_binding_digest.is_some()
        || snapshot.resolution_binding_digest.is_some()
    {
        return Some(ControlRefusalCode::GrantScopeMismatch);
    }

    None
}

const fn action_phase_matches(
    purpose: TurnPurpose,
    phase: SessionPhase,
    task_state: TaskState,
) -> bool {
    match purpose {
        TurnPurpose::Ordinary => {
            matches!(phase, SessionPhase::TurnOpen) && matches!(task_state, TaskState::Active)
        }
        TurnPurpose::Recovery => {
            matches!(phase, SessionPhase::RecoveryOpen)
                && matches!(
                    task_state,
                    TaskState::Active | TaskState::Quiescing | TaskState::FinalizationPending
                )
        }
        TurnPurpose::Finalizer => {
            matches!(phase, SessionPhase::FinalizerOpen)
                && matches!(task_state, TaskState::FinalizationPending)
        }
    }
}

fn same_unique_strings(left: &[String], right: &[String]) -> bool {
    let left_set: BTreeSet<_> = left.iter().collect();
    let right_set: BTreeSet<_> = right.iter().collect();
    left_set.len() == left.len() && right_set.len() == right.len() && left_set == right_set
}

fn effects_are_unique(effects: &[EffectClass]) -> bool {
    let unique: HashSet<_> = effects.iter().collect();
    unique.len() == effects.len()
}

fn lease_map(leases: &[LeaseBasis]) -> Option<BTreeMap<&str, &LeaseBasis>> {
    let mut mapped = BTreeMap::new();
    for lease in leases {
        if mapped.insert(lease.lease_id.as_str(), lease).is_some() {
            return None;
        }
    }
    Some(mapped)
}

#[allow(
    clippy::too_many_lines,
    reason = "the pure evaluator keeps fail-closed admission order visible in one function"
)]
fn evaluate_turn(input: &TurnEvaluationInput) -> TurnDecision {
    if input.control_schema_version != CONTROL_SCHEMA_VERSION {
        return refusal(input, ControlRefusalCode::UnknownControlSchema);
    }
    if let Some(code) = health_refusal(input.health) {
        return refusal(input, code);
    }
    if !input.active_policy_known {
        return refusal(input, ControlRefusalCode::ControlPolicyMissing);
    }
    if input.work_binding.is_some() && !input.work_binding_current {
        return refusal(input, ControlRefusalCode::StaleFence);
    }
    if !input.host_assurance.covers(input.required_assurance) {
        return assurance_refusal(input, None, input.required_assurance);
    }
    if let Some((effect, required_assurance)) = effect_assurance_refusal(input) {
        return assurance_refusal(input, Some(effect), required_assurance);
    }
    let effective_mediation =
        effective_mediated_effects(input.host_assurance, &input.mediated_effects);
    if let Some(effect) = first_uncovered_effect(input, &effective_mediation) {
        return detailed_refusal(
            input,
            ControlRefusalCode::ControlAssuranceInsufficient,
            Some(effect),
            Some(minimum_assurance_for_effect(effect)),
            Some(&input.mediated_effects),
            Some(&effective_mediation),
        );
    }
    if let Some(effect) = first_uncovered_effect(input, &input.policy_effects) {
        return detailed_refusal(
            input,
            ControlRefusalCode::CapabilityNotPermitted,
            Some(effect),
            None,
            Some(&input.mediated_effects),
            Some(&effective_mediation),
        );
    }

    let Some(task_id) = input.task_id else {
        return refusal(input, ControlRefusalCode::TaskUnbound);
    };
    let Some(task_state) = input.task_state else {
        return refusal(input, ControlRefusalCode::TaskUnbound);
    };
    if !matches!(input.participant_membership, ParticipantMembership::Member) {
        return refusal(input, ControlRefusalCode::TaskAccessDenied);
    }
    if let Some(code) = phase_refusal(
        input.phase,
        input.intent.purpose,
        task_state,
        input.pending_delivery.is_some(),
    ) {
        return refusal(input, code);
    }
    if input.current_epochs.project_policy != input.session_epochs.project_policy {
        return refusal(input, ControlRefusalCode::PolicyEpochChanged);
    }
    if input.current_epochs.task_admission != input.session_epochs.task_admission {
        return refusal(input, ControlRefusalCode::TaskAdmissionEpochChanged);
    }
    if let Some(code) = packet_refusal(input.packet_safety) {
        return refusal(input, code);
    }
    if input.has_unknown_action_outcome {
        return refusal(input, ControlRefusalCode::ActionOutcomeUnknown);
    }
    if !input.authority_satisfied {
        return refusal(input, ControlRefusalCode::MissingAuthority);
    }
    if turn_input_has_invalid_shape(input) {
        return refusal(input, ControlRefusalCode::GrantScopeMismatch);
    }
    if let Some(effect) = input
        .intent
        .requested_effects
        .iter()
        .copied()
        .find(|effect| !effect_fits_purpose(input.intent.purpose, *effect))
    {
        return detailed_refusal(
            input,
            ControlRefusalCode::GrantScopeMismatch,
            Some(effect),
            None,
            Some(&input.mediated_effects),
            Some(&effective_mediation),
        );
    }
    if !turn_leases_cover_resources(input) {
        return refusal(input, ControlRefusalCode::LeaseRequired);
    }

    let (delivery_cursor, inline_delivery) = match evaluate_delivery(input) {
        Ok(delivery) => delivery,
        Err(code) => return refusal(input, code),
    };
    if inline_delivery.as_ref().is_some_and(|page| page.has_more)
        && !matches!(input.intent.purpose, TurnPurpose::Recovery)
    {
        return refusal(input, ControlRefusalCode::RecoveryRequired);
    }
    let delivered_watermark = input.acknowledged_blocking_watermark.max(delivery_cursor);
    let partial_recovery = inline_delivery.as_ref().is_some_and(|page| page.has_more)
        && matches!(input.intent.purpose, TurnPurpose::Recovery);
    if partial_recovery
        && input
            .intent
            .requested_effects
            .iter()
            .any(|effect| !matches!(effect, EffectClass::Observe))
    {
        return refusal(input, ControlRefusalCode::GrantScopeMismatch);
    }
    if delivered_watermark < input.blocking_watermark && !partial_recovery {
        return refusal(input, ControlRefusalCode::DeltaRequired);
    }
    if matches!(input.phase, SessionPhase::SyncRequired)
        && matches!(input.intent.purpose, TurnPurpose::Ordinary)
        && inline_delivery.is_none()
    {
        return refusal(input, ControlRefusalCode::RecoveryRequired);
    }

    let Some(expires_at) = input
        .evaluated_at
        .checked_add_signed(TimeDelta::seconds(input.grant_ttl_seconds))
    else {
        return refusal(input, ControlRefusalCode::GrantScopeMismatch);
    };

    TurnDecision::Grant {
        basis: Box::new(TurnGrantBasis {
            session_id: input.session_id.clone(),
            task_id,
            work_binding: input.work_binding.clone(),
            purpose: input.intent.purpose,
            intent_fingerprint: input.intent.intent_fingerprint.clone(),
            project_policy_epoch: input.current_epochs.project_policy,
            task_admission_epoch: input.current_epochs.task_admission,
            confirmed_cursor: input.confirmed_cursor,
            delivery_cursor,
            blocking_watermark: input.blocking_watermark,
            inline_delivery,
            capability_map_revision: input.capability_map_revision,
            requested_effects: input.intent.requested_effects.clone(),
            resource_intents: input.intent.resource_intents.clone(),
            leases: input.leases.clone(),
            expires_at,
        }),
    }
}

fn turn_input_has_invalid_shape(input: &TurnEvaluationInput) -> bool {
    input.grant_ttl_seconds <= 0
        || input.grant_ttl_seconds > MAX_SHADOW_GRANT_TTL_SECONDS
        || input.capability_map_revision < 0
        || input.current_epochs.project_policy.0 < 0
        || input.current_epochs.task_admission.0 < 0
        || input.session_epochs.project_policy.0 < 0
        || input.session_epochs.task_admission.0 < 0
        || input.confirmed_cursor.0 < 0
        || input.head_cursor.0 < 0
        || input.blocking_watermark.0 < 0
        || input.acknowledged_blocking_watermark.0 < 0
        || input.session_id.0.trim().is_empty()
        || input.intent.idempotency_key.trim().is_empty()
        || input.intent.requested_effects.is_empty()
        || (input.work_binding.is_none() && !input.work_binding_current)
        || input
            .work_binding
            .as_ref()
            .is_some_and(|binding| !control_work_binding_has_valid_shape(binding))
        || !effects_are_unique(&input.intent.requested_effects)
        || input
            .intent
            .resource_intents
            .iter()
            .any(|resource| !resource.has_valid_shape())
        || input
            .intent
            .resource_intents
            .iter()
            .enumerate()
            .any(|(index, resource)| input.intent.resource_intents[..index].contains(resource))
        || !effects_are_unique(&input.policy_effects)
        || !effects_are_unique(&input.mediated_effects)
        || lease_map(&input.leases).is_none()
        || input.leases.iter().any(|lease| {
            lease.fence < 0
                || lease.lease_id.trim().is_empty()
                || lease.holder.0.trim().is_empty()
                || lease.holder != input.session_id
                || lease.expires_at <= input.evaluated_at
                || !lease.subject.has_valid_shape()
        })
}

fn control_work_binding_has_valid_shape(binding: &crate::domain::ControlWorkBinding) -> bool {
    binding.work_revision > 0 && binding.claim_fence > 0
}

fn turn_leases_cover_resources(input: &TurnEvaluationInput) -> bool {
    let requires_execution = input
        .intent
        .requested_effects
        .iter()
        .any(|effect| matches!(effect, EffectClass::MutateLocal | EffectClass::MutateShared));
    let requires_coordination = input
        .intent
        .requested_effects
        .contains(&EffectClass::Lifecycle);
    if !requires_execution && !requires_coordination {
        return true;
    }
    if input.intent.resource_intents.is_empty() {
        return false;
    }

    let resources_have = |kind| {
        input.intent.resource_intents.iter().all(|resource| {
            input.leases.iter().any(|lease| {
                lease.kind == kind
                    && matches!(lease.mode, LeaseMode::Exclusive)
                    && lease.subject.covers(resource)
            })
        })
    };
    (!requires_execution || resources_have(LeaseKind::Execution))
        && (!requires_coordination || resources_have(LeaseKind::Coordination))
}

fn first_uncovered_effect(
    input: &TurnEvaluationInput,
    allowed: &[EffectClass],
) -> Option<EffectClass> {
    input
        .intent
        .requested_effects
        .iter()
        .copied()
        .find(|effect| !allowed.contains(effect))
}

fn effect_assurance_refusal(
    input: &TurnEvaluationInput,
) -> Option<(EffectClass, ControlAssurance)> {
    for effect in &input.intent.requested_effects {
        let required = minimum_assurance_for_effect(*effect);
        if !input.host_assurance.covers(required) {
            return Some((*effect, required));
        }
    }
    None
}

const fn health_refusal(health: ControlHealth) -> Option<ControlRefusalCode> {
    match health {
        ControlHealth::Healthy => None,
        ControlHealth::Unavailable => Some(ControlRefusalCode::ControlUnavailable),
        ControlHealth::Corrupt => Some(ControlRefusalCode::StoreCorrupt),
        ControlHealth::UnknownSchema => Some(ControlRefusalCode::UnknownControlSchema),
    }
}

const fn packet_refusal(safety: PacketSafety) -> Option<ControlRefusalCode> {
    match safety {
        PacketSafety::Safe => None,
        PacketSafety::PinnedContradiction => Some(ControlRefusalCode::PinnedContradiction),
        PacketSafety::PinnedBudgetExceeded => Some(ControlRefusalCode::PinnedBudgetExceeded),
        PacketSafety::DeliveryBudgetExceeded => Some(ControlRefusalCode::DeliveryInvalid),
    }
}

fn evaluate_delivery(
    input: &TurnEvaluationInput,
) -> Result<(ChangeCursor, Option<crate::domain::DeliveryPage>), ControlRefusalCode> {
    if input.confirmed_cursor > input.head_cursor {
        return Err(ControlRefusalCode::DeliveryInvalid);
    }

    let Some(page) = &input.pending_delivery else {
        if input.confirmed_cursor < input.head_cursor {
            return Err(if input.confirmed_cursor == ChangeCursor::default() {
                ControlRefusalCode::ContextRequired
            } else {
                ControlRefusalCode::DeltaRequired
            });
        }
        return Ok((input.confirmed_cursor, None));
    };

    let advances_task_feed =
        page.to_cursor > page.from_cursor && input.confirmed_cursor < input.head_cursor;
    let context_only = page.to_cursor == page.from_cursor
        && page.to_cursor == page.head_cursor
        && input.confirmed_cursor == input.head_cursor
        && !page.has_more;
    let valid = page.from_cursor == input.confirmed_cursor
        && page.head_cursor == input.head_cursor
        && page.to_cursor <= page.head_cursor
        && page.has_more == (page.to_cursor < page.head_cursor)
        && (advances_task_feed || context_only)
        && !page.delivery_token.trim().is_empty();
    if !valid {
        return Err(ControlRefusalCode::DeliveryInvalid);
    }

    Ok((page.to_cursor, Some(page.clone())))
}

const fn phase_refusal(
    phase: SessionPhase,
    purpose: TurnPurpose,
    task_state: TaskState,
    has_inline_delivery: bool,
) -> Option<ControlRefusalCode> {
    match phase {
        SessionPhase::Unbound => return Some(ControlRefusalCode::TaskUnbound),
        SessionPhase::Exited => return Some(ControlRefusalCode::SessionExited),
        SessionPhase::TurnOpen => return Some(ControlRefusalCode::TurnAlreadyOpen),
        SessionPhase::CheckpointRequired => {
            return Some(ControlRefusalCode::CheckpointRequired);
        }
        SessionPhase::HandoffPending => return Some(ControlRefusalCode::LifecycleHold),
        SessionPhase::ContributionRequired | SessionPhase::ParticipantReady => {
            return Some(ControlRefusalCode::ParticipantNotReady);
        }
        SessionPhase::Ready
        | SessionPhase::SyncRequired
        | SessionPhase::RecoveryOpen
        | SessionPhase::FinalizerOpen => {}
    }

    match purpose {
        TurnPurpose::Ordinary => {
            if !matches!(task_state, TaskState::Active) {
                return Some(ControlRefusalCode::LifecycleHold);
            }
            match phase {
                SessionPhase::Ready => None,
                SessionPhase::SyncRequired if has_inline_delivery => None,
                SessionPhase::SyncRequired | SessionPhase::RecoveryOpen => {
                    Some(ControlRefusalCode::RecoveryRequired)
                }
                SessionPhase::FinalizerOpen => Some(ControlRefusalCode::LifecycleHold),
                _ => Some(ControlRefusalCode::TurnPurposeMismatch),
            }
        }
        TurnPurpose::Recovery => {
            if (matches!(phase, SessionPhase::RecoveryOpen)
                || (matches!(phase, SessionPhase::SyncRequired) && has_inline_delivery))
                && matches!(
                    task_state,
                    TaskState::Active | TaskState::Quiescing | TaskState::FinalizationPending
                )
            {
                None
            } else {
                Some(ControlRefusalCode::TurnPurposeMismatch)
            }
        }
        TurnPurpose::Finalizer => {
            if matches!(phase, SessionPhase::FinalizerOpen)
                && matches!(task_state, TaskState::FinalizationPending)
            {
                None
            } else {
                Some(ControlRefusalCode::TurnPurposeMismatch)
            }
        }
    }
}

const fn effect_fits_purpose(purpose: TurnPurpose, effect: EffectClass) -> bool {
    match purpose {
        TurnPurpose::Ordinary => !matches!(effect, EffectClass::Coordinate),
        TurnPurpose::Recovery => {
            matches!(effect, EffectClass::Observe | EffectClass::Communicate)
        }
        TurnPurpose::Finalizer => matches!(
            effect,
            EffectClass::Observe | EffectClass::Communicate | EffectClass::Lifecycle
        ),
    }
}

fn refusal(input: &TurnEvaluationInput, code: ControlRefusalCode) -> TurnDecision {
    detailed_refusal(input, code, None, None, None, None)
}

fn detailed_refusal(
    input: &TurnEvaluationInput,
    code: ControlRefusalCode,
    effect: Option<EffectClass>,
    required_assurance: Option<ControlAssurance>,
    declared_mediated_effects: Option<&[EffectClass]>,
    effective_mediated_effects: Option<&[EffectClass]>,
) -> TurnDecision {
    TurnDecision::Refuse {
        directive: control_directive(
            &input.intent.idempotency_key,
            code,
            effect,
            required_assurance,
            declared_mediated_effects,
            effective_mediated_effects,
        ),
    }
}

fn assurance_refusal(
    input: &TurnEvaluationInput,
    effect: Option<EffectClass>,
    required_assurance: ControlAssurance,
) -> TurnDecision {
    let effective = effective_mediated_effects(input.host_assurance, &input.mediated_effects);
    TurnDecision::Refuse {
        directive: control_directive(
            &input.intent.idempotency_key,
            ControlRefusalCode::ControlAssuranceInsufficient,
            effect,
            Some(required_assurance),
            Some(&input.mediated_effects),
            Some(&effective),
        ),
    }
}

/// Builds the common policy-decision directive used by turn and lease gates.
#[must_use]
pub(crate) fn control_directive(
    request_key: &str,
    code: ControlRefusalCode,
    effect: Option<EffectClass>,
    required_assurance: Option<ControlAssurance>,
    declared_mediated_effects: Option<&[EffectClass]>,
    effective_mediated_effects: Option<&[EffectClass]>,
) -> ControlDirective {
    let (target, satisfaction, recovery_effects) = directive_shape(code);
    ControlDirective {
        directive_id: format!("{}:{}", request_key, code.as_str()),
        code,
        effect,
        required_assurance,
        declared_mediated_effects: declared_mediated_effects.map(<[_]>::to_vec),
        effective_mediated_effects: effective_mediated_effects.map(<[_]>::to_vec),
        target,
        satisfaction,
        recovery_effects,
    }
}

fn directive_shape(
    code: ControlRefusalCode,
) -> (DirectiveTarget, DirectiveSatisfaction, Vec<EffectClass>) {
    match code {
        ControlRefusalCode::MissingAuthority => (
            DirectiveTarget::Human,
            DirectiveSatisfaction::HumanAuthority,
            vec![EffectClass::Observe],
        ),
        ControlRefusalCode::PinnedContradiction
        | ControlRefusalCode::PinnedBudgetExceeded
        | ControlRefusalCode::RecoveryRequired
        | ControlRefusalCode::ActionOutcomeUnknown => (
            DirectiveTarget::Agent,
            DirectiveSatisfaction::RecoveryCheckpoint,
            vec![EffectClass::Observe, EffectClass::Communicate],
        ),
        ControlRefusalCode::ControlUnavailable
        | ControlRefusalCode::StoreCorrupt
        | ControlRefusalCode::UnknownControlSchema
        | ControlRefusalCode::ControlPolicyMissing
        | ControlRefusalCode::ControlAssuranceInsufficient
        | ControlRefusalCode::CapabilityNotPermitted
        | ControlRefusalCode::TaskUnbound
        | ControlRefusalCode::TaskAccessDenied
        | ControlRefusalCode::PolicyEpochChanged
        | ControlRefusalCode::TaskAdmissionEpochChanged
        | ControlRefusalCode::LeaseRequired
        | ControlRefusalCode::ContextRequired
        | ControlRefusalCode::DeltaRequired
        | ControlRefusalCode::DeliveryInvalid
        | ControlRefusalCode::CheckpointRequired
        | ControlRefusalCode::TurnAlreadyOpen
        | ControlRefusalCode::TurnPurposeMismatch
        | ControlRefusalCode::LifecycleHold
        | ControlRefusalCode::ParticipantNotReady
        | ControlRefusalCode::GrantExpired
        | ControlRefusalCode::GrantScopeMismatch
        | ControlRefusalCode::StaleFence
        | ControlRefusalCode::ResourceRemapped
        | ControlRefusalCode::SessionExited => (
            DirectiveTarget::Host,
            DirectiveSatisfaction::HostTransition,
            vec![EffectClass::Observe],
        ),
    }
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::{
        ObjectHash,
        domain::{
            ActionBeginDecision, ActionBeginSnapshot, ActionGrantBasis, ActionGrantState,
            AuthorityState, ControlAssurance, ControlEpochs, DeliveryPage, ParentTurnState,
            ParticipantMembership, ProjectId, ProjectPolicyEpoch, ResolutionAssurance,
            ResourceCoverage, ResourceSubject, SessionId, TaskAdmissionEpoch, TaskId, TurnIntent,
        },
    };

    fn hash(seed: &str) -> ObjectHash {
        ObjectHash::from_canonical_bytes(seed.as_bytes())
    }

    fn input() -> TurnEvaluationInput {
        TurnEvaluationInput {
            control_schema_version: CONTROL_SCHEMA_VERSION,
            session_id: SessionId("session-a".into()),
            task_id: Some(TaskId::new()),
            work_binding: None,
            work_binding_current: true,
            participant_membership: ParticipantMembership::Member,
            task_state: Some(TaskState::Active),
            phase: SessionPhase::Ready,
            health: ControlHealth::Healthy,
            active_policy_known: true,
            host_assurance: ControlAssurance::Advisory,
            required_assurance: ControlAssurance::Advisory,
            policy_effects: all_effects(),
            mediated_effects: all_effects(),
            current_epochs: ControlEpochs {
                project_policy: ProjectPolicyEpoch(4),
                task_admission: TaskAdmissionEpoch(9),
            },
            session_epochs: ControlEpochs {
                project_policy: ProjectPolicyEpoch(4),
                task_admission: TaskAdmissionEpoch(9),
            },
            confirmed_cursor: ChangeCursor(12),
            head_cursor: ChangeCursor(12),
            pending_delivery: None,
            packet_safety: PacketSafety::Safe,
            blocking_watermark: ChangeCursor(12),
            acknowledged_blocking_watermark: ChangeCursor(12),
            has_unknown_action_outcome: false,
            authority_satisfied: true,
            capability_map_revision: 3,
            leases: Vec::new(),
            intent: TurnIntent {
                idempotency_key: "turn-a".into(),
                intent_fingerprint: hash("turn intent"),
                purpose: TurnPurpose::Ordinary,
                requested_effects: vec![EffectClass::Observe],
                resource_intents: Vec::new(),
            },
            evaluated_at: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
            grant_ttl_seconds: 30,
        }
    }

    fn all_effects() -> Vec<EffectClass> {
        vec![
            EffectClass::Observe,
            EffectClass::Communicate,
            EffectClass::Coordinate,
            EffectClass::MutateLocal,
            EffectClass::MutateShared,
            EffectClass::ExternalSideEffect,
            EffectClass::Lifecycle,
        ]
    }

    fn refusal_code(observation: &ObservedTurnDecision) -> Option<ControlRefusalCode> {
        match &observation.decision {
            TurnDecision::Refuse { directive } => Some(directive.code),
            TurnDecision::Grant { .. } | TurnDecision::Defer { .. } => None,
        }
    }

    fn action() -> (ActionGrantBasis, ActionBeginSnapshot) {
        let task_id = TaskId::new();
        let session_id = SessionId("session-a".into());
        let epochs = ControlEpochs {
            project_policy: ProjectPolicyEpoch(4),
            task_admission: TaskAdmissionEpoch(9),
        };
        let subjects = vec![ResourceSubject::Path {
            project_id: ProjectId("project-a".into()),
            segments: vec!["src".into(), "control.rs".into()],
            coverage: ResourceCoverage::Exact,
        }];
        let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let leases = vec![LeaseBasis {
            lease_id: "lease-a".into(),
            holder: session_id.clone(),
            kind: LeaseKind::Execution,
            mode: LeaseMode::Exclusive,
            subject: subjects[0].clone(),
            fence: 7,
            expires_at: now + TimeDelta::seconds(60),
        }];
        let binding = Some(hash("resolution binding"));
        let grant = ActionGrantBasis {
            control_schema_version: CONTROL_SCHEMA_VERSION,
            grant_id: "action-grant-a".into(),
            parent_turn_id: "turn-grant-a".into(),
            session_id: session_id.clone(),
            task_id,
            turn_purpose: TurnPurpose::Ordinary,
            effect: EffectClass::MutateShared,
            resource_subjects: subjects.clone(),
            request_fingerprint: hash("write request"),
            authority_references: vec!["host-policy:workspace-write".into()],
            epochs,
            blocking_watermark: ChangeCursor(12),
            capability_map_revision: 3,
            leases: leases.clone(),
            resolution_binding_digest: binding.clone(),
            expires_at: now + TimeDelta::seconds(30),
        };
        let snapshot = ActionBeginSnapshot {
            control_schema_version: CONTROL_SCHEMA_VERSION,
            parent_turn_id: "turn-grant-a".into(),
            parent_turn_state: ParentTurnState::Open,
            grant_state: ActionGrantState::Available,
            session_id,
            task_id,
            phase: SessionPhase::TurnOpen,
            task_state: TaskState::Active,
            turn_purpose: TurnPurpose::Ordinary,
            effect: EffectClass::MutateShared,
            resource_subjects: subjects,
            request_fingerprint: hash("write request"),
            authority_references: vec!["host-policy:workspace-write".into()],
            authority_state: AuthorityState::Valid,
            current_epochs: epochs,
            acknowledged_blocking_watermark: ChangeCursor(12),
            capability_map_revision: 3,
            leases,
            resolution_binding_digest: binding,
            resolution_assurance: ResolutionAssurance::PinnedThroughInvocation,
            observed_at: now,
        };
        (grant, snapshot)
    }

    fn action_refusal_code(
        grant: &ActionGrantBasis,
        snapshot: &ActionBeginSnapshot,
    ) -> Option<ControlRefusalCode> {
        match observe_action_begin(grant, snapshot).decision {
            ActionBeginDecision::Begin { .. } => None,
            ActionBeginDecision::Refuse { code } => Some(code),
        }
    }

    #[test]
    fn synchronized_turn_observation_is_deterministic() {
        let input = input();
        let first = observe_turn(&input);
        let replay = observe_turn(&input);

        assert_eq!(first, replay);
        assert_eq!(
            serde_json::to_vec(&first).unwrap(),
            serde_json::to_vec(&replay).unwrap()
        );
        assert!(matches!(first.decision, TurnDecision::Grant { .. }));
    }

    #[test]
    fn effect_assurance_floors_cap_declared_host_mediation() {
        for effect in [EffectClass::Observe, EffectClass::Communicate] {
            assert_eq!(
                minimum_assurance_for_effect(effect),
                ControlAssurance::Advisory
            );
        }
        for effect in [
            EffectClass::Coordinate,
            EffectClass::MutateLocal,
            EffectClass::MutateShared,
            EffectClass::ExternalSideEffect,
            EffectClass::Lifecycle,
        ] {
            assert_eq!(
                minimum_assurance_for_effect(effect),
                ControlAssurance::TurnGated
            );
        }
        assert_eq!(
            effective_mediated_effects(ControlAssurance::Advisory, &all_effects()),
            vec![EffectClass::Observe, EffectClass::Communicate]
        );
        assert_eq!(
            effective_mediated_effects(ControlAssurance::TurnGated, &all_effects()),
            all_effects()
        );
    }

    #[test]
    fn advisory_mutation_refuses_even_with_declared_effect_and_live_lease() {
        let mut input = input();
        let subject = ResourceSubject::Path {
            project_id: ProjectId("project-a".into()),
            segments: vec!["src".into()],
            coverage: ResourceCoverage::Tree,
        };
        input.intent.requested_effects = vec![EffectClass::MutateLocal];
        input.intent.resource_intents = vec![subject.clone()];
        input.leases = vec![LeaseBasis {
            lease_id: "lease-effect-floor".into(),
            holder: input.session_id.clone(),
            kind: LeaseKind::Execution,
            mode: LeaseMode::Exclusive,
            subject,
            fence: 1,
            expires_at: input.evaluated_at + TimeDelta::seconds(60),
        }];

        let observation = observe_turn(&input);
        let TurnDecision::Refuse { directive } = observation.decision else {
            panic!("advisory mutation must refuse");
        };
        assert_eq!(
            directive.code,
            ControlRefusalCode::ControlAssuranceInsufficient
        );
        assert_eq!(directive.effect, Some(EffectClass::MutateLocal));
        assert_eq!(
            directive.required_assurance,
            Some(ControlAssurance::TurnGated)
        );
        assert_eq!(directive.declared_mediated_effects, Some(all_effects()));
        assert_eq!(
            directive.effective_mediated_effects,
            Some(vec![EffectClass::Observe, EffectClass::Communicate])
        );

        input.host_assurance = ControlAssurance::TurnGated;
        assert!(matches!(
            observe_turn(&input).decision,
            TurnDecision::Grant { .. }
        ));
    }

    #[test]
    fn project_assurance_refusal_is_not_misattributed_to_an_effect() {
        let mut input = input();
        input.required_assurance = ControlAssurance::TurnGated;

        let TurnDecision::Refuse { directive } = observe_turn(&input).decision else {
            panic!("advisory host must not satisfy a turn-gated project policy");
        };
        assert_eq!(
            directive.code,
            ControlRefusalCode::ControlAssuranceInsufficient
        );
        assert_eq!(directive.effect, None);
        assert_eq!(
            directive.required_assurance,
            Some(ControlAssurance::TurnGated)
        );
    }

    #[test]
    fn internal_coordinate_effect_is_not_a_model_turn_capability() {
        let mut input = input();
        input.host_assurance = ControlAssurance::TurnGated;
        input.intent.requested_effects = vec![EffectClass::Coordinate];

        assert_eq!(
            refusal_code(&observe_turn(&input)),
            Some(ControlRefusalCode::GrantScopeMismatch)
        );
        let TurnDecision::Refuse { directive } = observe_turn(&input).decision else {
            panic!("coordinate must remain lease-boundary only");
        };
        assert_eq!(directive.effect, Some(EffectClass::Coordinate));
    }

    #[test]
    fn undeclared_effect_refusal_names_the_complete_mediation_envelope() {
        let mut input = input();
        input.host_assurance = ControlAssurance::TurnGated;
        input.mediated_effects = vec![EffectClass::Observe];
        input.intent.requested_effects = vec![EffectClass::MutateLocal];

        let TurnDecision::Refuse { directive } = observe_turn(&input).decision else {
            panic!("an observe-only host must not mediate mutation");
        };
        assert_eq!(
            directive.code,
            ControlRefusalCode::ControlAssuranceInsufficient
        );
        assert_eq!(directive.effect, Some(EffectClass::MutateLocal));
        assert_eq!(
            directive.required_assurance,
            Some(ControlAssurance::TurnGated)
        );
        assert_eq!(
            directive.declared_mediated_effects,
            Some(vec![EffectClass::Observe])
        );
        assert_eq!(
            directive.effective_mediated_effects,
            Some(vec![EffectClass::Observe])
        );
    }

    #[test]
    fn unsupported_effect_refusal_names_the_policy_exclusion() {
        let mut input = input();
        input.host_assurance = ControlAssurance::TurnGated;
        input.policy_effects = vec![EffectClass::Observe, EffectClass::Communicate];
        input.intent.requested_effects = vec![EffectClass::MutateLocal];

        let TurnDecision::Refuse { directive } = observe_turn(&input).decision else {
            panic!("an effect outside the active policy must refuse");
        };
        assert_eq!(directive.code, ControlRefusalCode::CapabilityNotPermitted);
        assert_eq!(directive.effect, Some(EffectClass::MutateLocal));
        assert_eq!(directive.required_assurance, None);
        assert_eq!(directive.declared_mediated_effects, Some(all_effects()));
        assert_eq!(directive.effective_mediated_effects, Some(all_effects()));
    }

    #[test]
    fn lease_policy_uses_the_same_assurance_capability_and_epoch_ladder() {
        let declared = vec![EffectClass::Observe, EffectClass::MutateLocal];
        let policy_effects = vec![EffectClass::Observe];
        let refusal = evaluate_lease_policy(&LeasePolicyInput {
            request_key: "lease_acquire:lease-a",
            host_assurance: ControlAssurance::TurnGated,
            declared_mediated_effects: &declared,
            project_required_assurance: ControlAssurance::TurnGated,
            policy_effects: &policy_effects,
            session_policy_epoch: ProjectPolicyEpoch(2),
            active_policy_epoch: ProjectPolicyEpoch(2),
            effect: EffectClass::MutateLocal,
        })
        .expect_err("unsupported lease effect must refuse");
        assert_eq!(
            refusal.directive.code,
            ControlRefusalCode::CapabilityNotPermitted
        );
        assert_eq!(refusal.directive.effect, Some(EffectClass::MutateLocal));
        assert_eq!(refusal.directive.required_assurance, None);
        assert!(!refusal.adopt_project_policy_epoch);

        let policy_effects = vec![EffectClass::Observe, EffectClass::MutateLocal];
        let refusal = evaluate_lease_policy(&LeasePolicyInput {
            request_key: "lease_acquire:lease-b",
            host_assurance: ControlAssurance::TurnGated,
            declared_mediated_effects: &declared,
            project_required_assurance: ControlAssurance::TurnGated,
            policy_effects: &policy_effects,
            session_policy_epoch: ProjectPolicyEpoch(1),
            active_policy_epoch: ProjectPolicyEpoch(2),
            effect: EffectClass::MutateLocal,
        })
        .expect_err("stale lease epoch must refuse");
        assert_eq!(
            refusal.directive.code,
            ControlRefusalCode::PolicyEpochChanged
        );
        assert!(refusal.adopt_project_policy_epoch);
    }

    #[test]
    fn fresh_delivery_is_inlined_instead_of_refused() {
        let mut input = input();
        input.phase = SessionPhase::SyncRequired;
        input.head_cursor = ChangeCursor(15);
        input.blocking_watermark = ChangeCursor(14);
        input.pending_delivery = Some(DeliveryPage {
            from_cursor: ChangeCursor(12),
            to_cursor: ChangeCursor(15),
            head_cursor: ChangeCursor(15),
            has_more: false,
            content_digest: hash("delta page"),
            delivery_token: "delivery-a".into(),
        });

        let observation = observe_turn(&input);
        match observation.decision {
            TurnDecision::Grant { basis } => {
                assert_eq!(basis.confirmed_cursor, ChangeCursor(12));
                assert_eq!(basis.delivery_cursor, ChangeCursor(15));
                assert!(basis.inline_delivery.is_some());
            }
            TurnDecision::Refuse { directive } => {
                panic!("expected inline grant, got {:?}", directive.code);
            }
            TurnDecision::Defer { deferral } => {
                panic!("expected inline grant, got defer {:?}", deferral.code);
            }
        }
    }

    #[test]
    fn malformed_delivery_page_refuses() {
        let mut input = input();
        input.phase = SessionPhase::SyncRequired;
        input.head_cursor = ChangeCursor(15);
        input.pending_delivery = Some(DeliveryPage {
            from_cursor: ChangeCursor(11),
            to_cursor: ChangeCursor(15),
            head_cursor: ChangeCursor(15),
            has_more: false,
            content_digest: hash("bad page"),
            delivery_token: "delivery-a".into(),
        });

        assert_eq!(
            refusal_code(&observe_turn(&input)),
            Some(ControlRefusalCode::DeliveryInvalid)
        );
    }

    #[test]
    fn oversized_delivery_refuses_with_a_typed_delivery_error() {
        let mut input = input();
        input.packet_safety = PacketSafety::DeliveryBudgetExceeded;

        assert_eq!(
            refusal_code(&observe_turn(&input)),
            Some(ControlRefusalCode::DeliveryInvalid)
        );
    }

    #[test]
    fn partial_delivery_page_requires_a_recovery_turn() {
        let mut input = input();
        input.phase = SessionPhase::SyncRequired;
        input.head_cursor = ChangeCursor(15);
        input.blocking_watermark = ChangeCursor(14);
        input.pending_delivery = Some(DeliveryPage {
            from_cursor: ChangeCursor(12),
            to_cursor: ChangeCursor(14),
            head_cursor: ChangeCursor(15),
            has_more: true,
            content_digest: hash("partial page"),
            delivery_token: "delivery-partial".into(),
        });

        assert_eq!(
            refusal_code(&observe_turn(&input)),
            Some(ControlRefusalCode::RecoveryRequired)
        );
        input.intent.purpose = TurnPurpose::Recovery;
        assert!(matches!(
            observe_turn(&input).decision,
            TurnDecision::Grant { .. }
        ));
        input.intent.requested_effects = vec![EffectClass::Observe, EffectClass::Communicate];
        assert_eq!(
            refusal_code(&observe_turn(&input)),
            Some(ControlRefusalCode::GrantScopeMismatch)
        );
    }

    #[test]
    fn stale_policy_epoch_precedes_a_would_be_grant() {
        let mut input = input();
        input.current_epochs.project_policy = ProjectPolicyEpoch(5);

        assert_eq!(
            refusal_code(&observe_turn(&input)),
            Some(ControlRefusalCode::PolicyEpochChanged)
        );
    }

    #[test]
    fn checkpoint_and_unknown_outcome_are_fail_closed_observations() {
        let mut checkpoint = input();
        checkpoint.phase = SessionPhase::CheckpointRequired;
        assert_eq!(
            refusal_code(&observe_turn(&checkpoint)),
            Some(ControlRefusalCode::CheckpointRequired)
        );

        let mut unknown = input();
        unknown.has_unknown_action_outcome = true;
        assert_eq!(
            refusal_code(&observe_turn(&unknown)),
            Some(ControlRefusalCode::ActionOutcomeUnknown)
        );
    }

    #[test]
    fn recovery_turn_cannot_request_mutation() {
        let mut input = input();
        input.host_assurance = ControlAssurance::TurnGated;
        input.phase = SessionPhase::RecoveryOpen;
        input.intent.purpose = TurnPurpose::Recovery;
        input.intent.requested_effects = vec![EffectClass::MutateShared];

        assert_eq!(
            refusal_code(&observe_turn(&input)),
            Some(ControlRefusalCode::GrantScopeMismatch)
        );
    }

    #[test]
    fn lifecycle_phase_precedes_packet_safety() {
        let mut input = input();
        input.phase = SessionPhase::CheckpointRequired;
        input.packet_safety = PacketSafety::PinnedContradiction;

        assert_eq!(
            refusal_code(&observe_turn(&input)),
            Some(ControlRefusalCode::CheckpointRequired)
        );
    }

    #[test]
    fn exact_action_basis_would_begin_deterministically() {
        let (grant, snapshot) = action();
        let first = observe_action_begin(&grant, &snapshot);
        let replay = observe_action_begin(&grant, &snapshot);

        assert_eq!(first, replay);
        assert!(matches!(
            first.decision,
            ActionBeginDecision::Begin { grant_id } if grant_id == grant.grant_id
        ));
    }

    #[test]
    fn action_begin_rechecks_epochs_watermark_and_fences() {
        let (grant, snapshot) = action();

        let mut stale_epoch = snapshot.clone();
        stale_epoch.current_epochs.task_admission = TaskAdmissionEpoch(10);
        assert_eq!(
            action_refusal_code(&grant, &stale_epoch),
            Some(ControlRefusalCode::TaskAdmissionEpochChanged)
        );

        let mut stale_delivery = snapshot.clone();
        stale_delivery.acknowledged_blocking_watermark = ChangeCursor(11);
        assert_eq!(
            action_refusal_code(&grant, &stale_delivery),
            Some(ControlRefusalCode::DeltaRequired)
        );

        let mut stale_fence = snapshot;
        stale_fence.leases[0].fence += 1;
        assert_eq!(
            action_refusal_code(&grant, &stale_fence),
            Some(ControlRefusalCode::StaleFence)
        );
    }

    #[test]
    fn filesystem_action_requires_pinned_unchanged_resolution() {
        let (grant, snapshot) = action();

        let mut unpinned = snapshot.clone();
        unpinned.resolution_assurance = ResolutionAssurance::DetectionOnly;
        assert_eq!(
            action_refusal_code(&grant, &unpinned),
            Some(ControlRefusalCode::ControlAssuranceInsufficient)
        );

        let mut remapped = snapshot;
        remapped.resolution_binding_digest = Some(hash("different target"));
        assert_eq!(
            action_refusal_code(&grant, &remapped),
            Some(ControlRefusalCode::ResourceRemapped)
        );
    }

    #[test]
    fn mutation_requires_live_holder_owned_covering_lease() {
        let (grant, snapshot) = action();

        let mut missing_grant = grant.clone();
        missing_grant.leases.clear();
        let mut missing_snapshot = snapshot.clone();
        missing_snapshot.leases.clear();
        assert_eq!(
            action_refusal_code(&missing_grant, &missing_snapshot),
            Some(ControlRefusalCode::LeaseRequired)
        );

        let mut expired = snapshot.clone();
        expired.leases[0].expires_at = expired.observed_at;
        assert_eq!(
            action_refusal_code(&grant, &expired),
            Some(ControlRefusalCode::StaleFence)
        );

        let mut wrong_holder_grant = grant.clone();
        wrong_holder_grant.leases[0].holder = SessionId("other-session".into());
        let mut wrong_holder_snapshot = snapshot;
        wrong_holder_snapshot.leases[0].holder = SessionId("other-session".into());
        assert_eq!(
            action_refusal_code(&wrong_holder_grant, &wrong_holder_snapshot),
            Some(ControlRefusalCode::StaleFence)
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the matcher regression keeps every binding and stale-cut assertion together"
    )]
    fn verification_match_is_kind_safe_cross_workspace_and_stale_at_the_cut() {
        use crate::domain::{
            ActorContext, AssuranceLevel, ControlWorkBinding, ExecutionObservation,
            ExecutionSourceBasis, RootExecutionId, VerificationEvidence, VerificationKind,
            VerificationResult, WorkClaimId, WorkEvidenceKind, WorkId, WorkRunId,
        };

        let run_id = WorkRunId::new();
        let session_id = SessionId("verification-host".into());
        let binding = ControlWorkBinding {
            root_execution_id: RootExecutionId::new(),
            work_id: WorkId::new(),
            run_id,
            work_revision: 3,
            claim_id: WorkClaimId::new(),
            claim_fence: 2,
        };
        let actor = ActorContext {
            actor_id: "host-adapter".into(),
            actor_kind: "system".into(),
            assurance: AssuranceLevel::Asserted,
            run_id: Some(run_id.0.to_string()),
            session_id: Some(session_id.clone()),
            source_tool: Some("host-control:turn_checkpoint".into()),
            source_skill: None,
            provenance_chain: Vec::new(),
            reason: "record host fact".into(),
        };
        let project_id = ProjectId("verification-project".into());
        let mutation_time = Utc.timestamp_millis_opt(10_000).unwrap();
        let verification_time = Utc.timestamp_millis_opt(20_000).unwrap();
        let latest_mutation = ExecutionObservation {
            schema_version: crate::domain::SCHEMA_VERSION,
            project_id: project_id.clone(),
            binding: binding.clone(),
            session_id: session_id.clone(),
            grant_id: "mutation-grant".into(),
            observation_id: "source-mutation".into(),
            action_fingerprint: hash("mutate source"),
            effect: EffectClass::MutateLocal,
            outcome: crate::domain::ExecutionOutcome::Succeeded,
            source_changed: true,
            source_basis: Some(ExecutionSourceBasis {
                workspace_id: "workspace-a".into(),
                source_revision: "content-revision-1".into(),
            }),
            observed_at: Some(mutation_time),
            actor: actor.clone(),
            recorded_at: mutation_time,
        };
        let producer = ExecutionObservation {
            schema_version: crate::domain::SCHEMA_VERSION,
            project_id: project_id.clone(),
            binding: binding.clone(),
            session_id: session_id.clone(),
            grant_id: "verification-grant".into(),
            observation_id: "test-command".into(),
            action_fingerprint: hash("cargo test command"),
            effect: EffectClass::Observe,
            outcome: crate::domain::ExecutionOutcome::Succeeded,
            source_changed: false,
            source_basis: Some(ExecutionSourceBasis {
                workspace_id: "workspace-b".into(),
                source_revision: "content-revision-1".into(),
            }),
            observed_at: Some(verification_time),
            actor: actor.clone(),
            recorded_at: verification_time,
        };
        let evidence = VerificationEvidence {
            schema_version: crate::domain::SCHEMA_VERSION,
            project_id,
            binding,
            session_id,
            producer_observation: hash("producer observation"),
            source_basis: producer.source_basis.clone().unwrap(),
            check_kind: VerificationKind::Test,
            check_fingerprint: producer.action_fingerprint.clone(),
            result: VerificationResult::Passed,
            completed_at: verification_time,
            summary: "host recorded tests".into(),
            refs: Vec::new(),
            actor,
            recorded_at: verification_time,
        };
        let required = evidence.check_fingerprint.clone();
        let exact = VerificationEvidenceMatchInput {
            candidate_kind: WorkEvidenceKind::Verification,
            evidence: Some(&evidence),
            producer: Some(&producer),
            latest_mutation: &latest_mutation,
            evidence_position: 4,
            latest_mutation_position: 1,
            required_check_fingerprint: &required,
        };
        assert_eq!(match_verification_evidence(&exact), Ok(()));

        let wrong_kind = VerificationEvidenceMatchInput {
            candidate_kind: WorkEvidenceKind::Generic,
            ..exact
        };
        assert_eq!(
            match_verification_evidence(&wrong_kind),
            Err(VerificationEvidenceMismatch::WrongKind)
        );

        let mut later_mutation = latest_mutation;
        later_mutation
            .source_basis
            .as_mut()
            .unwrap()
            .source_revision = "content-revision-2".into();
        let stale = VerificationEvidenceMatchInput {
            candidate_kind: WorkEvidenceKind::Verification,
            evidence: Some(&evidence),
            producer: Some(&producer),
            latest_mutation: &later_mutation,
            evidence_position: 4,
            latest_mutation_position: 3,
            required_check_fingerprint: &required,
        };
        assert_eq!(
            match_verification_evidence(&stale),
            Err(VerificationEvidenceMismatch::StaleSourceRevision)
        );
    }
}
