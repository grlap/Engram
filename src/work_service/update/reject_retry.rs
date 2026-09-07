//! Keyless rejection retries identify intent independently of their own effects.
//! Committed results prove the atomic cancellation and waiver; replay never
//! treats a changed child as the still-current cancelled result.

use super::{
    CanonicalObject, LocalWorkService, ObjectHash, ProjectId, REJECT_PROTOCOL_OPERATION, Serialize,
    SessionId, SqliteStore, StoreError, WorkId, WorkLifecycle, WorkProtocolBasis,
};

#[derive(Serialize)]
struct RejectionKey<'a> {
    project_id: &'a ProjectId,
    session_id: &'a SessionId,
    protocol_operation: &'static str,
    child_id: WorkId,
    intent: &'a ObjectHash,
}

impl LocalWorkService {
    pub(in crate::work_service) fn rejection_idempotency_key(
        &self,
        basis: &WorkProtocolBasis,
        intent: &ObjectHash,
    ) -> Result<String, StoreError> {
        let child = basis.focused_work.as_ref().ok_or_else(|| {
            StoreError::InvalidWorkProjection("rejection has no bound child".into())
        })?;
        let key = CanonicalObject::freeze(&RejectionKey {
            project_id: &self.project_id,
            session_id: &self.session_id,
            protocol_operation: REJECT_PROTOCOL_OPERATION,
            child_id: child.work_id,
            intent,
        })?;
        Ok(format!("auto:{}", key.hash().as_str()))
    }
}

pub(super) fn guard_committed(
    store: &SqliteStore,
    basis: &WorkProtocolBasis,
    value: &serde_json::Value,
) -> Result<(), StoreError> {
    let committed: crate::RejectRequiredChildReceipt = serde_json::from_value(value.clone())?;
    if committed.child.lifecycle != WorkLifecycle::Cancelled
        || committed.waiver.work_id != committed.child.work_id
        || committed.waiver.work_revision != committed.child.revision
        || committed.child.parent_id.is_none()
    {
        return Err(StoreError::InvalidWorkProjection(
            "committed rejection has inconsistent cancellation and waiver".into(),
        ));
    }
    if basis.focused_work.as_ref() != Some(&committed.child) {
        return Err(refusal(
            store,
            basis,
            "the child changed after the original rejection committed",
        ));
    }
    Ok(())
}

pub(super) fn refusal(
    store: &SqliteStore,
    basis: &WorkProtocolBasis,
    reason: &'static str,
) -> StoreError {
    refusal_with_remedy(store, basis, reason, |child, parent| {
        let parent_navigation = parent.map_or_else(String::new, |parent| {
            format!(" and engram work show {parent}")
        });
        format!(
            "inspect the child and any recorded rejection with engram work show {child}{parent_navigation}; do not repeat cancellation or waiver blindly"
        )
    })
}

pub(super) fn pending_refusal(
    store: &SqliteStore,
    basis: &WorkProtocolBasis,
    recorded_basis: &WorkProtocolBasis,
) -> StoreError {
    if recorded_basis.focused_work == basis.focused_work {
        return refusal_with_remedy(
            store,
            basis,
            "the recorded claim or execution basis changed; the child is unchanged",
            |child, _| {
                format!(
                    "inspect current execution state with engram work show {child}; a rejection with a new intent (different reason text or an explicit key) is admissible under current authority checks; the original pending attempt is not refreshed"
                )
            },
        );
    }
    refusal_with_remedy(
        store,
        basis,
        "the child changed since the original rejection attempt",
        |child, parent| {
            let waiver = parent.map_or_else(String::new, |parent| format!("; if still required and {parent} is open and waivable, use engram work update {parent} --waive {child} --reason \"why\""));
            format!(
                "inspect current child state with engram work show {child}; if cancellation is admitted, use engram work update {child} --cancel \"why\"{waiver}"
            )
        },
    )
}

fn refusal_with_remedy(
    store: &SqliteStore,
    basis: &WorkProtocolBasis,
    reason: &'static str,
    remedy: impl FnOnce(&str, Option<&str>) -> String,
) -> StoreError {
    let Some(child) = basis.focused_work.as_ref() else {
        return StoreError::InvalidWorkProjection("rejection retry has no bound child".into());
    };
    let parent_ref = match child
        .parent_id
        .map(|id| store.get_work_item(id))
        .transpose()
    {
        Ok(parent) => parent.map(|parent| parent.short_ref),
        Err(error) => return error,
    };
    let remedy = remedy(&child.short_ref, parent_ref.as_deref()).into_boxed_str();
    StoreError::WorkRejectRefused {
        child_ref: child.short_ref.clone(),
        parent_ref,
        reason,
        remedy,
    }
}
