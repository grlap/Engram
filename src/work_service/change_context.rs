//! Private context hydrated from canonical records, never persisted in delivery.

use super::{
    CompletionSeal, FeedId, ObjectId, SqliteStore, StoreError, WorkCheckpoint, WorkClaimState,
    WorkEvent, WorkEvidence, WorkFeedEntry, WorkTransition,
};
use crate::storage::WorkRecordAddress;

#[cfg(test)]
mod tests;

pub(super) const NOTE_CHECKPOINT_REASON: &str =
    "record note evidence and checkpoint ambient local work";
pub(super) const COMPLETION_CAPTURE_REASON: &str =
    "capture completion evidence for ambient local work";
pub(super) const COMPLETION_CHECKPOINT_REASON: &str =
    "checkpoint the exact completion evidence cut";

pub(super) fn hydrate(
    store: &SqliteStore,
    entry: &WorkFeedEntry,
    object: &serde_json::Value,
) -> Result<(Option<WorkRecordAddress>, Option<ObjectId>), StoreError> {
    let capture = |hash| Some(WorkRecordAddress { hash, member: None });
    match entry.object_kind.as_str() {
        "work_evidence" | "work_observation" | "work_restored_evidence" => {
            Ok((capture(entry.object_id.clone()), None))
        }
        "work_checkpoint" => {
            let checkpoint: WorkCheckpoint = serde_json::from_value(object.clone())?;
            let note = checkpoint_capture(store, &checkpoint)?;
            Ok((
                capture(note.unwrap_or_else(|| entry.object_id.clone())),
                None,
            ))
        }
        "work_event" => {
            let event: WorkEvent = serde_json::from_value(object.clone())?;
            match &event.transition {
                WorkTransition::Checkpointed { checkpoint: id } => {
                    let checkpoint: WorkCheckpoint = store
                        .get(id)?
                        .ok_or_else(|| invalid("checkpoint event's checkpoint is missing"))?;
                    let note = if checkpoint_event_matches(&event, &checkpoint)
                        && event.actor.source_tool == checkpoint.actor.source_tool
                        && event.actor.reason == checkpoint.actor.reason
                    {
                        checkpoint_capture(store, &checkpoint)?
                    } else {
                        None
                    };
                    Ok((note.and_then(capture), None))
                }
                WorkTransition::Completed { seal } => {
                    let seal: CompletionSeal = store
                        .get(seal)?
                        .ok_or_else(|| invalid("completion seal is missing"))?;
                    let Some(id) = &seal.checkpoint else {
                        return Ok((None, None));
                    };
                    let checkpoint: WorkCheckpoint = store
                        .get(id)?
                        .ok_or_else(|| invalid("completion checkpoint is missing"))?;
                    // This is attributed operation provenance, not authenticated
                    // authority. Ordinary checkpoints included by a seal are kept.
                    let owned = completion_owns_checkpoint(&event, &seal, &checkpoint);
                    Ok((None, owned.then(|| id.clone())))
                }
                _ => Ok((None, None)),
            }
        }
        _ => Ok((None, None)),
    }
}

fn checkpoint_capture(
    store: &SqliteStore,
    checkpoint: &WorkCheckpoint,
) -> Result<Option<ObjectId>, StoreError> {
    if capture_provenance(checkpoint).is_none()
        || checkpoint.acknowledged_run_position.feed != FeedId::RunExecution(checkpoint.run_id)
        || checkpoint.acknowledged_run_position.position <= 0
    {
        return Ok(None);
    }
    // One indexed point read per capture checkpoint in the bounded delivery page;
    // neither the run history nor operation receipts are scanned.
    let cut = &checkpoint.acknowledged_run_position;
    let entries = store.work_feed_between(&cut.feed, cut.position - 1, cut.position)?;
    let entry = entries
        .first()
        .ok_or_else(|| invalid("capture checkpoint cut has no event"))?;
    if entry.object_kind != "work_event" {
        return Ok(None);
    }
    let event: WorkEvent = store
        .get(&entry.object_id)?
        .ok_or_else(|| invalid("note event is missing"))?;
    let WorkTransition::EvidenceAdded { evidence } = &event.transition else {
        return Ok(None);
    };
    let note: WorkEvidence = store
        .get(evidence)?
        .ok_or_else(|| invalid("note evidence is missing"))?;
    Ok(capture_checkpoint_matches(&event, &note, checkpoint).then(|| evidence.clone()))
}

pub(super) fn capture_checkpoint_matches(
    event: &WorkEvent,
    note: &WorkEvidence,
    checkpoint: &WorkCheckpoint,
) -> bool {
    capture_provenance(checkpoint).is_some_and(|(tool, reason)| {
        note.actor.source_tool.as_deref() == Some(tool)
            && note.actor.reason == reason
            && event.actor.source_tool.as_deref() == Some(tool)
            && event.actor.reason == reason
    }) && checkpoint_event_matches(event, checkpoint)
        && note.work_id == checkpoint.work_id
        && note.run_id == checkpoint.run_id
        && note.claim_id == checkpoint.claim_id
        && note.claim_fence == checkpoint.claim_fence
        && note.actor.session_id == checkpoint.actor.session_id
}

fn checkpoint_event_matches(event: &WorkEvent, checkpoint: &WorkCheckpoint) -> bool {
    event.work_id == checkpoint.work_id
        && event.run_id == Some(checkpoint.run_id)
        && event.claim.as_ref().is_some_and(|claim| {
            claim.work_id == checkpoint.work_id
                && claim.run_id == checkpoint.run_id
                && claim.claim_id == checkpoint.claim_id
                && claim.fence == checkpoint.claim_fence
        })
        && event.actor.session_id == checkpoint.actor.session_id
}

fn capture_provenance(checkpoint: &WorkCheckpoint) -> Option<(&'static str, &'static str)> {
    match (
        checkpoint.actor.source_tool.as_deref(),
        checkpoint.actor.reason.as_str(),
    ) {
        (Some("work_update"), NOTE_CHECKPOINT_REASON) => {
            Some(("work_update", NOTE_CHECKPOINT_REASON))
        }
        (Some("work_complete"), COMPLETION_CHECKPOINT_REASON) => {
            Some(("work_complete", COMPLETION_CAPTURE_REASON))
        }
        _ => None,
    }
}

pub(super) fn completion_owns_checkpoint(
    event: &WorkEvent,
    seal: &CompletionSeal,
    checkpoint: &WorkCheckpoint,
) -> bool {
    event.work_id == seal.work_id
        && event.run_id == Some(seal.run_id)
        && event.claim.as_ref().is_some_and(|claim| {
            claim.work_id == seal.work_id
                && claim.run_id == seal.run_id
                && claim.claim_id == seal.claim_id
                // Completion stores the post-transition claim; the seal and
                // checkpoint retain the fence that authorized the operation.
                && claim.state == WorkClaimState::Completed
                && seal.completed_claim_fence() == Some(claim.fence)
        })
        && checkpoint.work_id == seal.work_id
        && checkpoint.run_id == seal.run_id
        && checkpoint.claim_id == seal.claim_id
        && checkpoint.claim_fence == seal.claim_fence
        && checkpoint.actor.session_id == seal.actor.session_id
        && checkpoint.actor.source_tool.as_deref() == Some("work_complete")
}

fn invalid(message: &str) -> StoreError {
    StoreError::InvalidWorkProjection(message.into())
}
