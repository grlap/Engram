//! Reconstruct the native obligation decision at its immutable original cut.
use super::{context, definitions};
use crate::domain::{
    CompletionSeal, FeedPosition, WorkObligation, WorkObligationResolution,
    WorkObligationResolutionEvent,
};
use crate::storage::work::acceptance_evaluation::source::within_named_root;
use crate::storage::work::completion::{ForeignChange, foreign_change_may_match};
use crate::storage::work::task_sync::evidence::acceptance::movement;
use crate::storage::work::task_sync::evidence::acceptance::{History, Result, invalid};
use crate::storage::work::task_sync::evidence::verification;
use crate::{FeedId, ObjectId};

pub(super) fn validate(
    history: &History<'_>,
    seal: &CompletionSeal,
    feed: &FeedId,
    definition: &WorkObligation,
    resolution: &WorkObligationResolutionEvent,
    position: i64,
) -> Result<bool> {
    match &resolution.resolution {
        WorkObligationResolution::Satisfied {
            evidence,
            evaluated_cut,
        } => {
            let (kind, _) = verification::applicability::qualified_bytes(
                history.connection,
                history.project,
                &history.request.origin,
                evidence,
            )?;
            if kind == crate::task_sync::evidence::consumption::CONSUMPTION_KIND {
                return imported(
                    history,
                    seal,
                    feed,
                    definition,
                    resolution,
                    evidence,
                    evaluated_cut,
                    position,
                );
            }
            if evaluated_cut.feed != *feed
                || evaluated_cut.position > seal.completion_cut.position
                || evaluated_cut.position >= position
                || definition.trigger_position.position > evaluated_cut.position
                || !history.member(
                    feed,
                    evidence,
                    "verification_evidence",
                    evaluated_cut.position,
                )?
            {
                return Ok(false);
            }
            if let Some(decision) = &resolution.registered_input_decision {
                if &decision.verification != evidence {
                    return Ok(false);
                }
                let check = movement::validate_decision(
                    history,
                    feed,
                    decision,
                    seal.work_id,
                    seal.run_id,
                    evaluated_cut.position,
                    Some(&definition.requirement),
                )?;
                let root = context::root_at(
                    history,
                    feed,
                    check.binding.claim_id,
                    evaluated_cut.position,
                )?;
                if !source_allowed(history, feed, definition, root.as_ref())? {
                    return Ok(false);
                }
                return Ok(
                    crate::control::acceptance_binding_criterion(&definition.rule).is_none()
                        || movement::position(history, feed, evidence)?
                            > definition.trigger_position.position,
                );
            }
            ordinary(history, feed, definition, evidence, evaluated_cut)
        }
        WorkObligationResolution::Waived { waived_by, reason } => {
            Ok(resolution.registered_input_decision.is_none()
                && !waived_by.trim().is_empty()
                && waived_by.trim() == waived_by
                && !reason.trim().is_empty()
                && reason.trim() == reason)
        }
        WorkObligationResolution::Displaced {
            binding,
            trigger_workspace_id,
        } => {
            if resolution.registered_input_decision.is_some()
                || !crate::control::is_source_change_obligation(&definition.rule)
            {
                return Ok(false);
            }
            let trigger = definitions::source(history, &definition.triggering_observation)?;
            let root = context::root_at(history, feed, trigger.binding.claim_id, position - 1)?;
            Ok(root.as_ref().is_some_and(|root| &root.id == binding)
                && trigger
                    .source_basis
                    .as_ref()
                    .is_some_and(|basis| &basis.workspace_id == trigger_workspace_id)
                && context::classify(
                    history,
                    feed,
                    root.as_ref(),
                    &trigger,
                    definition.trigger_position.position,
                )? == ForeignChange::Displaced)
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn imported(
    history: &History<'_>,
    seal: &CompletionSeal,
    feed: &FeedId,
    definition: &WorkObligation,
    resolution: &WorkObligationResolutionEvent,
    id: &ObjectId,
    cut: &FeedPosition,
    position: i64,
) -> Result<bool> {
    use crate::task_sync::evidence::consumption::{
        CONSUMPTION_KIND, ConsumedProof, ImportedEvidenceConsumption,
    };
    let record: ImportedEvidenceConsumption = history.load(id, CONSUMPTION_KIND)?;
    // The same immutable structural validator as native doctor; it never asks
    // foreign claim ids to acquire live authority in this store.
    crate::storage::work::task_sync::evidence::consumption::load_on(history.connection, id)?;
    if resolution.registered_input_decision.is_some()
        || cut.feed != *feed
        || cut.position > seal.completion_cut.position
        || cut.position >= position
        || record.request.binding.run_id != seal.run_id
        || record.request.binding.work_id != seal.work_id
        || record.destination_cut < definition.trigger_position.position
        || !history.member(feed, id, CONSUMPTION_KIND, cut.position)?
        || movement::position(history, feed, id)? <= definition.trigger_position.position
    {
        return Ok(false);
    }
    let root = context::root_at(
        history,
        feed,
        record.request.binding.claim_id,
        record.destination_cut,
    )?;
    if !source_allowed(history, feed, definition, root.as_ref())? {
        return Ok(false);
    }
    let Some(root) = root else {
        return Ok(false);
    };
    if root.id != record.named_root
        || history
            .source_at(feed, record.destination_cut, &root.event)?
            .as_ref()
            .and_then(crate::domain::VerificationSourceBasis::source_id)
            != Some(&record.source)
    {
        return Ok(false);
    }
    match &record.proof {
        ConsumedProof::Verification {
            origin,
            original,
            requirement,
            criterion,
            ..
        } => {
            if requirement != &definition.requirement {
                return Ok(false);
            }
            // The consumption remains a valid historical fact. A NEW onward
            // use must still resolve its original execution against today's
            // terminal union, just as an ordinary verification does below.
            let (check, _, execution) = verification::applicability::candidate_facts_on(
                history.connection,
                history.project,
                origin,
                original,
            )?;
            if let Some(key) = execution
                && verification::union_on(history.connection, &key)?.conflicted
            {
                return Ok(false);
            }
            if let Some(bound) = crate::control::acceptance_binding_criterion(&definition.rule) {
                return Ok(*criterion == Some(bound));
            }
            if crate::control::is_stock_source_change_obligation(&definition.rule, requirement) {
                return Ok(true);
            }
            Ok(check.completed_at >= definition.opened_at)
        }
        ConsumedProof::Acceptance { .. } => Ok(crate::control::acceptance_binding_criterion(
            &definition.rule,
        )
        .is_some_and(|criterion| {
            record.governing.acceptance_bindings.iter().any(|bound| {
                bound.criterion == criterion && bound.requirement == definition.requirement
            })
        })),
    }
}

fn source_allowed(
    history: &History<'_>,
    feed: &FeedId,
    definition: &WorkObligation,
    root: Option<&context::Root>,
) -> Result<bool> {
    if crate::control::acceptance_binding_criterion(&definition.rule).is_some() {
        return Ok(true);
    }
    let trigger = definitions::source(history, &definition.triggering_observation)?;
    Ok(foreign_change_may_match(
        context::classify(
            history,
            feed,
            root,
            &trigger,
            definition.trigger_position.position,
        )?,
        root.is_some(),
    ))
}

fn ordinary(
    history: &History<'_>,
    feed: &FeedId,
    definition: &WorkObligation,
    evidence: &ObjectId,
    cut: &FeedPosition,
) -> Result<bool> {
    let (check, producer, execution) = verification::applicability::candidate_facts_on(
        history.connection,
        history.project,
        &history.request.origin,
        evidence,
    )?;
    if let Some(key) = execution
        && verification::union_on(history.connection, &key)?.conflicted
    {
        return Ok(false);
    }
    let evidence_position = movement::position(history, feed, evidence)?;
    let producer_position = if check.bound_from.is_some() {
        evidence_position
    } else {
        movement::position(history, feed, &check.producer_observation)?
    };
    let root = context::root_at(history, feed, check.binding.claim_id, cut.position)?;
    if !source_allowed(history, feed, definition, root.as_ref())? {
        return Ok(false);
    }
    let sources = context::sources(history, feed, root.as_ref(), cut.position)?;
    let binding_rule = crate::control::acceptance_binding_criterion(&definition.rule).is_some();
    let trigger = if binding_rule {
        None
    } else {
        Some(definitions::source(
            history,
            &definition.triggering_observation,
        )?)
    };
    let assessment = crate::control::assess_obligation_satisfaction(
        &crate::control::ObligationSatisfactionInput {
            judged_basis: check.applicability.as_ref().and(sources.judged.as_ref()),
            evidence: &check,
            producer: &producer,
            latest_mutation: sources
                .latest_mutation
                .as_ref()
                .map(|(at, change)| (change, *at)),
            named_root: root
                .as_ref()
                .map(|root| crate::control::NamedRootEvidenceMatch {
                    workspace_id: &root.event.workspace_id,
                    generation: root.event.generation,
                    binding_position: root.position,
                    latest_sighting: sources
                        .latest_sighting
                        .as_ref()
                        .map(|(at, sighting)| (sighting, *at)),
                    unknown_change_position: sources.unknown_change,
                }),
            evidence_position,
            producer_position: Some(producer_position),
            measured_sighting: sources.measured.as_ref().map(|(at, basis)| (basis, *at)),
            trigger: trigger.as_ref(),
            evaluated_cut: cut,
        },
        definition,
    );
    if assessment != crate::control::ObligationAssessment::Matches {
        return Ok(false);
    }
    if binding_rule && check.source_basis.source_id().is_none() {
        let mut barrier = false;
        if !history.walk(feed, cut.position, |at, id, kind| {
            if kind != "unadmitted_execution_observation" {
                return Ok(());
            }
            let Some(change) = movement::observation(history, id, kind)? else {
                return Ok(());
            };
            if !change.source_changed
                || root.as_ref().is_some_and(|root| {
                    change
                        .source_basis
                        .as_ref()
                        .is_some_and(|basis| !within_named_root(&root.event, basis))
                })
            {
                return Ok(());
            }
            barrier |= !crate::storage::work::feeds::unadmitted_check_follows(
                (producer_position, evidence_position),
                check.completed_at,
                at,
                &change,
            );
            Ok(())
        })? {
            return Err(invalid(
                "historical unadmitted-change interval is incomplete",
            ));
        }
        if barrier {
            return Ok(false);
        }
    }
    Ok(true)
}
