use super::{
    CanonicalObject, DateTime, LocalWorkService, ObjectHash, ProjectId, Serialize, SessionId,
    StoreError, Utc, WorkDecomposition, WorkId, WorkProtocolBasis,
};

#[derive(Serialize)]
struct DecompositionKey<'a> {
    project_id: &'a ProjectId,
    session_id: &'a SessionId,
    protocol_operation: &'static str,
    parent_id: WorkId,
    intent: &'a ObjectHash,
}

impl LocalWorkService {
    pub(in crate::work_service) fn refresh_decomposition_retry_basis(
        &self,
        store: &mut crate::storage::SqliteStore,
        key: &str,
        stored: &serde_json::Value,
        current: &WorkProtocolBasis,
    ) -> Result<(), StoreError> {
        store
            .refresh_pending_work_protocol_attempt_basis(
                &self.project_id,
                &self.session_id,
                crate::storage::DECOMPOSE_PROTOCOL_OPERATION,
                key,
                stored,
                current,
            )
            .map_err(|error| match error {
                StoreError::WorkOperationIdempotencyConflict { .. } => retry_conflict(
                    current,
                    "the original attempt changed or completed concurrently",
                ),
                other => other,
            })
    }

    pub(in crate::work_service) fn decomposition_idempotency_key(
        &self,
        basis: &WorkProtocolBasis,
        intent: &ObjectHash,
    ) -> Result<String, StoreError> {
        let parent = basis.focused_work.as_ref().ok_or_else(|| {
            StoreError::InvalidWorkProjection("decomposition has no parent".into())
        })?;
        let key = CanonicalObject::freeze(&DecompositionKey {
            project_id: &self.project_id,
            session_id: &self.session_id,
            protocol_operation: crate::storage::DECOMPOSE_PROTOCOL_OPERATION,
            parent_id: parent.work_id,
            intent,
        })?;
        Ok(format!("auto:{}", key.hash().as_str()))
    }
}

pub(super) fn guard_decomposition_retry(
    stored_value: &serde_json::Value,
    current: &WorkProtocolBasis,
    core_result: Option<&serde_json::Value>,
) -> Result<(), StoreError> {
    let stored: WorkProtocolBasis = serde_json::from_value(stored_value.clone())?;
    let committed: Option<WorkDecomposition> = core_result
        .map(|value| serde_json::from_value(value.clone()))
        .transpose()?;
    if let Some(reason) = decomposition_basis_difference(&stored, current, committed.as_ref()) {
        return Err(retry_conflict(current, reason));
    }
    Ok(())
}

pub(super) fn retry_conflict(current: &WorkProtocolBasis, reason: &'static str) -> StoreError {
    match current.focused_work.as_ref() {
        Some(parent) => StoreError::WorkDecompositionRetryConflict {
            parent_ref: parent.short_ref.clone(),
            reason,
        },
        None => StoreError::InvalidWorkProjection("decomposition has no parent".into()),
    }
}

fn decomposition_basis_difference(
    stored: &WorkProtocolBasis,
    current: &WorkProtocolBasis,
    committed: Option<&WorkDecomposition>,
) -> Option<&'static str> {
    let mut stored = stored.retry_stable();
    let mut current = current.retry_stable();
    let (Some(before), Some(after)) = (&mut stored.focused_work, &mut current.focused_work) else {
        return Some("the parent binding changed since the first attempt");
    };
    if before.active_run_id.is_none()
        && after.active_run_id.is_some()
        && committed.is_some_and(|result| {
            result.parent.work_id == before.work_id
                && result.parent.project_id == before.project_id
                && result.parent.active_run_id == after.active_run_id
        })
    {
        // Only this exact scoped operation's committed result can prove that
        // decomposition itself bootstrapped the restored parent's native run.
        before.active_run_id = after.active_run_id;
    }
    if before.active_run_id != after.active_run_id {
        return Some("the parent run changed since the first attempt");
    }
    before.revision = 0;
    after.revision = 0;
    before.updated_at = DateTime::<Utc>::UNIX_EPOCH;
    after.updated_at = DateTime::<Utc>::UNIX_EPOCH;
    for basis in [&mut stored, &mut current] {
        if let Some(claim) = basis.claim.as_mut() {
            claim.accepted_work_revision = 0;
        }
    }
    if stored.claim != current.claim {
        Some("the parent claim, holder, or claim epoch changed since the first attempt")
    } else if stored != current {
        Some("the parent planning state changed since the first attempt")
    } else {
        None
    }
}
