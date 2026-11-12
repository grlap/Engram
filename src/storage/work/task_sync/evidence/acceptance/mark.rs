//! Recover item-wide mark authorship without widening run-holder affiliation.

use super::{History, Result, params};
use crate::domain::{AcceptanceEvaluation, SessionId, WorkEvent, WorkTransition};
use crate::{FeedId, ObjectId};

pub(super) fn author(
    history: &History<'_>,
    record: &AcceptanceEvaluation,
    evaluation: &ObjectId,
) -> Result<Option<SessionId>> {
    let feed = FeedId::Project(history.project.clone());
    let Some(cut) = history.cut(&feed) else {
        return Ok(None);
    };
    // The mark must already have a provable author when the evaluation was
    // recorded. A later revision cannot supply missing historical authority.
    let position: Option<i64> = history.connection.query_row(
        "SELECT MAX(position) FROM work_task_sync_evidence_feed_members
         WHERE project_id=?1 AND origin_id=?2 AND feed=?3
           AND object_id=?4 AND object_kind='acceptance_evaluation'",
        params![
            history.project.0,
            history.request.origin.as_str(),
            serde_json::to_string(&feed)?,
            evaluation.as_str()
        ],
        |row| row.get(0),
    )?;
    let Some(position) = position.filter(|position| *position <= cut.through) else {
        return Ok(None);
    };
    let mut original = Vec::new();
    let mut current = Vec::new();
    let mut invalid = false;
    let mut budget = crate::task_sync::evidence::limits::StagingBudget::default();
    if !history.walk_current(&feed, |at, id, kind| {
        if kind != "work_event" {
            return Ok(());
        }
        let bytes = history.bytes(id, kind)?;
        budget
            .reserve(bytes.len())
            .map_err(crate::task_sync::evidence::EvidenceError::from)?;
        let event: WorkEvent = serde_json::from_slice(&bytes)?;
        if event.project_id != *history.project {
            invalid = true;
        }
        if event.work_id != record.work_id {
            return Ok(());
        }
        if event.root_id != record.root_id {
            invalid = true;
        }
        // Keep the first event as the baseline, including a restored or
        // detached mark whose author no native event establishes. Thereafter
        // only creation/revision can change the mark, just as in native use.
        if current.is_empty()
            || matches!(
                event.transition,
                WorkTransition::Created { .. } | WorkTransition::Revised { .. }
            )
        {
            if at < position {
                original.push(event.clone());
            }
            current.push(event);
        }
        Ok(())
    })? || invalid
    {
        return Ok(None);
    }
    let author =
        crate::storage::work::acceptance_evaluation::historical_evaluation_mark_author(original);
    let current_author =
        crate::storage::work::acceptance_evaluation::historical_evaluation_mark_author(current);
    // Current facts may withdraw eligibility, never invent an earlier author.
    Ok(author.filter(|author| current_author.as_ref() == Some(author)))
}
