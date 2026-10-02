//! `execution_observe`: a host records execution it observed without
//! admission. The record is a fact under the host's attribution, never a
//! grant, a begin or a turn result. An identical retry under the same
//! observing session and key returns the original receipt.

use chrono::{DateTime, Utc};
use rusqlite::TransactionBehavior;
use serde::Serialize;

use super::{CONTROL_SCHEMA_VERSION, SessionId, SqliteStore, StoreError, work};
use crate::canonical::CanonicalObject;
use crate::domain::{
    ActorContext, ExecutionObservationDecision, ExecutionObservationReceipt, ExecutionObserveInput,
    MAX_EXECUTION_OBSERVE_RESULT_BYTES, ObservationAccounting, ObservationAdmission,
    ObservationAuditReason, ObservationPolicyBasis, ObservedCheckSummary, ProjectId,
    RecordedOccurrence, UNADMITTED_EXECUTION_OBSERVATION_SCHEMA_VERSION,
    UnadmittedExecutionObservation,
};

/// The operation name an observation's idempotency row is kept under.
pub(super) const EXECUTION_OBSERVE_OPERATION: &str = "execution_observe";

/// Everything an observation request asserts, under the session that sent
/// it. Credentials and the server clock stay out, so a retry after a
/// reconnection by the same session replays.
#[derive(Serialize)]
struct ExecutionObserveIntent<'a> {
    control_schema_version: u16,
    operation: &'static str,
    session_id: &'a SessionId,
    idempotency_key: &'a str,
    binding: &'a crate::domain::ControlWorkBinding,
    root_basis: &'a crate::domain::ObservationRootBasis,
    observed_interval: &'a crate::domain::ObservedInterval,
    occurrence: &'a crate::domain::ObservedOccurrence,
    causality: &'a crate::domain::ObservationCausality,
    policy_basis: &'a ObservationPolicyBasis,
}

impl<'a> ExecutionObserveIntent<'a> {
    fn of(session_id: &'a SessionId, input: &'a ExecutionObserveInput) -> Self {
        Self {
            control_schema_version: CONTROL_SCHEMA_VERSION,
            operation: EXECUTION_OBSERVE_OPERATION,
            session_id,
            idempotency_key: &input.idempotency_key,
            binding: &input.binding,
            root_basis: &input.root_basis,
            observed_interval: &input.observed_interval,
            occurrence: &input.occurrence,
            causality: &input.causality,
            policy_basis: &input.policy_basis,
        }
    }
}

/// The receipt a stored observation answers with, rebuilt from the record and
/// the obligations it opened. A new change names the record itself as its
/// anchor, which the stored record cannot carry.
fn receipt_of(
    observation: &UnadmittedExecutionObservation,
    observation_id: crate::ObjectId,
    position: crate::domain::FeedPosition,
    opened_obligations: Vec<crate::ObjectId>,
) -> ExecutionObservationReceipt {
    let accounting = match &observation.accounting {
        ObservationAccounting::SourceChange { .. } => ObservationAccounting::SourceChange {
            source_change: Some(observation_id.clone()),
        },
        other => other.clone(),
    };
    ExecutionObservationReceipt {
        decision: ExecutionObservationDecision::Recorded,
        observation: observation_id,
        position,
        observing_session: observation.observing_session.clone(),
        binding: observation.binding.clone(),
        admission: ObservationAdmission::Unadmitted,
        causality: observation.causality.clone(),
        policy_basis: observation.policy_basis.clone(),
        accounting,
        opened_obligations,
        observed_checks: observation
            .occurrence
            .checks()
            .into_iter()
            .map(|check| ObservedCheckSummary {
                host_check_id: check.check.host_check_id.clone(),
                credit: check.credit,
            })
            .collect(),
        recorded_at: observation.recorded_at,
    }
}

/// Doctor: whether a stored `execution_observe` row answers with exactly the
/// receipt its record gives, at the record's run-feed position, for the
/// session and key that sent it, under the intent that record's request
/// freezes to.
///
/// # Errors
///
/// Returns [`StoreError`] only when a row cannot be read.
pub(super) fn execution_observe_row_matches(
    connection: &rusqlite::Connection,
    session_id: &str,
    idempotency_key: &str,
    intent_hash: &str,
    intent_json: &[u8],
    result: serde_json::Value,
) -> Result<bool, StoreError> {
    use rusqlite::OptionalExtension;
    let Ok(receipt) = serde_json::from_value::<ExecutionObservationReceipt>(result) else {
        return Ok(false);
    };
    let stored: Option<(Vec<u8>, i64)> = connection
        .query_row(
            "SELECT object.canonical_json, entry.position
             FROM objects object
             JOIN work_feed_entries entry ON entry.object_id = object.object_id
             WHERE object.object_id = ?1 AND object.object_kind = ?2
               AND entry.feed_kind = 'run_execution' AND entry.feed_id = ?3",
            rusqlite::params![
                receipt.observation.as_str(),
                work::UNADMITTED_OBSERVATION_KIND,
                receipt.binding.run_id.0.to_string()
            ],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((bytes, position)) = stored else {
        return Ok(false);
    };
    let Ok(observation) = CanonicalObject::stored(&receipt.observation, bytes)
        .and_then(|object| object.decode::<UnadmittedExecutionObservation>())
    else {
        return Ok(false);
    };
    let input = ExecutionObserveInput {
        idempotency_key: idempotency_key.to_owned(),
        binding: observation.binding.clone(),
        root_basis: observation.root_basis.clone(),
        observed_interval: observation.observed_interval,
        occurrence: observation.occurrence.reported(),
        causality: observation.causality.clone(),
        policy_basis: observation.policy_basis.clone(),
    };
    let intent = CanonicalObject::freeze(&ExecutionObserveIntent::of(
        &observation.observing_session,
        &input,
    ))?;
    let opened = work::opened_obligations_of_on(
        connection,
        observation.binding.run_id,
        &receipt.observation,
    )?;
    let expected = receipt_of(
        &observation,
        receipt.observation.clone(),
        crate::domain::FeedPosition {
            feed: crate::domain::FeedId::RunExecution(observation.binding.run_id),
            position,
        },
        opened,
    );
    Ok(observation.observing_session.0 == session_id
        && intent.key().as_str() == intent_hash
        && intent.bytes() == intent_json
        && receipt == expected)
}

impl SqliteStore {
    /// Records one observation for the bound control session.
    ///
    /// # Errors
    ///
    /// The control session's own refusals for a superseded connection, an
    /// unbound session or a mismatched project or routing token;
    /// [`StoreError::ExecutionObservationInvalid`] for a malformed or
    /// oversized request; [`StoreError::ExecutionObservationPolicyBasisMismatch`]
    /// when accounting names another policy than the project's current one;
    /// [`StoreError::ExecutionObservationBasisMismatch`]
    /// when the binding, cut or root basis is not what the store holds;
    /// [`StoreError::ControlOperationIdempotencyConflict`] when the key was
    /// used for a different request. A refusal records nothing.
    #[allow(
        clippy::too_many_arguments,
        reason = "the host record keeps routing, attribution, request and time explicit"
    )]
    pub(crate) fn record_unadmitted_execution_observation(
        &mut self,
        project_id: &ProjectId,
        session_id: &SessionId,
        connection_token: &str,
        routing_token: &str,
        observer: &ActorContext,
        input: ExecutionObserveInput,
        now: DateTime<Utc>,
    ) -> Result<ExecutionObservationReceipt, StoreError> {
        let intent = CanonicalObject::freeze(&ExecutionObserveIntent::of(session_id, &input))?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::verify_control_connection(&transaction, session_id, connection_token)?;
        let session = Self::load_control_session_on(&transaction, session_id)?
            .ok_or_else(|| StoreError::ControlSessionNotBound(session_id.0.clone()))?;
        Self::verify_control_session(&session, project_id, routing_token)?;
        // A committed record replays before anything mutable is consulted.
        if let Some(replay) = Self::replay_control_operation(
            &transaction,
            session_id,
            EXECUTION_OBSERVE_OPERATION,
            &input.idempotency_key,
            intent.key(),
        )? {
            transaction.commit()?;
            return Ok(replay);
        }
        input
            .validate_shape(now)
            .map_err(StoreError::ExecutionObservationInvalid)?;
        let idempotency_key = input.idempotency_key.clone();
        if let ObservationPolicyBasis::AccountIfEligible {
            project_policy_epoch,
            policy,
            obligation_rule_set,
        } = &input.policy_basis
        {
            let active = Self::load_active_control_policy(&transaction)?;
            if active.epoch != *project_policy_epoch
                || active.policy_id != *policy
                || active.obligation_rule_set != *obligation_rule_set
            {
                return Err(StoreError::ExecutionObservationPolicyBasisMismatch(
                    format!(
                        "the project's current policy is epoch {} policy {} with obligation rule set {}",
                        active.epoch.0, active.policy_id, active.obligation_rule_set
                    ),
                ));
            }
        }
        let cut = input.root_basis.capture_run_cut;
        let claim_epoch_event =
            work::historical_claim_epoch_on(&transaction, project_id, &input.binding, cut)?;
        work::check_root_basis_on(&transaction, &input.binding, &input.root_basis)?;
        let occurrence = RecordedOccurrence::record(input.occurrence);
        if let Some(sighting) = occurrence
            .source_change()
            .and_then(|change| change.sighting())
        {
            work::check_sighting_matches_root_basis_on(
                &transaction,
                &input.binding,
                &input.root_basis,
                &sighting.source_basis,
            )?;
        }
        for check in occurrence.checks() {
            if let Some(basis) = &check.check.source_basis {
                work::check_source_root_at_cut_on(
                    &transaction,
                    &input.binding,
                    basis,
                    cut,
                    &format!("observed check {:?}", check.check.host_check_id),
                )?;
            }
        }
        let accounting = match &input.policy_basis {
            ObservationPolicyBasis::AuditOnly {} => ObservationAccounting::AuditOnly {
                reason: ObservationAuditReason::ExplicitAudit,
            },
            ObservationPolicyBasis::AccountIfEligible { .. } => work::decide_accounting_on(
                &transaction,
                &input.binding,
                &input.root_basis,
                occurrence.source_change(),
                work::current_run_feed_cut_on(&transaction, input.binding.run_id)?.position,
            )?,
        };
        let observation = UnadmittedExecutionObservation {
            schema_version: UNADMITTED_EXECUTION_OBSERVATION_SCHEMA_VERSION,
            project_id: project_id.clone(),
            observing_session: session_id.clone(),
            observer: observer.clone(),
            binding: input.binding,
            claim_epoch_event,
            root_basis: input.root_basis,
            observed_interval: input.observed_interval,
            occurrence,
            causality: input.causality,
            policy_basis: input.policy_basis,
            admission: ObservationAdmission::Unadmitted,
            accounting,
            recorded_at: now,
        };
        let (observation_id, position) =
            work::append_unadmitted_observation_on(&transaction, &observation)?;
        let opened = work::open_unadmitted_obligations_on(
            &transaction,
            &observation_id,
            &observation,
            &position,
        )?;
        let receipt = receipt_of(&observation, observation_id, position, opened);
        let receipt_bytes = crate::canonical::canonical_bytes(&receipt)?;
        if receipt_bytes.len() > MAX_EXECUTION_OBSERVE_RESULT_BYTES {
            return Err(StoreError::ExecutionObservationInvalid(format!(
                "the receipt would exceed {MAX_EXECUTION_OBSERVE_RESULT_BYTES} bytes; nothing was recorded"
            )));
        }
        Self::persist_control_operation(
            &transaction,
            session_id,
            EXECUTION_OBSERVE_OPERATION,
            &idempotency_key,
            &intent,
            &receipt,
            now,
        )?;
        transaction.commit()?;
        Ok(receipt)
    }
}
