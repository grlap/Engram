//! A remote run's immutable terminal and each delivery of it to a destination.
//!
//! The terminal belongs to the project, the delivery to its destination. Only
//! a delivery the qualifier accepts mints verification, which then goes through
//! the same writer, feeds and matcher as any other; nothing local is synthesized.

use crate::CanonicalObject;
use crate::domain::{
    ExecutionOutcome, ExecutionSourceBasis, NamedRootBindingKind, REMOTE_COMPLETION_SCHEMA_VERSION,
    REMOTE_TERMINAL_ORIGIN_KIND, REMOTE_VERIFICATION_ASSOCIATION_KIND,
    RegisteredVerificationRecord, RemoteRunCompletion, RemoteRunInput, RemoteRunMode,
    RemoteTerminalOrigin, RemoteVerificationAssociationRecord, RemoteVerificationQualification,
    RemoteVerificationReason, RemoteVerificationReceipt, RemoteVerificationRequest, SCHEMA_VERSION,
    SourceRootState, VerificationEvidence, VerificationKind, VerificationProducerFacts,
    VerificationResult, VerificationSourceBasis, VerificationSourceIdentity,
};
use crate::storage::{SqliteStore, StoreError};
use crate::{ActorContext, ObjectId, ProjectId, SessionId};
use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

#[cfg(test)]
mod tests;

/// The row that keeps one project's run to one terminal: its key names the
/// project, host and run, and its fingerprint the terminal facts.
pub(in crate::storage::work) const ORIGIN_OPERATION: &str = "remote_verification_origin";
fn remote_stage(mode: RemoteRunMode) -> &'static str {
    match mode {
        RemoteRunMode::Full => "full",
        RemoteRunMode::Focused => "focused",
    }
}

fn invalid(field: &str, reason: &str) -> StoreError {
    StoreError::RemoteVerificationInvalid {
        field: field.into(),
        reason: reason.into(),
    }
}

fn origin_key(project: &ProjectId, completion: &RemoteRunCompletion) -> Result<String, StoreError> {
    serde_json::to_string(&(project, &completion.origin.host_id, &completion.run_id))
        .map_err(StoreError::Json)
}

/// The check's identity. The wire names no check, so this is derived from the
/// run; a requirement pinned to some other check's fingerprint stays unmatched.
fn check_fingerprint(completion: &RemoteRunCompletion) -> Result<ObjectId, StoreError> {
    Ok(CanonicalObject::freeze(&(
        REMOTE_VERIFICATION_ASSOCIATION_KIND,
        &completion.origin.host_id,
        &completion.run_id,
        completion.mode,
    ))?
    .key()
    .clone())
}

pub(in crate::storage) fn record_remote_verification_on(
    tx: &Transaction<'_>,
    project: &ProjectId,
    session: &SessionId,
    actor: &ActorContext,
    request: &RemoteVerificationRequest,
    now: DateTime<Utc>,
) -> Result<RemoteVerificationReceipt, StoreError> {
    request
        .validate_new()
        .map_err(|(field, reason)| invalid(field, reason))?;
    let completion = &request.completion;
    let supported = completion.schema_version == REMOTE_COMPLETION_SCHEMA_VERSION;
    let destination = if supported {
        validate_destination(tx, project, session, request, now)?
    } else {
        Err(format!(
            "this build reads completion schema {REMOTE_COMPLETION_SCHEMA_VERSION}, not {}",
            completion.schema_version
        ))
    };
    let settled = if supported && completion.is_authoritative_terminal() {
        Some(settle_origin(tx, project, session, actor, completion, now)?)
    } else {
        None
    };
    let (origin, conflict) = settled.map_or((None, false), |(id, conflict)| (Some(id), conflict));
    let qualification = match &destination {
        Err(detail) => RemoteVerificationQualification::refused(
            if supported {
                RemoteVerificationReason::AssociationStale
            } else {
                RemoteVerificationReason::ProducerUnsupported
            },
            Some(detail.clone()),
        ),
        Ok(_) if conflict => RemoteVerificationQualification::refused(
            RemoteVerificationReason::ResultConflict,
            Some("this run already has different terminal facts".into()),
        ),
        Ok(_) => crate::control::remote_result_disqualification(
            completion,
            &request.association.intended_use,
            now,
        )
        .map_or_else(
            RemoteVerificationQualification::credited,
            |(reason, detail)| RemoteVerificationQualification::refused(reason, detail),
        ),
    };
    if destination.is_ok()
        && !conflict
        && let Some(origin) = &origin
        && let Some(receipt) = repeated_delivery(tx, session, request, origin)?
    {
        return Ok(receipt);
    }
    let binding = &request.association.binding;
    let mut actor = actor.clone();
    if destination.is_ok() {
        actor.run_id = Some(binding.run_id.0.to_string());
    }
    let record = RemoteVerificationAssociationRecord {
        schema_version: SCHEMA_VERSION,
        project_id: project.clone(),
        session_id: session.clone(),
        request: request.clone(),
        origin: origin.clone(),
        qualification: qualification.clone(),
        actor: actor.clone(),
        associated_at: now,
    };
    let object = CanonicalObject::mint(&record)?;
    SqliteStore::insert_object(tx, REMOTE_VERIFICATION_ASSOCIATION_KIND, &object)?;
    super::task_sync::evidence::verification::index_remote_record_on(
        tx,
        object.key(),
        REMOTE_VERIFICATION_ASSOCIATION_KIND,
        object.bytes(),
    )?;
    let Ok(item) = destination else {
        super::feeds::append_to_project_feed(
            tx,
            project,
            REMOTE_VERIFICATION_ASSOCIATION_KIND,
            &object,
        )?;
        return Ok(RemoteVerificationReceipt {
            operation: object.key().clone(),
            origin,
            verifications: Vec::new(),
            qualification,
        });
    };
    let positions = super::feeds::append_to_work_feeds(
        tx,
        project,
        item.root_id,
        Some(binding.run_id),
        None,
        REMOTE_VERIFICATION_ASSOCIATION_KIND,
        &object,
    )?;
    super::note_index::append_native(
        tx,
        &positions,
        object.key(),
        &super::note_index::NativeNote::RemoteVerification(&record),
    )?;
    let mut verifications = Vec::new();
    if qualification.credited {
        let completed_at = completion.finished_at.ok_or_else(|| {
            StoreError::InvalidWorkProjection("a credited result has no finish time".into())
        })?;
        let record_id = super::append_control_verification_evidence_on(
            tx,
            &VerificationEvidence {
                applicability: None,
                schema_version: SCHEMA_VERSION,
                project_id: project.clone(),
                binding: binding.clone(),
                session_id: session.clone(),
                producer_observation: object.key().clone(),
                source_basis: source_basis(request)?,
                environment: None,
                check_kind: VerificationKind::Test,
                check_fingerprint: check_fingerprint(completion)?,
                result: VerificationResult::Passed,
                completed_at,
                summary: completion.headline(),
                refs: Vec::new(),
                actor,
                recorded_at: now,
                bound_from: None,
            },
        )?;
        verifications.push(RegisteredVerificationRecord {
            stage: remote_stage(completion.mode).into(),
            record_id,
        });
    }
    Ok(RemoteVerificationReceipt {
        operation: object.key().clone(),
        origin,
        verifications,
        qualification,
    })
}

fn source_basis(
    request: &RemoteVerificationRequest,
) -> Result<VerificationSourceBasis, StoreError> {
    let association = &request.association;
    if let RemoteRunInput::SourceId(input) = &request.completion.input {
        let source_id = input.matching_source_id().ok_or_else(|| {
            StoreError::InvalidWorkProjection(
                "a credited remote shared run has no matching source_id endpoints".into(),
            )
        })?;
        return Ok(VerificationSourceBasis {
            workspace_id: association.root.workspace_id.clone(),
            identity: VerificationSourceIdentity::SourceId {
                source_id: source_id.clone(),
            },
            source_root_generation: Some(association.root.generation),
            source_root_state: Some(SourceRootState::Named),
        });
    }
    Ok(ExecutionSourceBasis {
        workspace_id: association.root.workspace_id.clone(),
        source_revision: association.intended_use.revision(),
        git_tree: association.intended_use.git_tree(),
        source_root_generation: Some(association.root.generation),
        source_root_state: Some(SourceRootState::Named),
    }
    .into())
}

/// The destination's item, or why it is stale. A database failure stays an
/// error; only a destination that has moved is a result to report.
fn validate_destination(
    tx: &Transaction<'_>,
    project: &ProjectId,
    session: &SessionId,
    request: &RemoteVerificationRequest,
    now: DateTime<Utc>,
) -> Result<Result<crate::domain::WorkItem, String>, StoreError> {
    let binding = &request.association.binding;
    let item = match super::query::load_work_item(tx, binding.work_id) {
        Ok(item) => item,
        Err(StoreError::WorkNotFound(_)) => return Ok(Err("the work item is unknown".into())),
        Err(error) => return Err(error),
    };
    if item.project_id != *project {
        return Ok(Err("the work item belongs to another project".into()));
    }
    if item.revision != binding.work_revision {
        return Ok(Err("the work item changed since the binding".into()));
    }
    match super::planning::validate_control_work_binding_on(tx, project, session, binding, now) {
        Ok(()) => {}
        Err(StoreError::ControlWorkBindingStale { .. } | StoreError::WorkClaimMismatch { .. }) => {
            return Ok(Err("the claim, fence or run moved since the binding".into()));
        }
        Err(error) => return Err(error),
    }
    let root = &request.association.root;
    let current =
        super::latest_named_root_event_on(tx, binding.run_id, binding.claim_id, i64::MAX)?
            .map(|(_, event)| event);
    let Some(current) = current else {
        return Ok(Err("the claim has no named root".into()));
    };
    if current.kind != NamedRootBindingKind::Bound
        || current.workspace_id != root.workspace_id
        || current.generation != root.generation
        || current.claim_fence != binding.claim_fence
        || current.session_id != *session
    {
        return Ok(Err("the named root moved".into()));
    }
    Ok(Ok(item))
}

/// The run's terminal origin and whether these facts differ from it. The first
/// authoritative facts stay; later different ones never replace them.
fn settle_origin(
    tx: &Transaction<'_>,
    project: &ProjectId,
    session: &SessionId,
    actor: &ActorContext,
    completion: &RemoteRunCompletion,
    now: DateTime<Utc>,
) -> Result<(ObjectId, bool), StoreError> {
    let key = origin_key(project, completion)?;
    let facts = CanonicalObject::freeze(completion)?;
    let stored: Option<(String, Vec<u8>)> = tx
        .query_row(
            "SELECT request_hash, result_json FROM work_operation_results
             WHERE operation = ?1 AND idempotency_key = ?2",
            params![ORIGIN_OPERATION, key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    if let Some((hash, bytes)) = stored {
        let origin: ObjectId = serde_json::from_slice(&bytes)?;
        return Ok((origin, hash != facts.key().as_str()));
    }
    let object = CanonicalObject::mint(&RemoteTerminalOrigin {
        schema_version: SCHEMA_VERSION,
        project_id: project.clone(),
        session_id: session.clone(),
        completion: completion.clone(),
        actor: actor.clone(),
        recorded_at: now,
    })?;
    SqliteStore::insert_object(tx, REMOTE_TERMINAL_ORIGIN_KIND, &object)?;
    super::feeds::append_to_project_feed(tx, project, REMOTE_TERMINAL_ORIGIN_KIND, &object)?;
    super::persist_operation_result(tx, ORIGIN_OPERATION, &key, facts.key(), object.key())?;
    Ok((object.key().clone(), false))
}

/// The receipt of the same result already delivered to the same destination
/// for the same use, under any key: one result earns one credit there.
fn repeated_delivery(
    tx: &Transaction<'_>,
    session: &SessionId,
    request: &RemoteVerificationRequest,
    origin: &ObjectId,
) -> Result<Option<RemoteVerificationReceipt>, StoreError> {
    let run = request.association.binding.run_id.0.to_string();
    let mut statement = tx.prepare(
        "SELECT object_id FROM work_feed_entries
         WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND object_kind = ?2
         ORDER BY position",
    )?;
    let ids = statement
        .query_map(params![run, REMOTE_VERIFICATION_ASSOCIATION_KIND], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for id in ids {
        let id = ObjectId::from_stored(id).ok_or_else(|| {
            StoreError::InvalidWorkProjection("a run feed names an invalid record id".into())
        })?;
        let record: RemoteVerificationAssociationRecord =
            super::feeds::load_typed_work_object(tx, &id, REMOTE_VERIFICATION_ASSOCIATION_KIND)?;
        if record.origin.as_ref() != Some(origin)
            || record.session_id != *session
            || record.request.completion != request.completion
            || record.request.association != request.association
        {
            continue;
        }
        let mut statement = tx.prepare(
            "SELECT evidence_id FROM work_run_evidence
             WHERE run_id = ?1 AND producer_observation_id = ?2",
        )?;
        let verifications = statement
            .query_map(params![run, id.as_str()], |row| row.get::<_, String>(0))?
            .map(|record_id| {
                ObjectId::from_stored(record_id?)
                    .map(|record_id| RegisteredVerificationRecord {
                        stage: remote_stage(record.request.completion.mode).into(),
                        record_id,
                    })
                    .ok_or_else(|| {
                        StoreError::InvalidWorkProjection(
                            "a run evidence row names an invalid record id".into(),
                        )
                    })
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        return Ok(Some(RemoteVerificationReceipt {
            operation: id,
            origin: record.origin,
            verifications,
            qualification: record.qualification,
        }));
    }
    Ok(None)
}

/// The facts the one matcher consumes. The execution time is the run's own
/// finish, never the later local association.
pub(super) fn producer_facts_on(
    connection: &Connection,
    id: &ObjectId,
) -> Result<VerificationProducerFacts, StoreError> {
    let record: RemoteVerificationAssociationRecord =
        super::feeds::load_typed_work_object(connection, id, REMOTE_VERIFICATION_ASSOCIATION_KIND)?;
    producer_facts(&record, id)
}

pub(in crate::storage::work) fn producer_facts(
    record: &RemoteVerificationAssociationRecord,
    id: &ObjectId,
) -> Result<VerificationProducerFacts, StoreError> {
    let completion = &record.request.completion;
    if !record.qualification.credited || record.origin.is_none() {
        return Err(StoreError::InvalidWorkProjection(format!(
            "remote delivery {id} was not credited, so it produces no verification"
        )));
    }
    let binding = record.request.association.binding.clone();
    Ok(VerificationProducerFacts {
        project_id: record.project_id.clone(),
        binding,
        session_id: record.session_id.clone(),
        action_fingerprint: check_fingerprint(completion)?,
        outcome: ExecutionOutcome::Succeeded,
        run_outcome: Some(ExecutionOutcome::Succeeded),
        source_basis: Some(source_basis(&record.request)?),
        observed_at: completion.finished_at,
        actor: record.actor.clone(),
        recorded_at: record.associated_at,
    })
}

/// Whether a stored origin is what its operation could have written, with the
/// row that keeps its run to one terminal.
pub(super) fn origin_is_consistent_on(
    connection: &Connection,
    id: &ObjectId,
    origin: &RemoteTerminalOrigin,
) -> Result<bool, StoreError> {
    if origin.schema_version != SCHEMA_VERSION
        || origin.completion.schema_version != REMOTE_COMPLETION_SCHEMA_VERSION
        || !origin.completion.is_authoritative_terminal()
        || origin.actor.session_id.as_ref() != Some(&origin.session_id)
        || origin.completion.validate().is_err()
    {
        return Ok(false);
    }
    let key = origin_key(&origin.project_id, &origin.completion)?;
    let facts = CanonicalObject::freeze(&origin.completion)?;
    let stored: Option<(String, Vec<u8>)> = connection
        .query_row(
            "SELECT request_hash, result_json FROM work_operation_results
             WHERE operation = ?1 AND idempotency_key = ?2",
            params![ORIGIN_OPERATION, key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    Ok(stored.is_some_and(|(hash, bytes)| {
        hash == facts.key().as_str()
            && serde_json::from_slice::<ObjectId>(&bytes).is_ok_and(|stored| stored == *id)
    }))
}

/// Validates one origin row from its side: the origin it names must be that
/// run's origin, so a row cannot reserve a terminal it does not hold.
pub(super) fn verify_origin_row(
    connection: &Connection,
    key: &str,
    request_hash: &str,
    bytes: &[u8],
) -> Result<(), StoreError> {
    let id: ObjectId = serde_json::from_slice(bytes)?;
    let origin: RemoteTerminalOrigin =
        super::feeds::load_typed_work_object(connection, &id, REMOTE_TERMINAL_ORIGIN_KIND)?;
    if origin_key(&origin.project_id, &origin.completion)? != key
        || CanonicalObject::freeze(&origin.completion)?.key().as_str() != request_hash
    {
        return Err(StoreError::InvalidWorkProjection(
            "the row differs from the terminal origin it names".into(),
        ));
    }
    Ok(())
}

/// What is wrong with a stored delivery, if anything: the decision is
/// reconstructed from the stored request, except what depended on live
/// authority at delivery, which only the reason itself records. A credited
/// delivery must also have minted its one verification on its own run, and any
/// other delivery none: the receipt alone would not show a missing one, because
/// the doctor reads receipts and records separately.
pub(super) fn association_problem_on(
    connection: &Connection,
    id: &ObjectId,
    record: &RemoteVerificationAssociationRecord,
) -> Result<Option<&'static str>, StoreError> {
    let request = &record.request;
    let completion = &request.completion;
    let supported = completion.schema_version == REMOTE_COMPLETION_SCHEMA_VERSION;
    if record.schema_version != SCHEMA_VERSION
        || record.actor.session_id.as_ref() != Some(&record.session_id)
        || request.validate().is_err()
    {
        return Ok(Some("malformed_delivery"));
    }
    let qualification = &record.qualification;
    let reason = qualification.reason;
    if qualification.credited != reason.is_none()
        || (reason == Some(RemoteVerificationReason::ProducerUnsupported)) == supported
    {
        return Ok(Some("decision_differs"));
    }
    // Earlier deliveries left a remote finish ahead of the local clock
    // provisional. Retain that stored interpretation without gating new credit.
    let current_origin = supported && completion.is_authoritative_terminal();
    let historical_origin = supported && completion.settles_terminal_at(record.associated_at);
    if record.origin.is_some() != current_origin && record.origin.is_some() != historical_origin {
        return Ok(Some("origin_differs"));
    }
    let conflicting = match &record.origin {
        None => false,
        Some(origin) => {
            let origin: RemoteTerminalOrigin = match super::feeds::load_typed_work_object(
                connection,
                origin,
                REMOTE_TERMINAL_ORIGIN_KIND,
            ) {
                Ok(origin) => origin,
                Err(
                    StoreError::InvalidWorkProjection(_) | StoreError::ObjectKindMismatch { .. },
                ) => {
                    return Ok(Some("origin_differs"));
                }
                Err(error) => return Err(error),
            };
            if origin.project_id != record.project_id
                || origin_key(&origin.project_id, &origin.completion)?
                    != origin_key(&record.project_id, completion)?
            {
                return Ok(Some("origin_differs"));
            }
            origin.completion != *completion
        }
    };
    let decided = crate::control::remote_result_disqualification(
        completion,
        &request.association.intended_use,
        record.associated_at,
    );
    let historical = crate::control::historical_remote_result_disqualification(
        completion,
        &request.association.intended_use,
        record.associated_at,
    );
    let problem = match reason {
        // These came first, so the delivery may hold any native facts.
        Some(
            RemoteVerificationReason::ProducerUnsupported
            | RemoteVerificationReason::AssociationStale,
        ) => None,
        Some(RemoteVerificationReason::ResultConflict) => {
            (!conflicting).then_some("conflict_without_difference")
        }
        Some(reason) => (conflicting
            || (decided.is_none_or(|(decided, _)| decided != reason)
                && historical != Some(reason)))
        .then_some("decision_differs"),
        None => (conflicting || decided.is_some()).then_some("decision_differs"),
    };
    if problem.is_some() {
        return Ok(problem);
    }
    let minted: i64 = connection.query_row(
        "SELECT COUNT(*) FROM work_run_evidence
         WHERE run_id = ?1 AND producer_observation_id = ?2",
        params![
            request.association.binding.run_id.0.to_string(),
            id.as_str()
        ],
        |row| row.get(0),
    )?;
    Ok((minted != i64::from(qualification.credited)).then_some("verification_count_differs"))
}
