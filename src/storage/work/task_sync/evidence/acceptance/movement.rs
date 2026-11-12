//! Native movement and exemption predicates over qualified retained history.
use crate::domain::{
    AcceptanceEvaluation, AcceptanceVerdict, NamedRootBindingEvent, RegisteredInputDecision,
    SourceObservation, VerificationEvidence, VerificationRequirement, VerificationResult,
    VerificationSourceBasis, VerificationSourceIdentity, WorkClaimId, WorkEvent, WorkId,
    WorkObligation, WorkObligationResolutionEvent, WorkRunId, WorkTransition,
};
use crate::storage::work::acceptance_evaluation::{
    movement::{BasisMove, BasisMoveState},
    same_turn,
};
use crate::storage::work::task_sync::evidence::acceptance::{
    History, Refusal, Result, invalid, params,
};
use crate::storage::work::task_sync::evidence::verification::applicability::candidate_facts_on;
use crate::{FeedId, ObjectId};

pub(super) fn inspect(
    history: &History<'_>,
    record: &AcceptanceEvaluation,
    root: &NamedRootBindingEvent,
    feed: &FeedId,
) -> Result<Option<Refusal>> {
    let Some(declared) = record.source_basis.as_ref() else {
        return Ok(Some(Refusal::SourceDiffers));
    };
    let cut = record.evaluated_cut.position;
    if !latest_root(
        history,
        feed,
        root.claim_id,
        history.known_head(feed)?,
        record.named_root_binding.as_ref(),
    )? {
        return Ok(Some(Refusal::OriginContextDiffers));
    }
    let at_cut = history.source_at(feed, cut, root)?;
    if at_cut
        .as_ref()
        .is_none_or(|basis| !basis.identity.same_source(&declared.identity))
    {
        return Ok(Some(Refusal::SourceDiffers));
    }
    let live_commit = if let Some(decision) = &record.registered_input_decision {
        validate_input(history, record, root, feed, decision)?;
        true
    } else {
        false
    };
    let witnessed_after = super::witness::validate(history, record, root, feed)?;
    let mut git_input = live_commit || witnessed_after.is_some();
    for verdict in record
        .verdicts
        .iter()
        .filter(|v| v.verdict == AcceptanceVerdict::Pass)
    {
        for id in &verdict.evidence {
            let (kind, _) = super::super::verification::applicability::qualified_bytes(
                history.connection,
                history.project,
                &history.request.origin,
                id,
            )?;
            if kind == "verification_evidence" {
                let (check, producer, _) = candidate_facts_on(
                    history.connection,
                    history.project,
                    &history.request.origin,
                    id,
                )?;
                git_input |= producer.registered_git_basis(&check).is_some()
                    || producer.registered_source_id_basis(&check).is_some();
            }
        }
    }
    let exempt = exemptions(history, record, root, feed, live_commit)?;
    let mut state = BasisMoveState::new(
        Some(declared),
        Some(&declared.identity),
        git_input,
        witnessed_after,
        live_commit,
    );
    let mut barrier = false;
    if !history.walk_current(feed, |at, id, kind| {
        if at <= cut || barrier || exempt.covers_record(kind, id) {
            return Ok(());
        }
        match kind {
            "execution_observation" | "unadmitted_execution_observation" => {
                if let Some(observation) = observation(history, id, kind)?
                    && !observation.source_basis.as_ref().is_some_and(|basis| {
                        !crate::storage::work::acceptance_evaluation::source::within_named_root(
                            root, basis,
                        )
                    })
                {
                    barrier = state.observe(at, observation).is_some();
                }
            }
            "work_obligation" => {
                let obligation: WorkObligation = history.load(id, kind)?;
                state.obligation(&obligation.triggering_observation);
            }
            "work_obligation_resolution" => {
                let resolution: WorkObligationResolutionEvent = history.load(id, kind)?;
                if !exempt.covers_resolution(&resolution) {
                    if live_commit {
                        let obligation: WorkObligation =
                            history.load(&resolution.definition, "work_obligation")?;
                        if !crate::control::is_source_change_obligation(&obligation.rule)
                            || accounted(history, &obligation.triggering_observation)?.is_none()
                        {
                            state.check();
                        }
                    } else {
                        state.check();
                    }
                }
            }
            "verification_evidence" | "environment_evidence" => state.check(),
            _ => {}
        }
        Ok(())
    })? {
        return Ok(Some(Refusal::OriginContextUnavailable));
    }
    Ok(if barrier {
        Some(Refusal::SourceDiffers)
    } else {
        state.finish().map(|moved| match moved {
            BasisMove::Source(_, _) => Refusal::SourceDiffers,
            BasisMove::Check => Refusal::EvaluationBasisMoved,
        })
    })
}

fn exemptions(
    history: &History<'_>,
    record: &AcceptanceEvaluation,
    root: &NamedRootBindingEvent,
    feed: &FeedId,
    live_commit: bool,
) -> Result<same_turn::ExemptChecks> {
    let mut exempt = same_turn::ExemptChecks::default();
    let declared = record
        .source_basis
        .as_ref()
        .ok_or_else(|| invalid("historical evaluation has no declared source"))?;
    let current = history.current_source(feed, root)?;
    if !live_commit
        && current
            .as_ref()
            .is_none_or(|basis| !basis.identity.same_source(&declared.identity))
    {
        return Ok(exempt);
    }
    if !history.walk_current(feed, |at, id, kind| {
        if at <= record.evaluated_cut.position || kind != "verification_evidence" {
            return Ok(());
        }
        let check: VerificationEvidence = history.load(id, kind)?;
        if check.result != VerificationResult::Passed
            || !same_turn::on_declared(&check, declared, Some(root))
        {
            return Ok(());
        }
        if live_commit {
            let (_, producer, execution) = candidate_facts_on(
                history.connection,
                history.project,
                &history.request.origin,
                id,
            )?;
            if !same_turn::registered_on_declared(&check, &producer, declared) {
                return Ok(());
            }
            if let Some(key) = execution
                && super::super::verification::union_on(history.connection, &key)?.conflicted
            {
                return Ok(());
            }
        } else if crate::control::check_verification_applicability(
            check.applicability.as_ref(),
            current
                .as_ref()
                .and_then(VerificationSourceBasis::legacy_basis)
                .as_ref(),
        )
        .is_err()
        {
            return Ok(());
        }
        exempt.record(id.clone(), &check);
        Ok(())
    })? {
        return Err(invalid(
            "historical exemption feed contains an interior gap",
        ));
    }
    Ok(exempt)
}

pub(super) fn observation(
    history: &History<'_>,
    id: &ObjectId,
    kind: &str,
) -> Result<Option<SourceObservation>> {
    Ok(match kind {
        "execution_observation" => Some(SourceObservation::admitted(
            id.clone(),
            &history.load(id, kind)?,
        )),
        "unadmitted_execution_observation" => {
            SourceObservation::unadmitted(id.clone(), &history.load(id, kind)?)
        }
        _ => {
            return Err(invalid(
                "historical triggering record is not a source observation",
            ));
        }
    })
}

fn accounted(history: &History<'_>, id: &ObjectId) -> Result<Option<SourceObservation>> {
    let (kind, _) = super::super::verification::applicability::qualified_bytes(
        history.connection,
        history.project,
        &history.request.origin,
        id,
    )?;
    observation(history, id, &kind)
}

pub(super) fn position(history: &History<'_>, feed: &FeedId, id: &ObjectId) -> Result<i64> {
    Ok(history.connection.query_row("SELECT position FROM work_task_sync_evidence_feed_members WHERE project_id=?1 AND origin_id=?2 AND feed=?3 AND object_id=?4", params![history.project.0,history.request.origin.as_str(),serde_json::to_string(feed)?,id.as_str()], |row|row.get(0))?)
}

pub(super) fn latest_root(
    history: &History<'_>,
    feed: &FeedId,
    claim: WorkClaimId,
    through: i64,
    expected: Option<&ObjectId>,
) -> Result<bool> {
    let mut selected = None;
    if !history.walk_rows(feed, through, |_, id, kind| {
        if kind == "named_root_binding" {
            let root: NamedRootBindingEvent = history.load(id, kind)?;
            if root.claim_id == claim {
                selected = Some(id.clone());
            }
        } else if kind == "work_event" {
            let event: WorkEvent = history.load(id, kind)?;
            if matches!(event.transition, WorkTransition::Released { .. }) {
                selected = None;
            }
        }
        Ok(())
    })? {
        return Err(invalid(
            "historical named root feed contains an interior gap",
        ));
    }
    Ok(selected.as_ref() == expected)
}

fn validate_input(
    history: &History<'_>,
    record: &AcceptanceEvaluation,
    root: &NamedRootBindingEvent,
    feed: &FeedId,
    decision: &RegisteredInputDecision,
) -> Result<()> {
    if record.registered_input_witness.is_some()
        || !record.evidence_basis.contains(&decision.verification)
        || record.source_basis.as_ref().is_none_or(|basis| {
            !basis
                .identity
                .same_source(&VerificationSourceIdentity::from(&decision.source))
        })
        || record.named_root_binding.as_ref() != Some(&decision.named_root_binding)
    {
        return Err(invalid("historical evaluation input decision differs"));
    }
    let selected_root: NamedRootBindingEvent =
        history.load(&decision.named_root_binding, "named_root_binding")?;
    if selected_root != *root {
        return Err(invalid("historical evaluation input root differs"));
    }
    validate_decision(
        history,
        feed,
        decision,
        record.work_id,
        record.run_id,
        record.evaluated_cut.position,
        None,
    )?;
    Ok(())
}

pub(super) fn validate_decision(
    history: &History<'_>,
    feed: &FeedId,
    decision: &RegisteredInputDecision,
    work: WorkId,
    run: WorkRunId,
    cut: i64,
    requirement: Option<&VerificationRequirement>,
) -> Result<VerificationEvidence> {
    let (check, producer, execution) = candidate_facts_on(
        history.connection,
        history.project,
        &history.request.origin,
        &decision.verification,
    )?;
    let root: NamedRootBindingEvent =
        history.load(&decision.named_root_binding, "named_root_binding")?;
    let evidence_position = position(history, feed, &decision.verification)?;
    let producer_position = position(history, feed, &check.producer_observation)?;
    let root_position = position(history, feed, &decision.named_root_binding)?;
    crate::storage::work::registered_commit_currency::validate_decision_facts(
        crate::storage::work::registered_commit_currency::DecisionFacts {
            decision,
            evidence: &check,
            producer: &producer,
            root: &root,
            work,
            run,
            cut,
            evidence_position,
            producer_position,
            root_position,
            requirement,
        },
        || {
            latest_root(
                history,
                feed,
                root.claim_id,
                cut,
                Some(&decision.named_root_binding),
            )
            .map_err(super::super::store_error)
        },
    )?;
    if let Some(key) = execution
        && super::super::verification::union_on(history.connection, &key)?.conflicted
    {
        return Err(invalid("historical input terminal execution is conflicted"));
    }
    validate_newer_checks(history, feed, &check, evidence_position, cut)?;
    Ok(check)
}

fn validate_newer_checks(
    history: &History<'_>,
    feed: &FeedId,
    selected: &VerificationEvidence,
    selected_position: i64,
    cut: i64,
) -> Result<()> {
    use crate::storage::work::registered_commit_currency::{
        registered_check_failed, same_registered_check,
    };
    let mut newest_failed = false;
    if !history.walk(feed, cut, |at, id, kind| {
        if at <= selected_position || kind != "verification_evidence" {
            return Ok(());
        }
        let newer: VerificationEvidence = history.load(id, kind)?;
        if !same_registered_check(selected, &newer) {
            return Ok(());
        }
        let (_, producer, _) = candidate_facts_on(
            history.connection,
            history.project,
            &history.request.origin,
            id,
        )?;
        if let Some(failed) = registered_check_failed(&newer, &producer) {
            newest_failed = failed;
        }
        Ok(())
    })? {
        return Err(invalid(
            "historical registered-check interval is incomplete",
        ));
    }
    if newest_failed {
        return Err(invalid(
            "historical input decision precedes a failed registered check",
        ));
    }
    Ok(())
}
