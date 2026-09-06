//! One-hop required-child resolution shared by completion and advisory reads.
//! Current reads verify exact item/run/event bindings, never replay root history
//! or recursively verify seals. Completion and doctor validate the immutable
//! proof against the parent's recursively verified child-seal set separately.

use rusqlite::{Connection, OptionalExtension, params};

use super::feeds::load_typed_work_object;
use super::query::{
    active_root_execution_optional, latest_canonical_work_event_for_item_optional, load_work_item,
    load_work_run, parse_work_id, parse_work_run_id,
};
use crate::{
    ChildRequirement, ObjectHash, RequiredChildResolution, RootExecutionId, WorkEvent, WorkId,
    WorkItem, WorkLifecycle, WorkRunState, WorkTransition,
    storage::{SqliteStore, StoreError},
};

/// Transient explanation with no execution authority or exposed proof hashes.
#[derive(Clone, Debug)]
pub(crate) struct RequiredChildSuccessor {
    pub successor: WorkId,
    pub lifecycle: WorkLifecycle,
    pub reason: &'static str,
    pub resolution: Option<RequiredChildResolution>,
    pub can_waive: bool,
    pub waived: bool,
}

impl SqliteStore {
    pub(crate) fn required_child_successor(
        &self,
        child: &WorkItem,
    ) -> Result<Option<RequiredChildSuccessor>, StoreError> {
        if !is_candidate(child) {
            return Ok(None);
        }
        let Some(parent) = child.parent_id else {
            return Ok(None);
        };
        let run = self.latest_work_run(parent)?;
        // Retained parents stay bound to their own generation after root reopen.
        // A restored parent can have no run while its children already execute.
        let execution = match &run {
            Some(run) => Some(run.root_execution_id),
            None => active_root_execution_optional(&self.connection, child.root_id)?
                .map(|execution| execution.root_execution_id),
        };
        let mut successor = required_child_successor_on(&self.connection, child, execution)?;
        if let Some(state) = &mut successor
            && self
                .work_child_waivers(&load_work_item(&self.connection, parent)?, run.as_ref())?
                .contains(&child.work_id)
        {
            state.waived = true;
            state.can_waive = false;
            state.resolution = None;
            state.reason = "accounted by an explicit waiver";
        }
        Ok(successor)
    }
}

fn is_candidate(child: &WorkItem) -> bool {
    child.parent_id.is_some()
        && child.child_requirement == ChildRequirement::Required
        && child.lifecycle == WorkLifecycle::Superseded
}

pub(super) fn required_child_successor_on(
    connection: &Connection,
    child: &WorkItem,
    execution: Option<RootExecutionId>,
) -> Result<Option<RequiredChildSuccessor>, StoreError> {
    if !is_candidate(child) {
        return Ok(None);
    }
    let child = load_work_item(connection, child.work_id)?;
    let successor_id = child
        .superseded_by
        .ok_or_else(|| invalid("superseded child has no successor"))?;
    let successor = load_work_item(connection, successor_id)?;
    if successor.project_id != child.project_id || successor.work_id == child.work_id {
        return Err(invalid(
            "required-child successor has an invalid identity binding",
        ));
    }
    let mut result = RequiredChildSuccessor {
        successor: successor_id,
        lifecycle: successor.lifecycle,
        reason: "successor is not completed; explicit waiver still required",
        resolution: None,
        waived: false,
        can_waive: child
            .parent_id
            .map(|parent| load_work_item(connection, parent))
            .transpose()?
            .is_some_and(|parent| parent.lifecycle == WorkLifecycle::Open),
    };
    if successor.lifecycle != WorkLifecycle::Completed {
        return Ok(Some(result));
    }
    if successor.parent_id != child.parent_id || successor.root_id != child.root_id {
        result.reason =
            "successor is not a sibling under the same parent; explicit waiver still required";
        return Ok(Some(result));
    }
    if successor.child_requirement != ChildRequirement::Required {
        result.reason = "successor is optional; explicit waiver still required";
        return Ok(Some(result));
    }
    let Some(event) = latest_canonical_work_event_for_item_optional(connection, child.work_id)?
    else {
        result.reason = "no native supersession execution binding; explicit waiver still required";
        return Ok(Some(result));
    };
    let Some(execution) = execution else {
        result.reason = "parent has no native execution generation; explicit waiver still required";
        return Ok(Some(result));
    };
    if !supersession_binds(&event, &child, successor_id, execution) {
        result.reason =
            "supersession belongs to another execution generation; explicit waiver still required";
        return Ok(Some(result));
    }
    let stored_run: Option<String> = connection
        .query_row(
            "SELECT run_id FROM work_runs WHERE work_id = ?1 ORDER BY generation DESC LIMIT 1",
            [successor_id.0.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    let Some(run_id) = stored_run.map(|id| parse_work_run_id(&id)).transpose()? else {
        result.reason = "successor has no native completion seal; explicit waiver still required";
        return Ok(Some(result));
    };
    let run = load_work_run(connection, run_id)?;
    if run.root_execution_id != execution {
        result.reason =
            "successor belongs to another execution generation; explicit waiver still required";
        return Ok(Some(result));
    }
    let seal = run
        .completion_seal
        .filter(|_| run.state == WorkRunState::Completed)
        .ok_or_else(|| invalid("completed successor has no completed run/seal binding"))?;
    let bound: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_completion_seals
         WHERE seal_hash = ?1 AND work_id = ?2 AND run_id = ?3 AND root_execution_id = ?4)",
        params![
            seal.as_str(),
            successor_id.0.to_string(),
            run_id.0.to_string(),
            execution.0.to_string()
        ],
        |row| row.get(0),
    )?;
    if run.work_id != successor_id || !bound {
        return Err(invalid(
            "successor seal differs from its current execution binding",
        ));
    }
    let proof: crate::CompletionSeal =
        load_typed_work_object(connection, &seal, "completion_seal")?;
    if proof.work_id != successor_id
        || proof.run_id != run_id
        || proof.root_execution_id != execution
        || proof.root_id != child.root_id
        || proof.run_generation != run.generation
    {
        return Err(invalid(
            "successor canonical seal differs from its execution binding",
        ));
    }
    let stored_hash: String = connection.query_row(
        "SELECT latest_event_hash FROM work_items WHERE work_id = ?1",
        [child.work_id.0.to_string()],
        |row| row.get(0),
    )?;
    let supersession = ObjectHash::from_stored(stored_hash.clone())
        .ok_or(StoreError::InvalidStoredHash(stored_hash))?;
    result.reason = "resolved by successor";
    result.resolution = Some(RequiredChildResolution::ResolvedBySuccessor {
        work_id: child.work_id,
        work_revision: child.revision,
        supersession,
        successor: successor_id,
        successor_seal: seal,
    });
    Ok(Some(result))
}

/// Both current eligibility and frozen-proof validation use this exact rule.
pub(super) fn supersession_binds(
    event: &WorkEvent,
    child: &WorkItem,
    successor: WorkId,
    execution: RootExecutionId,
) -> bool {
    event.work == *child
        && event.work_id == child.work_id
        && event.revision == child.revision
        && event.project_id == child.project_id
        && event.root_id == child.root_id
        && child.child_requirement == ChildRequirement::Required
        && child.lifecycle == WorkLifecycle::Superseded
        && child.superseded_by == Some(successor)
        && event.root_execution.as_ref().is_some_and(|root| {
            root.root_execution_id == execution
                && root.root_id == child.root_id
                && root.project_id == child.project_id
        })
        && event.run.as_ref().is_some_and(|run| {
            run.root_execution_id == execution
                && run.work_id == child.work_id
                && event.run_id == Some(run.run_id)
                && run.state == WorkRunState::Cancelled
        })
        && matches!(&event.transition, WorkTransition::Disposed {
            lifecycle: WorkLifecycle::Superseded, replacement_id: Some(id), reason
        } if *id == successor && !reason.trim().is_empty())
}

pub(super) fn required_successor_resolutions_on(
    connection: &Connection,
    parent: WorkId,
    execution: RootExecutionId,
    waived: &std::collections::HashSet<WorkId>,
) -> Result<Vec<RequiredChildResolution>, StoreError> {
    let ids = connection
        .prepare(
            "SELECT work_id FROM work_items WHERE parent_id = ?1
         AND child_requirement = 'required' AND lifecycle = 'superseded' ORDER BY work_id",
        )?
        .query_map([parent.0.to_string()], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut resolutions = Vec::new();
    for id in ids {
        let id = parse_work_id(&id)?;
        if waived.contains(&id) {
            continue;
        }
        let child = load_work_item(connection, id)?;
        if let Some(resolution) = required_child_successor_on(connection, &child, Some(execution))?
            .and_then(|state| state.resolution)
        {
            resolutions.push(resolution);
        }
    }
    Ok(resolutions)
}

fn invalid(reason: &str) -> StoreError {
    StoreError::InvalidWorkProjection(reason.into())
}
