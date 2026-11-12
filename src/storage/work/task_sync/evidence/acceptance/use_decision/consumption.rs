//! Current policy/closure admission for a selected historical acceptance use.

use super::{Decision, History, KIND, ObjectId, Result, inspect, invalid, object_on};
use crate::domain::{AcceptanceResult, ActorContext, ControlWorkBinding};
use crate::storage::work::task_sync::evidence::verification::applicability::Destination;
use crate::task_sync::evidence::acceptance::HistoricalProof;
use crate::task_sync::evidence::consumption::ConsumedProof;

/// Local movement retires implicit credit, never the original proof's
/// provenance and terminal-conflict checks. Reuse the qualifier against the
/// captured destination and policy; native completion checks today's policy
/// and evidence separately. This context grants no current local authority.
pub(crate) fn validate_retirement_on(
    connection: &rusqlite::Connection,
    record: &crate::task_sync::evidence::consumption::ImportedEvidenceConsumption,
) -> Result<()> {
    let (_, bytes) = object_on(connection, &record.request.decision)?
        .ok_or_else(|| invalid("retiring acceptance decision is unavailable"))?;
    let decision: Decision = serde_json::from_slice(&bytes)?;
    let (_, bytes) = object_on(connection, &record.named_root)?
        .ok_or_else(|| invalid("retiring acceptance root is unavailable"))?;
    let root: crate::domain::NamedRootBindingEvent = serde_json::from_slice(&bytes)?;
    let (_, sighting) = crate::storage::work::feeds::latest_named_root_sighting_on(
        connection,
        record.request.binding.run_id,
        &root.workspace_id,
        root.generation,
        record.destination_cut,
        false,
    )?
    .ok_or_else(|| invalid("retiring acceptance source sighting is unavailable"))?;
    let basis = sighting
        .source_basis
        .filter(|basis| basis.source_id() == Some(&record.source))
        .ok_or_else(|| invalid("retiring acceptance source sighting differs"))?;
    let destination = Destination {
        item: record.governing.clone(),
        cut: record.destination_cut,
        root: record.named_root.clone(),
        basis,
    };
    let history = History::new(connection, &record.project_id, &decision.request)?;
    if inspect(&history, &destination, &record.policy, &record.source)?.is_some() {
        return Err(invalid(
            "retiring acceptance original is no longer applicable",
        ));
    }
    Ok(())
}

pub(crate) fn qualify_on(
    connection: &rusqlite::Connection,
    id: &ObjectId,
    binding: &ControlWorkBinding,
    destination: &Destination,
    source: &crate::source::SourceId,
    actor: &ActorContext,
) -> Result<(ConsumedProof, Vec<AcceptanceResult>)> {
    let (kind, bytes) = object_on(connection, id)?
        .ok_or_else(|| invalid("imported acceptance decision is unavailable"))?;
    if kind != KIND {
        return Err(invalid("selected use is not an acceptance decision"));
    }
    // A retained foreign use records another host's use, never this one's.
    if crate::storage::work::task_sync::evidence::is_imported_only(connection, id)? {
        return Err(invalid("selected use is retained foreign evidence"));
    }
    let decision: Decision = serde_json::from_slice(&bytes)?;
    let run = crate::storage::work::query::load_work_run(connection, binding.run_id)?;
    if decision.project != destination.item.project_id
        || decision.request.binding != *binding
        || decision.local_generation != run.generation
        || decision.named_root != destination.root
        || decision.governing_fingerprint
            != *crate::CanonicalObject::freeze(&destination.item)?.key()
        || decision.source != *source
        || decision.refusal.is_some()
    {
        return Err(invalid("imported acceptance destination changed"));
    }
    let policy = crate::storage::SqliteStore::load_acceptance_evaluation_policy_on(connection)?;
    let history = History::new(connection, &decision.project, &decision.request)?;
    if inspect(&history, destination, &policy, source)?.is_some() {
        return Err(invalid("imported acceptance is not currently applicable"));
    }
    let results = match &decision.request.proof {
        HistoricalProof::Evaluation { evaluation, .. } => {
            let evaluation = history.load(evaluation, "acceptance_evaluation")?;
            crate::storage::work::acceptance_evaluation::derive_acceptance_results(
                &evaluation,
                actor.assurance,
            )
        }
        HistoricalProof::NativeCompletion { seal, .. } => {
            history
                .load::<crate::domain::CompletionSeal>(seal, "completion_seal")?
                .acceptance
        }
    };
    Ok((
        ConsumedProof::Acceptance {
            origin: decision.request.origin,
            proof: decision.request.proof,
        },
        results,
    ))
}

/// Reconstruct a frozen use without applying later policy or feed facts.
pub(crate) fn frozen_results_on(
    connection: &rusqlite::Connection,
    record: &crate::task_sync::evidence::consumption::ImportedEvidenceConsumption,
) -> Result<Vec<AcceptanceResult>> {
    let (kind, bytes) = object_on(connection, &record.request.decision)?
        .ok_or_else(|| invalid("consumed acceptance decision is missing"))?;
    if kind != KIND {
        return Err(invalid("consumed acceptance decision kind differs"));
    }
    let decision: Decision = serde_json::from_slice(&bytes)?;
    let history = History::new(connection, &decision.project, &decision.request)?;
    let results = match &decision.request.proof {
        HistoricalProof::Evaluation {
            evaluation: id,
            governing_witness,
        } => {
            let evaluation: crate::domain::AcceptanceEvaluation =
                history.load(id, "acceptance_evaluation")?;
            let governing = history.governing(id, &evaluation, governing_witness)?;
            if !evaluation.all_pass() || governing.is_none() {
                return Err(invalid(
                    "consumed evaluation has no complete passing original",
                ));
            }
            crate::storage::work::acceptance_evaluation::derive_acceptance_results(
                &evaluation,
                record.actor.assurance,
            )
        }
        HistoricalProof::NativeCompletion { seal, .. } => {
            let seal: crate::domain::CompletionSeal = history.load(seal, "completion_seal")?;
            if seal.restored || !seal.restored_child_completions.is_empty() {
                return Err(invalid("consumed completion is reported history"));
            }
            seal.acceptance
        }
    };
    if results.len() != record.governing.acceptance.len()
        || results
            .iter()
            .zip(&record.governing.acceptance)
            .any(|(result, criterion)| !result.satisfied || &result.criterion != criterion)
    {
        return Err(invalid(
            "consumed proof does not derive its governing criteria",
        ));
    }
    Ok(results)
}
