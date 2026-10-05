//! Binding one native passed check to several held items of a changeset.
//!
//! The check ran once, on the original's run. Each target whose named root
//! holds the same content gets one verification record on its own run, whose
//! producer is the original execution and whose typed `bound_from` names the
//! original, the target's existing newest sighting, the host's measurement
//! and the intended criteria. Everything is validated before anything is
//! written; any refusal names every failing part and writes nothing.

use std::collections::HashSet;

use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, Transaction, params};

use super::super::StoreError;
use super::acceptance_evaluation::check_moved_on;
use super::completion::{append_control_verification_evidence_on, named_root_at_cut_on};
use super::feeds::{
    current_run_feed_cut_on, load_typed_work_object, run_feed_position_for_object_on,
    unadmitted_barrier_on,
};
use super::planning::validate_control_work_binding_on;
use super::query::{load_work_claim_optional, load_work_item, load_work_run};
use crate::ObjectId;
use crate::domain::{
    ActorContext, BoundCriterionEligibility, BoundVerificationSource, BoundVerificationTarget,
    ExecutionObservation, ExecutionOutcome, ExecutionSourceBasis, MAX_VERIFICATION_BIND_TARGETS,
    ProjectId, SCHEMA_VERSION, SessionId, SourceRootState, VerificationBindInput,
    VerificationBindOriginalRefusal, VerificationBindReceipt, VerificationBindRefusal,
    VerificationBindRequestRefusal, VerificationBindTarget, VerificationBindTargetReason,
    VerificationBindTargetRefusal, VerificationEvidence, VerificationResult, WorkClaimState,
    WorkItem, WorkLifecycle,
};

#[cfg(test)]
mod tests;

/// Longest idempotency key a bind request may carry.
const MAX_BIND_IDEMPOTENCY_KEY_BYTES: usize = 512;

/// A committed bind, or why nothing was written.
pub(in crate::storage) enum VerificationBindOutcome {
    Bound(VerificationBindReceipt),
    Refused(VerificationBindRefusal),
}

/// The validated original: its record, id and fixed run position.
struct Original {
    id: ObjectId,
    evidence: VerificationEvidence,
    position: i64,
}

/// One validated target, ready to write.
struct ValidTarget<'a> {
    request: &'a VerificationBindTarget,
    item: WorkItem,
    generation: i64,
}

/// Validates the request against the store as it stands in `transaction`
/// and, when every part holds, writes one bound record per target. On a
/// refusal nothing has been written; the caller rolls the transaction back.
///
/// # Errors
///
/// Storage failures only; every request fault is a refusal.
pub(in crate::storage) fn bind_verification_on(
    transaction: &Transaction<'_>,
    project_id: &ProjectId,
    session_id: &SessionId,
    binder: &ActorContext,
    input: &VerificationBindInput,
    now: DateTime<Utc>,
) -> Result<VerificationBindOutcome, StoreError> {
    let mut refusal = VerificationBindRefusal {
        original: None,
        request: None,
        targets: Vec::new(),
    };
    refusal.request = request_refusal(transaction, project_id, session_id, input, now)?;
    let original = match validate_original(transaction, project_id, session_id, input, now)? {
        Ok(original) => Some(original),
        Err(reason) => {
            refusal.original = Some(reason);
            None
        }
    };
    if refusal.request.is_none()
        && let Some(original) = &original
    {
        let basis = &original.evidence.source_basis;
        if input.measurement.workspace_id != basis.workspace_id {
            refusal.request = Some(VerificationBindRequestRefusal::MeasurementWorkspaceDiffers);
        } else if input.measurement.source_revision != basis.source_revision {
            refusal.request = Some(VerificationBindRequestRefusal::MeasurementRevisionDiffers);
        }
    }
    let mut seen = HashSet::new();
    let mut valid = Vec::new();
    for target in &input.targets {
        match validate_target(
            transaction,
            project_id,
            session_id,
            original.as_ref(),
            target,
            &mut seen,
            now,
        )? {
            Ok(target) => valid.push(target),
            Err(refused) => refusal.targets.push(refused),
        }
    }
    let Some(original) = original else {
        return Ok(VerificationBindOutcome::Refused(refusal));
    };
    if refusal.request.is_some() || !refusal.targets.is_empty() {
        return Ok(VerificationBindOutcome::Refused(refusal));
    }
    let mut bound = Vec::with_capacity(valid.len());
    for target in &valid {
        bound.push(write_target(
            transaction,
            session_id,
            binder,
            input,
            &original,
            target,
            now,
        )?);
    }
    Ok(VerificationBindOutcome::Bound(VerificationBindReceipt {
        original: original.id,
        replayed: false,
        bound,
    }))
}

/// Whether a stored bind receipt names, per requested target in order, the
/// record the bind wrote for it: a bound verification of `original` on that
/// target's binding, with its sighting, criteria and the request's
/// measurement, recorded by `session_id`, at the receipt's run position.
pub(in crate::storage) fn bound_receipt_matches_on(
    connection: &rusqlite::Connection,
    session_id: &SessionId,
    original: &ObjectId,
    measurement: &crate::domain::BindMeasurement,
    targets: &[VerificationBindTarget],
    bound: &[BoundVerificationTarget],
) -> Result<bool, StoreError> {
    if targets.len() != bound.len() {
        return Ok(false);
    }
    for (target, bound) in targets.iter().zip(bound) {
        let kind: Option<String> = connection
            .query_row(
                "SELECT object_kind FROM objects WHERE object_id = ?1",
                [bound.verification.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        if kind.as_deref() != Some("verification_evidence") {
            return Ok(false);
        }
        let evidence: VerificationEvidence =
            load_typed_work_object(connection, &bound.verification, "verification_evidence")?;
        let Some(source) = &evidence.bound_from else {
            return Ok(false);
        };
        let position = connection
            .query_row(
                "SELECT position FROM work_feed_entries
                 WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND object_id = ?2",
                params![
                    target.binding.run_id.0.to_string(),
                    bound.verification.as_str()
                ],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        if bound.work_id != target.binding.work_id
            || bound.run_id != target.binding.run_id
            || evidence.binding != target.binding
            || &evidence.session_id != session_id
            || &source.verification != original
            || source.sighting != target.sighting.observation
            || target.sighting.source_revision != evidence.source_basis.source_revision
            || &source.measurement != measurement
            || source.criteria != target.criteria
            || position != Some(bound.run_position)
        {
            return Ok(false);
        }
    }
    Ok(true)
}

/// Faults of the request as a whole, before any record is read.
fn request_refusal(
    transaction: &Transaction<'_>,
    project_id: &ProjectId,
    session_id: &SessionId,
    input: &VerificationBindInput,
    now: DateTime<Utc>,
) -> Result<Option<VerificationBindRequestRefusal>, StoreError> {
    let key = &input.idempotency_key;
    if key.trim().is_empty() || key.trim() != key || key.len() > MAX_BIND_IDEMPOTENCY_KEY_BYTES {
        return Ok(Some(VerificationBindRequestRefusal::InvalidIdempotencyKey));
    }
    if input.targets.is_empty() {
        return Ok(Some(VerificationBindRequestRefusal::NoTargets));
    }
    if input.targets.len() > MAX_VERIFICATION_BIND_TARGETS {
        return Ok(Some(VerificationBindRequestRefusal::TooManyTargets));
    }
    let held: i64 = transaction.query_row(
        "SELECT COUNT(*) FROM work_claims claim
         JOIN work_items item ON item.work_id = claim.work_id
         WHERE claim.holder_session_id = ?1 AND claim.state = 'active'
           AND claim.expires_at_ms > ?2 AND item.project_id = ?3",
        params![session_id.0, now.timestamp_millis(), project_id.0],
        |row| row.get(0),
    )?;
    // The original's item is held too and is never a target.
    if i64::try_from(input.targets.len()).unwrap_or(i64::MAX) > held.saturating_sub(1) {
        return Ok(Some(
            VerificationBindRequestRefusal::MoreTargetsThanHeldClaims,
        ));
    }
    Ok(None)
}

/// The original must be a native, passed check of a claim the caller still
/// holds, on that claim's current named root.
fn validate_original(
    transaction: &Transaction<'_>,
    project_id: &ProjectId,
    session_id: &SessionId,
    input: &VerificationBindInput,
    now: DateTime<Utc>,
) -> Result<Result<Original, VerificationBindOriginalRefusal>, StoreError> {
    use VerificationBindOriginalRefusal as Refused;
    let kind: Option<String> = transaction
        .query_row(
            "SELECT object_kind FROM objects WHERE object_id = ?1",
            [input.original.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    match kind.as_deref() {
        None => return Ok(Err(Refused::NotFound)),
        Some("verification_evidence") => {}
        Some(_) => return Ok(Err(Refused::NotVerification)),
    }
    let evidence: VerificationEvidence =
        load_typed_work_object(transaction, &input.original, "verification_evidence")?;
    if &evidence.project_id != project_id {
        return Ok(Err(Refused::NotFound));
    }
    if evidence.bound_from.is_some() {
        return Ok(Err(Refused::IsBound));
    }
    if evidence.result != VerificationResult::Passed {
        return Ok(Err(Refused::NotPassed));
    }
    let producer: ExecutionObservation = load_typed_work_object(
        transaction,
        &evidence.producer_observation,
        "execution_observation",
    )?;
    if producer.outcome != ExecutionOutcome::Succeeded {
        return Ok(Err(Refused::ProducerNotSucceeded));
    }
    let Some(generation) = evidence
        .source_basis
        .source_root_generation
        .filter(|_| evidence.source_basis.source_root_state == Some(SourceRootState::Named))
    else {
        return Ok(Err(Refused::Rootless));
    };
    let claim = load_work_claim_optional(transaction, evidence.binding.run_id)?;
    let held = claim.is_some_and(|claim| {
        claim.claim_id == evidence.binding.claim_id
            && &claim.holder == session_id
            && claim.state == WorkClaimState::Active
            && claim.expires_at > now
    });
    if !held {
        return Ok(Err(Refused::ClaimNotHeld));
    }
    let run_head = current_run_feed_cut_on(transaction, evidence.binding.run_id)?.position;
    let root = named_root_at_cut_on(
        transaction,
        evidence.binding.run_id,
        evidence.binding.claim_id,
        run_head,
    )?;
    let position =
        run_feed_position_for_object_on(transaction, evidence.binding.run_id, &input.original)?
            .position;
    // The original's producer is on its run, before it. A native record that
    // breaks this is a damaged store, not a request fault.
    let producer_position = run_feed_position_for_object_on(
        transaction,
        evidence.binding.run_id,
        &evidence.producer_observation,
    )?
    .position;
    if producer.binding.run_id != evidence.binding.run_id
        || producer.binding.work_id != evidence.binding.work_id
        || producer_position <= 0
        || producer_position >= position
    {
        return Err(StoreError::InvalidWorkProjection(format!(
            "verification {} is not after its producer on its own run",
            input.original
        )));
    }
    // Current means the claim's root is still the one the check ran under,
    // and the check still stands on its run exactly as an evaluation citing
    // it would read it: binding never restores credit the native rules have
    // retired. A change of unknown place after the producer is one that
    // read already refuses (a change that carries no revision); it is named
    // here too so the root's own state says so without the read.
    let root_current = root.is_some_and(|root| {
        root.workspace_id == evidence.source_basis.workspace_id
            && root.generation == generation
            && root
                .unknown_change_position
                .is_none_or(|unknown| unknown < producer_position)
    });
    if !root_current
        || check_moved_on(
            transaction,
            evidence.binding.run_id,
            &input.original,
            &evidence.producer_observation,
            &evidence.source_basis.source_revision,
            run_head,
        )?
    {
        return Ok(Err(Refused::RootNotCurrent));
    }
    Ok(Ok(Original {
        id: input.original.clone(),
        evidence,
        position,
    }))
}

/// One target's checks, in the order a holder would fix them.
#[allow(
    clippy::too_many_lines,
    reason = "every target refusal is decided in one visible sequence"
)]
fn validate_target<'a>(
    transaction: &Transaction<'_>,
    project_id: &ProjectId,
    session_id: &SessionId,
    original: Option<&Original>,
    target: &'a VerificationBindTarget,
    seen: &mut HashSet<crate::domain::WorkId>,
    now: DateTime<Utc>,
) -> Result<Result<ValidTarget<'a>, VerificationBindTargetRefusal>, StoreError> {
    use VerificationBindTargetReason as Reason;
    let binding = &target.binding;
    let item = match load_work_item(transaction, binding.work_id) {
        Ok(item) if &item.project_id == project_id => Some(item),
        Ok(_) | Err(StoreError::WorkNotFound(_)) => None,
        Err(error) => return Err(error),
    };
    let work_ref = item.as_ref().map_or_else(
        || binding.work_id.0.to_string(),
        |item| item.short_ref.clone(),
    );
    let refuse = |reason: Reason, expected: Option<String>, actual: Option<String>| {
        let remedy = match reason {
            Reason::SightingMissing
            | Reason::SightingNotNewest
            | Reason::SightingScopeDiffers
            | Reason::SightingRevisionDiffers
            | Reason::UnresolvedSourceAfterSighting => Some(
                "obtain genuine source accounting on that claim through a normal granted turn, then read its newest sighting again",
            ),
            Reason::CheckPredatesUnadmittedChange => Some(
                "run the check again on that item, after the change; this check completed before the change was recorded",
            ),
            _ => None,
        }
        .map(str::to_owned);
        Ok(Err(VerificationBindTargetRefusal {
            work_id: binding.work_id,
            work_ref: work_ref.clone(),
            reason,
            expected,
            actual,
            remedy,
        }))
    };
    if !seen.insert(binding.work_id) {
        return refuse(Reason::DuplicateTarget, None, None);
    }
    let Some(item) = item else {
        return refuse(Reason::NotFound, None, None);
    };
    if original.is_some_and(|original| original.evidence.binding.work_id == binding.work_id) {
        return refuse(Reason::IsOriginalItem, None, None);
    }
    if item.lifecycle != WorkLifecycle::Open {
        return refuse(Reason::WorkNotOpen, None, None);
    }
    if item.active_run_id != Some(binding.run_id) {
        return refuse(
            Reason::RunNotActive,
            item.active_run_id.map(|run| run.0.to_string()),
            Some(binding.run_id.0.to_string()),
        );
    }
    let run = load_work_run(transaction, binding.run_id)?;
    if run.root_execution_id != binding.root_execution_id {
        return refuse(
            Reason::RootExecutionMoved,
            Some(run.root_execution_id.0.to_string()),
            Some(binding.root_execution_id.0.to_string()),
        );
    }
    let claim = load_work_claim_optional(transaction, binding.run_id)?;
    let not_held = match &claim {
        None => Some("the run has no claim"),
        Some(claim) if claim.claim_id != binding.claim_id => {
            Some("the run's claim is another claim")
        }
        Some(claim) if &claim.holder != session_id => Some("another session holds the claim"),
        Some(claim) if claim.state != WorkClaimState::Active => Some("the claim is not active"),
        Some(claim) if claim.expires_at <= now => Some("the claim has expired"),
        Some(_) => None,
    };
    let (None, Some(claim)) = (not_held, claim) else {
        return refuse(Reason::ClaimNotHeld, None, not_held.map(str::to_owned));
    };
    if claim.fence != binding.claim_fence {
        return refuse(
            Reason::ClaimFenceMoved,
            Some(claim.fence.to_string()),
            Some(binding.claim_fence.to_string()),
        );
    }
    if item.revision != binding.work_revision {
        return refuse(
            Reason::WorkRevisionMoved,
            Some(item.revision.to_string()),
            Some(binding.work_revision.to_string()),
        );
    }
    let head = current_run_feed_cut_on(transaction, binding.run_id)?.position;
    let Some(root) = named_root_at_cut_on(transaction, binding.run_id, binding.claim_id, head)?
    else {
        return refuse(Reason::NoNamedRoot, None, None);
    };
    let original_basis = original.map(|original| &original.evidence.source_basis);
    if let Some(basis) = original_basis
        && root.workspace_id != basis.workspace_id
    {
        return refuse(
            Reason::WorkspaceDiffers,
            Some(basis.workspace_id.clone()),
            Some(root.workspace_id.clone()),
        );
    }
    let Some((sighting_position, sighting)) = &root.latest_sighting else {
        return refuse(Reason::SightingMissing, None, None);
    };
    if sighting.record != target.sighting.observation {
        return refuse(
            Reason::SightingNotNewest,
            Some(sighting.record.to_string()),
            Some(target.sighting.observation.to_string()),
        );
    }
    let Some(sighting_basis) = sighting.source_basis.as_ref().filter(|basis| {
        basis.workspace_id == root.workspace_id
            && basis.source_root_generation == Some(root.generation)
            && basis.source_root_state == Some(SourceRootState::Named)
    }) else {
        return refuse(Reason::SightingScopeDiffers, None, None);
    };
    // Name the value that differs: the host's reading of the sighting, then
    // the sighting against the original's revision.
    if sighting_basis.source_revision != target.sighting.source_revision {
        return refuse(
            Reason::SightingRevisionDiffers,
            Some(sighting_basis.source_revision.clone()),
            Some(target.sighting.source_revision.clone()),
        );
    }
    if let Some(basis) = original_basis
        && basis.source_revision != sighting_basis.source_revision
    {
        return refuse(
            Reason::SightingRevisionDiffers,
            Some(basis.source_revision.clone()),
            Some(sighting_basis.source_revision.clone()),
        );
    }
    if root
        .unknown_change_position
        .is_some_and(|unknown| unknown > *sighting_position)
    {
        return refuse(Reason::UnresolvedSourceAfterSighting, None, None);
    }
    // The record would follow every change on the run in position, but an
    // accounted unadmitted change's floor holds the check's own completion:
    // a check that completed before such a change was recorded could satisfy
    // nothing here, and would stand as the run's newest check of its kind.
    if let Some(original) = original
        && let Some((change, _)) = unadmitted_barrier_on(
            transaction,
            binding.run_id,
            (head + 1, head + 1),
            original.evidence.completed_at,
            head,
            Some((root.workspace_id.as_str(), root.generation)),
        )?
    {
        return refuse(
            Reason::CheckPredatesUnadmittedChange,
            Some(original.evidence.completed_at.to_rfc3339()),
            Some(change.label),
        );
    }
    // Strictly increasing positions: no repeat, and no list longer than the
    // item's criteria once the range holds.
    if target.criteria.windows(2).any(|pair| pair[0] >= pair[1]) {
        return refuse(
            Reason::CriteriaNotIncreasing,
            Some("strictly increasing positions".into()),
            Some(format!("{:?}", target.criteria)),
        );
    }
    let criteria = u32::try_from(item.acceptance.len()).unwrap_or(u32::MAX);
    if let Some(position) = target
        .criteria
        .iter()
        .find(|position| **position == 0 || **position > criteria)
    {
        return refuse(
            Reason::CriterionOutOfRange,
            Some(format!("1..={criteria}")),
            Some(position.to_string()),
        );
    }
    if let Some(original) = original {
        let already: bool = transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM work_run_evidence evidence
                 JOIN objects object ON object.object_id = evidence.evidence_id
                 WHERE evidence.run_id = ?1 AND evidence.evidence_kind = 'verification'
                   AND json_extract(object.canonical_json, '$.bound_from.verification') = ?2
             )",
            params![binding.run_id.0.to_string(), original.id.as_str()],
            |row| row.get(0),
        )?;
        if already {
            return refuse(Reason::AlreadyBound, None, None);
        }
    }
    // The same live-claim rules every control binding obeys; a storage fault
    // stays an error, never a refusal.
    match validate_control_work_binding_on(transaction, project_id, session_id, binding, now) {
        Ok(()) => {}
        Err(
            error @ (StoreError::WorkClaimMismatch { .. }
            | StoreError::ControlWorkBindingStale { .. }),
        ) => {
            // The live-claim rules name the cause; the host reads it here.
            return refuse(Reason::ClaimNotHeld, None, Some(error.to_string()));
        }
        Err(error) => return Err(error),
    }
    Ok(Ok(ValidTarget {
        request: target,
        item,
        generation: root.generation,
    }))
}

/// Writes one target's bound record and reads back what it satisfied.
fn write_target(
    transaction: &Transaction<'_>,
    session_id: &SessionId,
    binder: &ActorContext,
    input: &VerificationBindInput,
    original: &Original,
    target: &ValidTarget<'_>,
    now: DateTime<Utc>,
) -> Result<BoundVerificationTarget, StoreError> {
    let binding = target.request.binding.clone();
    let source = &original.evidence;
    let mut actor = binder.clone();
    actor.session_id = Some(session_id.clone());
    actor.run_id = Some(binding.run_id.0.to_string());
    let evidence = VerificationEvidence {
        schema_version: SCHEMA_VERSION,
        project_id: source.project_id.clone(),
        binding: binding.clone(),
        session_id: session_id.clone(),
        producer_observation: source.producer_observation.clone(),
        source_basis: ExecutionSourceBasis {
            workspace_id: source.source_basis.workspace_id.clone(),
            source_revision: source.source_basis.source_revision.clone(),
            source_root_generation: Some(target.generation),
            source_root_state: Some(SourceRootState::Named),
        },
        environment: source.environment.clone(),
        check_kind: source.check_kind,
        check_fingerprint: source.check_fingerprint.clone(),
        result: source.result,
        completed_at: source.completed_at,
        summary: source.summary.clone(),
        refs: source.refs.clone(),
        actor,
        recorded_at: now,
        bound_from: Some(BoundVerificationSource {
            verification: original.id.clone(),
            work_id: source.binding.work_id,
            run_id: source.binding.run_id,
            original_position: original.position,
            sighting: target.request.sighting.observation.clone(),
            measurement: input.measurement.clone(),
            criteria: target.request.criteria.clone(),
        }),
    };
    let verification = append_control_verification_evidence_on(transaction, &evidence)?;
    let run_position =
        run_feed_position_for_object_on(transaction, binding.run_id, &verification)?.position;
    let obligations_satisfied = transaction
        .prepare(
            "SELECT rule_id FROM work_run_obligations
             WHERE run_id = ?1 AND evidence_id = ?2
             ORDER BY trigger_position, obligation_id",
        )?
        .query_map(
            params![binding.run_id.0.to_string(), verification.as_str()],
            |row| row.get::<_, String>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    let criteria = target
        .request
        .criteria
        .iter()
        .map(|position| criterion_eligibility(&target.item, *position, &evidence))
        .collect();
    Ok(BoundVerificationTarget {
        work_id: binding.work_id,
        work_ref: target.item.short_ref.clone(),
        run_id: binding.run_id,
        verification,
        run_position,
        obligations_satisfied,
        criteria,
    })
}

/// Whether the bound check suits a criterion's binding: its kind, and its
/// pinned check when it names one. Never a verdict on the criterion.
fn criterion_eligibility(
    item: &WorkItem,
    position: u32,
    evidence: &VerificationEvidence,
) -> BoundCriterionEligibility {
    let binding = item
        .acceptance_bindings
        .iter()
        .find(|binding| {
            usize::try_from(position).is_ok_and(|position| binding.criterion == position)
        })
        .map(|binding| binding.requirement.clone());
    let reason = binding.as_ref().and_then(|requirement| {
        if requirement.check_kind != evidence.check_kind {
            Some(format!(
                "the criterion binds {:?} checks; this check is {:?}",
                requirement.check_kind, evidence.check_kind
            ))
        } else if requirement
            .check_fingerprint
            .as_ref()
            .is_some_and(|pin| pin != &evidence.check_fingerprint)
        {
            Some("the criterion pins another check".to_owned())
        } else {
            None
        }
    });
    BoundCriterionEligibility {
        position,
        binding,
        eligible: reason.is_none(),
        reason,
    }
}
