//! Native disclosure witnesses from a confirmed output's keyed predecessor chain.

use super::{Connection, ObjectId, StoreError, Transition, require, view};
use crate::{
    CanonicalObject, WorkItem,
    task_sync::{
        TaskDocument, TaskSyncMembershipView,
        delta::{
            BootstrapMember, BootstrapMembers, CheckpointSeed, DeltaBasis, PreparedExportAnchor,
            PreparedExportBasis,
        },
        ledger::{Binding, EVENT_KIND, Event, OutgoingReconciliation, PublicationObservation},
    },
};
use rusqlite::{OptionalExtension, params};
use std::collections::BTreeSet;

enum Member {
    Missing,
    Removed,
    Document(Box<TaskDocument>),
}

/// Resolve only an existing keyed document member. Unrelated older payloads
/// need no decode; a selected member is checked against its canonical owner.
fn member_on(
    connection: &Connection,
    record: &ObjectId,
    item: &WorkItem,
    budget: &mut view::read::ReadBudget,
) -> Result<Member, StoreError> {
    let projected: Option<Option<Vec<u8>>> = connection.query_row(
        "SELECT document_json FROM work_task_sync_document_members WHERE record_id=?1 AND work_id=?2 AND project_id=?3",
        params![record.as_str(), item.work_id.0.to_string(), item.project_id.0], |row| row.get(0)).optional()?;
    let Some(projected) = projected else {
        return Ok(Member::Missing);
    };
    let projected: Option<TaskDocument> = projected
        .map(|bytes| serde_json::from_slice(&bytes))
        .transpose()?;
    let kind: String = connection.query_row(
        "SELECT object_kind FROM objects WHERE object_id=?1",
        [record.as_str()],
        |row| row.get(0),
    )?;
    let value: serde_json::Value = view::read::object(connection, record, &kind, budget)?;
    let object = CanonicalObject::identified(record, &value)?;
    let mut canonical =
        super::super::closure::documents::canonical_members(&kind, &object, &item.project_id)?;
    let document = canonical.remove(&item.work_id);
    require(
        document == projected,
        "publication member projection differs from canonical bytes",
    )?;
    Ok(document.map_or(Member::Removed, |document| {
        Member::Document(Box::new(document))
    }))
}

fn preceding_on(
    connection: &Connection,
    binding_id: &ObjectId,
    binding: &Binding,
    item: &WorkItem,
    mut basis: PreparedExportBasis,
    budget: &mut view::read::ReadBudget,
) -> Result<Option<TaskDocument>, StoreError> {
    use super::super::closure::membership::head;
    let mut visited = BTreeSet::new();
    loop {
        let record = match &basis.anchor {
            PreparedExportAnchor::Prepared { event } => event.clone(),
            PreparedExportAnchor::Bootstrap { checkpoint } => checkpoint.clone(),
        };
        require(
            visited.len() < super::MAX_RECORDS && visited.insert(record.clone()),
            "publication membership chain is cyclic or exceeds supported bound",
        )?;
        let held = head::read_on(
            connection,
            &record,
            binding_id,
            binding,
            TaskSyncMembershipView::Export,
        )?;
        require(
            held.basis == head::MembershipBasis::Export(Box::new(basis.clone())),
            "publication predecessor membership differs",
        )?;
        match member_on(connection, &record, item, budget)? {
            Member::Missing => {}
            Member::Removed => return Ok(None),
            Member::Document(document) => return Ok(Some(*document)),
        }
        match basis.anchor {
            PreparedExportAnchor::Prepared { .. } => {
                let (intent, _) =
                    head::delta_preparation_on(connection, &record, binding_id, binding)?;
                let DeltaBasis::Outgoing { export_before, .. } = intent.basis else {
                    return Err(super::super::invalid(
                        "publication predecessor is not outgoing",
                    ));
                };
                basis = *export_before;
            }
            PreparedExportAnchor::Bootstrap { .. } => {
                let event: Event = view::read::object(connection, &record, EVENT_KIND, budget)?;
                let Transition::CustodyCheckpoint { checkpoint } = event.transition else {
                    return Err(super::super::invalid("publication bootstrap is missing"));
                };
                let Some(CheckpointSeed::PreparedExport { members, .. }) = checkpoint.seed else {
                    return Err(super::super::invalid("publication bootstrap role differs"));
                };
                let BootstrapMembers::Captured { mut members } = members else {
                    return Err(super::super::invalid(
                        "publication bootstrap lacks captured membership",
                    ));
                };
                return match members.remove(&item.work_id) {
                    None => Ok(None),
                    Some(BootstrapMember::Document { task }) => Ok(Some(*task)),
                    Some(BootstrapMember::Canonical { member }) => {
                        match member_on(connection, &member.record_id, item, budget)? {
                            Member::Document(document) => Ok(Some(*document)),
                            Member::Missing | Member::Removed => Err(super::super::invalid(
                                "publication bootstrap canonical member missing",
                            )),
                        }
                    }
                };
            }
        }
    }
}

pub(in crate::storage::work::task_sync) fn read_on(
    connection: &Connection,
    item: &WorkItem,
) -> Result<Vec<TaskDocument>, StoreError> {
    let mut documents = Vec::new();
    let mut budget = view::read::ReadBudget::default();
    for id in view::select::bindings(
        connection,
        view::select::Bindings::Project(&item.project_id),
    )? {
        let binding = view::read::binding_on(connection, &id, &mut budget)?;
        let outcome: Option<String> = connection.query_row(
            "SELECT object_id FROM objects INDEXED BY objects_task_sync_exchange_outgoing_completed
             WHERE object_kind='task_sync_exchange_event' AND json_valid(canonical_json)
               AND json_extract(canonical_json,'$.transition.transition')='push_observed'
               AND json_extract(canonical_json,'$.transition.observation.outcome')='confirmed'
               AND json_extract(canonical_json,'$.binding')=?1
             ORDER BY json_extract(canonical_json,'$.sequence') DESC LIMIT 1",
            [id.as_str()], |row| row.get(0)).optional()?;
        let Some(outcome) = outcome else {
            continue;
        };
        let outcome = ObjectId::from_stored(outcome)
            .ok_or_else(|| super::super::invalid("invalid publication outcome identity"))?;
        let event: Event = view::read::object(connection, &outcome, EVENT_KIND, &mut budget)?;
        let Transition::PushObserved {
            prepared,
            observation: PublicationObservation::Confirmed { .. },
        } = &event.transition
        else {
            return Err(super::super::invalid(
                "publication witness is not confirmed",
            ));
        };
        let checked = view::read::operation_on(connection, &id, &binding, prepared, &mut budget)?;
        require(
            checked.progress.completed.as_ref() == Some(&outcome),
            "publication witness differs from canonical completion",
        )?;
        let mut selected = None;
        for (_, retained) in checked.events.iter().rev() {
            if let Transition::OutgoingReconciled { reconciliation, .. } = &retained.transition
                && let OutgoingReconciliation::DeltaResolved { payload, .. } =
                    reconciliation.as_ref()
                && let Some(document) = payload.documents.get(&item.work_id)
            {
                selected = Some(document.clone());
                break;
            }
            if let Transition::DeltaPrepared { intent, payload } = &retained.transition {
                selected = Some(
                    if let Some(document) = payload.documents.get(&item.work_id) {
                        document.clone()
                    } else {
                        let DeltaBasis::Outgoing { export_before, .. } = &intent.basis else {
                            return Err(super::super::invalid(
                                "confirmed publication is not outgoing",
                            ));
                        };
                        preceding_on(
                            connection,
                            &id,
                            &binding,
                            item,
                            *export_before.clone(),
                            &mut budget,
                        )?
                    },
                );
                break;
            }
        }
        if selected.is_none()
            && let Some(intent) = checked.progress_intent()
        {
            // This explicit exchange-only fallback shares one bounded snapshot
            // per binding/commit, with keyed task lookup rather than a scan.
            selected = Some(super::super::origin::published_snapshot_member(
                connection,
                &id,
                &binding,
                &intent.commit,
                item.work_id,
            )?);
        }
        if let Some(Some(document)) = selected {
            documents.push(document);
        }
    }
    Ok(documents)
}
