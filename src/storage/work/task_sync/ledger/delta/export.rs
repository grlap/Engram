//! Selected outgoing bytes and their exact durable preparation.

use super::super::super::{capture, closure, origin, producing_origin};
use super::{
    Binding, Connection, DeltaIntent, Event, ObjectId, SqliteStore, StoreError, Transition,
    invalid, require, view,
};
use crate::task_sync::{
    CapturedTask, TaskSyncMembershipView,
    delta::{ConfirmedExportBasis, DELTA_PAYLOAD_FORMAT, DeltaBasis, DeltaCandidatePayload},
    ledger::{Direction, EVENT_KIND},
};
use rusqlite::params;
use std::collections::{BTreeMap, BTreeSet};

mod commit;
mod confirmed;
mod prepare;
mod progress;
mod resolution;
mod retry;

impl SqliteStore {
    /// Inspect a declared outgoing delta against one current read snapshot.
    /// Returns exact selected native bytes after source/export graph and cut
    /// comparisons. It creates no files, preparation, progress or publication
    /// claim. A later writer must repeat these checks under its own admission.
    ///
    /// # Errors
    /// Refuses missing/changed export or publication bases, current origin or
    /// restore drift, changed native bytes, and invalid connected graphs.
    pub fn inspect_task_sync_delta_export(
        &self,
        binding: &ObjectId,
        intent: &DeltaIntent,
    ) -> Result<DeltaCandidatePayload, StoreError> {
        self.work_read_snapshot(|store| {
            origin::with_origin_scope(&store.connection, || {
                inspect_on(&store.connection, binding, intent, None)
            })
        })
    }
}

fn confirmed_on(
    connection: &Connection,
    binding_id: &ObjectId,
    binding: &Binding,
    declared: Option<&ConfirmedExportBasis>,
) -> Result<(), StoreError> {
    // Like selected source reads, this observation relies on the admitted
    // cumulative projection. Doctor retains exhaustive derivation checks.
    view::read::check_head(connection, binding_id)?;
    let completed: Option<String> = connection.query_row(
        "SELECT last_outgoing_completed_id FROM work_task_sync_exchange_heads WHERE binding_id=?1",
        [binding_id.as_str()],
        |row| row.get(0),
    )?;
    require(
        completed.as_deref() == declared.map(|basis| basis.outcome.as_str()),
        "outgoing confirmed publication basis differs",
    )?;
    let Some(declared) = declared else {
        return Ok(());
    };
    canonical_confirmed_on(connection, binding_id, binding, declared).map(|_| ())
}

/// Validate the immutable outcome itself, independent of a newer current head.
pub(super) fn canonical_confirmed_on(
    connection: &Connection,
    binding_id: &ObjectId,
    binding: &Binding,
    declared: &ConfirmedExportBasis,
) -> Result<u64, StoreError> {
    let mut budget = view::read::ReadBudget::default();
    let outcome: Event =
        view::read::object(connection, &declared.outcome, EVENT_KIND, &mut budget)?;
    require(
        outcome.schema_version == 1
            && outcome.binding == *binding_id
            && matches!(&outcome.transition,
        Transition::PushObserved {prepared, observation:crate::task_sync::ledger::PublicationObservation::Confirmed {observed_tip}}
        if prepared == &declared.prepared && observed_tip == &declared.snapshot.commit.oid),
        "outgoing confirmed outcome differs",
    )?;
    // A legacy prepared request has no canonical membership comparison root.
    // Refuse it explicitly rather than reinterpret its full-tree fingerprint.
    let (prepared, sequence) = closure::membership::head::delta_preparation_on(
        connection,
        &declared.prepared,
        binding_id,
        binding,
    )?;
    require(
        prepared.direction == Direction::Outgoing && outcome.actor == prepared.actor,
        "confirmed export membership or attribution differs",
    )?;
    let mut query = connection.prepare("SELECT object_id FROM objects INDEXED BY objects_task_sync_exchange_prepared
        WHERE object_kind='task_sync_exchange_event' AND json_valid(canonical_json)
          AND json_extract(canonical_json,'$.binding')=?1
          AND json_extract(canonical_json,'$.transition.prepared')=?2
          AND json_extract(canonical_json,'$.transition.transition')='delta_local_commit_recorded' LIMIT 2")?;
    let selected = query
        .query_map(
            params![binding_id.as_str(), declared.prepared.as_str()],
            |row| row.get::<_, String>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    require(
        selected.len() == 1,
        "confirmed delta commit selection missing or forked",
    )?;
    let selected = ObjectId::from_stored(selected[0].clone())
        .ok_or_else(|| invalid("invalid confirmed delta commit identity"))?;
    let event: Event = view::read::object(connection, &selected, EVENT_KIND, &mut budget)?;
    require(
        event.schema_version == 1
            && event.binding == *binding_id
            && event.actor == prepared.actor
            && event.predecessor.as_ref() == Some(&declared.prepared)
            && sequence.checked_add(1) == Some(event.sequence)
            && matches!(&event.transition, Transition::DeltaLocalCommitRecorded {prepared, ..}
            if prepared == &declared.prepared),
        "confirmed delta selected commit differs",
    )?;
    let Transition::DeltaLocalCommitRecorded { commit, .. } = &event.transition else {
        return Err(invalid("confirmed delta local commit missing"));
    };
    crate::task_sync::delta::DeltaCommitSelection {
        prepared: declared.prepared.clone(),
        commit: commit.clone(),
    }
    .validate(binding_id, binding, &prepared, &declared.prepared)?;
    let resolved = confirmed::Confirmation {
        binding_id,
        binding,
        declared,
        original: &prepared,
        local_id: &selected,
        local_sequence: event.sequence,
        local_commit: commit,
        outcome: &outcome,
    }
    .resolved_on(connection, &mut budget)?;
    require(
        resolved
            || (prepared.candidate.output == declared.snapshot.membership
                && commit == &declared.snapshot.commit),
        "confirmed export membership or commit differs",
    )?;
    Ok(outcome.sequence)
}

/// Inspect the selected outgoing change against the exchange state it names.
/// A fresh request captures the selected native documents and requires the
/// native readset unchanged since its local cut. A retained candidate whose
/// preparation never committed was captured and checked that way under the
/// writer before its interrupted commit; custody admits only this exact
/// request since then, so its bytes are reused instead of native state that a
/// later local edit may have changed. Exchange-owned facts are rechecked.
pub(super) fn inspect_on(
    connection: &Connection,
    binding_id: &ObjectId,
    intent: &DeltaIntent,
    retained: Option<&DeltaCandidatePayload>,
) -> Result<DeltaCandidatePayload, StoreError> {
    let binding = view::read::binding_on(
        connection,
        binding_id,
        &mut view::read::ReadBudget::default(),
    )?;
    intent.validate(binding_id, &binding)?;
    require(
        intent.direction == Direction::Outgoing,
        "outgoing delta inspection requires an outgoing request",
    )?;
    super::local_store(connection, &binding)?;
    let DeltaBasis::Outgoing {
        export_before,
        confirmed_before,
    } = &intent.basis
    else {
        return Err(invalid("outgoing delta basis missing"));
    };
    super::readset::historical_on(connection, &binding.project, &intent.local_cut)?;
    confirmed_on(
        connection,
        binding_id,
        &binding,
        confirmed_before.as_deref(),
    )?;
    let documents = if let Some(retained) = retained {
        retained.validate(binding_id, &binding, intent)?;
        retained.documents.clone()
    } else {
        capture::capture_on(
            connection,
            &binding.project,
            &binding.profile,
            intent.candidate.changed.clone(),
        )?
        .documents
        .into_iter()
        .map(|(id, task)| {
            (
                id,
                match task {
                    CapturedTask::Present(task) => Some(*task),
                    CapturedTask::Absent | CapturedTask::Retired => None,
                },
            )
        })
        .collect::<BTreeMap<_, _>>()
    };
    let mut seeds = documents.keys().copied().collect::<BTreeSet<_>>();
    for task in documents.values().flatten() {
        seeds.extend(
            closure::graph::document_edges(task)
                .into_iter()
                .map(|(_, target)| target),
        );
    }
    let prior = closure::membership::capture_on(
        connection,
        binding_id,
        TaskSyncMembershipView::Export,
        seeds.clone(),
    )?;
    let head = closure::membership::head::read_on(
        connection,
        &prior.head,
        binding_id,
        &binding,
        TaskSyncMembershipView::Export,
    )?;
    require(
        head.basis == closure::membership::head::MembershipBasis::Export(export_before.clone()),
        "outgoing prepared export basis changed",
    )?;
    let mut root = prior.membership.inventory_root;
    for (id, next) in &documents {
        if let Some(old) = prior.component.documents.get(id) {
            root = root.excluding(old)?;
        } else {
            require(
                next.is_some(),
                "outgoing delta removes an absent export member",
            )?;
        }
        if let Some(next) = next {
            root = root.including(next)?;
        }
    }
    require(
        root == intent.candidate.output.inventory_root,
        "outgoing native membership differs from candidate",
    )?;
    seeds.extend(prior.component.documents.keys().copied());
    seeds.extend(prior.component.missing.iter().copied());
    closure::overlay::overlay(&binding.project, prior.component, &documents)?;
    if retained.is_none() {
        let native =
            closure::native::capture_on(connection, &binding.project, &binding.profile, seeds)?;
        super::readset::unchanged_on(
            connection,
            &binding.project,
            &intent.local_cut,
            native.documents.keys().chain(&native.missing).copied(),
        )?;
        super::scope::snapshots(&binding.project, &native)?;
    }
    let payload = DeltaCandidatePayload {
        format: DELTA_PAYLOAD_FORMAT.into(),
        project_id: binding.project.clone(),
        operation: intent.operation,
        output: intent.candidate.output.clone(),
        documents,
    };
    payload.validate(binding_id, &binding, intent)?;
    require(
        retained.is_none_or(|retained| *retained == payload),
        "retained outgoing delta differs from its exact request",
    )?;
    Ok(payload)
}
