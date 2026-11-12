//! Thin native adapters for canonical, explicitly selected imported uses.

use super::named_root::obligation_matches_named_root;
use super::{
    CanonicalObject, Connection, FeedId, FeedPosition, ObjectId, SCHEMA_VERSION, StoreError,
    Transaction, TypedEvidenceProjection, WorkEvidenceKind, WorkObligation,
    WorkObligationResolution, WorkObligationResolutionEvent, WorkObligationState,
    append_control_typed_evidence_on, append_obligation_resolution_on, current_run_feed_cut_on,
    load_complete_work_obligation_records_on, load_typed_work_object, named_root_context_on,
    run_feed_position_for_object_on,
};
use crate::domain::NamedRootBindingEvent;
use crate::storage::work::task_sync::evidence::consumption;
use crate::task_sync::evidence::consumption::{ConsumedProof, ImportedEvidenceConsumption};

type Result<T> = std::result::Result<T, StoreError>;

pub(super) fn append_note_on(
    connection: &Connection,
    positions: &[FeedPosition],
    object: &CanonicalObject,
) -> Result<()> {
    let record = object.decode()?;
    super::super::note_index::append_native(
        connection,
        positions,
        object.key(),
        &super::super::note_index::NativeNote::Imported(&record),
    )
}

pub(super) fn derive_on(
    item: &crate::WorkItem,
    request: &crate::domain::CompleteWorkRequest,
    evidence: &[ObjectId],
    results: &[crate::domain::AcceptanceResult],
) -> Result<Vec<crate::domain::AcceptanceResult>> {
    if !request.acceptance.is_empty() {
        return Err(StoreError::WorkCompletionRefused {
            work: item.work_id,
            reason: "select either imported acceptance proof or explicit acceptance results".into(),
        });
    }
    let derived =
        super::normalize_completion_acceptance_shape(item, results, request.actor.assurance)?;
    super::ensure_acceptance_citations_within(item, evidence, &derived)?;
    Ok(derived)
}

pub(super) fn select_on(
    connection: &Connection,
    request: &crate::domain::CompleteWorkRequest,
    uses: Option<&consumption::LiveUses>,
) -> Result<consumption::SelectedUses> {
    let Some(uses) = consumption::reselect_on(
        connection,
        request.work_id,
        request.run_id,
        &request.evidence,
        true,
        uses,
    )?
    else {
        return Ok(consumption::SelectedUses::default());
    };
    let run = super::load_work_run(connection, request.run_id)?;
    let binding = crate::domain::ControlWorkBinding {
        root_execution_id: run.root_execution_id,
        work_id: request.work_id,
        run_id: request.run_id,
        work_revision: request.expected_work_revision,
        claim_id: request.claim_id,
        claim_fence: request.claim_fence,
    };
    let selected = uses.validate_on(
        connection,
        &binding,
        &request.evidence,
        request.source_id.as_ref(),
        request.completed_at,
    )?;
    if request.source_fingerprint.is_some()
        && (selected.current_verification || selected.acceptance.is_some())
    {
        return Err(StoreError::WorkCompletionRefused {
            work: request.work_id,
            reason: "imported proof requires a complete source declaration".into(),
        });
    }
    Ok(selected)
}

pub(in crate::storage::work) fn append_on(
    transaction: &Transaction<'_>,
    record: &ImportedEvidenceConsumption,
    object: &CanonicalObject,
) -> Result<()> {
    let root: NamedRootBindingEvent =
        load_typed_work_object(transaction, &record.named_root, "named_root_binding")?;
    append_control_typed_evidence_on(
        transaction,
        &record.project_id,
        &record.request.binding,
        &record.session_id,
        &record.actor,
        record.recorded_at,
        object,
        &TypedEvidenceProjection {
            note: None,
            kind: WorkEvidenceKind::Imported,
            workspace_id: &root.workspace_id,
            source_revision: None,
            producer_session_id: &record.session_id,
            producer_observation: None,
            check_fingerprint: None,
            verification_result: None,
            observed_at: record.recorded_at,
            environment_fingerprint: None,
            environment_evidence: None,
            components_json: None,
        },
    )?;
    let cut = current_run_feed_cut_on(transaction, record.request.binding.run_id)?;
    let obligations =
        load_complete_work_obligation_records_on(transaction, record.request.binding.run_id)?;
    for obligation in obligations
        .iter()
        .filter(|row| row.state == WorkObligationState::Open)
    {
        if matches_obligation_on(transaction, record, &obligation.obligation, &cut)? {
            let event = WorkObligationResolutionEvent {
                schema_version: SCHEMA_VERSION,
                project_id: record.project_id.clone(),
                obligation_id: obligation.obligation.obligation_id,
                definition: obligation.definition_id.clone(),
                run_id: record.request.binding.run_id,
                resolution: WorkObligationResolution::Satisfied {
                    evidence: object.key().clone(),
                    evaluated_cut: cut.clone(),
                },
                registered_input_decision: None,
                actor: record.actor.clone(),
                created_at: record.recorded_at,
            };
            append_obligation_resolution_on(transaction, obligation, &event)?;
        }
    }
    Ok(())
}

/// Check an immutable resolution against its original local selection cut.
/// Later conflicts affect new use, never this historical accounting predicate.
pub(super) fn matches_obligation_on(
    connection: &Connection,
    record: &ImportedEvidenceConsumption,
    obligation: &WorkObligation,
    cut: &FeedPosition,
) -> Result<bool> {
    let binding = &record.request.binding;
    if obligation.project_id != record.project_id
        || obligation.work_id != binding.work_id
        || obligation.run_id != binding.run_id
        || cut.feed != FeedId::RunExecution(binding.run_id)
        || obligation.trigger_position.feed != cut.feed
        || obligation.trigger_position.position > record.destination_cut
    {
        return Ok(false);
    }
    let position =
        run_feed_position_for_object_on(connection, binding.run_id, &record.request.consumption)?;
    if position.position > cut.position || position.position <= obligation.trigger_position.position
    {
        return Ok(false);
    }
    let root = named_root_context_on(
        connection,
        binding.run_id,
        binding.claim_id,
        record.destination_cut,
    )?;
    if !obligation_matches_named_root(connection, obligation, root.as_ref())? {
        return Ok(false);
    }
    match &record.proof {
        ConsumedProof::Verification {
            requirement,
            criterion,
            origin,
            original,
            ..
        } => {
            if requirement != &obligation.requirement {
                return Ok(false);
            }
            if let Some(bound) = crate::control::acceptance_binding_criterion(&obligation.rule) {
                return Ok(*criterion == Some(bound));
            }
            // Complete shared source equality retains the stock source-change
            // exception. Other rules retain the original execution time floor.
            if crate::control::is_stock_source_change_obligation(&obligation.rule, requirement) {
                return Ok(true);
            }
            let (evidence, _, _) =
                super::super::task_sync::evidence::verification::applicability::candidate_facts_on(
                    connection,
                    &record.project_id,
                    origin,
                    original,
                )
                .map_err(super::super::task_sync::evidence::store_error)?;
            Ok(evidence.completed_at >= obligation.opened_at)
        }
        ConsumedProof::Acceptance { .. } => {
            // C verifies every bound criterion through its explicitly named B
            // decisions. It does not waive unrelated source-change rules.
            let Some(criterion) = crate::control::acceptance_binding_criterion(&obligation.rule)
            else {
                return Ok(false);
            };
            Ok(record.governing.acceptance_bindings.iter().any(|binding| {
                binding.criterion == criterion && binding.requirement == obligation.requirement
            }))
        }
    }
}

pub(super) fn historical_resolution_on(
    connection: &Connection,
    evidence: &ObjectId,
    obligation: &WorkObligation,
    cut: &FeedPosition,
) -> Result<bool> {
    let record = consumption::load_on(connection, evidence)?;
    matches_obligation_on(connection, &record, obligation, cut)
}

/// Existing seal evidence links the explicit local use, whose original proof
/// remains separate. Later terminal/policy changes do not rewrite this seal.
pub(in crate::storage::work) fn validate_acceptance_on(
    connection: &Connection,
    seal: &crate::domain::CompletionSeal,
) -> Result<()> {
    let mut selected = None;
    for id in &seal.evidence {
        let kind: String = connection.query_row(
            "SELECT object_kind FROM objects WHERE object_id=?1",
            [id.as_str()],
            |row| row.get(0),
        )?;
        if kind != crate::task_sync::evidence::consumption::CONSUMPTION_KIND {
            continue;
        }
        let record = consumption::load_on(connection, id)?;
        if !matches!(record.proof, ConsumedProof::Acceptance { .. }) {
            continue;
        }
        let governing: crate::WorkItem = load_typed_work_object(
            connection,
            &seal.accepted_work_revision_hash,
            "work_item_revision",
        )?;
        if selected.is_some()
            || record.request.binding.work_id != seal.work_id
            || record.request.binding.run_id != seal.run_id
            || record.governing.revision != seal.accepted_work_revision
            || record.governing_fingerprint != *CanonicalObject::freeze(&governing)?.key()
            || run_feed_position_for_object_on(connection, seal.run_id, id)?.position
                > seal.completion_cut.position
        {
            return Err(StoreError::InvalidWorkProjection(
                "completion imported proof binding differs".into(),
            ));
        }
        let mut results =
            super::super::task_sync::evidence::acceptance::frozen_results_on(connection, &record)
                .map_err(super::super::task_sync::evidence::store_error)?;
        for result in &mut results {
            result.evidence = vec![id.clone()];
        }
        selected = Some(results);
    }
    if selected
        .as_ref()
        .is_some_and(|results| results != &seal.acceptance)
    {
        return Err(StoreError::InvalidWork(
            "completion imported proof does not derive acceptance".into(),
        ));
    }
    Ok(())
}
