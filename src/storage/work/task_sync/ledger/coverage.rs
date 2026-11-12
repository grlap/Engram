//! Receipt-derived family progress. No second durable cursor or eligibility view.

use std::collections::BTreeMap;

use super::{Event, Ledger, ObjectId, SqliteStore, StoreError, Transition, require};
use crate::task_sync::ledger::{
    CaptureFrontier, CoverageFrontier, CoverageMeaning, Direction, ExchangeFamily, FamilyFrontiers,
    FamilyProgress, FamilyRequestState, OutgoingReconciliation, PublicationObservation,
};

impl SqliteStore {
    /// Read captured selections and actual coverage separately for each family.
    /// Settled refusals and deferred planning grant no coverage. Evidence
    /// retention never asserts current verification or acceptance eligibility.
    ///
    /// # Errors
    /// Refuses missing bindings, inconsistent canonical history or stale views.
    pub fn task_sync_family_frontiers(
        &self,
        binding: &ObjectId,
    ) -> Result<FamilyFrontiers, StoreError> {
        self.work_read_snapshot(|store| {
            let ledger = Ledger::load(&store.connection)?;
            require(
                ledger.bindings.contains_key(binding),
                "exchange binding is missing",
            )?;
            let mut chain = ledger
                .events
                .into_iter()
                .filter(|(_, event)| &event.binding == binding)
                .collect::<Vec<_>>();
            chain.sort_by_key(|(_, event)| event.sequence);
            derive(binding, &chain)
        })
    }
}

struct Request {
    family: ExchangeFamily,
    direction: Direction,
    capture: CaptureFrontier,
    state: FamilyRequestState,
    covered: Option<CoverageFrontier>,
    original: Option<ObjectId>,
}

fn request_mut<'a>(
    requests: &'a mut BTreeMap<ObjectId, Request>,
    id: &ObjectId,
) -> Result<&'a mut Request, StoreError> {
    requests
        .get_mut(id)
        .ok_or_else(|| super::super::invalid("family outcome preparation is missing"))
}

fn cover(request: &mut Request, id: &ObjectId, event: &Event, meaning: CoverageMeaning) {
    request.state = FamilyRequestState::Covered;
    request.covered = Some(CoverageFrontier {
        capture: request.capture.clone(),
        outcome: id.clone(),
        sequence: event.sequence,
        meaning,
    });
}

fn capture(
    requests: &mut BTreeMap<ObjectId, Request>,
    id: &ObjectId,
    event: &Event,
) -> Result<(), StoreError> {
    let selected = match &event.transition {
        Transition::Prepared { intent } => Some((
            ExchangeFamily::Planning,
            intent.direction,
            intent.operation,
            None,
        )),
        Transition::DeltaPrepared { intent, .. } => Some((
            ExchangeFamily::Planning,
            intent.direction,
            intent.operation,
            None,
        )),
        Transition::PlanningSuccessorPrepared { recipe, .. } => Some((
            ExchangeFamily::Planning,
            recipe.request.intent.direction,
            recipe.request.intent.operation,
            Some(recipe.request.original_prepared.clone()),
        )),
        Transition::EvidencePrepared { intent } => Some((
            ExchangeFamily::Evidence,
            intent.direction,
            intent.operation,
            None,
        )),
        _ => None,
    };
    if let Some((family, direction, operation, original)) = selected {
        require(
            requests
                .insert(
                    id.clone(),
                    Request {
                        family,
                        direction,
                        capture: CaptureFrontier {
                            prepared: id.clone(),
                            sequence: event.sequence,
                            operation,
                        },
                        state: FamilyRequestState::Pending,
                        covered: None,
                        original,
                    },
                )
                .is_none(),
            "family preparation identity is repeated",
        )?;
    }

    Ok(())
}

fn advance(
    requests: &mut BTreeMap<ObjectId, Request>,
    id: &ObjectId,
    event: &Event,
) -> Result<(), StoreError> {
    match &event.transition {
        Transition::ImportApplied { prepared, .. }
        | Transition::ReconciledImportApplied { prepared, .. }
        | Transition::DeltaImportApplied { prepared, .. }
        | Transition::PlanningSuccessorApplied { prepared, .. } => {
            let request = request_mut(requests, prepared)?;
            cover(request, id, event, CoverageMeaning::PlanningApplied);
            if let Some(original) = request.original.clone() {
                let old = request_mut(requests, &original)?;
                require(
                    old.state == FamilyRequestState::Deferred,
                    "successor does not resolve a deferred selection",
                )?;
                old.state = FamilyRequestState::ResolvedBySuccessor;
            }
        }
        Transition::EvidenceImportApplied { prepared, .. } => {
            cover(
                request_mut(requests, prepared)?,
                id,
                event,
                CoverageMeaning::EvidenceRetained,
            );
        }
        Transition::PlanningDeferred { prepared, .. } => {
            request_mut(requests, prepared)?.state = FamilyRequestState::Deferred;
        }
        Transition::EvidenceStopped { prepared, .. } => {
            request_mut(requests, prepared)?.state = FamilyRequestState::Refused;
        }
        Transition::PushObserved {
            prepared,
            observation,
        } => {
            let request = request_mut(requests, prepared)?;
            match observation {
                PublicationObservation::Confirmed { .. } => {
                    cover(request, id, event, CoverageMeaning::PublicationConfirmed);
                }
                PublicationObservation::Unknown { .. } => {
                    request.state = FamilyRequestState::PublicationUnknown;
                }
            }
        }
        Transition::OutgoingReconciled {
            prepared,
            reconciliation,
        } if matches!(
            reconciliation.as_ref(),
            OutgoingReconciliation::Stopped { .. }
        ) =>
        {
            request_mut(requests, prepared)?.state = FamilyRequestState::Refused;
        }
        _ => {}
    }

    Ok(())
}

/// Called only after the ordinary reducer has validated the canonical chain.
pub(super) fn derive(
    binding: &ObjectId,
    chain: &[(ObjectId, Event)],
) -> Result<FamilyFrontiers, StoreError> {
    let mut requests = BTreeMap::<ObjectId, Request>::new();
    for (id, event) in chain {
        capture(&mut requests, id, event)?;
        advance(&mut requests, id, event)?;
    }
    let mut families = vec![
        FamilyProgress::empty(ExchangeFamily::Planning, Direction::Incoming),
        FamilyProgress::empty(ExchangeFamily::Planning, Direction::Outgoing),
        FamilyProgress::empty(ExchangeFamily::Evidence, Direction::Incoming),
        FamilyProgress::empty(ExchangeFamily::Evidence, Direction::Outgoing),
    ];
    for request in requests.values() {
        let row = families
            .iter_mut()
            .find(|row| row.family == request.family && row.direction == request.direction)
            .ok_or_else(|| super::super::invalid("unknown exchange family direction"))?;
        row.captured_requests += 1;
        if row
            .captured
            .as_ref()
            .is_none_or(|old| old.sequence < request.capture.sequence)
        {
            row.captured = Some(request.capture.clone());
            row.latest_state = Some(request.state);
        }
        if let Some(coverage) = &request.covered {
            row.covered_requests += 1;
            if row
                .covered
                .as_ref()
                .is_none_or(|old| old.sequence < coverage.sequence)
            {
                row.covered = Some(coverage.clone());
            }
        } else if request.state != FamilyRequestState::ResolvedBySuccessor {
            row.uncovered_requests += 1;
        }
        match request.state {
            FamilyRequestState::Pending | FamilyRequestState::PublicationUnknown => {
                row.pending_requests += 1;
            }
            FamilyRequestState::Deferred => row.deferred_requests += 1,
            FamilyRequestState::Refused => row.refused_requests += 1,
            FamilyRequestState::ResolvedBySuccessor => row.resolved_by_successor += 1,
            FamilyRequestState::Covered => {}
        }
    }
    let (tail, event) = chain
        .last()
        .ok_or_else(|| super::super::invalid("exchange chain is empty"))?;
    Ok(FamilyFrontiers {
        binding: binding.clone(),
        tail: tail.clone(),
        sequence: event.sequence,
        families,
    })
}
