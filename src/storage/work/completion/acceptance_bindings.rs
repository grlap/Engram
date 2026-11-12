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
    retired_imports: &[ObjectId],
    evaluated: bool,
    source_fingerprint: Option<&str>,
    source_id: Option<&crate::source::SourceId>,
    mut acceptance: Vec<AcceptanceResult>,
    currency: &super::super::registered_commit_currency::LiveCurrency,
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
        let imported: bool = connection.query_row(
            "SELECT object_kind=?2 FROM objects WHERE object_id=?1",
            rusqlite::params![
                satisfying.as_str(),
                crate::task_sync::evidence::consumption::CONSUMPTION_KIND
            ],
            |row| row.get(0),
        )?;
        let mut import_floor = None;
        if imported {
            let record =
                super::super::task_sync::evidence::consumption::load_on(connection, satisfying)?;
            let position =
                run_feed_position_for_object_on(connection, run_id, satisfying)?.position;
            let newer = newest_verification_of_kind_on(
                connection,
                run_id,
                &binding.requirement,
                cut,
                root.as_ref(),
            )?;
            if let Some((newer_position, _, checked)) = &newer
                && *newer_position > position
                && checked.result != crate::domain::VerificationResult::Passed
            {
                return Err(StoreError::WorkCompletionRefused {
                    work: item.work_id,
                    reason: "a later local check failed after the imported verification use".into(),
                });
            }
            if !retired_imports.contains(satisfying)
                && newer
                    .as_ref()
                    .is_none_or(|(newer_position, _, _)| *newer_position <= position)
            {
                if record.request.binding.work_id != item.work_id
                    || record.request.binding.run_id != run_id
                    || record.request.binding.claim_id != claim.claim_id
                    || record.request.binding.claim_fence != claim.fence
                {
                    return Err(StoreError::WorkCompletionRefused {
                        work: item.work_id,
                        reason: "imported verification belongs to another completing claim".into(),
                    });
                }
                if !evaluated
                    && completion_evidence.contains(satisfying)
                    && let Some(result) = acceptance.get_mut(index)
                    && !result.evidence.contains(satisfying)
                {
                    result.evidence.push(satisfying.clone());
                }
                continue;
            }
            import_floor = Some(position);
        }
        let mut carried_by = satisfying.clone();
        let newest = newest_verification_of_kind_on(
            connection,
            run_id,
            &binding.requirement,
            cut,
            root.as_ref(),
        )?;
        let newest = if let Some((_, _, evidence)) = &newest
            && super::super::load_verification_producer_on(connection, evidence)?
                .has_registered_live_input(evidence)
        {
            if let Some(decision) =
                select_bound_live_input_on(connection, item, binding, &records, cut, currency)?
            {
                let evidence = load_typed_work_object(
                    connection,
                    &decision.verification,
                    "verification_evidence",
                )?;
                let position =
                    run_feed_position_for_object_on(connection, run_id, &decision.verification)?
                        .position;
                Some((position, decision.verification, evidence))
            } else {
                newest
            }
        } else {
            newest
        };
        if let Some((position, newest, evidence)) = newest {
            if let Some(import_floor) = import_floor {
                let obligation = super::binding_obligation(&records, binding).ok_or_else(|| {
                    StoreError::InvalidWorkProjection("bound obligation is missing".into())
                })?;
                let assessment = assessment::VerificationAtCut::load_on(
                    connection,
                    evidence.clone(),
                    &newest,
                    cut.clone(),
                )?
                .assess_on(connection, &obligation.obligation)?;
                if position <= import_floor
                    || assessment != crate::control::ObligationAssessment::Matches
                {
                    return Err(StoreError::WorkCompletionRefused {
                        work: item.work_id,
                        reason: format!(
                            "criterion {} needs a current passing native check after its obligation and retired imported use",
                            binding.criterion
                        ),
                    });
                }
                carried_by = newest.clone();
            }
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
            let legacy_basis = evidence.source_basis.legacy_basis();
            let registered_git = producer.registered_git_basis(&evidence).is_some();
            let registered_shared = producer.registered_source_id_basis(&evidence).is_some();
            let live_commit = currency.check_on(connection, &newest)?;
            if source_fingerprint.is_some() && registered_shared {
                return Err(StoreError::WorkCompletionRefused {
                    work: item.work_id,
                    reason: "source_fingerprint cannot declare a source_id input; omit the declaration for unevaluated completion".into(),
                });
            }
            if let Some(source_id) = source_id
                && (registered_git || registered_shared)
                && producer.matches_registered_source(
                    &evidence,
                    &crate::domain::VerificationSourceIdentity::SourceId {
                        source_id: source_id.clone(),
                    },
                ) != Some(true)
            {
                return Err(StoreError::WorkCompletionRefused {
                    work: item.work_id,
                    reason: format!(
                        "criterion {} selected registered verification {newest} does not match the complete source_id declared at completion; use --source-id-file for the current checked input, or run a fresh passing check on the intended source",
                        binding.criterion
                    ),
                });
            }
            if registered_git
                && let Some(supplied) = source_fingerprint
                && producer.matches_registered_git_input(&evidence, supplied) != Some(true)
            {
                return Err(StoreError::WorkCompletionRefused {
                    work: item.work_id,
                    reason: format!(
                        "criterion {} selected registered verification {newest} ran on {}, but completion supplied {supplied}; supply --source-fingerprint {} if that is the input being completed, or omit the declaration for unevaluated completion; otherwise run a check on the intended input",
                        binding.criterion,
                        evidence.source_basis.legacy_revision().unwrap_or_default(),
                        evidence.source_basis.legacy_revision().unwrap_or_default(),
                    ),
                });
            }
            if registered_git
                || registered_shared
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
                            .unwrap_or_else(|| {
                                evidence.source_basis.legacy_revision().unwrap_or_default()
                            })
                            .to_owned(),
                        ..legacy_basis.clone().ok_or_else(|| {
                            StoreError::InvalidWorkProjection(
                                "registered legacy producer has non-legacy verification evidence"
                                    .into(),
                            )
                        })?
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
                let barrier = if live_commit {
                    None
                } else {
                    assessment::unadmitted_barrier(
                        connection,
                        run_id,
                        (producer_position, position),
                        evidence.completed_at,
                        cut.position,
                        root.as_ref(),
                    )?
                };
                if let Some((mismatch, decider)) = binding_freshness_mismatch_live(
                    latest_mutation
                        .as_ref()
                        .filter(|_| !live_commit)
                        .map(|(position, mutation)| (mutation, *position)),
                    (&evidence, position),
                    (&producer, producer_position),
                    &binding.requirement,
                    root.as_ref(),
                    measured
                        .as_ref()
                        .filter(|_| !live_commit)
                        .map(|(position, basis)| (basis, *position)),
                    judged_basis.as_ref(),
                    live_commit,
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
        } else if root.is_some() || retired_imports.contains(satisfying) {
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

/// Performs only the live source decision for satisfied native commit
/// bindings before stock report accounting. Full binding validation stays
/// after the source barrier, preserving the legacy refusal order. It still
/// validates every criterion before sealing; any refusal rolls back the
/// report accounting as well.
pub(super) fn bound_live_input_consumed_on(
    connection: &Connection,
    item: &WorkItem,
    claim: &WorkClaim,
    cut: &FeedPosition,
    currency: &super::super::registered_commit_currency::LiveCurrency,
) -> Result<bool, StoreError> {
    if item.acceptance_bindings.is_empty() {
        return Ok(false);
    }
    let records = load_complete_work_obligation_records_on(connection, claim.run_id)?;
    let root = named_root_context_on(connection, claim.run_id, claim.claim_id, cut.position)?;
    let mut consumed = false;
    for binding in &item.acceptance_bindings {
        if satisfied_binding(&records, binding).is_none() {
            continue;
        }
        let Some((_, _, evidence)) = newest_verification_of_kind_on(
            connection,
            claim.run_id,
            &binding.requirement,
            cut,
            root.as_ref(),
        )?
        else {
            continue;
        };
        if super::super::load_verification_producer_on(connection, &evidence)?
            .has_registered_live_input(&evidence)
            && select_bound_live_input_on(connection, item, binding, &records, cut, currency)?
                .is_some()
        {
            consumed = true;
        }
    }
    Ok(consumed)
}

/// A current source does not carry evidence across a criterion's definition.
/// Both the early source preflight and the final binder use the same floor
/// as resolution of a newly opened binding obligation.
fn select_bound_live_input_on(
    connection: &Connection,
    item: &WorkItem,
    binding: &crate::domain::AcceptanceBinding,
    records: &[super::super::WorkObligationRecord],
    cut: &FeedPosition,
    currency: &super::super::registered_commit_currency::LiveCurrency,
) -> Result<Option<crate::domain::RegisteredInputDecision>, StoreError> {
    let Some(decision) = currency.select_on(connection, &binding.requirement, cut.position)? else {
        return Ok(None);
    };
    let record = super::binding_obligation(records, binding).ok_or_else(|| {
        StoreError::InvalidWorkProjection("a satisfied acceptance binding has no obligation".into())
    })?;
    if !super::registered_obligation_matches_on(
        connection,
        &decision,
        &record.obligation,
        cut.position,
    )? {
        return Err(StoreError::WorkCompletionRefused {
            work: item.work_id,
            reason: format!(
                "criterion {} requires a check recorded after its current binding definition; run a fresh passing check of the current input",
                binding.criterion
            ),
        });
    }
    Ok(Some(decision))
}

/// Why the newest verification of a bound kind does not carry its criterion
/// past the run's latest source change, or `None` when it does. Recording
/// order alone proves nothing about what was checked: a record appended after
/// the change may still verify the older source, so the rule that matches
/// verification evidence to a mutation decides. A change the host recorded
/// with no source revision or time gives that rule nothing to compare, and
/// would refuse every verification forever; recording order is then all that
/// can be said, and it is what is asked.
#[cfg(test)]
pub(super) fn binding_freshness_mismatch(
    mutation: Option<(&crate::domain::SourceObservation, i64)>,
    evidence: (&VerificationEvidence, i64),
    producer: (&dyn crate::domain::VerificationProducer, i64),
    requirement: &crate::domain::VerificationRequirement,
    root: Option<&NamedRootContext>,
    measured_sighting: Option<(&crate::domain::VerificationSourceBasis, i64)>,
    judged_basis: Option<&crate::domain::ExecutionSourceBasis>,
) -> Option<(
    crate::domain::VerificationEvidenceMismatch,
    Option<crate::domain::StaleSourceDecider>,
)> {
    binding_freshness_mismatch_live(
        mutation,
        evidence,
        producer,
        requirement,
        root,
        measured_sighting,
        judged_basis,
        false,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "live currency supplements the existing matching context"
)]
fn binding_freshness_mismatch_live(
    mutation: Option<(&crate::domain::SourceObservation, i64)>,
    (evidence, evidence_position): (&VerificationEvidence, i64),
    (producer, producer_position): (&dyn crate::domain::VerificationProducer, i64),
    requirement: &crate::domain::VerificationRequirement,
    root: Option<&NamedRootContext>,
    measured_sighting: Option<(&crate::domain::VerificationSourceBasis, i64)>,
    judged_basis: Option<&crate::domain::ExecutionSourceBasis>,
    live_commit: bool,
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
        named_root: root.map(|root| {
            let mut input = root.match_input();
            if live_commit {
                input.latest_sighting = None;
                input.unknown_change_position = None;
            }
            input
        }),
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
