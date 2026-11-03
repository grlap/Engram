//! Work-session source-root persistence and shared host transactions.

use super::{
    ActorContext, CONTROL_SCHEMA_VERSION, CanonicalObject, Connection, ControlWorkBinding,
    DateTime, NamedRootBindingEvent, NamedRootBindingFingerprint, NamedRootBindingReceipt,
    ObjectId, OptionalExtension, SCHEMA_VERSION, SessionId, SqliteStore, StoreError, Transaction,
    Utc, params, work,
};
use crate::storage::{MAX_TYPED_EVIDENCE_REF_BYTES, MAX_TYPED_EVIDENCE_REFS};

pub(crate) mod request_budget;
mod transaction;
pub(super) use transaction::control_lifecycle_transaction;
#[cfg(test)]
pub(in crate::storage) use transaction::control_transaction;

impl SqliteStore {
    /// Records the host's explicit source-root selection for the exact live
    /// claim on this work session. Agent-facing work operations do not
    /// expose this event.
    #[allow(
        clippy::too_many_arguments,
        reason = "the host binding keeps routing, claim, source and retry inputs explicit"
    )]
    pub(crate) fn bind_named_root(
        &mut self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        connection_token: &str,
        authority: super::HostSessionAuthority,
        claim_id: crate::domain::WorkClaimId,
        claim_fence: i64,
        workspace_id: &str,
        generation: i64,
        named_at: DateTime<Utc>,
        kind: super::NamedRootBindingKind,
        end_reason: Option<super::NamedRootEndReason>,
        actor: &mut ActorContext,
        idempotency_key: &str,
        now: DateTime<Utc>,
    ) -> Result<NamedRootBindingReceipt, StoreError> {
        if workspace_id.trim().is_empty()
            || workspace_id.trim() != workspace_id
            || workspace_id.len() > 512
            || generation <= 0
            || claim_fence <= 0
            || named_at > now
            || matches!(kind, super::NamedRootBindingKind::Bound) != end_reason.is_none()
        {
            return Err(StoreError::NamedRootBindingRefused(
                "named-root binding requires a trimmed workspace (at most 512 bytes), positive generation and fence, a nonfuture named_at, and a reason exactly when ended".into(),
            ));
        }
        let intent = CanonicalObject::freeze(&NamedRootBindingFingerprint {
            control_schema_version: CONTROL_SCHEMA_VERSION,
            session_id,
            claim_id: &claim_id,
            claim_fence,
            workspace_id,
            generation,
            named_at,
            kind,
            end_reason,
            idempotency_key,
        })?;
        let work_schema_version = self.work_schema_version;
        control_lifecycle_transaction(&mut self.connection, |transaction| {
            work::require_work_schema_version(transaction, work_schema_version)?;
            authority.verify_on(transaction, project_id, session_id, connection_token)?;
            if let Some(replay) = Self::replay_host_operation::<NamedRootBindingReceipt>(
                transaction,
                project_id,
                session_id,
                "named_root_bind",
                idempotency_key,
                intent.key(),
            )? {
                let event: NamedRootBindingEvent =
                    work::load_typed_work_object(transaction, &replay.event, "named_root_binding")?;
                if event.project_id != *project_id || event.session_id != *session_id {
                    return Err(StoreError::NamedRootBindingRefused(
                        "named-root retry belongs to another project or session".into(),
                    ));
                }
                return Ok(replay);
            }
            // Only a new name carries new host text: ending a root repeats the
            // stored bound workspace byte for byte, so a root named before this
            // rule can still be ended.
            // The host treats this endpoint's own refusal code as definitive, so
            // the text rule keeps it.
            if kind == super::NamedRootBindingKind::Bound
                && crate::domain::holds_refused_source_text(workspace_id)
            {
                return Err(StoreError::NamedRootBindingRefused(
                    "workspace_id holds a control or bidirectional formatting character".into(),
                ));
            }
            if actor.session_id.as_ref() != Some(session_id) {
                return Err(StoreError::NamedRootBindingRefused(
                    "named-root binding requires an active host session".into(),
                ));
            }
            let prior_bound = if kind == super::NamedRootBindingKind::Ended {
                let stored: Option<(String, Vec<u8>)> = transaction
                    .query_row(
                        "SELECT object_id, canonical_json FROM objects
                         WHERE object_kind = 'named_root_binding'
                           AND json_extract(canonical_json, '$.claim_id') = ?1
                           AND json_extract(canonical_json, '$.generation') = ?2
                           AND json_extract(canonical_json, '$.kind') = 'bound'",
                        params![claim_id.0.to_string(), generation],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;
                let (stored_id, bytes) = stored.ok_or_else(|| {
                    StoreError::NamedRootBindingRefused(
                        "named-root end has no recorded bound generation".into(),
                    )
                })?;
                let id = ObjectId::from_stored(stored_id.clone())
                    .ok_or(StoreError::InvalidStoredKey(stored_id))?;
                Some(CanonicalObject::stored(&id, bytes)?.decode::<NamedRootBindingEvent>()?)
            } else {
                None
            };
            let (work_id, run_id, root_execution_id) = if let Some(bound) = &prior_bound {
                if bound.project_id != *project_id
                    || bound.workspace_id != workspace_id
                    || bound.named_at != named_at
                    || (end_reason == Some(super::NamedRootEndReason::ExplicitClear)
                        && bound.session_id != *session_id)
                {
                    return Err(StoreError::NamedRootBindingRefused(
                        "named-root end must repeat its bound generation's project, workspace and named_at, and an explicit clear must come from the naming session".into(),
                    ));
                }
                (bound.work_id, bound.run_id, bound.root_execution_id)
            } else {
                let stored_run: Option<String> = transaction
                    .query_row(
                        "SELECT run_id FROM work_claims WHERE claim_id = ?1",
                        [claim_id.0.to_string()],
                        |row| row.get(0),
                    )
                    .optional()?;
                let run_id = stored_run
                    .and_then(|value| uuid::Uuid::parse_str(&value).ok())
                    .map(crate::domain::WorkRunId)
                    .ok_or_else(|| {
                        StoreError::NamedRootBindingRefused(
                            "named-root claim is not current in this store".into(),
                        )
                    })?;
                let claim =
                    work::load_work_claim_optional(transaction, run_id)?.ok_or_else(|| {
                        StoreError::NamedRootBindingRefused(
                            "named-root claim has no canonical state".into(),
                        )
                    })?;
                if claim.claim_id != claim_id
                    || claim.state != crate::domain::WorkClaimState::Active
                    || claim.expires_at <= now
                {
                    return Err(StoreError::NamedRootBindingRefused(
                        "named-root claim is not active".into(),
                    ));
                }
                let item = work::load_work_item(transaction, claim.work_id)?;
                let run = work::load_work_run(transaction, run_id)?;
                if item.project_id != *project_id || run.work_id != item.work_id {
                    return Err(StoreError::InvalidWorkProjection(
                        "named-root claim crosses its project or run".into(),
                    ));
                }
                if matches!(authority, super::HostSessionAuthority::Work) {
                    work::validate_control_work_binding_on(
                        transaction,
                        project_id,
                        session_id,
                        &ControlWorkBinding {
                            root_execution_id: run.root_execution_id,
                            work_id: item.work_id,
                            run_id,
                            work_revision: item.revision,
                            claim_id,
                            claim_fence,
                        },
                        now,
                    )
                    .map_err(|error| match error {
                        StoreError::ControlWorkBindingStale { .. } => {
                            StoreError::NamedRootBindingRefused(error.to_string())
                        }
                        other => other,
                    })?;
                }
                (item.work_id, run_id, run.root_execution_id)
            };
            let previous =
                work::latest_named_root_event_on(transaction, run_id, claim_id, i64::MAX)?
                    .map(|(_, event)| event);
            let sequence_valid = match (kind, previous.as_ref()) {
                (super::NamedRootBindingKind::Bound, None) => true,
                (super::NamedRootBindingKind::Bound, Some(previous)) => {
                    generation > previous.generation
                }
                (super::NamedRootBindingKind::Ended, Some(previous)) => {
                    previous.kind == super::NamedRootBindingKind::Bound
                        && previous.generation == generation
                }
                (super::NamedRootBindingKind::Ended, None) => false,
            };
            if !sequence_valid {
                return Err(StoreError::NamedRootBindingRefused(
                    "named-root generation or lifecycle transition is stale for the claim".into(),
                ));
            }
            actor.run_id = Some(run_id.0.to_string());
            let event = NamedRootBindingEvent {
                schema_version: SCHEMA_VERSION,
                project_id: project_id.clone(),
                root_execution_id,
                work_id,
                run_id,
                claim_id,
                claim_fence,
                session_id: session_id.clone(),
                workspace_id: workspace_id.to_owned(),
                generation,
                named_at,
                kind,
                end_reason,
                actor: actor.clone(),
                recorded_at: now,
            };
            let (event_id, position) = work::append_named_root_binding_on(transaction, &event)?;
            let receipt = NamedRootBindingReceipt {
                event: event_id,
                position,
                workspace_id: workspace_id.to_owned(),
                generation,
                kind,
            };
            authority.persist(
                transaction,
                project_id,
                session_id,
                "named_root_bind",
                idempotency_key,
                &intent,
                &receipt,
                now,
            )?;
            Ok(receipt)
        })
    }
}

pub(super) fn validate_typed_evidence_refs(refs: &[String]) -> Result<(), StoreError> {
    if refs.len() > MAX_TYPED_EVIDENCE_REFS {
        return Err(StoreError::InvalidWork(format!(
            "typed evidence accepts at most {MAX_TYPED_EVIDENCE_REFS} references"
        )));
    }
    if refs.iter().any(|reference| {
        let trimmed = reference.trim();
        trimmed.is_empty() || trimmed != reference || reference.len() > MAX_TYPED_EVIDENCE_REF_BYTES
    }) {
        return Err(StoreError::InvalidWork(format!(
            "typed evidence references must be trimmed, nonempty, and at most {MAX_TYPED_EVIDENCE_REF_BYTES} bytes"
        )));
    }
    Ok(())
}
