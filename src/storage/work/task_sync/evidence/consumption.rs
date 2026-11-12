//! Native, explicitly selected use of already qualified retained proof.

use chrono::{DateTime, Utc};
use rusqlite::{Connection, TransactionBehavior};

use super::verification::applicability::{
    MeasuredDestination, binding_destination_on, measure_binding_destination,
};
use super::{SqliteStore, StoreError, is_imported_only, store_error};
use crate::domain::{ActorContext, ControlWorkBinding, SCHEMA_VERSION};
use crate::task_sync::evidence::consumption::{
    CONSUMPTION_KIND, ConsumedProof, ConsumptionReceipt, ConsumptionRequest,
    ImportedEvidenceConsumption,
};
use crate::{CanonicalObject, ObjectId, ProjectId, SessionId};

mod history;
mod projections;
mod selection;
pub(in crate::storage::work) use projections::expected_projection_on;
pub(crate) use projections::{rebuild, verify};
pub(crate) use selection::{LiveUses, SelectedUses, measure_selected_on, reselect_on};

type Result<T> = std::result::Result<T, StoreError>;

fn object_on(connection: &Connection, id: &ObjectId) -> Result<Option<(String, Vec<u8>)>> {
    super::object_on(connection, id).map_err(store_error)
}

fn invalid(reason: &'static str) -> StoreError {
    StoreError::InvalidWork(reason.into())
}

impl SqliteStore {
    pub(crate) fn imported_consumption_record(
        &self,
        id: &ObjectId,
    ) -> Result<ImportedEvidenceConsumption> {
        load_on(&self.connection, id)
    }
    /// Consume a selected B decision as an explicit local use. The original
    /// observed check and its producer keep their original identity and time.
    ///
    /// # Errors
    /// Refuses stale destination authority, source, context or terminal facts.
    pub fn consume_imported_verification(
        &mut self,
        project: &ProjectId,
        session: &SessionId,
        actor: &ActorContext,
        request: &ConsumptionRequest,
        now: DateTime<Utc>,
    ) -> Result<ConsumptionReceipt> {
        self.consume_imported_proof(project, session, actor, request, now, true)
    }

    /// Explicitly attach a C decision for ordinary local completion selection.
    /// This records no model verdict and installs no foreign completion seal.
    ///
    /// # Errors
    /// Refuses stale local policy/source or an incomplete historical proof.
    pub fn consume_imported_acceptance(
        &mut self,
        project: &ProjectId,
        session: &SessionId,
        actor: &ActorContext,
        request: &ConsumptionRequest,
        now: DateTime<Utc>,
    ) -> Result<ConsumptionReceipt> {
        self.consume_imported_proof(project, session, actor, request, now, false)
    }

    fn consume_imported_proof(
        &mut self,
        project: &ProjectId,
        session: &SessionId,
        actor: &ActorContext,
        request: &ConsumptionRequest,
        now: DateTime<Utc>,
        verification: bool,
    ) -> Result<ConsumptionReceipt> {
        if let Some(receipt) = replay_on(
            &self.connection,
            project,
            session,
            actor,
            request,
            verification,
        )? {
            return Ok(receipt);
        }
        let measured =
            measure_binding_destination(&self.connection, &request.binding).map_err(store_error)?;
        if request.consumption.as_str().len() != 32 {
            return Err(invalid("consumption requires a prepared record identity"));
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(receipt) =
            replay_on(&transaction, project, session, actor, request, verification)?
        {
            return Ok(receipt);
        }
        if actor.session_id.as_ref() != Some(session)
            || actor.run_id.as_deref() != Some(request.binding.run_id.0.to_string().as_str())
        {
            return Err(invalid(
                "imported consumption actor differs from its binding",
            ));
        }
        let destination = binding_destination_on(
            &transaction,
            project,
            session,
            &request.binding,
            &measured,
            now,
            None,
        )
        .map_err(store_error)?;
        let proof = if verification {
            super::verification::applicability::qualify_consumption_on(
                &transaction,
                &request.decision,
                &request.binding,
                &destination,
                &measured.source,
            )
            .map_err(store_error)?
            .proof
        } else {
            super::acceptance::qualify_consumption_on(
                &transaction,
                &request.decision,
                &request.binding,
                &destination,
                &measured.source,
                actor,
            )
            .map_err(store_error)?
            .0
        };
        let run = crate::storage::work::query::load_work_run(&transaction, request.binding.run_id)?;
        let record = ImportedEvidenceConsumption {
            schema_version: SCHEMA_VERSION,
            project_id: project.clone(),
            request: request.clone(),
            session_id: session.clone(),
            actor: actor.clone(),
            generation: run.generation,
            named_root: destination.root,
            governing_fingerprint: CanonicalObject::freeze(&destination.item)?.key().clone(),
            governing: destination.item,
            policy: SqliteStore::load_acceptance_evaluation_policy_on(&transaction)?,
            source: measured.source,
            destination_cut: destination.cut,
            proof,
            recorded_at: now,
        };
        let object = CanonicalObject::identified(&request.consumption, &record)?;
        crate::task_sync::evidence::limits::enforce(
            crate::task_sync::evidence::limits::LimitKind::RecordTooLarge,
            Some(&request.consumption),
            crate::task_sync::evidence::limits::Dimension::Bytes,
            object.bytes().len(),
            crate::task_sync::evidence::limits::MAX_RECORD_BYTES,
        )
        .map_err(crate::task_sync::evidence::EvidenceError::from)
        .map_err(|error| invalid_error(&error))?;
        crate::storage::work::completion::append_imported_evidence_on(
            &transaction,
            &record,
            &object,
        )?;
        projections::record_consumed_on(&transaction, &record)?;
        transaction.commit()?;
        Ok(receipt(request, false))
    }
}

fn replay_on(
    connection: &Connection,
    project: &ProjectId,
    session: &SessionId,
    actor: &ActorContext,
    request: &ConsumptionRequest,
    verification: bool,
) -> Result<Option<ConsumptionReceipt>> {
    if let Some((kind, bytes)) = object_on(connection, &request.consumption)? {
        if kind != CONSUMPTION_KIND || is_imported_only(connection, &request.consumption)? {
            return Err(invalid("imported consumption identity conflicts"));
        }
        let old: ImportedEvidenceConsumption = serde_json::from_slice(&bytes)?;
        if old.request != *request
            || old.project_id != *project
            || old.session_id != *session
            || old.actor != *actor
            || matches!(old.proof, ConsumedProof::Verification { .. }) != verification
        {
            return Err(invalid("imported consumption retry intent differs"));
        }
        return Ok(Some(receipt(request, true)));
    }
    Ok(None)
}

fn invalid_error(error: &crate::task_sync::evidence::EvidenceError) -> StoreError {
    StoreError::InvalidWork(error.to_string())
}

fn receipt(request: &ConsumptionRequest, replayed: bool) -> ConsumptionReceipt {
    ConsumptionReceipt {
        consumption: request.consumption.clone(),
        decision: request.decision.clone(),
        replayed,
    }
}

pub(crate) fn load_on(
    connection: &Connection,
    id: &ObjectId,
) -> Result<ImportedEvidenceConsumption> {
    let (kind, bytes) =
        object_on(connection, id)?.ok_or_else(|| invalid("imported consumption is unavailable"))?;
    if kind != CONSUMPTION_KIND {
        return Err(invalid("object is not an imported consumption"));
    }
    let record: ImportedEvidenceConsumption = serde_json::from_slice(&bytes)?;
    if record.schema_version != SCHEMA_VERSION
        || record.request.consumption != *id
        || record.actor.session_id.as_ref() != Some(&record.session_id)
        || record.actor.run_id.as_deref()
            != Some(record.request.binding.run_id.0.to_string().as_str())
        || record.generation < 1
        || record.source.validate().is_err()
        || CanonicalObject::freeze(&record.governing)?.key() != &record.governing_fingerprint
        || record.governing.work_id != record.request.binding.work_id
        || record.governing.project_id != record.project_id
    {
        return Err(invalid(
            "imported consumption canonical binding is malformed",
        ));
    }
    history::validate_on(connection, &record)?;
    Ok(record)
}

/// Local currency alone may retire implicit satisfaction. Structural/history
/// checks and terminal conflicts are separate and must never be swallowed.
fn destination_current_on(
    connection: &Connection,
    record: &ImportedEvidenceConsumption,
    binding: &ControlWorkBinding,
    measured: &MeasuredDestination,
) -> Result<bool> {
    let item = crate::storage::work::query::load_work_item(connection, binding.work_id)?;
    let run = crate::storage::work::query::load_work_run(connection, binding.run_id)?;
    let claim = crate::storage::work::query::load_work_claim_optional(connection, binding.run_id)?;
    let root = crate::storage::work::feeds::latest_named_root_binding_on(
        connection,
        binding.run_id,
        binding.claim_id,
        i64::MAX,
    )?;
    Ok(record.request.binding == *binding
        && item.active_run_id == Some(binding.run_id)
        && run.generation == record.generation
        && !run.is_finished()
        && claim.is_some_and(|claim| {
            claim.claim_id == binding.claim_id
                && claim.fence == binding.claim_fence
                && claim.holder == record.session_id
        })
        && root.is_some_and(|(_, id, event)| {
            id == record.named_root && event.kind == crate::domain::NamedRootBindingKind::Bound
        })
        && CanonicalObject::freeze(&item)?.key() == &record.governing_fingerprint
        && measured.source == record.source)
}

pub(crate) fn current_on(
    connection: &Connection,
    record: &ImportedEvidenceConsumption,
    binding: &ControlWorkBinding,
    measured: &MeasuredDestination,
    now: DateTime<Utc>,
) -> Result<Option<Vec<crate::domain::AcceptanceResult>>> {
    if record.request.binding != *binding {
        return Err(invalid("imported consumption belongs to another claim"));
    }
    let destination = binding_destination_on(
        connection,
        &record.project_id,
        &record.session_id,
        binding,
        measured,
        now,
        None,
    )
    .map_err(store_error)?;
    let run = crate::storage::work::query::load_work_run(connection, binding.run_id)?;
    if run.generation != record.generation
        || destination.root != record.named_root
        || CanonicalObject::freeze(&destination.item)?.key() != &record.governing_fingerprint
        || measured.source != record.source
    {
        return Err(invalid(
            "imported consumption source or governing generation moved",
        ));
    }
    match &record.proof {
        ConsumedProof::Verification { .. } => {
            let qualified = super::verification::applicability::qualify_consumption_on(
                connection,
                &record.request.decision,
                binding,
                &destination,
                &measured.source,
            )
            .map_err(store_error)?;
            // A later equivalent terminal may enlarge this set. Conflicts are
            // refused by the common qualifier; old consumption stays immutable.
            if let (
                ConsumedProof::Verification {
                    origin,
                    original,
                    origin_cut,
                    requirement,
                    criterion,
                    execution,
                    ..
                },
                ConsumedProof::Verification {
                    origin: actual_origin,
                    original: actual_original,
                    origin_cut: actual_cut,
                    requirement: actual_requirement,
                    criterion: actual_criterion,
                    execution: actual_execution,
                    ..
                },
            ) = (&record.proof, &qualified.proof)
                && (origin != actual_origin
                    || original != actual_original
                    || origin_cut != actual_cut
                    || requirement != actual_requirement
                    || criterion != actual_criterion
                    || execution != actual_execution)
            {
                return Err(invalid("imported consumption original decision differs"));
            }
            Ok(None)
        }
        ConsumedProof::Acceptance { .. } => {
            let (proof, results) = super::acceptance::qualify_consumption_on(
                connection,
                &record.request.decision,
                binding,
                &destination,
                &measured.source,
                &record.actor,
            )
            .map_err(store_error)?;
            if proof != record.proof {
                return Err(invalid("imported consumption acceptance proof differs"));
            }
            Ok(Some(results))
        }
    }
}

/// A read-only currency predicate for native evaluation citations. Actual new
/// writes also use the measured destination admission above, outside the writer.
pub(crate) fn verification_current_on(
    connection: &Connection,
    record: &ImportedEvidenceConsumption,
    source: &crate::source::SourceId,
) -> Result<bool> {
    let ConsumedProof::Verification {
        origin,
        original,
        origin_cut,
        requirement,
        criterion,
        ..
    } = &record.proof
    else {
        return Ok(false);
    };
    let binding = &record.request.binding;
    let item = crate::storage::work::query::load_work_item(connection, binding.work_id)?;
    let run = crate::storage::work::query::load_work_run(connection, binding.run_id)?;
    if item.active_run_id != Some(binding.run_id)
        || run.generation != record.generation
        || run.is_finished()
        || source != &record.source
        || CanonicalObject::freeze(&item)?.key() != &record.governing_fingerprint
    {
        return Ok(false);
    }
    let root = crate::storage::work::feeds::latest_named_root_binding_on(
        connection,
        binding.run_id,
        binding.claim_id,
        i64::MAX,
    )?;
    if root.as_ref().is_none_or(|(_, id, event)| {
        id != &record.named_root || event.kind != crate::domain::NamedRootBindingKind::Bound
    }) {
        return Ok(false);
    }
    let context = super::verification::applicability::VerificationUse {
        project: &record.project_id,
        origin,
        binding,
        original,
        criterion: *criterion,
        requirement: Some(requirement),
        governing: &record.governing_fingerprint,
        root: &record.named_root,
        source,
    };
    if !super::verification::applicability::eligible_for_on(
        connection,
        &record.request.decision,
        &context,
    )
    .map_err(store_error)?
    {
        return Ok(false);
    }
    super::verification::applicability::candidate_in_context(
        connection,
        &record.project_id,
        origin,
        original,
        origin_cut,
    )
    .map_err(store_error)?;
    Ok(true)
}
