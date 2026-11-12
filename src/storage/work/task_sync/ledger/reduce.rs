//! The canonical exchange transition reducer, shared by validation and views.

use super::{
    BTreeMap, BTreeSet, Binding, Connection, Direction, Event, ObjectId, PublicationObservation,
    StoreError, Transition, oid, require, validate_receipt,
};
use crate::task_sync::{
    delta::{DeltaCommitSelection, DeltaIntent, PlanningSuccessorRecipe},
    ledger::{EvidenceIntent, Intent, OutgoingFailure, OutgoingReconciliation, RetainedCommit},
};

#[cfg(test)]
mod tests;

#[derive(Clone, Copy)]
enum Request<'a> {
    Legacy(&'a Intent),
    Delta(&'a DeltaIntent),
    Evidence(&'a EvidenceIntent),
    Successor(&'a PlanningSuccessorRecipe),
}

#[derive(Clone, Copy)]
struct Pending<'a> {
    prepared: &'a ObjectId,
    request: Request<'a>,
    commit: Option<&'a RetainedCommit>,
    retry: Option<&'a ObjectId>,
    push_started: Option<&'a ObjectId>,
    /// The current start has a recorded exact non-fast-forward refusal.
    rejected: bool,
    stopped: bool,
}

impl Pending<'_> {
    fn actor(&self) -> &crate::ActorContext {
        match self.request {
            Request::Legacy(intent) => &intent.actor,
            Request::Delta(intent) => &intent.actor,
            Request::Evidence(intent) => &intent.actor,
            Request::Successor(recipe) => &recipe.request.intent.actor,
        }
    }

    fn direction(&self) -> Direction {
        match self.request {
            Request::Legacy(intent) => intent.direction,
            Request::Delta(intent) => intent.direction,
            Request::Evidence(intent) => intent.direction,
            Request::Successor(recipe) => recipe.request.intent.direction,
        }
    }

    fn operation(&self) -> uuid::Uuid {
        match self.request {
            Request::Legacy(intent) => intent.operation,
            Request::Delta(intent) => intent.operation,
            Request::Evidence(intent) => intent.operation,
            Request::Successor(recipe) => recipe.request.intent.operation,
        }
    }

    fn key(&self) -> &str {
        match self.request {
            Request::Legacy(intent) => &intent.key,
            Request::Delta(intent) => &intent.key,
            Request::Evidence(intent) => &intent.key,
            Request::Successor(recipe) => &recipe.request.intent.key,
        }
    }

    fn legacy(&self) -> Result<&Intent, StoreError> {
        match self.request {
            Request::Legacy(intent) => Ok(intent),
            Request::Delta(_) | Request::Evidence(_) | Request::Successor(_) => Err(
                super::super::invalid("legacy transition requires a legacy planning request"),
            ),
        }
    }

    fn delta(&self) -> Result<&DeltaIntent, StoreError> {
        match self.request {
            Request::Delta(intent) => Ok(intent),
            Request::Legacy(_) | Request::Evidence(_) | Request::Successor(_) => Err(
                super::super::invalid("delta transition requires a delta planning request"),
            ),
        }
    }

    fn successor(&self) -> Result<&PlanningSuccessorRecipe, StoreError> {
        match self.request {
            Request::Successor(recipe) => Ok(recipe),
            _ => Err(super::super::invalid(
                "successor transition requires a successor request",
            )),
        }
    }

    fn evidence(&self) -> Result<&EvidenceIntent, StoreError> {
        match self.request {
            Request::Evidence(intent) => Ok(intent),
            _ => Err(super::super::invalid(
                "evidence transition requires an evidence request",
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum Stage {
    Settled,
    Prepared,
    Committed,
    Pushing,
    Unknown,
}

impl Stage {
    pub(super) fn text(self) -> &'static str {
        match self {
            Self::Settled => "settled",
            Self::Prepared => "prepared",
            Self::Committed => "committed",
            Self::Pushing => "pushing",
            Self::Unknown => "unknown",
        }
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct RequestProgress {
    pub operation: String,
    pub key: String,
    pub prepared: ObjectId,
    pub latest: ObjectId,
    pub completed: Option<ObjectId>,
    pub direction: Direction,
    pub stage: Stage,
}
#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct Head {
    pub tail: ObjectId,
    pub sequence: u64,
    pub pending: Option<ObjectId>,
    pub stage: Stage,
    pub superseded_by: Option<ObjectId>,
    pub last_incoming: Option<ObjectId>,
    pub last_outgoing: Option<ObjectId>,
    pub last_checkpoint: Option<ObjectId>,
}
pub(super) struct ChainView {
    pub head: Head,
    pub operations: BTreeMap<String, RequestProgress>,
}

/// Receipt provenance is validated only for full canonical validation. Keyed
/// historical reads deliberately do not open imported task/origin payloads.
pub(super) fn reduce(
    connection: Option<&Connection>,
    binding: &Binding,
    chain: &[(&ObjectId, &Event)],
) -> Result<ChainView, StoreError> {
    reduce_inner(connection, binding, chain, true, None)
}

pub(super) fn operation(
    binding: &Binding,
    chain: &[(&ObjectId, &Event)],
) -> Result<ChainView, StoreError> {
    reduce_inner(None, binding, chain, false, None)
}

/// A new operation starts from an independently checked settled head. The same
/// transition match validates full history, selected operations and appends.
pub(super) fn next(
    connection: &Connection,
    binding: &Binding,
    head: &Head,
    chain: &[(&ObjectId, &Event)],
) -> Result<ChainView, StoreError> {
    require(
        head.stage == Stage::Settled && head.pending.is_none(),
        "new exchange bypasses unresolved operation",
    )?;
    reduce_inner(Some(connection), binding, chain, false, Some(head))
}

pub(super) fn continued(
    connection: &Connection,
    binding: &Binding,
    chain: &[(&ObjectId, &Event)],
) -> Result<ChainView, StoreError> {
    reduce_inner(Some(connection), binding, chain, false, None)
}

fn reduce_inner(
    connection: Option<&Connection>,
    binding: &Binding,
    chain: &[(&ObjectId, &Event)],
    activation: bool,
    seed: Option<&Head>,
) -> Result<ChainView, StoreError> {
    let mut rows = BTreeMap::new();
    let mut last_incoming = seed.and_then(|head| head.last_incoming.clone());
    let mut last_outgoing = seed.and_then(|head| head.last_outgoing.clone());
    let mut last_checkpoint = seed.and_then(|head| head.last_checkpoint.clone());
    let mut previous = if activation {
        None
    } else if let Some(seed) = seed {
        Some(&seed.tail)
    } else {
        chain
            .first()
            .and_then(|(_, event)| event.predecessor.as_ref())
    };
    let mut keys = BTreeSet::new();
    let mut operations = BTreeSet::new();
    let mut pending = None;
    let mut stage = seed.map_or(Stage::Settled, |head| head.stage);
    let first_sequence = if activation {
        1
    } else if let Some(seed) = seed {
        seed.sequence
            .checked_add(1)
            .ok_or_else(|| super::super::invalid("exchange sequence exceeds supported bound"))?
    } else {
        chain.first().map_or(1, |(_, event)| event.sequence)
    };
    for (index, (event_id, event)) in chain.iter().enumerate() {
        require(
            event.schema_version == 1 && event.sequence > 0,
            "invalid exchange event schema or sequence",
        )?;
        super::attribution(&event.actor)?;
        require(
            chain
                .first()
                .is_some_and(|(_, first)| event.binding == first.binding),
            "exchange event binding differs",
        )?;
        require(
            event.sequence
                == u64::try_from(index)
                    .ok()
                    .and_then(|index| index.checked_add(first_sequence))
                    .ok_or(())
                    .map_err(|()| {
                        super::super::invalid("exchange sequence exceeds supported bound")
                    })?
                && event.predecessor.as_ref() == previous,
            "exchange predecessor chain is broken",
        )?;
        if index == 0 && activation {
            require(
                matches!(event.transition, Transition::Activated) && event.actor == binding.actor,
                "invalid exchange activation",
            )?;
        } else {
            match &event.transition {
                Transition::Activated => {
                    return Err(super::super::invalid("duplicate exchange activation"));
                }
                Transition::CustodyCheckpoint { checkpoint } => {
                    require(
                        (activation || seed.is_some()) && stage == Stage::Settled,
                        "custody checkpoint bypasses an unresolved exchange",
                    )?;
                    checkpoint.validate(&event.binding, binding)?;
                    if let Some(connection) = connection {
                        super::super::closure::source::resolve_seed_on(
                            connection, event_id, binding, checkpoint,
                        )?;
                    }
                    require(
                        checkpoint.previous_checkpoint == last_checkpoint,
                        "custody checkpoint predecessor differs",
                    )?;
                    require(
                        keys.insert(&checkpoint.key) && operations.insert(checkpoint.operation),
                        "duplicate exchange request key or operation",
                    )?;
                    last_checkpoint = Some((*event_id).clone());
                    // A checkpoint never becomes an earlier operation's receipt.
                    pending = None;
                }
                Transition::Prepared { intent } => {
                    intent.validate()?;
                    require(
                        stage == Stage::Settled,
                        "new exchange bypasses unresolved operation",
                    )?;
                    require(
                        keys.insert(&intent.key) && operations.insert(intent.operation),
                        "duplicate exchange request key or operation",
                    )?;
                    require(
                        event.actor == intent.actor,
                        "exchange intent attribution differs",
                    )?;
                    pending = Some(Pending {
                        prepared: event_id,
                        request: Request::Legacy(intent),
                        commit: Some(&intent.commit),
                        retry: None,
                        push_started: None,
                        rejected: false,
                        stopped: false,
                    });
                    stage = Stage::Prepared;
                }
                Transition::DeltaPrepared { intent, payload } => {
                    payload.validate_historical(&event.binding, binding, intent)?;
                    // A selected operation has no preceding-operation history.
                    // Only the complete binding chain can establish this fact.
                    if (activation || seed.is_some())
                        && let crate::task_sync::delta::DeltaBasis::Outgoing {
                            confirmed_before,
                            ..
                        } = &intent.basis
                    {
                        require(
                            confirmed_before.as_ref().map(|basis| &basis.outcome)
                                == last_outgoing.as_ref(),
                            "delta confirmed publication is not the preceding completed export",
                        )?;
                    }
                    if let Some(connection) = connection {
                        super::delta::validate_prepared_on(connection, binding, event, intent)?;
                    }
                    require(
                        stage == Stage::Settled,
                        "new exchange bypasses unresolved operation",
                    )?;
                    require(
                        keys.insert(&intent.key) && operations.insert(intent.operation),
                        "duplicate exchange request key or operation",
                    )?;
                    require(
                        event.actor == intent.actor,
                        "exchange intent attribution differs",
                    )?;
                    pending = Some(Pending {
                        prepared: event_id,
                        request: Request::Delta(intent),
                        commit: None,
                        retry: None,
                        push_started: None,
                        rejected: false,
                        stopped: false,
                    });
                    stage = Stage::Prepared;
                }
                Transition::PlanningSuccessorPrepared { recipe, .. } => {
                    recipe.validate_historical_prepared(event, binding)?;
                    if let Some(connection) = connection {
                        super::delta::successor::validate_prepared_on(
                            connection, binding, event_id, event, recipe,
                        )?;
                    }
                    require(
                        stage == Stage::Settled,
                        "new exchange bypasses unresolved operation",
                    )?;
                    let intent = &recipe.request.intent;
                    require(
                        keys.insert(&intent.key) && operations.insert(intent.operation),
                        "duplicate exchange request key or operation",
                    )?;
                    pending = Some(Pending {
                        prepared: event_id,
                        request: Request::Successor(recipe),
                        commit: None,
                        retry: None,
                        push_started: None,
                        rejected: false,
                        stopped: false,
                    });
                    stage = Stage::Prepared;
                }
                Transition::EvidencePrepared { intent } => {
                    intent.validate(&event.binding, binding)?;
                    require(
                        stage == Stage::Settled,
                        "new exchange bypasses unresolved operation",
                    )?;
                    require(
                        keys.insert(&intent.key) && operations.insert(intent.operation),
                        "duplicate exchange request key or operation",
                    )?;
                    require(
                        event.actor == intent.actor,
                        "exchange intent attribution differs",
                    )?;
                    pending = Some(Pending {
                        prepared: event_id,
                        request: Request::Evidence(intent),
                        commit: None,
                        retry: None,
                        push_started: None,
                        rejected: false,
                        stopped: false,
                    });
                    stage = Stage::Prepared;
                }
                transition => {
                    let mut current = pending.ok_or_else(|| {
                        super::super::invalid("exchange transition has no intent")
                    })?;
                    let prepared = current.prepared;
                    require(
                        &event.actor == current.actor(),
                        "exchange transition attribution differs",
                    )?;
                    stage = match transition {
                        Transition::LocalCommitRecorded { prepared: selected } => {
                            let intent = current.legacy()?;
                            require(
                                selected == prepared
                                    && intent.direction == Direction::Outgoing
                                    && stage == Stage::Prepared,
                                "invalid local commit transition",
                            )?;
                            Stage::Committed
                        }
                        Transition::ImportApplied {
                            prepared: selected,
                            receipt,
                        } => {
                            let intent = current.legacy()?;
                            require(
                                selected == prepared
                                    && intent.direction == Direction::Incoming
                                    && stage == Stage::Prepared,
                                "invalid import transition",
                            )?;
                            require(
                                receipt.source_commit == intent.commit.oid
                                    && receipt.source_tree == intent.commit.tree
                                    && receipt.before_basis == intent.local_planning_basis
                                    && receipt.after_basis == intent.candidate.planning_fingerprint,
                                "import receipt differs from intent",
                            )?;
                            if let Some(connection) = connection {
                                validate_receipt(connection, binding, receipt)?;
                            }
                            Stage::Settled
                        }
                        Transition::ReconciledImportApplied {
                            prepared: selected,
                            receipt,
                        } => {
                            let intent = current.legacy()?;
                            require(
                                selected == prepared
                                    && intent.direction == Direction::Incoming
                                    && stage == Stage::Prepared,
                                "invalid reconciled import transition",
                            )?;
                            super::super::reconciliation::validate_receipt_intent(intent, receipt)?;
                            super::super::reconciliation::validate_receipt_base(intent, receipt)?;
                            if let Some(connection) = connection {
                                super::super::reconciliation::validate_receipt_on(
                                    connection, event_id, binding, intent, receipt,
                                )?;
                            }
                            Stage::Settled
                        }
                        Transition::DeltaLocalCommitRecorded {
                            prepared: selected,
                            commit,
                        } => {
                            let intent = current.delta()?;
                            require(
                                selected == prepared && stage == Stage::Prepared,
                                "invalid delta local commit transition",
                            )?;
                            DeltaCommitSelection {
                                prepared: selected.clone(),
                                commit: commit.clone(),
                            }
                            .validate(
                                &event.binding,
                                binding,
                                intent,
                                prepared,
                            )?;
                            current.commit = Some(commit);
                            Stage::Committed
                        }
                        Transition::DeltaImportApplied {
                            prepared: selected,
                            receipt,
                        } => {
                            let intent = current.delta()?;
                            require(
                                selected == prepared && stage == Stage::Prepared,
                                "invalid delta import transition",
                            )?;
                            receipt.validate(&event.binding, binding, intent)?;
                            if let Some(connection) = connection {
                                super::delta::validate_receipt_on(
                                    connection, event_id, event, binding, intent, receipt,
                                )?;
                            }
                            Stage::Settled
                        }
                        Transition::PlanningSuccessorApplied {
                            prepared: selected,
                            receipt,
                        } => {
                            let recipe = current.successor()?;
                            require(
                                selected == prepared
                                    && selected == &recipe.prepared
                                    && *event_id == &recipe.outcome
                                    && stage == Stage::Prepared
                                    && **event
                                        == recipe.applied_event(
                                            &event.binding,
                                            binding,
                                            (**receipt).clone(),
                                        )?,
                                "successor outcome differs from its retained request",
                            )?;
                            if let Some(connection) = connection {
                                super::delta::successor::validate_receipt_on(
                                    connection, binding, event_id, event, recipe, receipt,
                                )?;
                            }
                            Stage::Settled
                        }
                        Transition::PlanningDeferred {
                            prepared: selected,
                            hold,
                            candidate_fingerprint,
                            blocking_claims,
                            after_cut,
                        } => {
                            let intent = current.delta()?;
                            // Claim proofs cite stored event ids, including historical ids;
                            // only the newly created hold must have a minted UUID identity.
                            require(
                                selected == prepared
                                    && stage == Stage::Prepared
                                    && intent.direction == Direction::Incoming
                                    && hold.as_str().len() == 32
                                    && candidate_fingerprint
                                        == &intent.candidate.content_fingerprint
                                    && !blocking_claims.is_empty()
                                    && blocking_claims.len() <= super::MAX_RECORDS
                                    && blocking_claims.windows(2).all(|pair| pair[0] < pair[1])
                                    && after_cut.binding == intent.local_cut.binding
                                    && after_cut.origin == intent.local_cut.origin
                                    && after_cut.restore_occurrence
                                        == intent.local_cut.restore_occurrence
                                    && after_cut.position.feed == intent.local_cut.position.feed
                                    && after_cut.position.position
                                        >= intent.local_cut.position.position,
                                "deferred planning differs from its preparation or candidate",
                            )?;
                            if let Some(connection) = connection {
                                super::delta::deferred::validate_on(connection, event, intent)?;
                            }
                            Stage::Settled
                        }
                        Transition::EvidenceImportApplied {
                            prepared: selected,
                            receipt,
                        } => {
                            let intent = current.evidence()?;
                            require(
                                selected == prepared && stage == Stage::Prepared,
                                "invalid evidence import transition",
                            )?;
                            receipt.validate(intent)?;
                            if let Some(connection) = connection {
                                super::evidence::validate_receipt_on(
                                    connection, binding, intent, receipt,
                                )?;
                            }
                            Stage::Settled
                        }
                        Transition::EvidenceStopped {
                            prepared: selected,
                            outcome,
                        } => {
                            current.evidence()?;
                            require(
                                selected == prepared && stage == Stage::Prepared,
                                "invalid evidence stopped transition",
                            )?;
                            outcome.validate()?;
                            Stage::Settled
                        }
                        Transition::PushStarted { prepared: selected } => {
                            require(
                                selected == prepared
                                    && current.direction() == Direction::Outgoing
                                    && stage == Stage::Committed
                                    && current.retry.is_none(),
                                "invalid push transition",
                            )?;
                            current.push_started = Some(event_id);
                            Stage::Pushing
                        }
                        Transition::PushRejected {
                            prepared: selected,
                            push_started,
                        } => {
                            require(
                                selected == prepared
                                    && current.direction() == Direction::Outgoing
                                    && stage == Stage::Pushing
                                    && current.push_started == Some(push_started)
                                    && !current.rejected,
                                "push refusal must name the current start once",
                            )?;
                            current.rejected = true;
                            Stage::Pushing
                        }
                        Transition::OutgoingReconciled {
                            prepared: selected,
                            reconciliation,
                        } => {
                            require(
                                selected == prepared && current.direction() == Direction::Outgoing,
                                "reconciliation requires the original outgoing operation",
                            )?;
                            match reconciliation.as_ref() {
                                OutgoingReconciliation::Resolved { failure, resolved } => {
                                    current.legacy()?;
                                    require(
                                        current.retry.is_none(),
                                        "outgoing retry is exhausted",
                                    )?;
                                    validate_outgoing_failure(
                                        failure,
                                        stage,
                                        current.push_started,
                                        current.rejected,
                                    )?;
                                    resolved.validate()?;
                                    let original = current.commit.ok_or_else(|| {
                                        super::super::invalid("outgoing commit is missing")
                                    })?;
                                    require(
                                        resolved.direction == Direction::Outgoing
                                            && &resolved.actor == current.actor()
                                            && resolved.operation != current.operation()
                                            && resolved.key != current.key()
                                            && resolved.expected_head.as_deref()
                                                == Some(original.oid.as_str())
                                            && resolved.commit.oid != original.oid
                                            && resolved.commit.parents.len() == 2
                                            && resolved.commit.parents[0] == original.oid
                                            && resolved.commit.parents[1] == failure.fetched_tip()
                                            && original.oid != failure.fetched_tip(),
                                        "outgoing reconciliation must retain both original data parents",
                                    )?;
                                    require(
                                        operations.insert(resolved.operation)
                                            && keys.insert(&resolved.key),
                                        "reconciliation retention operation or key is already in use",
                                    )?;
                                    require(
                                        resolved.commit.retention_ref
                                            == format!(
                                                "refs/engram/task-sync/exchange/{}/commit",
                                                resolved.operation
                                            ),
                                        "reconciled commit belongs to another retention operation",
                                    )?;
                                    current.commit = Some(&resolved.commit);
                                    current.retry = Some(event_id);
                                    current.push_started = None;
                                    current.rejected = false;
                                    Stage::Committed
                                }
                                OutgoingReconciliation::DeltaResolved {
                                    failure,
                                    resolved,
                                    payload,
                                    commit,
                                } => {
                                    require(
                                        current.retry.is_none(),
                                        "outgoing retry is exhausted",
                                    )?;
                                    validate_outgoing_failure(
                                        failure,
                                        stage,
                                        current.push_started,
                                        current.rejected,
                                    )?;
                                    payload.validate_historical(
                                        &event.binding,
                                        binding,
                                        resolved,
                                    )?;
                                    crate::task_sync::ledger::DeltaResolutionRecipe {
                                        failure,
                                        resolved,
                                        commit,
                                    }
                                    .validate(
                                        &event.binding,
                                        binding,
                                        prepared,
                                        current.delta()?,
                                        current.commit.ok_or_else(|| {
                                            super::super::invalid("outgoing commit is missing")
                                        })?,
                                    )?;
                                    require(
                                        operations.insert(resolved.operation)
                                            && keys.insert(&resolved.key),
                                        "reconciliation retention operation or key is already in use",
                                    )?;
                                    current.commit = Some(commit);
                                    current.retry = Some(event_id);
                                    current.push_started = None;
                                    current.rejected = false;
                                    Stage::Committed
                                }
                                OutgoingReconciliation::PushStarted { reconciliation } => {
                                    require(
                                        stage == Stage::Committed
                                            && current.retry == Some(reconciliation),
                                        "retry push requires its exact reconciliation",
                                    )?;
                                    current.push_started = Some(event_id);
                                    Stage::Pushing
                                }
                                OutgoingReconciliation::Stopped { failure } => {
                                    require(
                                        current.retry.is_some(),
                                        "outgoing retry has not been selected",
                                    )?;
                                    validate_outgoing_failure(
                                        failure,
                                        stage,
                                        current.push_started,
                                        current.rejected,
                                    )?;
                                    current.stopped = true;
                                    Stage::Settled
                                }
                            }
                        }
                        Transition::PushObserved {
                            prepared: selected,
                            observation,
                        } => {
                            // A recorded refusal is known; no observation replaces it.
                            require(
                                selected == prepared
                                    && matches!(stage, Stage::Pushing | Stage::Unknown)
                                    && !current.rejected,
                                "invalid publication observation",
                            )?;
                            match observation {
                                PublicationObservation::Confirmed { observed_tip } => {
                                    oid(observed_tip)?;
                                    require(
                                        current
                                            .commit
                                            .is_some_and(|commit| observed_tip == &commit.oid),
                                        "publication observation requires the exact retained commit",
                                    )?;
                                    Stage::Settled
                                }
                                PublicationObservation::Unknown { observed_tip } => {
                                    if let Some(tip) = observed_tip {
                                        oid(tip)?;
                                    }
                                    Stage::Unknown
                                }
                            }
                        }
                        // The outer match handles these; refuse rather than panic.
                        Transition::Activated
                        | Transition::Prepared { .. }
                        | Transition::DeltaPrepared { .. }
                        | Transition::EvidencePrepared { .. }
                        | Transition::PlanningSuccessorPrepared { .. }
                        | Transition::CustodyCheckpoint { .. } => {
                            return Err(super::super::invalid("unexpected exchange transition"));
                        }
                    };
                    pending = Some(current);
                }
            }
        }
        if let Some(current) = pending {
            let completed =
                (stage == Stage::Settled && !current.stopped).then(|| (*event_id).clone());
            if completed.is_some()
                && !matches!(current.request, Request::Evidence(_))
                && !matches!(event.transition, Transition::PlanningDeferred { .. })
            {
                match current.direction() {
                    Direction::Incoming => last_incoming.clone_from(&completed),
                    Direction::Outgoing => last_outgoing.clone_from(&completed),
                }
            }
            rows.insert(
                current.operation().to_string(),
                RequestProgress {
                    operation: current.operation().to_string(),
                    key: current.key().to_owned(),
                    prepared: current.prepared.clone(),
                    latest: (*event_id).clone(),
                    completed,
                    direction: current.direction(),
                    stage,
                },
            );
        }
        previous = Some(*event_id);
    }
    let (tail, event) = chain
        .last()
        .ok_or_else(|| super::super::invalid("exchange activation is missing"))?;
    Ok(ChainView {
        head: Head {
            tail: (*tail).clone(),
            sequence: event.sequence,
            pending: if stage == Stage::Settled {
                None
            } else {
                pending.map(|current| current.prepared.clone())
            },
            stage,
            superseded_by: None,
            last_incoming,
            last_outgoing,
            last_checkpoint,
        },
        operations: rows,
    })
}

fn validate_outgoing_failure(
    failure: &OutgoingFailure,
    stage: Stage,
    push_started: Option<&ObjectId>,
    rejected: bool,
) -> Result<(), StoreError> {
    oid(failure.fetched_tip())?;
    require(
        match failure {
            OutgoingFailure::AncestryChanged { .. } => {
                stage == Stage::Committed && push_started.is_none()
            }
            OutgoingFailure::NonFastForward {
                push_started: selected,
                ..
            } => stage == Stage::Pushing && push_started == Some(selected) && rejected,
        },
        "outgoing failure must name a known ancestry or push refusal; uncertainty stays unresolved",
    )
}
