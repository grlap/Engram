//! Independent follow-ups for work stranded below a terminal ancestor.

use super::super::query::classified_prerequisite_projections;
use super::super::query::load_root_execution;
use super::{
    Connection, CreateWorkRequest, DateTime, HashSet, OptionalExtension, Redactor, SCHEMA_VERSION,
    SqliteStore, StoreError, Utc, WorkClaimState, WorkEventDraft, WorkItem, WorkLifecycle,
    WorkOrigin, WorkRunState, WorkTransition, append_work_event, assert_revision, create_root_on,
    inspect_work_request, load_active_blocker_projections, load_work_claim_optional,
    load_work_item, load_work_run, normalize_text, params, persist_operation_result,
    persist_work_item, persist_work_run, replay_operation, request_object,
    require_work_item_relation_integrity, short_ref,
};
use super::{persist_root_execution, waive_root_contributor};
use crate::RootExecutionState;
use crate::WorkPrerequisiteState;
use crate::domain::{ProvenanceLink, ProvenanceRelation};
use crate::{ChildRequirement, DetachWorkRequest};

#[cfg(test)]
mod tests;

impl SqliteStore {
    /// Creates an independent successor and supersedes the stranded child atomically.
    /// Ancestors, claims, fence heads, and sealed/terminal executions stay unchanged.
    /// An open root's live execution receives ordinary disposal's contributor waiver
    /// when needed. Historical bytes stay intact; old authority is never reused.
    ///
    /// # Errors
    /// Returns an attributed refusal for noneligible work, stale revisions, or invalid input.
    pub fn detach_work<R: Redactor>(
        &mut self,
        request: &DetachWorkRequest,
        redactor: &R,
    ) -> Result<WorkItem, StoreError> {
        inspect_work_request(redactor, request, &request.actor)?;
        let reason = normalize_text(&request.reason, "detach reason")?;
        let object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(root) = replay_operation::<WorkItem>(
            &transaction,
            "detach_work",
            &request.idempotency_key,
            object.hash(),
        )? {
            transaction.commit()?;
            return Ok(root);
        }
        let mut item = load_work_item(&transaction, request.work_id)?;
        if item.project_id != request.project_id {
            return Err(StoreError::InvalidWork(
                "detach target must belong to the bound project".into(),
            ));
        }
        assert_revision(&item, request.expected_work_revision)?;
        validate_detach_on(&transaction, &item, request.detached_at)?;
        let mut actor = request.actor.clone();
        actor.provenance_chain.push(ProvenanceLink {
            relation: ProvenanceRelation::DerivedFrom,
            source: "work_detach".into(),
            reference: Some(item.work_id.0.to_string()),
        });
        let creation = CreateWorkRequest {
            notes: Vec::new(),
            project_id: item.project_id.clone(),
            parent_id: None,
            child_requirement: ChildRequirement::Required,
            title: item.title.clone(),
            outcome: item.outcome.clone(),
            acceptance: item.acceptance.clone(),
            kind: item.kind,
            priority: item.priority,
            labels: item.labels.clone(),
            assigned_to: None,
            deferred_until: None,
            origin: WorkOrigin::Local,
            source_snapshot_id: None,
            actor,
            idempotency_key: request.idempotency_key.clone(),
            created_at: request.detached_at,
        };
        inspect_work_request(redactor, &creation, &creation.actor)?;
        let root = create_root_on(&transaction, &creation, &[], redactor)?;
        let mut run = item
            .active_run_id
            .map(|id| load_work_run(&transaction, id))
            .transpose()?;
        let mut reconciled_execution = None;
        if let Some(old_run) = &run {
            let mut execution = load_root_execution(&transaction, old_run.root_execution_id)?;
            if execution.root_id != item.root_id || execution.project_id != item.project_id {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "detach root execution {:?} crosses the work project or root boundary",
                    execution.root_execution_id
                )));
            }
            if execution.state == RootExecutionState::Active
                && load_work_item(&transaction, item.root_id)?.lifecycle == WorkLifecycle::Open
                && let Some(claim) = load_work_claim_optional(&transaction, old_run.run_id)?
                && claim.state == WorkClaimState::Active
                && execution.expected_contributors.contains(&claim.holder)
                && waive_root_contributor(
                    &mut execution,
                    &claim.holder,
                    &request.actor.actor_id,
                    &reason,
                )
            {
                execution.revision += 1;
                execution.updated_at = request.detached_at;
                persist_root_execution(&transaction, &execution)?;
                reconciled_execution = Some(execution);
            }
        }
        if let Some(run) = &mut run {
            let fence = transaction.query_row(
                "SELECT claim_fence_head FROM work_runs WHERE run_id = ?1",
                [run.run_id.0.to_string()],
                |row| row.get(0),
            )?;
            run.state = WorkRunState::Cancelled;
            run.executor = None;
            run.revision += 1;
            run.updated_at = request.detached_at;
            persist_work_run(&transaction, run, fence)?;
        }
        item.lifecycle = WorkLifecycle::Superseded;
        item.superseded_by = Some(root.work_id);
        item.active_run_id = None;
        item.revision += 1;
        item.updated_at = request.detached_at;
        persist_work_item(&transaction, &item)?;
        append_work_event(
            &transaction,
            &WorkEventDraft {
                schema_version: SCHEMA_VERSION,
                project_id: item.project_id.clone(),
                root_id: item.root_id,
                work_id: item.work_id,
                run_id: run.as_ref().map(|run| run.run_id),
                revision: item.revision,
                work: item.clone(),
                run,
                root_execution: reconciled_execution,
                claim: None,
                handoff_offer: None,
                blocker: None,
                transition: WorkTransition::Disposed {
                    lifecycle: WorkLifecycle::Superseded,
                    replacement_id: Some(root.work_id),
                    reason,
                },
                actor: request.actor.clone(),
                created_at: request.detached_at,
            },
        )?;
        persist_operation_result(
            &transaction,
            "detach_work",
            &request.idempotency_key,
            object.hash(),
            &root,
        )?;
        transaction.commit()?;
        Ok(root)
    }
}

fn validate_detach_on(
    connection: &Connection,
    item: &WorkItem,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    let refuse = |reason: &str, remedy: String| StoreError::WorkDetachRefused {
        work_id: item.work_id,
        reason: reason.into(),
        remedy,
    };
    let show = || format!("engram work show {}", item.short_ref);
    if item.lifecycle != WorkLifecycle::Open || item.parent_id.is_none() {
        return Err(refuse("detach requires an open child", show()));
    }
    let mut parent_id = item.parent_id;
    let mut visited = HashSet::new();
    let mut terminal = false;
    let mut reached_root = false;
    while let Some(id) = parent_id {
        if !visited.insert(id) || visited.len() > 1_024 {
            return Err(StoreError::InvalidWorkProjection(
                "detach hierarchy is cyclic or exceeds the corruption guard".into(),
            ));
        }
        let parent = load_work_item(connection, id)?;
        if parent.project_id != item.project_id || parent.root_id != item.root_id {
            return Err(StoreError::InvalidWorkProjection(
                "detach ancestor crosses project or root".into(),
            ));
        }
        terminal |= matches!(
            parent.lifecycle,
            WorkLifecycle::Completed | WorkLifecycle::Cancelled | WorkLifecycle::Superseded
        );
        reached_root |= id == item.root_id;
        parent_id = parent.parent_id;
    }
    if !reached_root {
        return Err(StoreError::InvalidWorkProjection(
            "detach hierarchy does not reach its root".into(),
        ));
    }
    if !terminal {
        return Err(refuse(
            "detach requires a completed, cancelled, or superseded ancestor",
            show(),
        ));
    }
    let descendant: Option<String> = connection.query_row(
        "WITH RECURSIVE descendants(work_id, short_ref, lifecycle) AS (
             SELECT work_id, short_ref, lifecycle FROM work_items WHERE parent_id = ?1
             UNION SELECT child.work_id, child.short_ref, child.lifecycle FROM work_items child
             JOIN descendants parent ON child.parent_id = parent.work_id
         ) SELECT short_ref FROM descendants WHERE lifecycle IN ('open', 'proposed') ORDER BY work_id LIMIT 1",
        [item.work_id.0.to_string()], |row| row.get(0)).optional()?;
    if let Some(child) = descendant {
        return Err(refuse(
            "resolve open descendants before detaching their parent",
            format!("engram work show {child}"),
        ));
    }
    if let Some(run_id) = item.active_run_id {
        // Prefer the handoff-specific refusal when an offer and its claim are
        // both live; neither admission check may silently subsume the other.
        let offered: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM work_handoff_offers WHERE run_id = ?1 AND state = 'offered' AND expires_at_ms > ?2)",
            params![run_id.0.to_string(), now.timestamp_millis()], |row| row.get(0))?;
        if offered {
            return Err(refuse(
                "wait for the live child handoff to expire before detaching",
                show(),
            ));
        }
        if let Some(claim) = load_work_claim_optional(connection, run_id)?
            && claim.state == WorkClaimState::Active
            && claim.expires_at > now
        {
            return Err(refuse(
                "wait for the live child claim to expire before detaching",
                show(),
            ));
        }
    }
    require_work_item_relation_integrity(connection, item.work_id)?;
    let blocker_count = load_active_blocker_projections(connection, item.work_id)?.len();
    if blocker_count > 0 {
        return Err(refuse(
            &format!("resolve {blocker_count} independent active blocker(s) before detaching"),
            if blocker_count == 1 {
                format!("engram work update {} --unblock", item.short_ref)
            } else {
                show()
            },
        ));
    }
    if let Some((prerequisite, _)) = classified_prerequisite_projections(connection, item.work_id)?
        .into_iter()
        .find(|(_, state)| *state != WorkPrerequisiteState::Satisfied)
    {
        return Err(refuse(
            "complete or explicitly remove the incomplete prerequisite before detaching",
            format!(
                "engram work update {} --drop-after {}",
                item.short_ref,
                short_ref(prerequisite)
            ),
        ));
    }
    if item.deferred_until.is_some_and(|until| until > now) {
        return Err(refuse(
            "wait for the deferred wake time or explicitly reschedule it before detaching",
            show(),
        ));
    }
    Ok(())
}
