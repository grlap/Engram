//! Witness native history at the selected dense cuts; never substitute live state.

use std::collections::BTreeMap;

use super::{
    Connection, NativeEvidenceCaptureRequest, Result, Selection, invalid, params, stored_id,
};
use crate::storage::work::query;
use crate::task_sync::evidence::witnesses::{
    FeedBoundary, FeedMember, GoverningBasis, GoverningSnapshotWitness, OriginFeed, OriginFeedCut,
    OriginFeedSegment, SnapshotAvailability,
};
use crate::task_sync::evidence::{RecordKind, limits};
use crate::{CanonicalObject, FeedId, ObjectId, WorkItem};

pub(super) fn within_cut(
    connection: &Connection,
    request: &NativeEvidenceCaptureRequest,
    id: &ObjectId,
) -> Result<bool> {
    for cut in &request.feeds {
        let (kind, feed) = query::feed_parts(&cut.feed);
        let present: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM work_feed_entries WHERE feed_kind=?1 AND feed_id=?2 AND object_id=?3 AND position<=?4)",
            params![kind, feed, id.as_str(), cut.through], |row| row.get(0),
        )?;
        if present {
            return Ok(true);
        }
    }
    Ok(false)
}

fn feed_scope(selection: &Selection<'_>, feed: &FeedId) -> Result<()> {
    let project = match feed {
        FeedId::Project(project) => project.clone(),
        FeedId::RootWork(work) => query::load_work_item(selection.connection, *work)?.project_id,
        FeedId::RunExecution(run) => {
            let run = query::load_work_run(selection.connection, *run)?;
            query::load_work_item(selection.connection, run.work_id)?.project_id
        }
    };
    if project != *selection.project {
        return Err(invalid("native evidence feed belongs to another project"));
    }
    Ok(())
}

pub(super) fn feeds(selection: &mut Selection<'_>) -> Result<Vec<ObjectId>> {
    let mut cuts = Vec::new();
    for cut in &selection.request.feeds {
        feed_scope(selection, &cut.feed)?;
        let (kind, feed) = query::feed_parts(&cut.feed);
        let head: Option<i64> = selection
            .connection
            .query_row(
                "SELECT position FROM work_feed_heads WHERE feed_kind=?1 AND feed_id=?2",
                params![kind, feed],
                |row| row.get(0),
            )
            .optional()?;
        if head.is_none_or(|head| cut.through > head) {
            return Err(invalid("native evidence feed cut is unavailable"));
        }
        let mut entries = Vec::new();
        {
            let mut statement = selection.connection.prepare(
                "SELECT position,object_kind,object_id FROM work_feed_entries WHERE feed_kind=?1 AND feed_id=?2 AND position<=?3 ORDER BY position",
            )?;
            for row in statement.query_map(params![kind, feed, cut.through], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })? {
                let (position, object_kind, object_id) = row?;
                if position != i64::try_from(entries.len()).unwrap_or(i64::MAX) + 1 {
                    return Err(invalid("native evidence feed contains an interior gap"));
                }
                RecordKind::parse(&object_kind)?;
                let object_id = stored_id(object_id)?;
                super::super::require_native(selection.connection, &object_id)?;
                limits::enforce(
                    limits::LimitKind::BatchTooLarge,
                    None,
                    limits::Dimension::Objects,
                    entries.len() + 1,
                    limits::MAX_BATCH_OBJECTS,
                )
                .map_err(crate::task_sync::evidence::EvidenceError::from)?;
                entries.push(FeedMember {
                    position,
                    object_kind,
                    object_id,
                });
            }
        }
        if i64::try_from(entries.len()).ok() != Some(cut.through) {
            return Err(invalid("native evidence cut endpoint is missing"));
        }
        selection
            .pending
            .extend(entries.iter().map(|entry| entry.object_id.clone()));
        let source = OriginFeed {
            origin: selection.request.origin.clone(),
            project_id: selection.project.clone(),
            feed: cut.feed.clone(),
        };
        let prefix = FeedBoundary {
            source: source.clone(),
            through: 0,
            segment: None,
            endpoint: None,
        };
        let segments = if entries.is_empty() {
            Vec::new()
        } else {
            let segment = OriginFeedSegment {
                source: source.clone(),
                from_exclusive: 0,
                through: cut.through,
                predecessor: None,
                anchor: None,
                entries: entries.clone(),
            };
            segment.validate(&prefix)?;
            vec![selection.mint(RecordKind::OriginFeedSegment, &segment)?]
        };
        let witness = OriginFeedCut {
            source,
            through: cut.through,
            endpoint: entries.last().cloned(),
            segments,
        };
        let segments = witness
            .segments
            .iter()
            .map(|id| {
                let record = selection
                    .records
                    .get(id)
                    .ok_or_else(|| invalid("prepared native segment is missing"))?;
                Ok((
                    id.clone(),
                    serde_json::from_slice::<OriginFeedSegment>(record.canonical_bytes())?,
                ))
            })
            .collect::<Result<BTreeMap<_, _>>>()?;
        witness.validate_chain(&prefix, &segments, &BTreeMap::new())?;
        cuts.push(selection.mint(RecordKind::OriginFeedCut, &witness)?);
    }
    Ok(cuts)
}

use rusqlite::OptionalExtension;

pub(super) fn governing(selection: &mut Selection<'_>) -> Result<Vec<ObjectId>> {
    let evaluations = selection
        .records
        .values()
        .filter(|record| {
            record.kind() == RecordKind::AcceptanceEvaluation
                && record.origin() == &selection.request.origin
        })
        .map(|record| {
            Ok((
                record.object_id().clone(),
                serde_json::from_slice::<crate::AcceptanceEvaluation>(record.canonical_bytes())?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    let mut result = Vec::new();
    for (id, evaluation) in evaluations {
        if !selection.request.feeds.iter().any(|cut| {
            cut.feed == evaluation.evaluated_cut.feed
                && cut.through >= evaluation.evaluated_cut.position
        }) {
            return Err(invalid("native evaluation governing cut is unavailable"));
        }
        let mut snapshot = None;
        for record in selection
            .records
            .values()
            .filter(|record| record.origin() == &selection.request.origin)
        {
            let item = match record.kind() {
                RecordKind::WorkEvent => {
                    serde_json::from_slice::<crate::WorkEvent>(record.canonical_bytes())?.work
                }
                RecordKind::WorkItemRevision => {
                    serde_json::from_slice::<WorkItem>(record.canonical_bytes())?
                }
                _ => continue,
            };
            if item.work_id == evaluation.work_id
                && item.revision == evaluation.work_revision
                && CanonicalObject::freeze(&item)?.key() == &evaluation.work_revision_hash
            {
                snapshot = Some(item);
                break;
            }
        }
        let snapshot = snapshot
            .ok_or_else(|| invalid("native evaluation governing snapshot is unavailable"))?;
        let basis = GoverningBasis {
            evaluation: id,
            project_id: evaluation.project_id,
            work_id: evaluation.work_id,
            work_revision: evaluation.work_revision,
            content_fingerprint: evaluation.work_revision_hash,
        };
        let witness = GoverningSnapshotWitness {
            basis: basis.clone(),
            snapshot: SnapshotAvailability::Exact {
                source_snapshot: snapshot.source_snapshot_id.clone(),
                snapshot: Box::new(snapshot),
            },
        };
        witness.validate(&basis)?;
        result.push(selection.mint(RecordKind::GoverningSnapshotWitness, &witness)?);
    }
    Ok(result)
}
