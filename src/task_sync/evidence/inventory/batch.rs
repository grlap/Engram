//! Validate all typed references before computing selected atomic components.

use super::super::{EvidenceError, EvidencePack, RecordEnvelope, closure, limits};
use super::dependencies;
use crate::ObjectId;
use std::collections::BTreeMap;

fn refused(id: &ObjectId, problem: &'static str) -> EvidenceError {
    EvidenceError::Dependency {
        object: id.clone(),
        problem,
    }
}

/// Resolve links from the closed typed inventory across a complete batch.
/// `known` must return an already admitted original with its original origin;
/// caller-supplied lists cannot omit links, including links in known members.
/// This reads no files and admits nothing.
///
/// # Errors
/// Refuses incomplete/wrong-kind context, global identity conflicts, changed
/// originals, mixed projects and all complete encoded payload/profile bounds.
pub fn plan_batch(
    packs: &[EvidencePack],
    mut known: impl FnMut(&ObjectId) -> Result<Option<RecordEnvelope>, EvidenceError>,
) -> Result<closure::ClosurePlan, EvidenceError> {
    let selected = selected(packs)?;
    let mut cached = BTreeMap::<ObjectId, Option<RecordEnvelope>>::new();
    let mut staging = limits::StagingBudget::default();
    let mut inventory = Vec::with_capacity(selected.len());
    for record in selected.values() {
        let mut references = Vec::new();
        for link in dependencies(record)? {
            let target = if let Some(target) = selected.get(&link.object_id) {
                *target
            } else {
                if !cached.contains_key(&link.object_id) {
                    cache(&mut cached, &mut staging, &link.object_id, &mut known)?;
                }
                cached[&link.object_id]
                    .as_ref()
                    .ok_or_else(|| refused(&link.object_id, "required original is unavailable"))?
            };
            if target.object_id() != &link.object_id
                || (!link.kinds.is_empty() && !link.kinds.contains(&target.kind()))
            {
                return Err(refused(&link.object_id, "required original kind differs"));
            }
            references.push(closure::ObjectReference {
                object_id: link.object_id,
                object_kind: target.kind(),
            });
        }
        references.sort();
        references.dedup();
        inventory.push(closure::ClosureRecord {
            record,
            dependencies: references,
        });
    }
    closure::plan_components(&inventory, |id| {
        if !cached.contains_key(id) {
            cache(&mut cached, &mut staging, id, &mut known)?;
        }
        Ok(cached[id].clone())
    })
}

fn cache(
    cached: &mut BTreeMap<ObjectId, Option<RecordEnvelope>>,
    staging: &mut limits::StagingBudget,
    id: &ObjectId,
    known: &mut impl FnMut(&ObjectId) -> Result<Option<RecordEnvelope>, EvidenceError>,
) -> Result<(), EvidenceError> {
    let value = known(id)?;
    if let Some(record) = &value {
        staging.reserve(record.canonical_bytes().len())?;
    }
    cached.insert(id.clone(), value);
    Ok(())
}

fn selected(packs: &[EvidencePack]) -> Result<BTreeMap<ObjectId, &RecordEnvelope>, EvidenceError> {
    let Some(first) = packs.first() else {
        return Err(EvidenceError::InvalidPack("evidence batch is empty"));
    };
    let (mut bytes, mut objects) = (0_usize, 0_usize);
    for pack in packs {
        bytes = bytes.saturating_add(pack.to_bytes()?.len());
        objects = objects.saturating_add(pack.records().len());
        limits::enforce(
            limits::LimitKind::BatchTooLarge,
            None,
            limits::Dimension::Bytes,
            bytes,
            limits::MAX_BATCH_BYTES,
        )?;
        limits::enforce(
            limits::LimitKind::BatchTooLarge,
            None,
            limits::Dimension::Objects,
            objects,
            limits::MAX_BATCH_OBJECTS,
        )?;
    }
    let mut selected = BTreeMap::<ObjectId, &RecordEnvelope>::new();
    let mut pack_ids = std::collections::BTreeSet::new();
    for pack in packs {
        if pack.project_id() != first.project_id() || !pack_ids.insert(pack.pack_id()) {
            return Err(EvidenceError::InvalidPack(
                "evidence batch project or pack identity differs",
            ));
        }
        for record in pack.records() {
            super::check_project(record, first.project_id())?;
            if let Some(previous) = selected.insert(record.object_id().clone(), record)
                && (!previous.same_record(record) || previous.origin() != record.origin())
            {
                return Err(EvidenceError::RecordConflict(record.object_id().clone()));
            }
        }
    }
    if let Some(id) = pack_ids.iter().find(|id| selected.contains_key(*id)) {
        return Err(EvidenceError::RecordConflict((*id).clone()));
    }
    Ok(selected)
}
