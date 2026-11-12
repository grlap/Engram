//! One source-relative derivation for projection upkeep and cold replay.
//! Canonical delta admission remains the atomic graph writer's responsibility.

use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, params};

use super::super::super::{
    baseline::TaskSyncImportPlanning, folder::IMPORT_PLANNING_KIND, ledger::view::read,
};
use super::super::membership::head::{self, MembershipBasis};
use super::{Members, OwnerCache, member_on};
use crate::{
    ObjectId, WorkId,
    storage::StoreError,
    task_sync::{
        TaskDocument, TaskSyncMembershipView,
        delta::{
            DELTA_PLANNING_KIND, DeltaBasis, DeltaPlanning, IncomingSourceAnchor, OutputMembership,
            PreparedExportAnchor,
        },
        ledger::{Binding, Direction, EVENT_KIND, Event, Transition, require},
    },
};

#[cfg(test)]
mod tests;

#[cfg(test)]
thread_local! {
    static MEMBER_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

pub(in crate::storage::work::task_sync::closure) struct MembershipDelta {
    pub role: &'static str,
    pub before_head: ObjectId,
    pub before: OutputMembership,
    pub output: OutputMembership,
    pub changes: BTreeMap<WorkId, Option<(ObjectId, TaskDocument)>>,
}

impl MembershipDelta {
    pub(in crate::storage::work::task_sync::closure) fn require_before(
        &self,
        head: &ObjectId,
        output: &OutputMembership,
    ) -> Result<(), StoreError> {
        require(
            head == &self.before_head && output == &self.before,
            "delta membership predecessor or comparison root differs",
        )
    }

    // The caller can supply just changed old members. Unselected context is
    // never removed, and a tombstone requires a previously present member.
    pub(in crate::storage::work::task_sync::closure) fn apply(
        &self,
        members: &mut Members,
        before: &OutputMembership,
    ) -> Result<(), StoreError> {
        require(
            before == &self.before,
            "delta membership before-root differs",
        )?;
        let mut root = before.inventory_root;
        for (work, next) in &self.changes {
            if let Some((_, old)) = members.get(work) {
                root = root.excluding(old)?;
            } else {
                require(
                    next.is_some(),
                    "delta removes an absent source/export member",
                )?;
            }
            if let Some((_, document)) = next {
                require(
                    document.id == *work,
                    "delta source/export member task differs",
                )?;
                root = root.including(document)?;
            }
        }
        require(
            root == self.output.inventory_root,
            "delta source/export membership root differs",
        )?;
        // Complete all fallible comparisons before changing the in-memory view.
        for (work, next) in &self.changes {
            match next {
                Some(member) => {
                    members.insert(*work, member.clone());
                }
                None => {
                    members.remove(work);
                }
            }
        }
        Ok(())
    }
}

fn projected_member(
    connection: &Connection,
    record: &ObjectId,
    work: WorkId,
    binding: &Binding,
) -> Result<TaskDocument, StoreError> {
    #[cfg(test)]
    MEMBER_READS.with(|count| count.set(count.get() + 1));
    let row: Option<(String, Option<Vec<u8>>)> = connection.query_row(
        "SELECT project_id,document_json FROM work_task_sync_document_members WHERE record_id=?1 AND work_id=?2",
        params![record.as_str(), work.0.to_string()], |row| Ok((row.get(0)?, row.get(1)?)),
    ).optional()?;
    let Some((project, Some(bytes))) = row else {
        return Err(super::super::super::invalid(
            "selected source member is missing or retired",
        ));
    };
    require(
        project == binding.project.0,
        "selected source member project differs",
    )?;
    let task: TaskDocument = serde_json::from_slice(&bytes)?;
    require(task.id == work, "selected source member task differs")?;
    Ok(task)
}

pub(in crate::storage::work::task_sync::closure) fn resolve_on(
    connection: &Connection,
    event_id: &ObjectId,
    event: &Event,
    binding: &Binding,
    canonical: bool,
) -> Result<Option<MembershipDelta>, StoreError> {
    match &event.transition {
        Transition::DeltaPrepared { intent, payload }
            if intent.direction == Direction::Outgoing =>
        {
            payload.validate_historical(&event.binding, binding, intent)?;
            let DeltaBasis::Outgoing { export_before, .. } = &intent.basis else {
                return Err(super::super::super::invalid("outgoing delta basis differs"));
            };
            let before_head = match &export_before.anchor {
                PreparedExportAnchor::Prepared { event } => event,
                PreparedExportAnchor::Bootstrap { checkpoint } => checkpoint,
            };
            let previous = head::read_on(
                connection,
                before_head,
                &event.binding,
                binding,
                TaskSyncMembershipView::Export,
            )?;
            require(
                previous.basis == MembershipBasis::Export(export_before.clone()),
                "prior prepared export basis differs",
            )?;
            Ok(Some(MembershipDelta {
                role: "export",
                before_head: before_head.clone(),
                before: export_before.membership.clone(),
                output: intent.candidate.output.clone(),
                changes: payload
                    .documents
                    .iter()
                    .map(|(id, task)| {
                        (
                            *id,
                            task.as_ref().map(|task| (event_id.clone(), task.clone())),
                        )
                    })
                    .collect(),
            }))
        }
        Transition::DeltaImportApplied { prepared, receipt }
        | Transition::PlanningSuccessorApplied { prepared, receipt } => {
            let mut budget = read::ReadBudget::default();
            let preparation: Event = read::object(connection, prepared, EVENT_KIND, &mut budget)?;
            require(
                preparation.binding == event.binding,
                "delta source preparation binding differs",
            )?;
            let (intent, payload, source_payload, source_owner) =
                match (&event.transition, preparation.transition) {
                    (
                        Transition::DeltaImportApplied { .. },
                        Transition::DeltaPrepared { intent, payload },
                    ) => (intent, payload, None, prepared.clone()),
                    (
                        Transition::PlanningSuccessorApplied { .. },
                        Transition::PlanningSuccessorPrepared { recipe, payload },
                    ) => {
                        let original: Event = read::object(
                            connection,
                            &recipe.request.original_prepared,
                            EVENT_KIND,
                            &mut budget,
                        )?;
                        let Transition::DeltaPrepared {
                            intent: original_intent,
                            payload: original_payload,
                        } = original.transition
                        else {
                            return Err(super::super::super::invalid(
                                "successor original source preparation is missing",
                            ));
                        };
                        recipe
                            .request
                            .validate(&event.binding, binding, &original_intent)?;
                        original_payload.validate_historical(
                            &event.binding,
                            binding,
                            &original_intent,
                        )?;
                        (
                            Box::new(recipe.request.intent),
                            payload,
                            Some(original_payload),
                            recipe.request.original_prepared,
                        )
                    }
                    _ => {
                        return Err(super::super::super::invalid(
                            "delta source preparation family differs",
                        ));
                    }
                };
            if source_payload.is_some() {
                payload.validate_historical_successor(&event.binding, binding, &intent)?;
                receipt.validate_successor(&event.binding, binding, &intent)?;
            } else {
                payload.validate_historical(&event.binding, binding, &intent)?;
                receipt.validate(&event.binding, binding, &intent)?;
            }
            let DeltaBasis::Incoming {
                source_before,
                source_after,
                local_before,
                ..
            } = &intent.basis
            else {
                return Err(super::super::super::invalid(
                    "incoming source basis differs",
                ));
            };
            let planning: DeltaPlanning = read::object(
                connection,
                &receipt.planning,
                DELTA_PLANNING_KIND,
                &mut budget,
            )?;
            planning.validate(&binding.project, prepared, &intent)?;
            let local: TaskSyncImportPlanning = read::object(
                connection,
                &planning.local_planning,
                IMPORT_PLANNING_KIND,
                &mut budget,
            )?;
            local.validate(&binding.project)?;
            require(
                local.inventory_root == intent.candidate.output.inventory_root.hex()
                    && local.profile == intent.candidate.output.profile
                    && local.previous.as_ref() == Some(&local_before.baseline)
                    && local
                        .changes
                        .keys()
                        .copied()
                        .collect::<std::collections::BTreeSet<_>>()
                        == intent.candidate.changed
                    && local
                        .changes
                        .iter()
                        .all(|(id, task)| payload.documents.get(id) == Some(task)),
                "delta local planning output differs",
            )?;
            if canonical {
                let previous: TaskSyncImportPlanning = read::object(
                    connection,
                    &local_before.baseline,
                    IMPORT_PLANNING_KIND,
                    &mut budget,
                )?;
                previous.validate(&binding.project)?;
                require(
                    previous.inventory_root == local_before.membership.inventory_root.hex()
                        && previous.profile == local_before.membership.profile,
                    "prior local planning basis differs",
                )?;
            }
            let mut owners = OwnerCache::new();
            let mut changes = BTreeMap::new();
            for (work, member) in planning.source_changes {
                let resolved = match member {
                    Some(member) => {
                        let task = if canonical {
                            member_on(
                                connection,
                                &member.record_id,
                                work,
                                binding,
                                &mut budget,
                                &mut owners,
                            )?
                        } else {
                            projected_member(connection, &member.record_id, work, binding)?
                        };
                        Some((member.record_id, task))
                    }
                    None => None,
                };
                require(
                    source_payload
                        .as_deref()
                        .unwrap_or(payload.as_ref())
                        .documents
                        .get(&work)
                        .is_some_and(|task| {
                            task.as_ref() == resolved.as_ref().map(|(_, task)| task)
                                && (source_payload.is_none()
                                    || resolved
                                        .as_ref()
                                        .is_none_or(|(owner, _)| owner == &source_owner))
                        }),
                    "delta source link differs from its prepared document",
                )?;
                changes.insert(work, resolved);
            }
            let before_head = match &source_before.anchor {
                IncomingSourceAnchor::Applied { outcome } => outcome,
                IncomingSourceAnchor::Bootstrap { checkpoint } => checkpoint,
            };
            let previous = head::read_on(
                connection,
                before_head,
                &event.binding,
                binding,
                TaskSyncMembershipView::Source,
            )?;
            require(
                previous.basis == MembershipBasis::Source(source_before.clone()),
                "prior incoming source snapshot differs",
            )?;
            Ok(Some(MembershipDelta {
                role: "source",
                before_head: before_head.clone(),
                before: source_before.snapshot.membership.clone(),
                output: source_after.membership.clone(),
                changes,
            }))
        }
        _ => Ok(None),
    }
}
