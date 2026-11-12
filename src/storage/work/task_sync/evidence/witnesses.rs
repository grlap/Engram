//! Qualified historical feed context. These tables never feed native execution.

use super::{
    Connection, EvidenceError, EvidencePack, ObjectId, OptionalExtension, PersistenceError,
    ProjectId, RecordEnvelope, RecordKind, Result, StoreError, invalid, object_on, params,
    stored_id,
};
use crate::task_sync::evidence::witnesses::{
    FeedBoundary, FeedMember, GoverningBasis, GoverningSnapshotWitness, OriginFeed, OriginFeedCut,
    OriginFeedSegment, WitnessProblem,
};
use std::collections::BTreeMap;

pub(super) fn create_schema(connection: &Connection) -> std::result::Result<(), StoreError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS work_task_sync_evidence_witnesses (
             object_id TEXT PRIMARY KEY REFERENCES objects(object_id),
             object_kind TEXT NOT NULL,
             project_id TEXT NOT NULL,
             origin_id TEXT NOT NULL REFERENCES objects(object_id)
         ) STRICT;
         CREATE TABLE IF NOT EXISTS work_task_sync_evidence_feed_members (
             project_id TEXT NOT NULL,
             origin_id TEXT NOT NULL REFERENCES objects(object_id),
             feed TEXT NOT NULL,
             position INTEGER NOT NULL CHECK(position>0),
             object_kind TEXT NOT NULL,
             object_id TEXT NOT NULL REFERENCES objects(object_id),
             segment_id TEXT NOT NULL REFERENCES objects(object_id),
             PRIMARY KEY(project_id,origin_id,feed,position)
         ) STRICT;",
    )?;
    Ok(())
}

pub(super) fn clear(connection: &Connection) -> Result<()> {
    connection.execute_batch("DELETE FROM work_task_sync_evidence_feed_members; DELETE FROM work_task_sync_evidence_witnesses;")?;
    Ok(())
}

fn problem(problem: WitnessProblem) -> PersistenceError {
    EvidenceError::Witness(problem).into()
}
fn feed_key(source: &OriginFeed) -> Result<String> {
    Ok(serde_json::to_string(&source.feed)?)
}

fn qualified_body(
    connection: &Connection,
    project: &ProjectId,
    origin: &ObjectId,
    id: &ObjectId,
    kind: RecordKind,
) -> Result<Vec<u8>> {
    let qualified: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_task_sync_evidence_members member
         JOIN work_task_sync_evidence_packs pack ON pack.pack_id=member.pack_id
         WHERE pack.project_id=?1 AND member.origin_id=?2 AND member.object_id=?3 AND member.object_kind=?4)",
        params![project.0,origin.as_str(),id.as_str(),kind.as_str()], |row| row.get(0))?;
    if !qualified {
        return Err(invalid(
            "witness dependency lacks its original qualified membership",
        ));
    }
    let (actual, bytes) =
        object_on(connection, id)?.ok_or_else(|| invalid("witness dependency is missing"))?;
    if actual != kind.as_str() {
        return Err(EvidenceError::RecordConflict(id.clone()).into());
    }
    Ok(bytes)
}

fn scope(source: &OriginFeed, project: &ProjectId, origin: &ObjectId) -> Result<()> {
    if source.project_id != *project || source.origin != *origin {
        return Err(problem(WitnessProblem::SourceScopeDiffers));
    }
    Ok(())
}

fn segment_on(
    connection: &Connection,
    source: &OriginFeed,
    id: &ObjectId,
) -> Result<OriginFeedSegment> {
    let body = qualified_body(
        connection,
        &source.project_id,
        &source.origin,
        id,
        RecordKind::OriginFeedSegment,
    )?;
    let segment: OriginFeedSegment = serde_json::from_slice(&body)?;
    if segment.source != *source {
        return Err(problem(WitnessProblem::SourceScopeDiffers));
    }
    let indexed: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM work_task_sync_evidence_witnesses WHERE object_id=?1 AND object_kind=?2 AND project_id=?3 AND origin_id=?4)",params![id.as_str(),RecordKind::OriginFeedSegment.as_str(),source.project_id.0,source.origin.as_str()],|row| row.get(0))?;
    if !indexed {
        return Err(problem(WitnessProblem::CutUnavailable));
    }
    Ok(segment)
}

fn previous(connection: &Connection, segment: &OriginFeedSegment) -> Result<FeedBoundary> {
    match &segment.predecessor {
        None => Ok(FeedBoundary {
            source: segment.source.clone(),
            through: 0,
            segment: None,
            endpoint: None,
        }),
        Some(id) => {
            let prior = segment_on(connection, &segment.source, id)?;
            Ok(FeedBoundary {
                source: prior.source,
                through: prior.through,
                segment: Some(id.clone()),
                endpoint: prior.entries.last().cloned(),
            })
        }
    }
}

fn known_member(
    connection: &Connection,
    source: &OriginFeed,
    position: i64,
) -> Result<Option<FeedMember>> {
    let found: Option<(String,String)> = connection.query_row(
        "SELECT object_kind,object_id FROM work_task_sync_evidence_feed_members WHERE project_id=?1 AND origin_id=?2 AND feed=?3 AND position=?4",
        params![source.project_id.0,source.origin.as_str(),feed_key(source)?,position], |row| Ok((row.get(0)?,row.get(1)?))).optional()?;
    found
        .map(|(object_kind, id)| {
            Ok(FeedMember {
                position,
                object_kind,
                object_id: stored_id(id)?,
            })
        })
        .transpose()
}

fn validate_segment(
    connection: &Connection,
    project: &ProjectId,
    record: &RecordEnvelope,
    writing: bool,
) -> Result<()> {
    let segment: OriginFeedSegment = serde_json::from_slice(record.canonical_bytes())?;
    scope(&segment.source, project, record.origin())?;
    segment.validate(&previous(connection, &segment)?)?;
    for entry in &segment.entries {
        qualified_body(
            connection,
            project,
            record.origin(),
            &entry.object_id,
            RecordKind::parse(&entry.object_kind)?,
        )?;
        match known_member(connection, &segment.source, entry.position)? {
            Some(known) if known != *entry => {
                return Err(problem(WitnessProblem::SourceFeedConflict {
                    position: entry.position,
                }));
            }
            None if !writing => {
                return Err(problem(WitnessProblem::SourceFeedGap {
                    position: entry.position,
                }));
            }
            _ => {}
        }
        if writing {
            connection.execute("INSERT INTO work_task_sync_evidence_feed_members(project_id,origin_id,feed,position,object_kind,object_id,segment_id) VALUES (?1,?2,?3,?4,?5,?6,?7) ON CONFLICT(project_id,origin_id,feed,position) DO NOTHING",
                params![project.0,record.origin().as_str(),feed_key(&segment.source)?,entry.position,entry.object_kind,entry.object_id.as_str(),record.object_id().as_str()])?;
        }
    }
    Ok(())
}

fn validate_cut(
    connection: &Connection,
    project: &ProjectId,
    record: &RecordEnvelope,
) -> Result<()> {
    let cut: OriginFeedCut = serde_json::from_slice(record.canonical_bytes())?;
    scope(&cut.source, project, record.origin())?;
    let mut segments = BTreeMap::new();
    for id in &cut.segments {
        segments.insert(id.clone(), segment_on(connection, &cut.source, id)?);
    }
    let prefix = if let Some(first) = cut.segments.first() {
        previous(connection, &segments[first])?
    } else {
        FeedBoundary {
            source: cut.source.clone(),
            through: 0,
            segment: None,
            endpoint: None,
        }
    };
    let mut known = BTreeMap::new();
    if let Some(endpoint) = &prefix.endpoint {
        let member = known_member(connection, &cut.source, endpoint.position)?
            .ok_or_else(|| problem(WitnessProblem::CutUnavailable))?;
        known.insert(member.position, member);
    }
    for segment in segments.values() {
        for entry in &segment.entries {
            let member =
                known_member(connection, &cut.source, entry.position)?.ok_or_else(|| {
                    problem(WitnessProblem::SourceFeedGap {
                        position: entry.position,
                    })
                })?;
            known.insert(member.position, member);
        }
    }
    cut.validate_chain(&prefix, &segments, &known)?;
    Ok(())
}

fn validate_governing(
    connection: &Connection,
    project: &ProjectId,
    record: &RecordEnvelope,
) -> Result<()> {
    let witness: GoverningSnapshotWitness = serde_json::from_slice(record.canonical_bytes())?;
    let bytes = qualified_body(
        connection,
        project,
        record.origin(),
        &witness.basis.evaluation,
        RecordKind::AcceptanceEvaluation,
    )?;
    let evaluation: crate::AcceptanceEvaluation = serde_json::from_slice(&bytes)?;
    let expected = GoverningBasis {
        evaluation: witness.basis.evaluation.clone(),
        project_id: evaluation.project_id,
        work_id: evaluation.work_id,
        work_revision: evaluation.work_revision,
        content_fingerprint: evaluation.work_revision_hash,
    };
    if expected.project_id != *project {
        return Err(problem(WitnessProblem::SourceScopeDiffers));
    }
    witness.validate(&expected)?;
    if let crate::task_sync::evidence::witnesses::SnapshotAvailability::Exact {
        source_snapshot: Some(id),
        ..
    } = &witness.snapshot
    {
        let bytes = qualified_body(
            connection,
            project,
            record.origin(),
            id,
            RecordKind::WorkSourceSnapshot,
        )?;
        let _: crate::WorkSourceSnapshot = serde_json::from_slice(&bytes)?;
    }
    Ok(())
}

fn is_witness(kind: RecordKind) -> bool {
    matches!(
        kind,
        RecordKind::OriginFeedSegment
            | RecordKind::OriginFeedCut
            | RecordKind::GoverningSnapshotWitness
    )
}

pub(super) fn retain_records_on<'a>(
    connection: &Connection,
    project: &ProjectId,
    records: impl IntoIterator<Item = &'a RecordEnvelope>,
) -> Result<()> {
    let records = records.into_iter().collect::<Vec<_>>();
    // Segments may arrive in arbitrary pack order. Resolve only selected pending
    // links; retained prefixes are keyed and are never reloaded as full history.
    let mut pending = BTreeMap::new();
    for &record in records
        .iter()
        .filter(|record| record.kind() == RecordKind::OriginFeedSegment)
    {
        let segment: OriginFeedSegment = serde_json::from_slice(record.canonical_bytes())?;
        if let Some((prior, _)) = pending.insert(record.object_id().clone(), (record, segment))
            && prior.origin() != record.origin()
        {
            return Err(invalid("witness origin qualification differs"));
        }
    }
    let mut dependents: BTreeMap<ObjectId, Vec<ObjectId>> = BTreeMap::new();
    let mut ready = std::collections::VecDeque::new();
    for (id, (_, segment)) in &pending {
        if let Some(prior) = &segment.predecessor
            && pending.contains_key(prior)
        {
            dependents
                .entry(prior.clone())
                .or_default()
                .push(id.clone());
        } else {
            ready.push_back(id.clone());
        }
    }
    while let Some(id) = ready.pop_front() {
        let (record, _) = pending
            .remove(&id)
            .ok_or_else(|| problem(WitnessProblem::ContinuityDiffers))?;
        validate_segment(connection, project, record, true)?;
        index(connection, project, record)?;
        if let Some(children) = dependents.remove(&id) {
            ready.extend(children);
        }
    }
    if !pending.is_empty() {
        return Err(problem(WitnessProblem::ContinuityDiffers));
    }
    for &record in records.iter().filter(|record| {
        is_witness(record.kind()) && record.kind() != RecordKind::OriginFeedSegment
    }) {
        match record.kind() {
            RecordKind::OriginFeedCut => validate_cut(connection, project, record)?,
            RecordKind::GoverningSnapshotWitness => {
                validate_governing(connection, project, record)?;
            }
            _ => unreachable!(),
        }
        index(connection, project, record)?;
    }
    Ok(())
}

/// Cold repair reads one original at a time. Valid segment predecessors always
/// have a smaller boundary; cuts and snapshots are checked after every segment.
pub(super) fn rebuild_on(connection: &Connection) -> Result<()> {
    let mut statement = connection.prepare(
        "SELECT DISTINCT pack.project_id,member.origin_id,member.object_id,member.object_kind,original.canonical_json
         FROM work_task_sync_evidence_members member
         JOIN work_task_sync_evidence_packs pack ON pack.pack_id=member.pack_id
         JOIN objects original ON original.object_id=member.object_id
         WHERE member.object_kind IN ('evidence_origin_feed_segment','evidence_origin_feed_cut','evidence_governing_snapshot')
         ORDER BY CASE member.object_kind WHEN 'evidence_origin_feed_segment' THEN 0 ELSE 1 END,
         json_extract(original.canonical_json,'$.from_exclusive'),member.object_id",
    )?;
    for row in statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Vec<u8>>(4)?,
        ))
    })? {
        let (project, origin, id, kind, bytes) = row?;
        let record =
            RecordEnvelope::from_canonical(stored_id(origin)?, stored_id(id)?, &kind, bytes)?;
        retain_records_on(connection, &ProjectId(project), [&record])?;
    }
    Ok(())
}

fn index(connection: &Connection, project: &ProjectId, record: &RecordEnvelope) -> Result<()> {
    let actual: Option<(String,String,String)> = connection.query_row("SELECT object_kind,project_id,origin_id FROM work_task_sync_evidence_witnesses WHERE object_id=?1",[record.object_id().as_str()],|row| Ok((row.get(0)?,row.get(1)?,row.get(2)?))).optional()?;
    let expected = (
        record.kind().as_str().to_owned(),
        project.0.clone(),
        record.origin().to_string(),
    );
    if let Some(actual) = actual {
        if actual != expected {
            return Err(invalid("witness origin qualification differs"));
        }
    } else {
        connection.execute("INSERT INTO work_task_sync_evidence_witnesses(object_id,object_kind,project_id,origin_id) VALUES (?1,?2,?3,?4)",params![record.object_id().as_str(),expected.0,expected.1,expected.2])?;
    }
    Ok(())
}

pub(super) fn verify_pack(connection: &Connection, pack: &EvidencePack) -> Result<()> {
    for record in pack
        .records()
        .iter()
        .filter(|record| is_witness(record.kind()))
    {
        let actual: (String,String,String) = connection.query_row("SELECT object_kind,project_id,origin_id FROM work_task_sync_evidence_witnesses WHERE object_id=?1",[record.object_id().as_str()],|row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)))?;
        if actual
            != (
                record.kind().as_str().to_owned(),
                pack.project_id().0.clone(),
                record.origin().to_string(),
            )
        {
            return Err(invalid("witness projection differs"));
        }
        match record.kind() {
            RecordKind::OriginFeedSegment => {
                validate_segment(connection, pack.project_id(), record, false)?;
            }
            RecordKind::OriginFeedCut => validate_cut(connection, pack.project_id(), record)?,
            RecordKind::GoverningSnapshotWitness => {
                validate_governing(connection, pack.project_id(), record)?;
            }
            _ => unreachable!(),
        }
    }
    Ok(())
}

pub(super) fn verify_counts(connection: &Connection) -> Result<()> {
    let extras: bool = connection.query_row("SELECT EXISTS(SELECT 1 FROM work_task_sync_evidence_witnesses witness WHERE NOT EXISTS(SELECT 1 FROM work_task_sync_evidence_members member WHERE member.object_id=witness.object_id)) OR EXISTS(SELECT 1 FROM work_task_sync_evidence_feed_members member JOIN objects segment ON segment.object_id=member.segment_id WHERE segment.object_kind!='evidence_origin_feed_segment' OR NOT EXISTS(SELECT 1 FROM json_each(segment.canonical_json,'$.entries') entry WHERE json_extract(entry.value,'$.position')=member.position AND json_extract(entry.value,'$.object_id')=member.object_id AND json_extract(entry.value,'$.object_kind')=member.object_kind))",[],|row| row.get(0))?;
    if extras {
        return Err(invalid(
            "evidence witness projection contains unrelated rows",
        ));
    }
    // Entry equality alone does not qualify an extra row under another source.
    // Check once per segment/scope, rather than decoding it for every entry.
    let mut statement = connection.prepare(
        "SELECT DISTINCT project_id,origin_id,feed,segment_id
         FROM work_task_sync_evidence_feed_members",
    )?;
    for row in statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
        ))
    })? {
        let (project, origin, feed, segment) = row?;
        let source = OriginFeed {
            project_id: ProjectId(project),
            origin: stored_id(origin)?,
            feed: serde_json::from_str(&feed)?,
        };
        let segment = segment_on(connection, &source, &stored_id(segment)?)?;
        if feed != feed_key(&segment.source)? {
            return Err(invalid("evidence feed projection source differs"));
        }
    }
    Ok(())
}
