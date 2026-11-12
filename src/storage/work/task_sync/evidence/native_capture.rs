//! Explicit native selection, closed before any retained pack is published.

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::memory::Redactor;
use crate::storage::SqliteStore;
use crate::task_sync::evidence::{EvidencePack, RecordEnvelope, RecordKind, inventory, limits};
use crate::{ActorContext, CanonicalObject, FeedId, ObjectId, ProjectId, SessionId};
use serde::{Deserialize, Serialize};

use super::{Connection, Result, invalid, object_on, packs, params, stored_id};

mod context;
mod disclosure;
#[cfg(test)]
mod tests;

const CAPTURE_KIND: &str = "task_sync_native_evidence_capture";

/// An actual dense native feed boundary, never a destination delivery cursor.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeEvidenceFeed {
    pub feed: FeedId,
    pub through: i64,
}

/// The caller prepares one capture identity and repeats this exact selection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeEvidenceCaptureRequest {
    pub capture: ObjectId,
    pub origin: ObjectId,
    /// Explicit disclosure selection, immutable for this capture and its pages.
    pub profile: crate::task_sync::progress::ExportProfile,
    pub roots: Vec<ObjectId>,
    pub feeds: Vec<NativeEvidenceFeed>,
    /// A lower operation budget; the shared profile remains the upper bound.
    pub maximum_pack_records: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NativeEvidenceCursor {
    pub capture: ObjectId,
    /// An ordinal in this immutable capture, not a source-feed position.
    pub after: usize,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NativeEvidenceCapture {
    pub cursor: NativeEvidenceCursor,
    pub packs: Vec<ObjectId>,
    pub feed_cuts: Vec<ObjectId>,
    pub governing: Vec<ObjectId>,
    pub replayed: bool,
}

pub struct NativeEvidencePage {
    pub packs: Vec<EvidencePack>,
    pub next: Option<NativeEvidenceCursor>,
}

/// Private local construction, excluded from the interchange inventory.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Capture {
    project: ProjectId,
    request: NativeEvidenceCaptureRequest,
    actor: ActorContext,
    packs: Vec<ObjectId>,
    feed_cuts: Vec<ObjectId>,
    governing: Vec<ObjectId>,
    disclosure_labels: BTreeMap<crate::WorkId, Vec<String>>,
}

impl Capture {
    fn receipt(&self, replayed: bool) -> NativeEvidenceCapture {
        NativeEvidenceCapture {
            cursor: NativeEvidenceCursor {
                capture: self.request.capture.clone(),
                after: 0,
            },
            packs: self.packs.clone(),
            feed_cuts: self.feed_cuts.clone(),
            governing: self.governing.clone(),
            replayed,
        }
    }
}

impl SqliteStore {
    /// Freeze an explicitly attributed native selection and its complete context.
    /// Original records remain native; retention adds only immutable pack metadata.
    ///
    /// # Errors
    /// Refuses changed retries, unavailable history, foreign seeds and profile bounds.
    pub fn capture_native_evidence<R: Redactor>(
        &mut self,
        project: &ProjectId,
        session: &SessionId,
        actor: &ActorContext,
        request: &NativeEvidenceCaptureRequest,
        redactor: &R,
    ) -> Result<NativeEvidenceCapture> {
        crate::storage::admit_live_actor_session(actor)?;
        crate::storage::work::planning::assert_actor_session(actor, session)?;
        crate::storage::work::feeds::inspect_work_request(redactor, request, actor)?;
        let mut tx = self
            .connection
            .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)?;
        if let Some(old) = load(&tx, &request.capture)? {
            if old.project != *project || old.request != *request || old.actor != *actor {
                return Err(invalid("native evidence capture retry intent differs"));
            }
            let packs = old
                .packs
                .iter()
                .map(|id| packs::load_on(&tx, project, id))
                .collect::<Result<Vec<_>>>()?;
            disclosure::validate(
                &tx,
                &request.profile,
                packs.iter().flat_map(EvidencePack::records),
                &old.disclosure_labels,
            )?;
            return Ok(old.receipt(true));
        }
        validate_request(request)?;
        let origin = crate::storage::work::task_sync::producing_origin::load_on(&tx, project)?
            .ok_or_else(|| invalid("native evidence requires an explicit producing origin"))?;
        if origin.origin_id != request.origin {
            return Err(invalid("native evidence producing origin differs"));
        }
        let mut selected = Selection::new(&tx, project, request);
        for id in &request.roots {
            super::require_native(&tx, id)?;
            if !context::within_cut(&tx, request, id)? {
                return Err(invalid(
                    "native evidence seed is outside the selected feed cuts",
                ));
            }
            selected.pending.push_back(id.clone());
        }
        let feed_cuts = context::feeds(&mut selected)?;
        selected.close()?;
        let governing = context::governing(&mut selected)?;
        selected.close()?;
        let disclosure_labels = disclosure::validate(
            &tx,
            &request.profile,
            selected.records.values(),
            &BTreeMap::new(),
        )?;
        let prepared = prepare_packs(
            project,
            &request.origin,
            selected.records.values(),
            request.maximum_pack_records,
        )?;
        // Reuse the same closed-inventory check as incoming publication. No local
        // object can stand in for a missing transport member of this capture.
        inventory::plan_batch(&prepared, |_| Ok(None))?;
        for record in &selected.created {
            super::records::insert_on(&tx, record)?;
        }
        let admissions = prepared
            .iter()
            .map(|_| ObjectId::mint())
            .collect::<Vec<_>>();
        let batch = prepared.iter().zip(&admissions).collect::<Vec<_>>();
        super::retain_batch_on(&mut tx, project, &batch)?;
        let capture = Capture {
            project: project.clone(),
            request: request.clone(),
            actor: actor.clone(),
            packs: prepared.iter().map(|pack| pack.pack_id().clone()).collect(),
            feed_cuts,
            governing,
            disclosure_labels,
        };
        let object = CanonicalObject::identified(&request.capture, &capture)?;
        limits::enforce(
            limits::LimitKind::PlanTooLarge,
            Some(&request.capture),
            limits::Dimension::Bytes,
            object.bytes().len(),
            limits::MAX_PLAN_BYTES,
        )
        .map_err(crate::task_sync::evidence::EvidenceError::from)?;
        Self::insert_object(&tx, CAPTURE_KIND, &object)?;
        tx.commit()?;
        Ok(capture.receipt(false))
    }

    /// Read whole packs from the immutable selection without moving its endpoint.
    ///
    /// # Errors
    /// Refuses a wrong project, invalid ordinal, zero page bound or missing pack.
    pub fn export_native_evidence_capture(
        &self,
        project: &ProjectId,
        cursor: &NativeEvidenceCursor,
        maximum_packs: usize,
        profile: &crate::task_sync::progress::ExportProfile,
    ) -> Result<NativeEvidencePage> {
        if maximum_packs == 0 || maximum_packs > limits::MAX_BATCH_OBJECTS {
            return Err(invalid("native evidence page bound is invalid"));
        }
        let tx = self.connection.unchecked_transaction()?;
        let capture = load(&tx, &cursor.capture)?
            .ok_or_else(|| invalid("native evidence capture is missing"))?;
        if capture.project != *project || cursor.after > capture.packs.len() {
            return Err(invalid("native evidence capture cursor differs"));
        }
        if capture.request.profile != *profile {
            return Err(invalid("native evidence export disclosure profile differs"));
        }
        let end = cursor
            .after
            .saturating_add(maximum_packs)
            .min(capture.packs.len());
        let result = capture.packs[cursor.after..end]
            .iter()
            .map(|id| packs::load_on(&tx, project, id))
            .collect::<Result<Vec<_>>>()?;
        disclosure::validate(
            &tx,
            profile,
            result.iter().flat_map(EvidencePack::records),
            &capture.disclosure_labels,
        )?;
        crate::task_sync::evidence::closure::validate_batch_payloads(
            result
                .iter()
                .map(EvidencePack::to_bytes)
                .collect::<std::result::Result<Vec<_>, _>>()?
                .iter()
                .map(Vec::as_slice),
        )?;
        tx.commit()?;
        Ok(NativeEvidencePage {
            packs: result,
            next: (end < capture.packs.len()).then(|| NativeEvidenceCursor {
                capture: cursor.capture.clone(),
                after: end,
            }),
        })
    }
}

fn load(connection: &Connection, id: &ObjectId) -> Result<Option<Capture>> {
    let Some((kind, bytes)) = object_on(connection, id)? else {
        return Ok(None);
    };
    if kind != CAPTURE_KIND {
        return Err(invalid("native evidence capture identity conflicts"));
    }
    Ok(Some(serde_json::from_slice(&bytes)?))
}

fn validate_request(request: &NativeEvidenceCaptureRequest) -> Result<()> {
    request
        .profile
        .validate()
        .map_err(|_| invalid("native evidence disclosure profile is invalid"))?;
    if request.capture.as_str().len() != 32
        || request.origin.as_str().len() != 32
        || request.roots.is_empty()
        || request.feeds.is_empty()
        || request.maximum_pack_records == 0
        || request.maximum_pack_records > limits::MAX_PACK_RECORDS
        || request.roots.iter().collect::<BTreeSet<_>>().len() != request.roots.len()
        || request.feeds.iter().enumerate().any(|(index, cut)| {
            cut.through < 0
                || request.feeds[..index]
                    .iter()
                    .any(|old| old.feed == cut.feed)
        })
    {
        return Err(invalid(
            "native evidence selection is empty, repeated or malformed",
        ));
    }
    limits::enforce(
        limits::LimitKind::PlanTooLarge,
        Some(&request.capture),
        limits::Dimension::Bytes,
        serde_json_canonicalizer::to_vec(request)?.len(),
        limits::MAX_PLAN_BYTES,
    )
    .map_err(crate::task_sync::evidence::EvidenceError::from)?;
    Ok(())
}

struct Selection<'a> {
    connection: &'a Connection,
    project: &'a ProjectId,
    request: &'a NativeEvidenceCaptureRequest,
    records: BTreeMap<ObjectId, RecordEnvelope>,
    pending: VecDeque<ObjectId>,
    created: Vec<RecordEnvelope>,
    budget: limits::StagingBudget,
}

impl<'a> Selection<'a> {
    fn new(
        connection: &'a Connection,
        project: &'a ProjectId,
        request: &'a NativeEvidenceCaptureRequest,
    ) -> Self {
        Self {
            connection,
            project,
            request,
            records: BTreeMap::new(),
            pending: VecDeque::new(),
            created: Vec::new(),
            budget: limits::StagingBudget::default(),
        }
    }
    fn add(&mut self, record: RecordEnvelope) -> Result<()> {
        if let Some(old) = self.records.get(record.object_id()) {
            if !old.same_record(&record) || old.origin() != record.origin() {
                return Err(invalid("native evidence original origin or bytes conflict"));
            }
            return Ok(());
        }
        inventory::check_project(&record, self.project)?;
        self.budget
            .reserve(record.canonical_bytes().len())
            .map_err(crate::task_sync::evidence::EvidenceError::from)?;
        limits::enforce(
            limits::LimitKind::BatchTooLarge,
            None,
            limits::Dimension::Objects,
            self.records.len() + 1,
            limits::MAX_BATCH_OBJECTS,
        )
        .map_err(crate::task_sync::evidence::EvidenceError::from)?;
        for dependency in inventory::dependencies(&record)? {
            self.pending.push_back(dependency.object_id);
        }
        self.records.insert(record.object_id().clone(), record);
        Ok(())
    }
    fn mint(&mut self, kind: RecordKind, body: &impl Serialize) -> Result<ObjectId> {
        let object = CanonicalObject::mint(body)?;
        let record = RecordEnvelope::from_canonical(
            self.request.origin.clone(),
            object.key().clone(),
            kind.as_str(),
            object.bytes().to_vec(),
        )?;
        self.add(record.clone())?;
        self.created.push(record);
        Ok(object.key().clone())
    }
    fn close(&mut self) -> Result<()> {
        while let Some(id) = self.pending.pop_front() {
            if self.records.contains_key(&id) {
                continue;
            }
            let (kind, bytes) = object_on(self.connection, &id)?
                .ok_or_else(|| invalid("native evidence required context is unavailable"))?;
            let kind = RecordKind::parse(&kind)?;
            let mut origins = self.connection.prepare("SELECT DISTINCT member.origin_id FROM work_task_sync_evidence_members member JOIN work_task_sync_evidence_packs pack ON pack.pack_id=member.pack_id WHERE member.object_id=?1 AND pack.project_id=?2 ORDER BY member.origin_id")?;
            let originals = origins
                .query_map(params![id.as_str(), self.project.0], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<std::result::Result<Vec<_>, _>>()?;
            if originals.len() > 1 {
                return Err(invalid("native evidence original origin is ambiguous"));
            }
            let origin = if let Some(origin) = originals.first() {
                stored_id(origin.clone())?
            } else if kind == RecordKind::ProducingOrigin {
                id.clone()
            } else {
                super::require_native(self.connection, &id)?;
                self.request.origin.clone()
            };
            self.add(RecordEnvelope::from_canonical(
                origin,
                id,
                kind.as_str(),
                bytes,
            )?)?;
        }
        Ok(())
    }
}

fn prepare_packs<'a>(
    project: &ProjectId,
    origin: &ObjectId,
    records: impl Iterator<Item = &'a RecordEnvelope>,
    maximum_records: usize,
) -> Result<Vec<EvidencePack>> {
    let mut result = Vec::new();
    let mut current = Vec::new();
    let mut id = ObjectId::mint();
    let mut size = EvidencePack::prepare(id.clone(), project.clone(), origin.clone(), Vec::new())?
        .to_bytes()?
        .len();
    for record in records {
        let encoded = serde_json::to_vec(record)?.len();
        if !current.is_empty()
            && (current.len() == maximum_records || size + encoded + 1 > limits::MAX_PACK_BYTES)
        {
            result.push(EvidencePack::prepare(
                id,
                project.clone(),
                origin.clone(),
                current,
            )?);
            id = ObjectId::mint();
            current = Vec::new();
            size = EvidencePack::prepare(id.clone(), project.clone(), origin.clone(), Vec::new())?
                .to_bytes()?
                .len();
        }
        size += encoded + usize::from(!current.is_empty());
        current.push(record.clone());
    }
    if !current.is_empty() {
        result.push(EvidencePack::prepare(
            id,
            project.clone(),
            origin.clone(),
            current,
        )?);
    }
    Ok(result)
}
