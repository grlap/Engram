//! An evaluation's citations: classification, position against the cut,
//! the evidence selection, pass admission by basis and binding, and the
//! gates a pass relied on.

use super::{
    AcceptanceBasis, AcceptanceEvaluation, AcceptanceEvaluationPolicy, AcceptanceVerdict,
    CitationContext, Connection, CriterionVerdict, CriterionVerdictInput,
    EvaluationCitationMismatch, ExecutionSourceBasis, MechanicalBasis, ObjectId, OptionalExtension,
    StoreError, VerificationEvidence, VerificationResult, WorkEvidence, WorkItem, WorkRunId,
    load_typed_work_object, normalize_note_text, params, refused,
};

pub(super) enum Citation {
    VerificationPassed {
        kind: crate::domain::VerificationKind,
        check_fingerprint: ObjectId,
        /// The source the check ran on.
        source_basis: ExecutionSourceBasis,
        /// The execution observation that ran the check.
        producer: ObjectId,
    },
    VerificationOther,
    Environment,
    Gate {
        name: String,
        passed: bool,
    },
    Note,
}

pub(super) fn bind_verdicts(
    connection: &Connection,
    item: &WorkItem,
    run_id: WorkRunId,
    policy: &AcceptanceEvaluationPolicy,
    cut: i64,
    inputs: &[CriterionVerdictInput],
) -> Result<Vec<CriterionVerdict>, StoreError> {
    let criteria = &item.acceptance;
    if inputs.len() != criteria.len()
        || inputs
            .iter()
            .any(|verdict| verdict.criterion > criteria.len())
    {
        return Err(refused(
            item.work_id,
            format!(
                "verdicts must cover exactly the {} current criteria by one-based position; re-read show",
                criteria.len()
            ),
        ));
    }
    let mut by_position: Vec<Option<&CriterionVerdictInput>> = vec![None; criteria.len()];
    for verdict in inputs {
        by_position[verdict.criterion - 1] = Some(verdict);
    }
    let mut bound = Vec::with_capacity(criteria.len());
    for (index, criterion) in criteria.iter().enumerate() {
        let input = by_position[index].ok_or_else(|| {
            refused(
                item.work_id,
                format!("criterion {} has no verdict", index + 1),
            )
        })?;
        let binding = item
            .acceptance_bindings
            .iter()
            .find(|binding| binding.criterion == index + 1);
        if let Some(binding) = binding
            && input.verdict == AcceptanceVerdict::Pass
            && input.basis != AcceptanceBasis::Observed
        {
            return Err(CitationContext { item, run_id, cut, criterion: index + 1, citation: "", position: None }.basis_refused(
                EvaluationCitationMismatch::ObservedBasisRequired,
                format!(
                    "criterion {} is bound to {} verification: a pass needs an observed basis citing host-minted verification evidence of that kind with a passed result, never judgment or an asserted gate",
                    index + 1,
                    super::super::planning::encode_state(binding.requirement.check_kind)?
                ),
            ));
        }
        let mut evidence = input.evidence.clone();
        evidence.sort();
        evidence.dedup();
        for hash in &evidence {
            let mut context = CitationContext {
                item,
                run_id,
                cut,
                criterion: index + 1,
                citation: hash.as_str(),
                position: None,
            };
            let citation = classify_citation(connection, run_id, hash)?.ok_or_else(|| {
                context.refused(
                    EvaluationCitationMismatch::NotOnRun,
                    format!(
                        "criterion {} cites {hash}, which is not evidence on this run",
                        index + 1
                    ),
                )
            })?;
            context.position = citation_position(connection, run_id, hash)?;
            match context.position {
                Some(position) if position <= cut => {}
                _ => {
                    return Err(context.refused(
                        EvaluationCitationMismatch::BeyondCut,
                        format!(
                            "criterion {} cites {hash}, which lies beyond evidence basis {cut}; re-read show",
                            index + 1
                        ),
                    ));
                }
            }
            if input.verdict == AcceptanceVerdict::Pass {
                admit_pass_citation(&context, input.basis, policy, &citation)?;
                if let Some(binding) = binding {
                    let matches = matches!(&citation, Citation::VerificationPassed { kind, check_fingerprint, .. }
                        if *kind == binding.requirement.check_kind
                            && binding
                                .requirement
                                .check_fingerprint
                                .as_ref()
                                .is_none_or(|required| required == check_fingerprint));
                    if !matches {
                        return Err(context.refused(
                            EvaluationCitationMismatch::BoundVerificationMismatch,
                            format!(
                                "criterion {} is bound to {} verification; {hash} is not passed host-minted verification evidence of that kind",
                                index + 1,
                                super::super::planning::encode_state(binding.requirement.check_kind)?
                            ),
                        ));
                    }
                }
            }
        }
        bound.push(CriterionVerdict {
            criterion: criterion.clone(),
            verdict: input.verdict,
            basis: input.basis,
            rationale: normalize_note_text(&input.rationale, "rationale")?,
            evidence,
        });
    }
    Ok(bound)
}

fn admit_pass_citation(
    context: &CitationContext<'_>,
    basis: AcceptanceBasis,
    policy: &AcceptanceEvaluationPolicy,
    citation: &Citation,
) -> Result<(), StoreError> {
    let position = context.criterion;
    match basis {
        AcceptanceBasis::Observed => match citation {
            Citation::VerificationPassed { .. } => Ok(()),
            _ => Err(context.refused(
                EvaluationCitationMismatch::PassedVerificationRequired,
                format!(
                    "criterion {position}: an observed pass requires every citation to be host-minted verification evidence with a passed result; {} is not",
                    context.citation
                ),
            )),
        },
        AcceptanceBasis::Asserted => {
            if policy.mechanical_basis == MechanicalBasis::Observed {
                return Err(context.basis_refused(
                    EvaluationCitationMismatch::ObservedPolicyRequired,
                    format!(
                        "criterion {position}: the project policy requires observed check evidence for a mechanical pass; agent gate records are not observed builds"
                    ),
                ));
            }
            match citation {
                Citation::Gate { passed: true, .. } => Ok(()),
                _ => Err(context.refused(
                    EvaluationCitationMismatch::PassingGateRequired,
                    format!(
                        "criterion {position}: an asserted pass requires every citation to be a gate record with no failure labels; {} is not",
                        context.citation
                    ),
                )),
            }
        }
        AcceptanceBasis::Judgment => Ok(()),
        AcceptanceBasis::HumanRequired => Err(refused(
            context.item.work_id,
            format!("criterion {position} cannot pass on a human_required basis"),
        )),
    }
}

pub(super) fn classify_citation(
    connection: &Connection,
    run_id: WorkRunId,
    hash: &ObjectId,
) -> Result<Option<Citation>, StoreError> {
    let row: Option<(String, Option<String>)> = connection
        .query_row(
            "SELECT evidence_kind, verification_result FROM work_run_evidence
             WHERE run_id = ?1 AND evidence_id = ?2",
            params![run_id.0.to_string(), hash.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((kind, result)) = row else {
        return Ok(None);
    };
    Ok(Some(match kind.as_str() {
        "verification" => {
            // The canonical object decides; the projection column is a
            // rebuildable index that must carry exactly the canonical result
            // word, so a missing value or any other variant refuses.
            let evidence: VerificationEvidence =
                load_typed_work_object(connection, hash, "verification_evidence")?;
            let expected = super::super::planning::encode_state(evidence.result)?;
            if result.as_deref() != Some(expected.as_str()) {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "verification evidence {hash} projection result disagrees with its canonical record"
                )));
            }
            if evidence.result == VerificationResult::Passed {
                Citation::VerificationPassed {
                    kind: evidence.check_kind,
                    check_fingerprint: evidence.check_fingerprint,
                    source_basis: evidence.source_basis,
                    producer: evidence.producer_observation,
                }
            } else {
                Citation::VerificationOther
            }
        }
        "environment" => Citation::Environment,
        _ => {
            let evidence: WorkEvidence = load_typed_work_object(connection, hash, "work_evidence")?;
            match evidence.gate {
                Some(gate) => Citation::Gate {
                    passed: gate.failed.is_empty(),
                    name: gate.name,
                },
                None => Citation::Note,
            }
        }
    }))
}

/// The run-feed position of one cited object, when it is on this run.
pub(super) fn citation_position(
    connection: &Connection,
    run_id: WorkRunId,
    hash: &ObjectId,
) -> Result<Option<i64>, StoreError> {
    Ok(connection
        .query_row(
            "SELECT position FROM work_feed_entries
             WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND object_id = ?2
             ORDER BY position LIMIT 1",
            params![run_id.0.to_string(), hash.as_str()],
            |row| row.get(0),
        )
        .optional()?)
}

/// Every evidence object on the run feed at or before `cut`, in feed order:
/// the selection the evaluator could have read.
pub(super) fn run_evidence_through(
    connection: &Connection,
    run_id: WorkRunId,
    cut: i64,
) -> Result<Vec<ObjectId>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT object_id FROM work_feed_entries
         WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND position <= ?2
           AND object_kind IN ('work_evidence', 'verification_evidence', 'environment_evidence')
         ORDER BY position",
    )?;
    let rows = statement
        .query_map(params![run_id.0.to_string(), cut], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter()
        .map(|stored| {
            ObjectId::from_stored(stored.clone()).ok_or(StoreError::InvalidStoredKey(stored))
        })
        .collect()
}

/// Gate names the passing verdicts rely on.
pub(super) fn cited_gate_names(
    connection: &Connection,
    run_id: WorkRunId,
    record: &AcceptanceEvaluation,
) -> Result<Vec<String>, StoreError> {
    let mut names = Vec::new();
    for verdict in record
        .verdicts
        .iter()
        .filter(|verdict| verdict.verdict == AcceptanceVerdict::Pass)
    {
        for hash in &verdict.evidence {
            if let Some(Citation::Gate { name, .. }) = classify_citation(connection, run_id, hash)?
            {
                names.push(name);
            }
        }
    }
    names.sort();
    names.dedup();
    Ok(names)
}

/// Whether a relied-on gate has a newer record after the evaluated cut. A
/// newer record replaces the one the verdict cited whatever its result: the
/// evaluator cannot freeze an older observation of the same check.
pub(super) fn gate_superseded_after(
    connection: &Connection,
    run_id: WorkRunId,
    position: i64,
    names: &[String],
) -> Result<bool, StoreError> {
    if names.is_empty() {
        return Ok(false);
    }
    let mut statement = connection.prepare(
        "SELECT object_id FROM work_feed_entries
         WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND position > ?2
           AND object_kind = 'work_evidence'
         ORDER BY position",
    )?;
    let rows = statement
        .query_map(params![run_id.0.to_string(), position], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for stored in rows {
        let hash =
            ObjectId::from_stored(stored.clone()).ok_or(StoreError::InvalidStoredKey(stored))?;
        let evidence: WorkEvidence = load_typed_work_object(connection, &hash, "work_evidence")?;
        if evidence.gate.is_some_and(|gate| names.contains(&gate.name)) {
            return Ok(true);
        }
    }
    Ok(false)
}
