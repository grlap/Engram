//! Selects and checks the verification that carries each bound criterion at completion.

use super::{
    AcceptanceResult, Connection, FeedPosition, NamedRootContext, ObjectId, StoreError,
    VerificationEvidence, WorkClaim, WorkItem, WorkRunId, assessment, encode_state,
    latest_source_mutation_on, load_complete_work_obligation_records_on, load_typed_work_object,
    named_root_context_on, run_feed_position_for_object_on, satisfied_binding,
};

/// Holds each bound criterion to its obligation at the completion cut. The
/// obligation is resolved there, or completion refused before this; a
/// satisfied one is contradicted when the newest verification of its kind at
/// the cut did not pass, since a later failed check outranks an earlier pass,
/// and is stale when that verification does not verify the run's latest
/// observed source change under the rule that matches verification evidence
/// to a mutation (source revision, position and time), since it certifies
/// code that has since moved. A criterion the author asserted then cites the
/// verification that carried it, so the seal says what the pass rested on; a
/// criterion an evaluation judged keeps exactly the citations the evaluation
/// recorded, and the obligation binding names the satisfying record.
#[allow(
    clippy::too_many_arguments,
    reason = "completion supplies the judged input beside its existing acceptance context"
)]
pub(super) fn bind_acceptance_to_obligations_on(
    connection: &Connection,
    item: &WorkItem,
    claim: &WorkClaim,
    cut: &FeedPosition,
    completion_evidence: &[ObjectId],
    evaluated: bool,
    source_fingerprint: Option<&str>,
    mut acceptance: Vec<AcceptanceResult>,
) -> Result<Vec<AcceptanceResult>, StoreError> {
    if item.acceptance_bindings.is_empty() {
        return Ok(acceptance);
    }
    let run_id = claim.run_id;
    let records = load_complete_work_obligation_records_on(connection, run_id)?;
    let root = named_root_context_on(connection, run_id, claim.claim_id, cut.position)?;
    let latest_mutation = if let Some(root) = &root {
        root.latest_mutation.clone()
    } else {
        latest_source_mutation_on(connection, run_id, cut.position)?
    };
    let measured = assessment::measured_sighting_on(
        connection,
        run_id,
        latest_mutation.as_ref(),
        root.as_ref(),
        cut.position,
    )?;
    for binding in &item.acceptance_bindings {
        let Some(index) = binding.criterion.checked_sub(1) else {
            return Err(StoreError::InvalidWorkProjection(format!(
                "work {} binds criterion 0; positions count from 1",
                item.work_id.0
            )));
        };
        // A waiver, by an authority the obligation path admitted, adds no
        // citation; the seal binds that waiver where it binds every
        // obligation.
        let Some(satisfying) = satisfied_binding(&records, binding) else {
            continue;
        };
        let mut carried_by = satisfying.clone();
        if let Some((position, newest, evidence)) = newest_verification_of_kind_on(
            connection,
            run_id,
            &binding.requirement,
            cut,
            root.as_ref(),
        )? {
            let kind = encode_state(binding.requirement.check_kind)?;
            if evidence.result != crate::domain::VerificationResult::Passed {
                return Err(StoreError::WorkBoundVerificationRefused {
                    work: item.work_id,
                    cause: Box::new(crate::domain::WorkBoundVerificationCause {
                        criterion: binding.criterion,
                        requirement: binding.requirement.clone(),
                        mismatch: crate::domain::VerificationEvidenceMismatch::ResultNotPassed,
                        verification: newest.clone(),
                        satisfied_by: satisfying.clone(),
                        producer_observation: evidence.producer_observation.clone(),
                        result: evidence.result,
                        remedy: crate::domain::BoundVerificationRemedy::RunPassingCheckAfter,
                        stale_source: None,
                    }),
                    reason: format!(
                        "criterion {} requires {kind} verification and is contradicted by newer verification evidence {newest} that did not pass; record a passing check after it, or drop the binding",
                        binding.criterion
                    ),
                });
            }
            let producer = super::super::load_verification_producer_on(connection, &evidence)?;
            let registered_git = producer.registered_git_basis(&evidence).is_some();
            if registered_git
                && let Some(supplied) = source_fingerprint
                && producer.matches_registered_git_input(&evidence, supplied) != Some(true)
            {
                return Err(StoreError::WorkCompletionRefused {
                    work: item.work_id,
                    reason: format!(
                        "criterion {} selected registered verification {newest} ran on {}, but completion supplied {supplied}; supply --source-fingerprint {} if that is the input being completed, or omit the declaration for unevaluated completion; otherwise run a check on the intended input",
                        binding.criterion,
                        evidence.source_basis.source_revision,
                        evidence.source_basis.source_revision,
                    ),
                });
            }
            if registered_git
                || root.is_some()
                || latest_mutation.is_some()
                || evidence.applicability.is_some()
            {
                let judged_basis = if registered_git {
                    Some(crate::domain::ExecutionSourceBasis {
                        // Unevaluated completion may omit a measurement, as
                        // before; an explicitly supplied input must match.
                        // Evaluated completion requires its declared input
                        // separately in acceptance assessment.
                        source_revision: source_fingerprint
                            .unwrap_or(&evidence.source_basis.source_revision)
                            .to_owned(),
                        ..evidence.source_basis.clone()
                    })
                } else if evidence.applicability.is_some() {
                    assessment::judged_basis_on(connection, run_id, root.as_ref(), cut.position)?
                } else {
                    None
                };
                let producer_position =
                    assessment::producer_position_on_run(connection, &evidence, position)?;
                // The floors of every unadmitted change on the run hold too,
                // not only the latest change's.
                let barrier = assessment::unadmitted_barrier(
                    connection,
                    run_id,
                    (producer_position, position),
                    evidence.completed_at,
                    cut.position,
                    root.as_ref(),
                )?;
                if let Some((mismatch, decider)) = binding_freshness_mismatch(
                    latest_mutation
                        .as_ref()
                        .map(|(position, mutation)| (mutation, *position)),
                    (&evidence, position),
                    (&producer, producer_position),
                    &binding.requirement,
                    root.as_ref(),
                    measured
                        .as_ref()
                        .map(|(position, basis)| (basis, *position)),
                    judged_basis.as_ref(),
                )
                .or_else(|| barrier.map(|mismatch| (mismatch, None)))
                {
                    use crate::domain::VerificationEvidenceMismatch as Mismatch;
                    let stale_source = decider.and_then(|decider| {
                        assessment::stale_verification_source(
                            decider,
                            &evidence,
                            latest_mutation
                                .as_ref()
                                .map(|(position, mutation)| (*position, mutation)),
                            root.as_ref(),
                            measured
                                .as_ref()
                                .map(|(position, basis)| (*position, basis)),
                        )
                    });
                    let cause = match mismatch {
                        Mismatch::StaleSourceRevision
                        | Mismatch::NotAfterMutation
                        | Mismatch::InvalidTime => "does not verify the run's latest source change",
                        _ => {
                            "is not admissible for it under the verification rule at the completion cut"
                        }
                    };
                    return Err(StoreError::WorkBoundVerificationRefused {
                        work: item.work_id,
                        cause: Box::new(crate::domain::WorkBoundVerificationCause {
                            criterion: binding.criterion,
                            requirement: binding.requirement.clone(),
                            mismatch,
                            verification: newest.clone(),
                            satisfied_by: satisfying.clone(),
                            producer_observation: evidence.producer_observation.clone(),
                            result: evidence.result,
                            remedy: crate::domain::BoundVerificationRemedy::RunCurrentCheck,
                            stale_source,
                        }),
                        reason: format!(
                            "criterion {} requires {kind} verification, and the newest one ({newest}) {cause} ({}); record a passing check of the current source, or drop the binding",
                            binding.criterion,
                            encode_state(mismatch)?
                        ),
                    });
                }
                // The rule ran and accepted this record, so it is what the
                // criterion rests on now. Without a source change no rule
                // ran, and the record that satisfied the obligation stays.
                carried_by = newest;
            }
        } else if root.is_some() {
            return Err(StoreError::WorkCompletionRefused {
                work: item.work_id,
                reason: format!(
                    "criterion {} requires a passing check in the current named source root",
                    binding.criterion
                ),
            });
        }
        // An evaluation's citations are its own and are sealed as recorded.
        // An asserted criterion cites the verification that carried it when
        // the completion's evidence set holds it, so the citation closure the
        // checkpoint acknowledged still holds.
        if !evaluated
            && completion_evidence.contains(&carried_by)
            && let Some(result) = acceptance.get_mut(index)
            && !result.evidence.contains(&carried_by)
        {
            result.evidence.push(carried_by);
        }
    }
    Ok(acceptance)
}

/// Why the newest verification of a bound kind does not carry its criterion
/// past the run's latest source change, or `None` when it does. Recording
/// order alone proves nothing about what was checked: a record appended after
/// the change may still verify the older source, so the rule that matches
/// verification evidence to a mutation decides. A change the host recorded
/// with no source revision or time gives that rule nothing to compare, and
/// would refuse every verification forever; recording order is then all that
/// can be said, and it is what is asked.
pub(super) fn binding_freshness_mismatch(
    mutation: Option<(&crate::domain::SourceObservation, i64)>,
    (evidence, evidence_position): (&VerificationEvidence, i64),
    (producer, producer_position): (&dyn crate::domain::VerificationProducer, i64),
    requirement: &crate::domain::VerificationRequirement,
    root: Option<&NamedRootContext>,
    measured_sighting: Option<(&crate::domain::ExecutionSourceBasis, i64)>,
    judged_basis: Option<&crate::domain::ExecutionSourceBasis>,
) -> Option<(
    crate::domain::VerificationEvidenceMismatch,
    Option<crate::domain::StaleSourceDecider>,
)> {
    if let Err(mismatch) = crate::control::check_verification_applicability(
        evidence.applicability.as_ref(),
        judged_basis,
    ) {
        return Some((mismatch, None));
    }
    if root.is_none()
        && let Some((mutation, mutation_position)) = mutation
        && (mutation.source_basis.is_none() || mutation.observed_at.is_none())
    {
        return (evidence_position <= mutation_position).then_some((
            crate::domain::VerificationEvidenceMismatch::NotAfterMutation,
            None,
        ));
    }
    crate::control::explain_verification_evidence(&crate::control::VerificationEvidenceMatchInput {
        judged_basis,
        candidate_kind: crate::domain::WorkEvidenceKind::Verification,
        evidence: Some(evidence),
        producer: Some(producer),
        latest_mutation: mutation,
        named_root: root.map(NamedRootContext::match_input),
        evidence_position,
        producer_position: Some(producer_position),
        measured_sighting,
        requirement,
    })
    .err()
}

/// The newest host-minted verification `requirement` recognizes (its kind,
/// and its pinned check when it names one) on the run at or before `cut`,
/// with its run-feed position.
fn newest_verification_of_kind_on(
    connection: &Connection,
    run_id: WorkRunId,
    requirement: &crate::domain::VerificationRequirement,
    cut: &FeedPosition,
    root: Option<&NamedRootContext>,
) -> Result<Option<(i64, ObjectId, VerificationEvidence)>, StoreError> {
    let stored: Vec<String> = connection
        .prepare(
            "SELECT evidence_id FROM work_run_evidence
             WHERE run_id = ?1 AND evidence_kind = 'verification'",
        )?
        .query_map([run_id.0.to_string()], |row| row.get(0))?
        .collect::<Result<_, _>>()?;
    let mut newest: Option<(i64, ObjectId, VerificationEvidence)> = None;
    for stored_hash in stored {
        let hash = ObjectId::from_stored(stored_hash.clone())
            .ok_or(StoreError::InvalidStoredKey(stored_hash))?;
        let position = run_feed_position_for_object_on(connection, run_id, &hash)?;
        if position.position > cut.position {
            continue;
        }
        let evidence: VerificationEvidence =
            load_typed_work_object(connection, &hash, "verification_evidence")?;
        if root.is_some_and(|root| {
            evidence.source_basis.workspace_id != root.binding.workspace_id
                || evidence.source_basis.source_root_generation != Some(root.binding.generation)
                || evidence.source_basis.source_root_state
                    != Some(crate::domain::SourceRootState::Named)
                || position.position <= root.binding_position
        }) {
            continue;
        }
        if evidence.check_kind != requirement.check_kind
            || requirement
                .check_fingerprint
                .as_ref()
                .is_some_and(|required| required != &evidence.check_fingerprint)
        {
            continue;
        }
        if newest
            .as_ref()
            .is_none_or(|(known, _, _)| position.position > *known)
        {
            newest = Some((position.position, hash, evidence));
        }
    }
    Ok(newest)
}
