//! A known live planning owner defers this request; unknown effects stay pending.

use super::{
    CanonicalObject, CompletedExchange, Connection, Event, InspectedDelta, Ledger, ObjectId,
    StoreError, Transaction, Transition, append, invalid, producing_origin, reconciliation,
    require,
};
use crate::storage::work::query;
use crate::task_sync::delta::DeltaIntent;
use crate::task_sync::reconciliation::{HOLD_KIND, ReconciliationHold};
use crate::{WorkClaimState, WorkEvent, WorkId};
use chrono::{DateTime, Utc};
use rusqlite::params;
use std::collections::BTreeSet;

/// The existing local work an incoming request's planning would reach: its
/// changed tasks and the targets of their document edges.
fn seeds_on(connection: &Connection, input: &InspectedDelta) -> Result<Vec<WorkId>, StoreError> {
    let mut seeds = input.intent.candidate.changed.clone();
    for task in input.payload.documents.values().flatten() {
        seeds.extend(
            super::super::super::closure::graph::document_edges(task)
                .into_iter()
                .map(|(_, target)| target),
        );
    }
    seeds
        .into_iter()
        .filter_map(|id| match query::load_work_item(connection, id) {
            Ok(item) if item.project_id == input.binding.project => Some(Ok(id)),
            Ok(_) => Some(Err(invalid("deferred planning scope crosses projects"))),
            Err(StoreError::WorkNotFound(_)) => None,
            Err(error) => Some(Err(error)),
        })
        .collect()
}

/// Refuse a request whose planning scope overlaps an active reconciliation
/// hold. Such a request can neither apply nor defer until that hold resolves,
/// and retaining it first would leave its bytes in the way of the hold's own
/// successor, so this runs before any candidate is retained.
pub(super) fn refuse_held_scope_on(
    connection: &Connection,
    input: &InspectedDelta,
) -> Result<(), StoreError> {
    let seeds = seeds_on(connection, input)?;
    if seeds.is_empty() {
        return Ok(());
    }
    for work in reconciliation::capture_affected_on(connection, &input.binding.project, &seeds)? {
        reconciliation::refuse_held_on(connection, work.work_id)?;
    }
    Ok(())
}

pub(super) fn try_defer_on(
    transaction: &Transaction<'_>,
    ledger: &mut Ledger,
    input: &InspectedDelta,
    prepared: &CanonicalObject,
    now: DateTime<Utc>,
) -> Result<Option<CompletedExchange>, StoreError> {
    let seeds = seeds_on(transaction, input)?;
    if seeds.is_empty() {
        return Ok(None);
    }
    let affected =
        reconciliation::capture_affected_on(transaction, &input.binding.project, &seeds)?;
    // A live owner defers the request even when its claim, or other work it
    // did, came after the request's local cut: that activity is exactly what
    // deferral waits for, and the hold captures the current bases. Only a
    // request applied now needs its native readset unchanged since the cut.
    let mut blocking_claims = BTreeSet::new();
    for work in &affected {
        for run in &work.runs {
            let Some(claim) = query::load_work_claim_optional(transaction, run.run_id)?
                .filter(|claim| claim.state == WorkClaimState::Active && claim.expires_at > now)
            else {
                continue;
            };
            let stored: String = transaction.query_row(
                "SELECT object_id FROM objects INDEXED BY objects_work_event_work_id
                 WHERE object_kind='work_event'
                   AND NOT EXISTS(SELECT 1 FROM work_task_sync_evidence_imported foreign_record WHERE foreign_record.object_id=objects.object_id)
                   AND json_extract(canonical_json,'$.work_id')=?1
                   AND json_extract(canonical_json,'$.claim.claim_id')=?2
                   AND json_extract(canonical_json,'$.claim.revision')=?3
                 ORDER BY json_extract(canonical_json,'$.revision') DESC LIMIT 1",
                params![
                    work.work_id.0.to_string(),
                    claim.claim_id.0.to_string(),
                    claim.revision
                ],
                |row| row.get(0),
            )?;
            let id = ObjectId::from_stored(stored)
                .ok_or_else(|| invalid("blocking claim event identity is invalid"))?;
            let event: WorkEvent =
                crate::storage::SqliteStore::get_typed_object_on(transaction, &id, "work_event")?
                    .ok_or_else(|| invalid("blocking claim event is missing"))?;
            require(
                event.claim.as_ref() == Some(&claim) && event.created_at <= now,
                "blocking claim differs from its immutable event",
            )?;
            blocking_claims.insert(id);
        }
    }
    if blocking_claims.is_empty() {
        super::readset::unchanged_on(
            transaction,
            &input.binding.project,
            &input.intent.local_cut,
            affected.iter().map(|basis| basis.work_id),
        )?;
        return Ok(None);
    }
    let hold = ReconciliationHold {
        schema_version: 1,
        project_id: input.binding.project.clone(),
        binding: input.binding_id.clone(),
        prepared: prepared.key().clone(),
        seeds,
        candidate_fingerprint: input.intent.candidate.content_fingerprint.clone(),
        candidate_revision: 1,
        hold_revision: 1,
        scope_revision: 1,
        owner_set_revision: 1,
        association: prepared.key().clone(),
        affected,
        non_run_owner_refs: Vec::new(),
        previous: None,
        actor: input.intent.actor.clone(),
        created_at: now,
    };
    let hold = CanonicalObject::mint(&hold)?;
    reconciliation::holds::place_on(transaction, &hold)?;
    let cut = producing_origin::cut_on(transaction, &input.binding.project)?;
    let mut after_cut = input.intent.local_cut.clone();
    after_cut.position.position = cut.position;
    after_cut.anchor = cut.anchor;
    let outcome = append(
        transaction,
        ledger,
        input,
        Transition::PlanningDeferred {
            prepared: prepared.key().clone(),
            hold: hold.key().clone(),
            candidate_fingerprint: input.intent.candidate.content_fingerprint.clone(),
            blocking_claims: blocking_claims.into_iter().collect(),
            after_cut,
        },
        now,
    )?;
    Ok(Some(CompletedExchange {
        prepared: prepared.key().clone(),
        outcome: outcome.key().clone(),
        event: outcome.decode()?,
    }))
}

/// Historical validation reads the original claim proofs, never today's holder.
pub(in crate::storage::work::task_sync::ledger) fn validate_on(
    connection: &Connection,
    event: &Event,
    intent: &DeltaIntent,
) -> Result<(), StoreError> {
    let Transition::PlanningDeferred {
        prepared,
        hold: id,
        candidate_fingerprint,
        blocking_claims,
        after_cut,
    } = &event.transition
    else {
        return Err(invalid("planning deferred outcome is missing"));
    };
    let hold: ReconciliationHold =
        crate::storage::SqliteStore::get_typed_object_on(connection, id, HOLD_KIND)?
            .ok_or_else(|| invalid("deferred planning hold is missing"))?;
    reconciliation::holds::validate_hold(connection, id, &hold)?;
    require(
        hold.prepared == *prepared
            && hold.binding == event.binding
            && intent.local_cut.position.feed == crate::FeedId::Project(hold.project_id.clone())
            && hold.actor == event.actor
            && hold.created_at == event.recorded_at
            && hold.candidate_fingerprint == *candidate_fingerprint,
        "deferred planning hold belongs to another candidate",
    )?;
    for claim_id in blocking_claims {
        let claim_event: WorkEvent =
            crate::storage::SqliteStore::get_typed_object_on(connection, claim_id, "work_event")?
                .ok_or_else(|| invalid("deferred planning claim proof is missing"))?;
        let claim = claim_event
            .claim
            .as_ref()
            .ok_or_else(|| invalid("deferred planning proof has no claim"))?;
        require(
            claim_event.project_id == hold.project_id
                && claim_event.work_id == claim.work_id
                && claim_event.run_id == Some(claim.run_id)
                && claim_event.created_at <= event.recorded_at
                && claim.state == WorkClaimState::Active
                && claim.expires_at > event.recorded_at
                && hold.affected.iter().any(|work| {
                    work.work_id == claim.work_id
                        && work.runs.iter().any(|run| {
                            run.run_id == claim.run_id
                                && run.claim_id == Some(claim.claim_id)
                                && run.claim_fence == claim.fence
                        })
                }),
            "deferred planning proof does not name a live held claim",
        )?;
    }
    producing_origin::check_cut_on(
        connection,
        &hold.project_id,
        &crate::task_sync::progress::ExportCut {
            position: after_cut.position.position,
            anchor: after_cut.anchor.clone(),
        },
    )
}
