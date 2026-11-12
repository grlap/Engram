//! Refuse a whole immutable closure when its selected disclosure forbids prose.

use std::collections::BTreeMap;

use super::{Connection, RecordEnvelope, RecordKind, Result, invalid};
use crate::task_sync::{notes, progress::ExportProfile};
use crate::{ActorContext, Scope, WorkId, WorkItem};

mod content;

pub(super) fn validate<'a>(
    connection: &Connection,
    profile: &ExportProfile,
    records: impl Iterator<Item = &'a RecordEnvelope> + Clone,
    retained_labels: &BTreeMap<WorkId, Vec<String>>,
) -> Result<BTreeMap<WorkId, Vec<String>>> {
    profile
        .validate()
        .map_err(|_| invalid("native evidence disclosure profile is invalid"))?;
    // A foreign dependency may have no native task. Its original snapshots still
    // carry its labels; never install those snapshots as native state to check it.
    let mut labels = retained_labels.clone();
    for record in records.clone() {
        let item = match record.kind() {
            RecordKind::WorkEvent => {
                let event: crate::WorkEvent = serde_json::from_slice(record.canonical_bytes())?;
                if (event.run.is_some() || event.claim.is_some() || event.handoff_offer.is_some())
                    && (profile.origin_machine.is_none()
                        || notes::restricted(&event.work.labels)
                        || actor_excluded(&event.actor))
                {
                    return Err(invalid("native evidence activity disclosure refused"));
                }
                if event.run.is_some() || event.claim.is_some() || event.handoff_offer.is_some() {
                    require_work_disclosure(connection, event.work_id, true)?;
                }
                Some(event.work)
            }
            RecordKind::WorkItemRevision => Some(serde_json::from_slice::<WorkItem>(
                record.canonical_bytes(),
            )?),
            RecordKind::WorkRestoredRecord => {
                let restored: crate::graph_snapshot::RestoredRecord =
                    serde_json::from_slice(record.canonical_bytes())?;
                labels
                    .entry(restored.work_id)
                    .or_default()
                    .extend(restored.item.labels);
                None
            }
            _ => None,
        };
        if let Some(item) = item {
            labels.entry(item.work_id).or_default().extend(item.labels);
        }
    }
    for values in labels.values_mut() {
        values.sort();
        values.dedup();
    }
    // Pages can contain only a task revision, with no note or activity record
    // to trigger their checks. Retained closure labels also apply to every page.
    for (work, values) in &labels {
        if notes::restricted(values) {
            return Err(invalid("native evidence task disclosure refused"));
        }
        require_work_disclosure(connection, *work, true)?;
    }
    for record in records {
        validate_record(connection, profile, &labels, record)?;
    }
    Ok(labels)
}

fn validate_record(
    connection: &Connection,
    profile: &ExportProfile,
    labels: &BTreeMap<WorkId, Vec<String>>,
    record: &RecordEnvelope,
) -> Result<()> {
    content::validate(profile, record)?;
    let check = |work, body: &str, refs: &[String], actor: &ActorContext| {
        check_note(connection, profile, labels, work, body, refs, actor)
    };
    match record.kind() {
        RecordKind::MemoryVersion => {
            let memory: crate::MemoryVersion = serde_json::from_slice(record.canonical_bytes())?;
            let Scope::Work { work, .. } = memory.scope else {
                return Err(invalid(
                    "native evidence memory disclosure requires work scope",
                ));
            };
            check(work, &memory.body, &memory.refs, &memory.actor)?;
            if notes::restricted(&memory.tags)
                || matches!(
                    memory.sensitivity,
                    crate::Sensitivity::Restricted | crate::Sensitivity::SecretRef
                )
                || notes::prose_exclusion(&memory.title).is_some()
            {
                return Err(invalid("native evidence memory disclosure refused"));
            }
        }
        RecordKind::WorkEvidence => {
            let note: crate::WorkEvidence = serde_json::from_slice(record.canonical_bytes())?;
            check(note.work_id, &note.summary, &note.refs, &note.actor)?;
        }
        RecordKind::WorkObservation => {
            let note: crate::domain::WorkObservation =
                serde_json::from_slice(record.canonical_bytes())?;
            check(note.work_id, &note.summary, &note.refs, &note.actor)?;
        }
        RecordKind::WorkCheckpoint => {
            let note: crate::WorkCheckpoint = serde_json::from_slice(record.canonical_bytes())?;
            check(note.work_id, &note.summary, &[], &note.actor)?;
        }
        RecordKind::WorkRestoredEvidence => {
            let note: crate::RestoredWorkEvidence =
                serde_json::from_slice(record.canonical_bytes())?;
            check(note.work_id, &note.summary, &note.refs, &note.actor)?;
        }
        RecordKind::WorkRestoredRecord => {
            let restored: crate::graph_snapshot::RestoredRecord =
                serde_json::from_slice(record.canonical_bytes())?;
            for note in restored.history.notes {
                check(restored.work_id, &note.summary, &note.refs, &note.actor)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn actor_excluded(actor: &ActorContext) -> bool {
    notes::author_exclusion(&actor.actor_id).is_some()
        || notes::author_exclusion(&actor.actor_kind).is_some()
}

fn check_note(
    connection: &Connection,
    profile: &ExportProfile,
    labels: &BTreeMap<WorkId, Vec<String>>,
    work: WorkId,
    body: &str,
    refs: &[String],
    actor: &ActorContext,
) -> Result<()> {
    if !profile.include_notes
        || !refs.is_empty()
        || notes::prose_exclusion(body).is_some()
        || actor_excluded(actor)
        || labels
            .get(&work)
            .is_some_and(|labels| notes::restricted(labels))
    {
        return Err(invalid("native evidence note disclosure refused"));
    }
    require_work_disclosure(connection, work, labels.contains_key(&work))
}

fn require_work_disclosure(
    connection: &Connection,
    work: WorkId,
    has_snapshot: bool,
) -> Result<()> {
    match crate::storage::work::query::load_work_item(connection, work) {
        Ok(item) if notes::restricted(&item.labels) => {
            Err(invalid("native evidence note disclosure refused"))
        }
        Ok(_) => Ok(()),
        Err(crate::storage::StoreError::WorkNotFound(_)) if has_snapshot => Ok(()),
        Err(error) => Err(error.into()),
    }
}
