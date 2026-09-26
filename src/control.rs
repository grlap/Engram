//! Deterministic behavioral-control evaluation.
//!
//! [`observe_turn`] remains a shadow evidence path. The same pure rules also
//! support the host-private persisted lifecycle, whose storage transaction is
//! responsible for minting and consuming authority.

use std::collections::HashSet;

use chrono::TimeDelta;
use serde::Serialize;

use crate::{
    CanonicalObject, ObjectId,
    domain::{
        BuiltinObligationRuleRef, BuiltinObligationTrigger, CONTROL_SCHEMA_VERSION, ContextPacket,
        ControlAssurance, ControlDirective, ControlRefusalCode, DirectiveSatisfaction,
        DirectiveTarget, EffectClass, ExecutionObservation, IssuedTurnGrant,
        OBLIGATION_RULE_SET_SCHEMA_VERSION, ObligationRuleDefinition, ObligationRuleSet,
        ObservedTurnDecision, ParticipantMembership, SessionPhase, TaskDelta, TurnBeginDecision,
        TurnBeginSnapshot, TurnCheckpointDecision, TurnCheckpointSnapshot, TurnDecision,
        TurnEvaluationInput, TurnGrantBasis, TurnGrantState, VerificationEvidence,
        VerificationEvidenceMismatch, VerificationRequirement, VerificationResult,
        WorkEvidenceKind, WorkObligation, WorkObligationId,
    },
    storage::StoreError,
};

const MAX_SHADOW_GRANT_TTL_SECONDS: i64 = 300;

/// Immutable inputs for matching typed verification evidence at one exact
/// dense run-feed cut.
#[derive(Clone, Copy)]
pub struct VerificationEvidenceMatchInput<'a> {
    pub candidate_kind: WorkEvidenceKind,
    pub evidence: Option<&'a VerificationEvidence>,
    pub producer: Option<&'a ExecutionObservation>,
    /// The newest source mutation the run observed at the cut, with its
    /// run-feed position, or `None` when the run observed no mutation: then
    /// the verification is of the run's source as it stands, and the checks
    /// that compare it with a mutation do not apply.
    pub latest_mutation: Option<(&'a ExecutionObservation, i64)>,
    pub evidence_position: i64,
    pub requirement: &'a VerificationRequirement,
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
    if evidence.check_kind != input.requirement.check_kind {
        return Err(VerificationEvidenceMismatch::CheckKindMismatch);
    }
    let mutation = input
        .latest_mutation
        .map(|(latest_mutation, position)| {
            let basis = latest_mutation
                .source_basis
                .as_ref()
                .ok_or(VerificationEvidenceMismatch::InvalidProducer)?;
            let observed_at = latest_mutation
                .observed_at
                .ok_or(VerificationEvidenceMismatch::InvalidProducer)?;
            Ok::<_, VerificationEvidenceMismatch>((latest_mutation, position, basis, observed_at))
        })
        .transpose()?;
    let same_run = producer.project_id == evidence.project_id
        && producer.binding == evidence.binding
        && producer.session_id == evidence.session_id
        && mutation.is_none_or(|(latest_mutation, _, _, _)| {
            evidence.project_id == latest_mutation.project_id
                && evidence.binding.root_execution_id == latest_mutation.binding.root_execution_id
                && evidence.binding.work_id == latest_mutation.binding.work_id
                && evidence.binding.run_id == latest_mutation.binding.run_id
        });
    if !same_run {
        return Err(VerificationEvidenceMismatch::WrongRun);
    }
    if let Some((latest_mutation, _, latest_basis, _)) = mutation
        && (!latest_mutation.source_changed
            || evidence.source_basis.source_revision != latest_basis.source_revision)
    {
        return Err(VerificationEvidenceMismatch::StaleSourceRevision);
    }
    if evidence.check_fingerprint != producer.action_fingerprint
        || input
            .requirement
            .check_fingerprint
            .as_ref()
            .is_some_and(|required| required != &evidence.check_fingerprint)
    {
        return Err(VerificationEvidenceMismatch::CheckFingerprintMismatch);
    }
    if input
        .requirement
        .required_environment
        .as_ref()
        .is_some_and(|required| evidence.environment.as_ref() != Some(required))
    {
        return Err(VerificationEvidenceMismatch::EnvironmentMismatch);
    }
    if evidence.result != VerificationResult::Passed {
        return Err(VerificationEvidenceMismatch::ResultNotPassed);
    }
    if mutation.is_some_and(|(_, latest_mutation_position, _, _)| {
        input.evidence_position <= latest_mutation_position
    }) {
        return Err(VerificationEvidenceMismatch::NotAfterMutation);
    }
    let actor_matches = evidence.actor.session_id.as_ref() == Some(&evidence.session_id)
        && evidence.actor.run_id.as_deref() == Some(evidence.binding.run_id.0.to_string().as_str())
        && producer.actor.session_id.as_ref() == Some(&producer.session_id)
        && producer.actor.run_id.as_deref() == Some(producer.binding.run_id.0.to_string().as_str());
    let times_are_monotone = mutation
        .is_none_or(|(_, _, _, latest_observed_at)| evidence.completed_at >= latest_observed_at)
        && evidence.completed_at <= evidence.recorded_at
        && producer.observed_at == Some(evidence.completed_at)
        && producer.recorded_at >= evidence.completed_at;
    if !actor_matches || !times_are_monotone {
        return Err(VerificationEvidenceMismatch::InvalidTime);
    }
    Ok(())
}

/// Rule id of the stock rule that asks for a passing test after each source
/// change. It records rather than blocks: completion resolves an obligation
/// it opened that no matching passing test followed as an attributed waiver,
/// and the item discloses that change as untested.
pub const SOURCE_CHANGE_RULE_ID: &str = "source_mutation_requires_test";

/// Whether an obligation opened for `rule` with `requirement` is the stock
/// source-change rule's, whose open obligations completion records as
/// untested instead of refusing. It matches the stock definition exactly, so
/// an operator-selected rule that reuses the id with another version or a
/// pinned check or environment still blocks.
#[must_use]
pub fn is_stock_source_change_obligation(
    rule: &BuiltinObligationRuleRef,
    requirement: &VerificationRequirement,
) -> bool {
    builtin_obligation_rule_set()
        .rules
        .iter()
        .any(|definition| {
            definition.trigger == BuiltinObligationTrigger::SourceChanged
                && definition.rule == *rule
                && definition.requirement == *requirement
        })
}

/// Stock immutable V1 rule table installed by project-policy bootstrap.
#[must_use]
pub fn builtin_obligation_rule_set() -> ObligationRuleSet {
    ObligationRuleSet {
        schema_version: OBLIGATION_RULE_SET_SCHEMA_VERSION,
        rules: vec![ObligationRuleDefinition {
            rule: BuiltinObligationRuleRef {
                rule_id: SOURCE_CHANGE_RULE_ID.into(),
                rule_version: 1,
            },
            trigger: BuiltinObligationTrigger::SourceChanged,
            requirement: VerificationRequirement {
                check_kind: crate::domain::VerificationKind::Test,
                check_fingerprint: None,
                required_environment: None,
            },
        }],
    }
}

/// Rule id prefix of an obligation an acceptance criterion's binding opens.
/// The bound criterion's one-based position follows it, so a run holds at
/// most one such obligation per criterion and trigger.
pub const ACCEPTANCE_BINDING_RULE_PREFIX: &str = "acceptance_criterion_requires_verification:";
const ACCEPTANCE_BINDING_RULE_VERSION: u16 = 1;

/// The rule an acceptance binding on the criterion at `criterion` opens.
#[must_use]
pub fn acceptance_binding_rule(criterion: usize) -> BuiltinObligationRuleRef {
    BuiltinObligationRuleRef {
        rule_id: format!("{ACCEPTANCE_BINDING_RULE_PREFIX}{criterion}"),
        rule_version: ACCEPTANCE_BINDING_RULE_VERSION,
    }
}

/// The criterion position an obligation's rule names, when an acceptance
/// binding opened it.
#[must_use]
pub fn acceptance_binding_criterion(rule: &BuiltinObligationRuleRef) -> Option<usize> {
    rule.rule_id
        .strip_prefix(ACCEPTANCE_BINDING_RULE_PREFIX)?
        .parse()
        .ok()
}

/// Evaluates one exact immutable rule set against one host observation.
#[must_use]
pub fn evaluate_obligation_rules(
    rule_set: &ObligationRuleSet,
    observation: &ExecutionObservation,
) -> Vec<(BuiltinObligationRuleRef, VerificationRequirement)> {
    rule_set
        .rules
        .iter()
        .filter(|definition| match definition.trigger {
            BuiltinObligationTrigger::SourceChanged => observation.source_changed,
            BuiltinObligationTrigger::Unknown => false,
        })
        .map(|definition| (definition.rule.clone(), definition.requirement.clone()))
        .collect()
}

/// Complete immutable inputs for resolving open obligations with one evidence
/// candidate at an exact dense run-feed cut.
pub struct ObligationSatisfactionInput<'a> {
    pub open_obligations: &'a [WorkObligation],
    pub evidence: &'a VerificationEvidence,
    pub producer: &'a ExecutionObservation,
    /// The newest source mutation at the cut with its run-feed position, or
    /// `None` when the run observed none. A builtin rule is triggered by a
    /// mutation and is never satisfied without one; a binding's obligation
    /// is satisfied by verification of the source as it stands.
    pub latest_mutation: Option<(&'a ExecutionObservation, i64)>,
    pub evidence_position: i64,
    pub evaluated_cut: &'a crate::domain::FeedPosition,
}

/// Returns the open definitions satisfied by one exact verification fact.
///
/// Storage owns snapshotting and append-only persistence. This function owns
/// the deterministic rule and anti-stale decision only.
#[must_use]
pub fn evaluate_obligation_satisfaction(
    input: &ObligationSatisfactionInput<'_>,
) -> Vec<WorkObligationId> {
    let expected_feed = crate::domain::FeedId::RunExecution(input.evidence.binding.run_id);
    if input.evaluated_cut.feed != expected_feed
        || input.evidence_position > input.evaluated_cut.position
    {
        return Vec::new();
    }
    input
        .open_obligations
        .iter()
        .filter(|obligation| {
            obligation.run_id == input.evidence.binding.run_id
                && obligation.trigger_position.feed == expected_feed
                && obligation.trigger_position.position <= input.evaluated_cut.position
                && input.evidence_position > obligation.trigger_position.position
                && (input.latest_mutation.is_some()
                    || acceptance_binding_criterion(&obligation.rule).is_some())
        })
        .filter(|obligation| {
            match_verification_evidence(&VerificationEvidenceMatchInput {
                candidate_kind: WorkEvidenceKind::Verification,
                evidence: Some(input.evidence),
                producer: Some(input.producer),
                latest_mutation: input.latest_mutation,
                evidence_position: input.evidence_position,
                requirement: &obligation.requirement,
            })
            .is_ok()
        })
        .map(|obligation| obligation.obligation_id)
        .collect()
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

#[derive(Serialize)]
struct ControlDeliveryContent<'a> {
    context: Option<&'a ContextPacket>,
    delta: &'a TaskDelta,
}

pub(crate) fn delivery_content_digest(
    context: Option<&ContextPacket>,
    delta: &TaskDelta,
) -> Result<ObjectId, StoreError> {
    Ok(
        CanonicalObject::freeze(&ControlDeliveryContent { context, delta })?
            .key()
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

/// Rechecks a persisted turn grant immediately before prompt dispatch.
///
/// This function performs no I/O. The storage layer must evaluate it and
/// consume an issued grant in the same transaction as the session revision
/// update.
#[must_use]
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
    // Anchor before membership, as turn permission checks them: storage
    // derives membership through the anchor, so a lost anchor also clears it.
    if !snapshot.anchor_exists {
        return TurnBeginDecision::Refuse {
            code: ControlRefusalCode::TaskUnbound,
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
    if snapshot.capability_map_revision != basis.capability_map_revision
        || !snapshot.delivery_tokens.is_empty()
    {
        return TurnBeginDecision::Refuse {
            code: ControlRefusalCode::GrantScopeMismatch,
        };
    }

    TurnBeginDecision::Begin
}

/// Checks whether a begun turn can transition to a durable checkpoint.
///
/// Checkpoint deliberately does not recheck expiry, policy epochs, or claim
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
    {
        return TurnCheckpointDecision::Refuse {
            code: ControlRefusalCode::GrantScopeMismatch,
        };
    }
    if matches!(snapshot.grant_state, TurnGrantState::Issued) {
        return TurnCheckpointDecision::Refuse {
            code: ControlRefusalCode::GrantNotBegun,
        };
    }
    if !matches!(snapshot.grant_state, TurnGrantState::Begun) {
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
    let Some(distance) = page.to_cursor.0.checked_sub(page.from_cursor.0) else {
        return false;
    };
    let Ok(expected_count) = usize::try_from(distance) else {
        return false;
    };
    page.has_more == (page.to_cursor < page.head_cursor)
        && delta.after == page.from_cursor
        && delta.cursor == page.to_cursor
        && delta.changes.len() == expected_count
        && delta.changes.iter().enumerate().all(|(offset, change)| {
            i64::try_from(offset).is_ok_and(|offset| {
                page.from_cursor
                    .0
                    .checked_add(offset)
                    .and_then(|cursor| cursor.checked_add(1))
                    .is_some_and(|cursor| change.cursor.0 == cursor)
            })
        })
}

fn effects_are_unique(effects: &[EffectClass]) -> bool {
    let unique: HashSet<_> = effects.iter().collect();
    unique.len() == effects.len()
}

#[allow(
    clippy::too_many_lines,
    reason = "the pure evaluator keeps fail-closed admission order visible in one function"
)]
fn evaluate_turn(input: &TurnEvaluationInput) -> TurnDecision {
    if input.control_schema_version != CONTROL_SCHEMA_VERSION {
        return refusal(input, ControlRefusalCode::UnknownControlSchema);
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
    if !input.anchor_exists {
        return refusal(input, ControlRefusalCode::TaskUnbound);
    }
    if !matches!(input.participant_membership, ParticipantMembership::Member) {
        return refusal(input, ControlRefusalCode::TaskAccessDenied);
    }
    if let Some(code) = phase_refusal(input.phase) {
        return refusal(input, code);
    }
    if input.current_epochs.project_policy != input.session_epochs.project_policy {
        return refusal(input, ControlRefusalCode::PolicyEpochChanged);
    }
    if input.current_epochs.task_admission != input.session_epochs.task_admission {
        return refusal(input, ControlRefusalCode::TaskAdmissionEpochChanged);
    }
    if turn_input_has_invalid_shape(input) {
        return refusal(input, ControlRefusalCode::GrantScopeMismatch);
    }
    if let Some(effect) = input
        .intent
        .requested_effects
        .iter()
        .copied()
        .find(|effect| !effect_fits_ordinary_turn(*effect))
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
            purpose: None,
            intent_fingerprint: input.intent.intent_fingerprint.clone(),
            project_policy_epoch: input.current_epochs.project_policy,
            task_admission_epoch: input.current_epochs.task_admission,
            confirmed_cursor: None,
            delivery_cursor: None,
            blocking_watermark: None,
            inline_delivery: None,
            capability_map_revision: input.capability_map_revision,
            requested_effects: input.intent.requested_effects.clone(),
            resource_intents: input.intent.resource_intents.clone(),
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
}

fn control_work_binding_has_valid_shape(binding: &crate::domain::ControlWorkBinding) -> bool {
    binding.work_revision > 0 && binding.claim_fence > 0
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

/// A `sync_required` row written before grants stopped carrying a delivery
/// page admits a turn as `ready` does: there is nothing left to catch up on.
const fn phase_refusal(phase: SessionPhase) -> Option<ControlRefusalCode> {
    match phase {
        SessionPhase::Ready | SessionPhase::SyncRequired => None,
        SessionPhase::Exited => Some(ControlRefusalCode::SessionExited),
        SessionPhase::TurnOpen => Some(ControlRefusalCode::TurnAlreadyOpen),
    }
}

/// Engram-internal coordination is never a turn's own effect.
const fn effect_fits_ordinary_turn(effect: EffectClass) -> bool {
    !matches!(effect, EffectClass::Coordinate)
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

/// Builds the common policy-decision directive used by turn gates.
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
        ControlRefusalCode::PinnedBudgetExceeded | ControlRefusalCode::RecoveryRequired => (
            DirectiveTarget::Agent,
            DirectiveSatisfaction::RecoveryCheckpoint,
            vec![EffectClass::Observe, EffectClass::Communicate],
        ),
        ControlRefusalCode::UnknownControlSchema
        | ControlRefusalCode::ControlAssuranceInsufficient
        | ControlRefusalCode::CapabilityNotPermitted
        | ControlRefusalCode::TaskUnbound
        | ControlRefusalCode::TaskAccessDenied
        | ControlRefusalCode::PolicyEpochChanged
        | ControlRefusalCode::TaskAdmissionEpochChanged
        | ControlRefusalCode::ContextRequired
        | ControlRefusalCode::DeltaRequired
        | ControlRefusalCode::DeliveryInvalid
        | ControlRefusalCode::TurnAlreadyOpen
        | ControlRefusalCode::TurnPurposeMismatch
        | ControlRefusalCode::GrantExpired
        | ControlRefusalCode::GrantNotBegun
        | ControlRefusalCode::GrantScopeMismatch
        | ControlRefusalCode::StaleFence
        | ControlRefusalCode::LeaseRequired
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
        ObjectId,
        domain::{
            ChangeCursor, ControlAssurance, ControlEpochs, DeliveryPage, ParticipantMembership,
            ProjectId, ProjectPolicyEpoch, ResourceCoverage, ResourceSubject, SessionId,
            TaskAdmissionEpoch, TaskId, TurnIntent, TurnPurpose,
        },
    };

    fn hash(seed: &str) -> ObjectId {
        ObjectId::from_canonical_bytes(seed.as_bytes())
    }

    fn input() -> TurnEvaluationInput {
        TurnEvaluationInput {
            control_schema_version: CONTROL_SCHEMA_VERSION,
            session_id: SessionId("session-a".into()),
            task_id: Some(TaskId::new()),
            work_binding: None,
            work_binding_current: true,
            participant_membership: ParticipantMembership::Member,
            anchor_exists: true,
            phase: SessionPhase::Ready,
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
            capability_map_revision: 3,
            intent: TurnIntent {
                idempotency_key: "turn-a".into(),
                intent_fingerprint: hash("turn intent"),
                purpose: Some(TurnPurpose::Ordinary),
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
            TurnDecision::Grant { .. } => None,
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
    fn delivery_density_rejects_cursor_arithmetic_overflow() {
        let task_id = TaskId::new();
        let page = DeliveryPage {
            from_cursor: ChangeCursor(i64::MIN),
            to_cursor: ChangeCursor(i64::MAX),
            head_cursor: ChangeCursor(i64::MAX),
            has_more: false,
            content_digest: hash("overflow-delivery"),
            delivery_token: "overflow-delivery".into(),
        };
        let delta = TaskDelta {
            task_id,
            after: page.from_cursor,
            cursor: page.to_cursor,
            changes: Vec::new(),
        };
        assert!(!delivery_delta_matches(&page, &delta));
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
    fn advisory_mutation_refuses_even_with_declared_effect() {
        let mut input = input();
        let subject = ResourceSubject::Path {
            project_id: ProjectId("project-a".into()),
            segments: vec!["src".into()],
            coverage: ResourceCoverage::Tree,
        };
        input.intent.requested_effects = vec![EffectClass::MutateLocal];
        input.intent.resource_intents = vec![subject.clone()];
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
            panic!("coordinate is not a model turn capability");
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
    fn a_grant_carries_no_delivery_page_or_cursors() {
        let TurnDecision::Grant { basis } = observe_turn(&input()).decision else {
            panic!("a ready session's ordinary turn must be granted");
        };
        assert_eq!(basis.purpose, None);
        assert_eq!(basis.confirmed_cursor, None);
        assert_eq!(basis.delivery_cursor, None);
        assert_eq!(basis.blocking_watermark, None);
        assert_eq!(basis.inline_delivery, None);
        let encoded = serde_json::to_value(&basis).expect("encode the basis");
        for retired in [
            "purpose",
            "confirmed_cursor",
            "delivery_cursor",
            "blocking_watermark",
            "inline_delivery",
        ] {
            assert!(
                encoded.get(retired).is_none(),
                "a new grant omits {retired}"
            );
        }
    }

    #[test]
    fn a_turn_needs_no_purpose_and_a_sync_required_row_admits_it() {
        let mut input = input();
        input.intent.purpose = None;
        assert!(matches!(
            observe_turn(&input).decision,
            TurnDecision::Grant { .. }
        ));

        input.phase = SessionPhase::SyncRequired;
        assert!(matches!(
            observe_turn(&input).decision,
            TurnDecision::Grant { .. }
        ));
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
    fn removed_turn_purposes_are_not_admitted() {
        assert!(serde_json::from_str::<TurnPurpose>("\"finalizer\"").is_err());
        assert!(serde_json::from_str::<TurnPurpose>("\"recovery\"").is_err());
        assert!(serde_json::from_str::<SessionPhase>("\"finalizer_open\"").is_err());
        assert_eq!(
            serde_json::from_str::<TurnPurpose>("\"ordinary\"").unwrap(),
            TurnPurpose::Ordinary
        );
    }

    fn begin() -> (IssuedTurnGrant, TurnBeginSnapshot) {
        let input = input();
        let TurnDecision::Grant { basis } = observe_turn(&input).decision else {
            panic!("the fixture turn must be granted");
        };
        let grant = IssuedTurnGrant {
            control_schema_version: CONTROL_SCHEMA_VERSION,
            grant_id: "grant-a".into(),
            request_key: input.intent.idempotency_key.clone(),
            basis: *basis,
            delivery: None,
            issued_at: input.evaluated_at,
        };
        let snapshot = TurnBeginSnapshot {
            control_schema_version: CONTROL_SCHEMA_VERSION,
            session_id: input.session_id.clone(),
            task_id: grant.basis.task_id,
            work_binding: None,
            work_binding_current: true,
            phase: SessionPhase::TurnOpen,
            participant_membership: ParticipantMembership::Member,
            anchor_exists: true,
            grant_state: TurnGrantState::Issued,
            current_epochs: input.current_epochs,
            capability_map_revision: input.capability_map_revision,
            delivery_tokens: Vec::new(),
            observed_at: input.evaluated_at,
        };
        (grant, snapshot)
    }

    #[test]
    fn turn_begin_takes_no_delivery_tokens() {
        let (grant, snapshot) = begin();
        assert_eq!(
            evaluate_turn_begin(&grant, &snapshot),
            TurnBeginDecision::Begin
        );

        let mut echoed = snapshot;
        echoed.delivery_tokens = vec!["token-from-an-old-page".into()];
        assert_eq!(
            evaluate_turn_begin(&grant, &echoed),
            TurnBeginDecision::Refuse {
                code: ControlRefusalCode::GrantScopeMismatch
            }
        );
    }

    #[test]
    fn turn_begin_rechecks_expiry_epochs_capability_map_and_claim() {
        let (grant, snapshot) = begin();
        let refused = |snapshot: &TurnBeginSnapshot| match evaluate_turn_begin(&grant, snapshot) {
            TurnBeginDecision::Refuse { code } => Some(code),
            TurnBeginDecision::Begin => None,
        };

        let mut expired = snapshot.clone();
        expired.observed_at = grant.basis.expires_at;
        assert_eq!(refused(&expired), Some(ControlRefusalCode::GrantExpired));

        let mut policy = snapshot.clone();
        policy.current_epochs.project_policy = ProjectPolicyEpoch(5);
        assert_eq!(
            refused(&policy),
            Some(ControlRefusalCode::PolicyEpochChanged)
        );

        let mut admission = snapshot.clone();
        admission.current_epochs.task_admission = TaskAdmissionEpoch(10);
        assert_eq!(
            refused(&admission),
            Some(ControlRefusalCode::TaskAdmissionEpochChanged)
        );

        let mut capability = snapshot.clone();
        capability.capability_map_revision = 4;
        assert_eq!(
            refused(&capability),
            Some(ControlRefusalCode::GrantScopeMismatch)
        );

        let mut stale = snapshot.clone();
        stale.work_binding_current = false;
        assert_eq!(refused(&stale), Some(ControlRefusalCode::StaleFence));

        let mut begun = snapshot;
        begun.grant_state = TurnGrantState::Begun;
        assert_eq!(
            refused(&begun),
            Some(ControlRefusalCode::GrantScopeMismatch)
        );
    }

    #[test]
    fn permission_and_turn_start_both_refuse_a_missing_anchor_as_task_unbound() {
        let mut unanchored = input();
        unanchored.anchor_exists = false;
        assert_eq!(
            refusal_code(&observe_turn(&unanchored)),
            Some(ControlRefusalCode::TaskUnbound)
        );

        let (grant, mut snapshot) = begin();
        snapshot.anchor_exists = false;
        assert_eq!(
            evaluate_turn_begin(&grant, &snapshot),
            TurnBeginDecision::Refuse {
                code: ControlRefusalCode::TaskUnbound
            }
        );

        // Storage derives membership through the anchor, so a lost anchor
        // clears both; each check still answers the anchor first.
        unanchored.participant_membership = ParticipantMembership::NotMember;
        assert_eq!(
            refusal_code(&observe_turn(&unanchored)),
            Some(ControlRefusalCode::TaskUnbound)
        );
        snapshot.participant_membership = ParticipantMembership::NotMember;
        assert_eq!(
            evaluate_turn_begin(&grant, &snapshot),
            TurnBeginDecision::Refuse {
                code: ControlRefusalCode::TaskUnbound
            }
        );
    }

    #[test]
    fn every_live_phase_has_one_answer() {
        let answer = |phase| {
            let mut input = input();
            input.phase = phase;
            refusal_code(&observe_turn(&input))
        };
        assert_eq!(answer(SessionPhase::Ready), None);
        assert_eq!(answer(SessionPhase::SyncRequired), None);
        assert_eq!(
            answer(SessionPhase::TurnOpen),
            Some(ControlRefusalCode::TurnAlreadyOpen)
        );
        assert_eq!(
            answer(SessionPhase::Exited),
            Some(ControlRefusalCode::SessionExited)
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
            obligation_rule_set: hash("obligation rule set"),
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
            obligation_rule_set: hash("obligation rule set"),
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
            environment: None,
            check_kind: VerificationKind::Test,
            check_fingerprint: producer.action_fingerprint.clone(),
            result: VerificationResult::Passed,
            completed_at: verification_time,
            summary: "host recorded tests".into(),
            refs: Vec::new(),
            actor,
            recorded_at: verification_time,
        };
        let rules = evaluate_obligation_rules(&builtin_obligation_rule_set(), &latest_mutation);
        assert_eq!(rules.len(), 1);
        assert_eq!(rules[0].0.rule_id, "source_mutation_requires_test");
        assert_eq!(rules[0].1.check_kind, VerificationKind::Test);
        assert_eq!(rules[0].1.check_fingerprint, None);
        assert!(evaluate_obligation_rules(&builtin_obligation_rule_set(), &producer).is_empty());
        let requirement = VerificationRequirement {
            check_kind: VerificationKind::Test,
            check_fingerprint: Some(evidence.check_fingerprint.clone()),
            required_environment: None,
        };
        let exact = VerificationEvidenceMatchInput {
            candidate_kind: WorkEvidenceKind::Verification,
            evidence: Some(&evidence),
            producer: Some(&producer),
            latest_mutation: Some((&latest_mutation, 1)),
            evidence_position: 4,
            requirement: &requirement,
        };
        assert_eq!(match_verification_evidence(&exact), Ok(()));
        let kind_only_requirement = VerificationRequirement {
            check_kind: VerificationKind::Test,
            check_fingerprint: None,
            required_environment: None,
        };
        assert_eq!(
            match_verification_evidence(&VerificationEvidenceMatchInput {
                requirement: &kind_only_requirement,
                ..exact
            }),
            Ok(())
        );
        let wrong_check_requirement = VerificationRequirement {
            check_kind: VerificationKind::Build,
            check_fingerprint: None,
            required_environment: None,
        };
        assert_eq!(
            match_verification_evidence(&VerificationEvidenceMatchInput {
                requirement: &wrong_check_requirement,
                ..exact
            }),
            Err(VerificationEvidenceMismatch::CheckKindMismatch)
        );

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
            latest_mutation: Some((&later_mutation, 3)),
            evidence_position: 4,
            requirement: &requirement,
        };
        assert_eq!(
            match_verification_evidence(&stale),
            Err(VerificationEvidenceMismatch::StaleSourceRevision)
        );
    }

    #[test]
    fn fixed_clock_environment_evidence_has_a_stable_rule_pin_hash() {
        use crate::{
            CanonicalObject,
            domain::{
                ActorContext, AssuranceLevel, ControlWorkBinding, EnvironmentComponents,
                EnvironmentEvidence, ExecutionSourceBasis, RootExecutionId, VerificationKind,
                VerificationRequirement, WorkClaimId, WorkId, WorkRunId,
            },
        };

        let fixed_time = Utc
            .with_ymd_and_hms(2026, 8, 28, 20, 0, 30)
            .single()
            .expect("fixed environment time");
        let components = EnvironmentComponents {
            toolchain: "rustc-1.89.0".into(),
            sandbox: Some("windows-host-sandbox-v1".into()),
            workspace_id: "workspace-a".into(),
            capability_map_revision: 7,
        };
        let environment_fingerprint = CanonicalObject::freeze(&components)
            .expect("freeze fixed environment components")
            .key()
            .clone();
        let run_id = WorkRunId(
            uuid::Uuid::parse_str("018f6d8c-3b10-7d6f-9a11-102030405060").expect("fixed run id"),
        );
        let session_id = SessionId("fixed-environment-host".into());
        let evidence = EnvironmentEvidence {
            schema_version: crate::domain::SCHEMA_VERSION,
            project_id: ProjectId("fixed-environment-project".into()),
            binding: ControlWorkBinding {
                root_execution_id: RootExecutionId(
                    uuid::Uuid::parse_str("018f6d8c-3b10-7d6f-9a11-102030405061")
                        .expect("fixed root execution id"),
                ),
                work_id: WorkId(
                    uuid::Uuid::parse_str("018f6d8c-3b10-7d6f-9a11-102030405062")
                        .expect("fixed work id"),
                ),
                run_id,
                work_revision: 4,
                claim_id: WorkClaimId(
                    uuid::Uuid::parse_str("018f6d8c-3b10-7d6f-9a11-102030405063")
                        .expect("fixed claim id"),
                ),
                claim_fence: 9,
            },
            session_id: session_id.clone(),
            source_basis: ExecutionSourceBasis {
                workspace_id: "workspace-a".into(),
                source_revision: "sha256:fixed-source-revision".into(),
            },
            environment_fingerprint,
            components: Some(components),
            observed_at: fixed_time,
            actor: ActorContext {
                actor_id: "fixed-host".into(),
                actor_kind: "host".into(),
                assurance: AssuranceLevel::Asserted,
                run_id: Some(run_id.0.to_string()),
                session_id: Some(session_id),
                source_tool: Some("host-control:turn_checkpoint".into()),
                source_skill: None,
                provenance_chain: Vec::new(),
                reason: "record fixed environment evidence".into(),
            },
            recorded_at: fixed_time,
        };
        let evidence_id = CanonicalObject::freeze(&evidence)
            .expect("freeze fixed environment evidence")
            .key()
            .clone();

        let requirement = VerificationRequirement {
            check_kind: VerificationKind::Test,
            check_fingerprint: Some(hash("cargo test --workspace")),
            required_environment: Some(evidence_id.clone()),
        };
        assert_eq!(
            requirement.required_environment.as_ref(),
            Some(&evidence_id)
        );
    }
}
