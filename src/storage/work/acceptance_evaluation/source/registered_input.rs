//! Prospective registered-input corroboration. Criterion proof remains in
//! citations; an immutable witness records the separate source decision.

use super::{
    Citation, CitationSource, Connection, NamedEvaluationRoot, NamedRootBindingEvent,
    NamedRootBindingKind, ObjectId, StoreError, WorkRunId, citation_position, classify_citation,
    load_typed_work_object, moved_after_check, naming_mismatch, passing_citations,
    registered_git_evaluation_on,
};
use crate::domain::{AcceptanceEvaluation, AcceptanceSourceBasis, VerificationEvidence};

pub(in crate::storage::work::acceptance_evaluation) fn select_registered_input_witness_on(
    connection: &Connection,
    run_id: WorkRunId,
    cut: i64,
    declared: Option<&AcceptanceSourceBasis>,
    root: Option<&NamedEvaluationRoot>,
) -> Result<Option<ObjectId>, StoreError> {
    let (Some(declared), Some(root)) = (declared, root) else {
        return Ok(None);
    };
    if !(declared.source_id().is_some()
        || declared
            .legacy_fingerprint()
            .is_some_and(crate::domain::is_git_input_revision))
    {
        return Ok(None);
    }
    // Prefer the newest qualifying check, rather than letting old failed or
    // stale history veto a later check of the same declared input.
    for candidate in super::super::run_evidence_through(connection, run_id, cut)?
        .into_iter()
        .rev()
    {
        if qualifies_on(connection, run_id, cut, declared, root, &candidate)? {
            return Ok(Some(candidate));
        }
    }
    Ok(None)
}

fn qualifies_on(
    connection: &Connection,
    run_id: WorkRunId,
    cut: i64,
    declared: &AcceptanceSourceBasis,
    root: &NamedEvaluationRoot,
    witness: &ObjectId,
) -> Result<bool, StoreError> {
    if !(declared.source_id().is_some()
        || declared
            .legacy_fingerprint()
            .is_some_and(crate::domain::is_git_input_revision))
        || declared
            .workspace_id
            .as_ref()
            .is_some_and(|workspace| workspace != &root.event.workspace_id)
        || citation_position(connection, run_id, witness)?.is_none_or(|position| position > cut)
        || !matches!(
            classify_citation(connection, run_id, witness)?,
            Some(Citation::VerificationPassed { .. })
        )
    {
        return Ok(false);
    }
    let evidence: VerificationEvidence =
        load_typed_work_object(connection, witness, "verification_evidence")?;
    let producer = crate::storage::work::load_verification_producer_on(connection, &evidence)?;
    let basis = &evidence.source_basis;
    let binding = &evidence.binding;
    if evidence.project_id != root.event.project_id
        || binding.run_id != run_id
        || binding.work_id != root.event.work_id
        || binding.root_execution_id != root.event.root_execution_id
        || binding.claim_id != root.event.claim_id
        || binding.claim_fence != root.event.claim_fence
        || producer.matches_registered_source(&evidence, &declared.identity) != Some(true)
        || naming_mismatch(
            connection,
            run_id,
            witness,
            &evidence.producer_observation,
            basis,
            root,
        )?
        .is_some()
    {
        return Ok(false);
    }
    Ok(moved_after_check(
        connection,
        run_id,
        witness,
        &evidence.producer_observation,
        CitationSource::WitnessGit(&declared.identity),
        cut,
        Some(root),
    )?
    .is_none())
}

/// Validate only the immutable proof at its historical cut. Later source
/// changes are assessed separately by live evaluation freshness.
pub(in crate::storage::work::acceptance_evaluation) fn validate_registered_input_witness_on(
    connection: &Connection,
    record: &AcceptanceEvaluation,
) -> Result<(), StoreError> {
    let Some(witness) = record.registered_input_witness.as_ref() else {
        return Ok(());
    };
    let invalid = || {
        StoreError::InvalidWorkProjection(
            "acceptance evaluation registered input witness does not corroborate its declared input at the evaluated cut".into(),
        )
    };
    let (Some(declared), Some(event_id)) = (
        record.source_basis.as_ref(),
        record.named_root_binding.as_ref(),
    ) else {
        return Err(invalid());
    };
    if record.evaluated_cut.feed != crate::domain::FeedId::RunExecution(record.run_id)
        || !record.evidence_basis.contains(witness)
    {
        return Err(invalid());
    }
    let event: NamedRootBindingEvent =
        load_typed_work_object(connection, event_id, "named_root_binding")?;
    let Some((position, latest_id, latest)) =
        crate::storage::work::feeds::latest_named_root_event_record_on(
            connection,
            record.run_id,
            event.claim_id,
            record.evaluated_cut.position,
        )?
    else {
        return Err(invalid());
    };
    if &latest_id != event_id
        || latest != event
        || event.kind != NamedRootBindingKind::Bound
        || event.project_id != record.project_id
        || event.work_id != record.work_id
        || event.run_id != record.run_id
    {
        return Err(invalid());
    }
    let root = NamedEvaluationRoot {
        position,
        event_id: latest_id,
        event,
    };
    if !qualifies_on(
        connection,
        record.run_id,
        record.evaluated_cut.position,
        declared,
        &root,
        witness,
    )? {
        return Err(invalid());
    }
    Ok(())
}

pub(in crate::storage::work::acceptance_evaluation) fn stored_git_evaluation_on(
    connection: &Connection,
    record: &AcceptanceEvaluation,
) -> Result<bool, StoreError> {
    if record.registered_input_decision.is_some() {
        super::super::validate_evaluation_input_witness_on(connection, record)?;
        return Ok(true);
    }
    if record.registered_input_witness.is_some() {
        validate_registered_input_witness_on(connection, record)?;
        return Ok(true);
    }
    // Never discover a new witness for an existing record. Its original
    // citations, declaration, and producer keep deciding its source lane.
    registered_git_evaluation_on(
        connection,
        record.run_id,
        record.evaluated_cut.position,
        record.source_basis.as_ref(),
        passing_citations(&record.verdicts).flat_map(|(_, citations, _)| citations),
    )
}
