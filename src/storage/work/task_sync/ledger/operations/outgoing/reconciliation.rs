//! Match artifact recipes to the original pending operation before appending.

use super::{
    Direction, InspectedExchange, Ledger, ObjectId, OutgoingProgress, StoreError, Transition,
    invalid, require,
};
use crate::task_sync::ledger::OutgoingReconciliation;

pub(super) fn prepared(
    ledger: &Ledger,
    input: &InspectedExchange,
    progress: &OutgoingProgress,
) -> Result<ObjectId, StoreError> {
    if let OutgoingProgress::Reconciled {
        prepared,
        reconciliation,
    } = progress
    {
        let original = ledger
            .events
            .get(prepared)
            .ok_or_else(|| invalid("original outgoing preparation is missing"))?;
        require(
            original.binding == input.binding_id
                && matches!(&original.transition,
            Transition::Prepared { intent } if intent.direction == Direction::Outgoing),
            "reconciliation requires an original retained outgoing preparation; managed deltas keep their proportional lifecycle",
        )?;
        let selected = match reconciliation.as_ref() {
            OutgoingReconciliation::Resolved { resolved, .. } => {
                let replay = ledger.events.values().any(|event| {
                    event.binding == input.binding_id
                        && matches!(&event.transition, Transition::OutgoingReconciled {
                        prepared: actual, reconciliation: stored,
                    } if actual == prepared && stored == reconciliation)
                });
                if !replay {
                    require(
                        !ledger.events.values().any(|event| {
                            event.binding == input.binding_id
                                && (event.transition.retention_operation()
                                    == Some(resolved.operation)
                                    || event.transition.retention_key()
                                        == Some(resolved.key.as_str()))
                        }),
                        "resolved outgoing operation or key is already retained",
                    )?;
                }
                Some(resolved.as_ref())
            }
            _ => ledger
                .events
                .values()
                .find_map(|event| match &event.transition {
                    Transition::OutgoingReconciled {
                        prepared: actual, ..
                    } if event.binding == input.binding_id && actual == prepared => {
                        event.transition.retained_intent()
                    }
                    _ => None,
                }),
        };
        require(
            selected == Some(&input.intent),
            "resolved outgoing recipe differs from inspected input",
        )?;
        return Ok(prepared.clone());
    }
    if let Some((id, actual)) =
        ledger
            .events
            .iter()
            .find_map(|(id, event)| match &event.transition {
                Transition::Prepared { intent }
                    if event.binding == input.binding_id
                        && intent.operation == input.intent.operation =>
                {
                    Some((id, intent.as_ref()))
                }
                _ => None,
            })
    {
        require(
            actual == &input.intent,
            "exchange intent differs from recorded request",
        )?;
        if matches!(
            progress,
            OutgoingProgress::Observed(_) | OutgoingProgress::PushRejected { .. }
        ) {
            require(!ledger.events.values().any(|event| matches!(&event.transition,
                Transition::OutgoingReconciled { prepared, .. }
                    if event.binding == input.binding_id && prepared == id && event.transition.retained_intent().is_some())),
                "publication observation must recheck the resolved outgoing recipe")?;
        }
        return Ok(id.clone());
    }
    // A retry is still observed or refused on the original ledger operation,
    // using the inspected resolved recipe and its own retained artifacts.
    require(
        matches!(
            progress,
            OutgoingProgress::Observed(_) | OutgoingProgress::PushRejected { .. }
        ),
        "retry publication requires its explicit reconciliation start",
    )?;
    ledger
        .events
        .values()
        .find_map(|event| match &event.transition {
            Transition::OutgoingReconciled { prepared, .. }
                if event.binding == input.binding_id
                    && event.transition.retained_intent() == Some(&input.intent) =>
            {
                Some(prepared.clone())
            }
            _ => None,
        })
        .ok_or_else(|| invalid("resolved outgoing recipe is missing; preserve retained artifacts"))
}
