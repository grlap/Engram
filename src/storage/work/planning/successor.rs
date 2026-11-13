//! Native transitions inside a retained successor's checked transaction.

use super::super::task_sync::reconciliation::successor::CheckedPlanningSuccessor;
use super::{
    AddWorkBlockerRequest, ChangeWorkPrerequisiteRequest, ClearWorkBlockerRequest, Connection,
    DateTime, ObjectId, PlanningValidation, ReviseWorkRequest, RootExecutionId, StoreError,
    Transaction, Utc, WorkBlocker, WorkEventDraft, WorkId, WorkItem, WorkLifecycle,
    WorkPlanningAuthority, WorkRunId, add_work_blocker_on, append_work_event,
    apply_revision_fields_on, authority, clear_work_blocker_on, load_work_item, relations,
    revise_work_on, validate_planning_authority,
};

mod effects;
pub(in crate::storage::work) use effects::{
    apply_successor_effects_on, require_successor_roots_on, select_successor_effects_on,
};

/// The ordinary transition keeps its normal hold admission and minted event.
/// The other variant can only borrow an already checked successor capability.
pub(in crate::storage::work) enum NativePlanningEvent<'a, 'tx, 'conn, 'recipe> {
    Ordinary,
    Successor {
        checked: &'a CheckedPlanningSuccessor<'tx, 'conn, 'recipe>,
        id: &'a ObjectId,
    },
}

impl NativePlanningEvent<'_, '_, '_, '_> {
    pub(in crate::storage::work) fn check(
        &self,
        transaction: &Transaction<'_>,
        work: WorkId,
    ) -> Result<(), StoreError> {
        if matches!(self, Self::Successor { .. }) {
            let item = load_work_item(transaction, work)?;
            self.check_scope(transaction, work, &item.project_id)?;
        }
        Ok(())
    }

    fn check_scope(
        &self,
        transaction: &Transaction<'_>,
        work: WorkId,
        project: &crate::ProjectId,
    ) -> Result<(), StoreError> {
        if let Self::Successor { checked, id } = self {
            if !std::ptr::eq(transaction, checked.transaction()) {
                return Err(StoreError::InvalidWorkProjection(
                    "native successor belongs to another transaction".into(),
                ));
            }
            checked.require_selected_event(work, id)?;
            // These cores change an existing native item. A new task named
            // by the candidate is created by the construction owner instead.
            if !checked
                .recipe()
                .before
                .iter()
                .any(|basis| basis.work_id == work)
                || checked.recipe().request.intent.local_cut.position.feed
                    != crate::FeedId::Project(project.clone())
            {
                return Err(StoreError::InvalidWorkProjection(
                    "native successor item is outside its checked project readset".into(),
                ));
            }
        }
        Ok(())
    }

    pub(in crate::storage::work) fn require_unheld(
        &self,
        transaction: &Transaction<'_>,
        work: WorkId,
    ) -> Result<(), StoreError> {
        match self {
            Self::Ordinary => {
                super::super::task_sync::reconciliation::refuse_held_on(transaction, work)
            }
            Self::Successor { checked, .. } => checked.require_affected(work),
        }
    }

    pub(super) fn authority(
        &self,
        transaction: &Transaction<'_>,
        item: &WorkItem,
        authority: &WorkPlanningAuthority,
        actor: &crate::ActorContext,
        at: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        self.check(transaction, item.work_id)?;
        match self {
            Self::Ordinary => validate_planning_authority(transaction, item, authority, actor, at),
            Self::Successor { checked, .. } => authority::validate_successor_planning_authority(
                checked, item, authority, actor, at,
            ),
        }
    }

    pub(in crate::storage::work) fn append(
        &self,
        transaction: &Transaction<'_>,
        event: &WorkEventDraft,
    ) -> Result<(ObjectId, Vec<crate::FeedPosition>), StoreError> {
        // The native core has persisted the new projection, but its matching
        // canonical event is not stored yet. Validate retained scope here;
        // the complete canonical item was checked at transition admission.
        self.check_scope(transaction, event.work_id, &event.project_id)?;
        match self {
            Self::Ordinary => append_work_event(transaction, event),
            Self::Successor { checked, id } => {
                super::super::feeds::append_successor_work_event(checked, event, id)
            }
        }
    }

    pub(in crate::storage::work) fn run_id(&self, work: WorkId) -> Result<WorkRunId, StoreError> {
        match self {
            Self::Ordinary => Ok(WorkRunId::new()),
            Self::Successor { checked, .. } => checked
                .recipe()
                .new_runs
                .get(&work)
                .copied()
                .ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "successor has no retained new run identity".into(),
                    )
                }),
        }
    }

    pub(in crate::storage::work) fn root_id(
        &self,
        work: WorkId,
    ) -> Result<RootExecutionId, StoreError> {
        match self {
            Self::Ordinary => Ok(RootExecutionId::new()),
            Self::Successor { checked, .. } => checked
                .recipe()
                .new_root_executions
                .get(&work)
                .copied()
                .ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "successor has no retained new root identity".into(),
                    )
                }),
        }
    }
}

pub(in crate::storage::work) fn revise_successor_on(
    checked: &CheckedPlanningSuccessor<'_, '_, '_>,
    request: &ReviseWorkRequest,
    event: &ObjectId,
) -> Result<WorkItem, StoreError> {
    revise_work_on(
        checked.transaction(),
        request,
        &NativePlanningEvent::Successor { checked, id: event },
    )
}

/// Select construction identities without writing. The caller projects any
/// retained reopen first; the mutating core checks its actual item and selects
/// obligations again inside the checked writer transaction.
pub(in crate::storage::work) fn plan_successor_revision_obligations_on(
    connection: &Connection,
    item_before_revision: &WorkItem,
    patch: &crate::WorkRevisionPatch,
) -> Result<
    (
        Vec<crate::task_sync::delta::PlannedSuccessorObligation>,
        Vec<crate::task_sync::delta::PlannedSuccessorResolution>,
    ),
    StoreError,
> {
    let mut item = item_before_revision.clone();
    let reauthored = apply_revision_fields_on(connection, &mut item, patch)?;
    item.revision += 1;
    let Some(run) = item.active_run_id else {
        return Ok((Vec::new(), Vec::new()));
    };
    super::super::completion::plan_binding_obligation_identities_on(
        connection,
        &item,
        run,
        &reauthored,
    )
}

pub(in crate::storage::work) fn change_prerequisite_successor_on(
    checked: &CheckedPlanningSuccessor<'_, '_, '_>,
    request: &ChangeWorkPrerequisiteRequest,
    add: bool,
    event: &ObjectId,
) -> Result<WorkItem, StoreError> {
    checked.require_affected(request.prerequisite_id)?;
    relations::change_work_prerequisite_native_on(
        checked.transaction(),
        request,
        add,
        PlanningValidation::Immediate,
        None,
        relations::PrerequisiteEndpointPolicy::Open,
        &NativePlanningEvent::Successor { checked, id: event },
    )
}

pub(in crate::storage::work) fn add_blocker_successor_on(
    checked: &CheckedPlanningSuccessor<'_, '_, '_>,
    request: &AddWorkBlockerRequest,
    blocker: &WorkBlocker,
    record: &ObjectId,
    event: &ObjectId,
) -> Result<WorkBlocker, StoreError> {
    checked.require_selected_event(request.work_id, event)?;
    if checked
        .recipe()
        .blocker_records
        .get(&request.work_id)
        .and_then(|ids| ids.get(&blocker.blocker_id))
        != Some(record)
    {
        return Err(StoreError::InvalidWorkProjection(
            "successor blocker record differs from its retained construction".into(),
        ));
    }
    add_work_blocker_on(
        checked.transaction(),
        request,
        &NativePlanningEvent::Successor { checked, id: event },
        Some((blocker, record)),
    )
}

pub(in crate::storage::work) fn clear_blocker_successor_on(
    checked: &CheckedPlanningSuccessor<'_, '_, '_>,
    request: &ClearWorkBlockerRequest,
    event: &ObjectId,
) -> Result<WorkItem, StoreError> {
    clear_work_blocker_on(
        checked.transaction(),
        request,
        &NativePlanningEvent::Successor { checked, id: event },
    )
}
