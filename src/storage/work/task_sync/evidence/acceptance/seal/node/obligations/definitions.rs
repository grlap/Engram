//! A seal binds all definitions that its original trigger rules required.
use super::Definitions;
use crate::domain::{
    BuiltinObligationRuleRef, CompletionSeal, ObligationRuleSet, SourceObservation,
    VerificationRequirement, WorkEvent,
};
use crate::storage::work::completion::{binding_trigger_matches, source_trigger_matches};
use crate::storage::work::task_sync::evidence::acceptance::{History, Result, invalid};
use crate::storage::work::task_sync::evidence::verification;
use crate::{FeedId, ObjectId, WorkItem};

pub(super) fn validate_all(
    history: &History<'_>,
    seal: &CompletionSeal,
    feed: &FeedId,
    definitions: &Definitions,
) -> Result<()> {
    let governing: WorkItem =
        history.load(&seal.accepted_work_revision_hash, "work_item_revision")?;
    for binding in &governing.acceptance_bindings {
        if !definitions.values().any(|(_, _, obligation)| {
            obligation.rule == crate::control::acceptance_binding_rule(binding.criterion)
                && obligation.requirement == binding.requirement
        }) {
            return Err(invalid(
                "historical bound criterion lacks its obligation definition",
            ));
        }
    }
    for (position, _, obligation) in definitions.values() {
        if obligation.trigger_position.feed != *feed
            || obligation.trigger_position.position >= *position
            || crate::storage::work::task_sync::evidence::acceptance::movement::position(
                history,
                feed,
                &obligation.triggering_observation,
            )? != obligation.trigger_position.position
        {
            return Err(invalid("historical obligation does not follow its trigger"));
        }
        if let Some(criterion) = crate::control::acceptance_binding_criterion(&obligation.rule) {
            let trigger: WorkEvent =
                history.load(&obligation.triggering_observation, "work_event")?;
            if !binding_trigger_matches(obligation, &trigger, criterion) {
                return Err(invalid(
                    "historical criterion obligation differs from its trigger",
                ));
            }
        } else {
            let trigger = source(history, &obligation.triggering_observation)?;
            if !source_trigger_matches(obligation, &trigger)
                || obligation.rule_set != trigger.obligation_rule_set
                || !rules(history, &trigger)?.iter().any(|(rule, requirement)| {
                    *rule == obligation.rule && *requirement == obligation.requirement
                })
            {
                return Err(invalid(
                    "historical source obligation differs from its selected rule",
                ));
            }
        }
    }
    if !history.walk(feed, seal.completion_cut.position, |position, id, kind| {
        if !matches!(
            kind,
            "execution_observation" | "unadmitted_execution_observation"
        ) {
            return Ok(());
        }
        let Some(trigger) =
            crate::storage::work::task_sync::evidence::acceptance::movement::observation(
                history, id, kind,
            )?
        else {
            return Ok(());
        };
        if !trigger.source_changed {
            return Ok(());
        }
        for (rule, requirement) in rules(history, &trigger)? {
            let matches = definitions
                .values()
                .filter(|(_, _, obligation)| {
                    obligation.run_id == seal.run_id
                        && obligation.triggering_observation == *id
                        && obligation.trigger_position.position == position
                        && obligation.rule_set == trigger.obligation_rule_set
                        && obligation.rule == rule
                        && obligation.requirement == requirement
                })
                .count();
            if matches != 1 {
                return Err(invalid(
                    "historical source change lacks its exact required obligation",
                ));
            }
        }
        Ok(())
    })? {
        return Err(invalid(
            "historical obligation triggers have an incomplete interval",
        ));
    }
    Ok(())
}

pub(super) fn source(history: &History<'_>, id: &ObjectId) -> Result<SourceObservation> {
    let (kind, _) = verification::applicability::qualified_bytes(
        history.connection,
        history.project,
        &history.request.origin,
        id,
    )?;
    crate::storage::work::task_sync::evidence::acceptance::movement::observation(
        history, id, &kind,
    )?
    .ok_or_else(|| invalid("historical obligation trigger was not an accounted source"))
}

fn rules(
    history: &History<'_>,
    observation: &SourceObservation,
) -> Result<Vec<(BuiltinObligationRuleRef, VerificationRequirement)>> {
    let rule_set: ObligationRuleSet =
        history.load(&observation.obligation_rule_set, "obligation_rule_set")?;
    Ok(crate::control::evaluate_obligation_rules(
        &rule_set,
        observation.source_changed,
    ))
}
