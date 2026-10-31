//! The registered run and its verifications enter the existing feeds atomically.

use crate::CanonicalObject;
use crate::domain::{
    ExecutionSourceBasis, NamedRootBindingKind, RegisteredHostRun, RegisteredVerificationInput,
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
    let source_basis = ExecutionSourceBasis {
        workspace_id: registration.root.workspace_id.clone(),
        source_revision: request.input.revision(),
        git_tree: match &request.input {
            RegisteredVerificationInput::Commit { .. } => None,
            RegisteredVerificationInput::StagedTree { tree, .. } => Some(tree.clone()),
        },
        source_root_generation: Some(registration.root.generation),
        source_root_state: Some(SourceRootState::Named),
    };
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
    let item = super::query::load_work_item(tx, binding.work_id)?;
    if item.project_id != *project {
        return Err(refused(
            "project_id",
            "registration belongs to another project",
        ));
    }
    if item.revision != binding.work_revision {
        return Err(refused("work_revision", "work changed since registration"));
    }
    super::planning::validate_control_work_binding_on(tx, project, session, binding, now).map_err(
        |error| match error {
            StoreError::ControlWorkBindingStale { .. } => {
                refused("claim", "claim, fence or run moved since registration")
            }
            other => other,
        },
    )?;
    let root = super::latest_named_root_event_on(tx, binding.run_id, binding.claim_id, i64::MAX)?
        .map(|(_, event)| event)
        .ok_or_else(|| refused("root", "registration has no named root"))?;
    if root.kind != NamedRootBindingKind::Bound
        || root.workspace_id != registration.root.workspace_id
        || root.generation != registration.root.generation
        || root.claim_fence != binding.claim_fence
        || root.session_id != *session
        || root.recorded_at > registration.registered_at
    {
        return Err(refused("root", "named root moved since registration"));
    }
    Ok(item)
}

pub(super) fn validate_request(
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
        || request
            .post_run
            .disposition
            .as_ref()
            .is_some_and(|text| !label(text, 2048))
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
        RegisteredVerificationInput::Commit { commit } => commit,
        RegisteredVerificationInput::StagedTree { base_commit, .. } => base_commit,
    };
    if commit.len() != 40
        || !commit
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
    {
        return Err(refused("input", "a full lowercase commit id is required"));
    }
    if !request.post_run.clean_before
        || !request.post_run.clean_after
        || request.post_run.outcome != RegisteredVerificationInputOutcome::Clean
        || !request.post_run.dirty_paths.is_empty()
    {
        return Err(refused(
            "post_run",
            "only unchanged clean inputs are eligible",
        ));
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
