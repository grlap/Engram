//! The registered run and its verifications enter the existing feeds atomically.

use crate::CanonicalObject;
use crate::domain::{
    NamedRootBindingKind, RegisteredHostRun, RegisteredVerificationInput,
    RegisteredVerificationInputOutcome, RegisteredVerificationOutcome,
    RegisteredVerificationReceipt, RegisteredVerificationRecord, RegisteredVerificationRequest,
    SCHEMA_VERSION, SourceRootState, VerificationEvidence, VerificationResult,
};
use crate::storage::{SqliteStore, StoreError};
use crate::{ActorContext, ProjectId, SessionId};
use chrono::{DateTime, Utc};
use rusqlite::Transaction;

#[cfg(test)]
mod tests;
#[cfg(test)]
pub(super) use tests::registered_verification_case;

fn refused(field: &str, reason: &str) -> StoreError {
    StoreError::RegisteredVerificationRefused {
        field: field.into(),
        reason: reason.into(),
    }
}

pub(in crate::storage) fn record_registered_verification_on(
    tx: &Transaction<'_>,
    project: &ProjectId,
    session: &SessionId,
    actor: &ActorContext,
    request: &RegisteredVerificationRequest,
    now: DateTime<Utc>,
) -> Result<RegisteredVerificationReceipt, StoreError> {
    validate_request(request, now)?;
    write_registered_verification_on(tx, project, session, actor, request, now)
}

/// Seeds a stored producer for tests of how history keeps being read. Production
/// ingress always goes through [`record_registered_verification_on`].
#[cfg(test)]
pub(in crate::storage) fn seed_historical_registered_verification_on(
    tx: &Transaction<'_>,
    project: &ProjectId,
    session: &SessionId,
    actor: &ActorContext,
    request: &RegisteredVerificationRequest,
    now: DateTime<Utc>,
) -> Result<RegisteredVerificationReceipt, StoreError> {
    validate_stored_request(request, now)?;
    write_registered_verification_on(tx, project, session, actor, request, now)
}

fn write_registered_verification_on(
    tx: &Transaction<'_>,
    project: &ProjectId,
    session: &SessionId,
    actor: &ActorContext,
    request: &RegisteredVerificationRequest,
    now: DateTime<Utc>,
) -> Result<RegisteredVerificationReceipt, StoreError> {
    let item = validate_association(tx, project, session, request, now)?;
    let registration = &request.registration;
    let binding = &registration.binding;
    let mut actor = actor.clone();
    actor.run_id = Some(binding.run_id.0.to_string());
    let producer = RegisteredHostRun {
        schema_version: SCHEMA_VERSION,
        project_id: project.clone(),
        session_id: session.clone(),
        request: request.clone(),
        actor: actor.clone(),
        recorded_at: now,
    };
    let object = CanonicalObject::mint(&producer)?;
    SqliteStore::insert_object(tx, "registered_host_run", &object)?;
    super::feeds::append_to_work_feeds(
        tx,
        project,
        item.root_id,
        Some(binding.run_id),
        None,
        "registered_host_run",
        &object,
    )?;
    let source_basis = request.input.source_basis(&registration.root);
    let mut verifications = Vec::new();
    for check in &request.checks {
        let Some(completed_at) = check.completed_at else {
            continue;
        };
        let result = match check.outcome {
            RegisteredVerificationOutcome::Passed => VerificationResult::Passed,
            RegisteredVerificationOutcome::Failed => VerificationResult::Failed,
            RegisteredVerificationOutcome::Indeterminate => VerificationResult::Indeterminate,
            RegisteredVerificationOutcome::NotRun => continue,
        };
        let evidence = VerificationEvidence {
            applicability: None,
            schema_version: SCHEMA_VERSION,
            project_id: project.clone(),
            binding: binding.clone(),
            session_id: session.clone(),
            producer_observation: object.key().clone(),
            source_basis: source_basis.clone(),
            environment: None,
            check_kind: check.check_kind,
            check_fingerprint: check.check_fingerprint.clone(),
            result,
            completed_at,
            summary: check.summary.clone().unwrap_or_else(|| check.stage.clone()),
            refs: check.refs.clone(),
            actor: actor.clone(),
            recorded_at: now,
            bound_from: None,
        };
        let record_id = super::append_control_verification_evidence_on(tx, &evidence)?;
        verifications.push(RegisteredVerificationRecord {
            stage: check.stage.clone(),
            record_id,
        });
    }
    Ok(RegisteredVerificationReceipt {
        operation: object.key().clone(),
        producer: object.key().clone(),
        verifications,
        run_outcome: request.run.outcome,
    })
}

fn validate_association(
    tx: &Transaction<'_>,
    project: &ProjectId,
    session: &SessionId,
    request: &RegisteredVerificationRequest,
    now: DateTime<Utc>,
) -> Result<crate::domain::WorkItem, StoreError> {
    let registration = &request.registration;
    if registration.initiating_session_id != *session {
        return Err(refused(
            "initiating_session_id",
            "registration belongs to another session",
        ));
    }
    let binding = &registration.binding;
    let item = match super::query::load_work_item(tx, binding.work_id) {
        Err(StoreError::WorkNotFound(_)) => {
            return Err(refused(
                "claim",
                "the registered work does not exist in this store",
            ));
        }
        loaded => loaded?,
    };
    if item.project_id != *project {
        return Err(refused(
            "project_id",
            "registration belongs to another project",
        ));
    }
    // The registered binding was this session's own claim when it was taken.
    if !super::planning::control_work_binding_was_valid_on(tx, project, session, binding)? {
        return Err(refused(
            "claim",
            "the registered claim and fence were never held by this session",
        ));
    }
    validate_claim_lineage(tx, session, binding)?;
    if item.lifecycle != crate::domain::WorkLifecycle::Open
        || item.active_run_id != Some(binding.run_id)
    {
        return Err(refused(
            "claim",
            "the work or its run ended since registration",
        ));
    }
    // The request time is taken before the write lock, so a parent that
    // completed meanwhile is found here whatever that time says.
    let run = super::query::load_work_run(tx, binding.run_id)?;
    if !matches!(
        run.state,
        crate::domain::WorkRunState::Claimed | crate::domain::WorkRunState::Active
    ) || !super::completion::run_uses_active_root_execution_in_scope(tx, &item, &run, None)?
    {
        return Err(refused(
            "claim",
            "the work or its run ended since registration",
        ));
    }
    if !super::completion::ancestors_admit_execution(tx, &item)? {
        return Err(refused(
            "claim",
            "a closed ancestor keeps the claim from being recovered",
        ));
    }
    validate_root_lineage(tx, registration)?;
    // A lost claim, closed ancestor or moved root is named before an edit
    // that may have come with it.
    if item.revision != binding.work_revision {
        return Err(refused("work_revision", "work changed since registration"));
    }
    let live = super::query::load_work_claim_optional(tx, binding.run_id)?.is_some_and(|claim| {
        claim.claim_id == binding.claim_id
            && claim.holder == *session
            && claim.state == crate::domain::WorkClaimState::Active
            && claim.expires_at > now
    });
    if !live {
        return Err(refused(
            "claim_live",
            "the initiating session's claim has lapsed; send the record again once it claims the work again",
        ));
    }
    // Holder writes wait while a handoff offer is open; acceptance ends the
    // binding, and a cancelled or expired offer leaves it.
    if super::execution::live_handoff_on(tx, binding.run_id, now)?.is_some() {
        return Err(refused(
            "claim_live",
            "a handoff offer is open; send the record again once it is cancelled or expires",
        ));
    }
    Ok(item)
}

/// The claim from the registered fence on: renewals, and recoveries by the
/// initiating session at a higher fence, continue the binding. A later epoch
/// that another session's claim or an accepted handoff started ends it, and
/// so does a release at or after the registered fence. Only those three
/// transitions move a claim between epochs: a renewal keeps holder and fence,
/// and the checks after this one refuse a lifecycle or run change.
fn validate_claim_lineage(
    tx: &Transaction<'_>,
    session: &SessionId,
    binding: &crate::domain::ControlWorkBinding,
) -> Result<(), StoreError> {
    use crate::domain::WorkTransition;
    for event in claim_transitions_on(tx, binding.run_id)? {
        // A claim or an accepted handoff starts the epoch at its fence, held
        // by its holder. A release records the fence it advanced to, so one
        // at or after the registered fence ended the registered epoch or a
        // later one.
        let started = match &event.transition {
            WorkTransition::Claimed { claim, .. } => Some((claim.fence, &claim.holder)),
            WorkTransition::HandedOff { fence, to, .. } => Some((*fence, to)),
            _ => None,
        };
        let ended = match (&event.transition, started) {
            (_, Some((fence, holder))) => fence > binding.claim_fence && holder != session,
            (WorkTransition::Released { fence, .. }, None) => *fence >= binding.claim_fence,
            _ => false,
        };
        if ended {
            return Err(refused(
                "claim",
                "the claim was released, handed off or taken by another session since registration",
            ));
        }
    }
    Ok(())
}

/// The claim's named root: the registration names the root bound when the run
/// was registered, and it must still be that naming. Checks credit only under
/// the generation their record names, so a root that has since ended or been
/// named again, which always takes a later generation, ends the binding.
fn validate_root_lineage(
    tx: &Transaction<'_>,
    registration: &crate::domain::RegisteredVerificationAssociation,
) -> Result<(), StoreError> {
    let binding = &registration.binding;
    // The newest event since the latest release: a release ends every
    // binding recorded before it.
    let (_, _, root) =
        super::feeds::latest_named_root_binding_on(tx, binding.run_id, binding.claim_id, i64::MAX)?
            .ok_or_else(|| refused("root", "registration has no named root"))?;
    // The host states registration time in milliseconds, so a root recorded
    // within that millisecond counts as there when the run was registered.
    if root.recorded_at.timestamp_millis() > registration.registered_at.timestamp_millis() {
        return Err(refused(
            "root",
            "the root ended or was named again since registration",
        ));
    }
    if root.kind != NamedRootBindingKind::Bound
        || root.workspace_id != registration.root.workspace_id
        || root.generation != registration.root.generation
    {
        return Err(refused("root", "named root moved since registration"));
    }
    Ok(())
}

/// The run's claim, release and handoff events, oldest first.
fn claim_transitions_on(
    tx: &Transaction<'_>,
    run_id: crate::domain::WorkRunId,
) -> Result<Vec<crate::domain::WorkEvent>, StoreError> {
    let mut statement = tx.prepare(
        "SELECT entry.object_id, object.canonical_json
         FROM work_feed_entries entry
         JOIN objects object ON object.object_id = entry.object_id
         WHERE entry.feed_kind = 'run_execution' AND entry.feed_id = ?1
           AND entry.object_kind = 'work_event'
           AND json_extract(object.canonical_json, '$.transition.kind')
               IN ('claimed', 'released', 'handed_off')
         ORDER BY entry.position",
    )?;
    let rows = statement.query_map(rusqlite::params![run_id.0.to_string()], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;
    rows.map(|row| {
        let (stored, bytes) = row?;
        let id = crate::ObjectId::from_stored(stored.clone())
            .ok_or(StoreError::InvalidStoredKey(stored))?;
        CanonicalObject::stored(&id, bytes)?.decode()
    })
    .collect()
}

pub(super) fn validate_request(
    request: &RegisteredVerificationRequest,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    validate_stored_request(request, now)?;
    match &request.input {
        RegisteredVerificationInput::SourceId { source_id } => source_id
            .validate()
            .map_err(|_| refused("input.source_id", "invalid shared source identity")),
        RegisteredVerificationInput::StagedTree { .. } => {
            Err(refused("input.kind", "the staged_tree input is retired"))
        }
        RegisteredVerificationInput::Commit { .. } => Err(refused(
            "input.kind",
            "the commit input is retired; report source_id",
        )),
    }
}

/// Stored producer facts retain their original admission rules. In particular,
/// retiring a producer later must not apply its new-write policy to history.
pub(super) fn validate_stored_request(
    request: &RegisteredVerificationRequest,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    let registration = &request.registration;
    let label = |text: &str, limit: usize| {
        !text.trim().is_empty()
            && text.trim() == text
            && text.len() <= limit
            && !crate::domain::holds_refused_source_text(text)
    };
    if registration
        .owning_session_id
        .as_ref()
        .is_some_and(|text| !label(text, 512))
        || matches!(&request.post_run,
            crate::domain::RegisteredVerificationPostRun::Legacy(post)
                if post.disposition.as_ref().is_some_and(|text| !label(text, 2048)))
    {
        return Err(refused(
            "registration",
            "optional attribution and disposition must be bounded safe labels",
        ));
    }
    if !registration.host_run_id.starts_with("test-")
        || !label(&registration.host_run_id, 128)
        || !label(&registration.request_identity, 128)
        || request.operation_key != format!("termal-run:{}", registration.host_run_id)
    {
        return Err(refused(
            "registration",
            "a durable registered run and its fixed operation key are required",
        ));
    }
    if registration.root.state != SourceRootState::Named
        || registration.root.generation <= 0
        || !label(&registration.root.workspace_id, 512)
    {
        return Err(refused(
            "root",
            "a named workspace and positive generation are required",
        ));
    }
    let commit = match &request.input {
        RegisteredVerificationInput::Commit { commit } => Some(commit),
        RegisteredVerificationInput::StagedTree { base_commit, .. } => Some(base_commit),
        RegisteredVerificationInput::SourceId { .. } => None,
    };
    if commit.is_some_and(|commit| {
        commit.len() != 40
            || !commit
                .bytes()
                .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    }) {
        return Err(refused("input", "a full lowercase commit id is required"));
    }
    match (&request.input, &request.post_run) {
        (
            RegisteredVerificationInput::SourceId { source_id: before },
            crate::domain::RegisteredVerificationPostRun::SourceId { source_id: after },
        ) => match before.compare(after) {
            Some(true) => {}
            Some(false) => {
                return Err(refused(
                    "post_run.source_id",
                    "shared source identity changed during the run",
                ));
            }
            None => {
                return Err(refused(
                    "post_run.source_id",
                    "shared source identity formats are incomparable",
                ));
            }
        },
        (
            RegisteredVerificationInput::Commit { .. }
            | RegisteredVerificationInput::StagedTree { .. },
            crate::domain::RegisteredVerificationPostRun::Legacy(post),
        ) => {
            if !post.clean_before
                || !post.clean_after
                || post.outcome != RegisteredVerificationInputOutcome::Clean
                || !post.dirty_paths.is_empty()
            {
                return Err(refused(
                    "post_run",
                    "only unchanged clean inputs are eligible",
                ));
            }
        }
        _ => {
            return Err(refused(
                "post_run",
                "post-run source representation must match input.kind",
            ));
        }
    }
    if registration.registered_at > request.run.finished_at
        || request.run.started_at > request.run.finished_at
        || request.run.finished_at > now
        || request.run.outcome == RegisteredVerificationOutcome::NotRun
        || !matches!(
            request.run.mode.as_str(),
            "full" | "focused" | "live" | "category"
        )
        || !label(&request.run.command, 8192)
        || request.checks.len() > 64
        || request.checks.is_empty()
    {
        return Err(refused(
            "run",
            "terminal run facts and timestamps are invalid",
        ));
    }
    let mut names = std::collections::HashSet::new();
    for check in &request.checks {
        if !label(&check.stage, 128)
            || !names.insert(&check.stage)
            || !label(&check.command, 8192)
            || check.started_at.is_some_and(|at| {
                at < request.run.started_at
                    || at < registration.registered_at
                    || at > request.run.finished_at
            })
            || check.completed_at.is_some_and(|at| {
                at > request.run.finished_at || check.started_at.is_none_or(|start| at < start)
            })
            || (matches!(
                check.outcome,
                RegisteredVerificationOutcome::Passed | RegisteredVerificationOutcome::Failed
            ) && check.completed_at.is_none())
            || (check.outcome == RegisteredVerificationOutcome::NotRun
                && (check.started_at.is_some() || check.completed_at.is_some()))
            || check
                .summary
                .as_ref()
                .is_some_and(|text| !label(text, 4096))
            || crate::storage::control_runtime::validate_typed_evidence_refs(&check.refs).is_err()
            || check
                .refs
                .iter()
                .any(|text| crate::domain::holds_refused_source_text(text))
            || check.counts.as_ref().is_some_and(|counts| {
                counts.len() > 32 || counts.keys().any(|text| !label(text, 128))
            })
        {
            return Err(refused(
                "checks",
                "stage identity, result or timestamps are invalid",
            ));
        }
    }
    if request.run.outcome == RegisteredVerificationOutcome::Passed
        && request
            .checks
            .iter()
            .any(|check| check.outcome != RegisteredVerificationOutcome::Passed)
    {
        return Err(refused(
            "run.outcome",
            "a passed run cannot contain incomplete or failed stages",
        ));
    }
    Ok(())
}
