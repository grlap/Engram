//! Pure selected-input collision checks and dependency-ordered atomic components.
//! Storage supplies the typed reference inventory and a keyed admitted-object lookup.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Write;

use serde::{Deserialize, Serialize};

use crate::ObjectId;

use super::{EvidenceError, RecordEnvelope, RecordKind, limits};

#[derive(Clone, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ObjectReference {
    pub object_id: ObjectId,
    pub object_kind: RecordKind,
}

/// Dependencies are evidence-object links only: never a content fingerprint,
/// inert run/session/claim scalar or unqualified feed position.
pub struct ClosureRecord<'a> {
    pub record: &'a RecordEnvelope,
    pub dependencies: Vec<ObjectReference>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct AtomicComponent {
    pub objects: Vec<ObjectId>,
    pub canonical_bytes: usize,
    /// Encoded record envelopes. Final retained pack headers are checked separately.
    pub encoded_bytes: usize,
}

/// Component indices address only this immutable construction plan, not feeds.
#[derive(Debug, Eq, PartialEq, Serialize)]
pub struct ClosurePlan {
    pub components: Vec<AtomicComponent>,
    pub batches: Vec<Vec<usize>>,
    pub already_known: Vec<ObjectId>,
}

/// Check the actual retained pack files of a batch, including container overhead.
/// A planner's envelope-byte accounting alone is insufficient at the byte boundary.
///
/// # Errors
/// Refuses oversized or malformed packs, oversized complete payloads and object counts.
pub fn validate_batch_payloads<'a>(
    payloads: impl IntoIterator<Item = &'a [u8]>,
) -> Result<(), EvidenceError> {
    let (mut bytes, mut objects) = (0_usize, 0_usize);
    for payload in payloads {
        bytes = bytes.saturating_add(payload.len());
        limits::enforce(
            limits::LimitKind::BatchTooLarge,
            None,
            limits::Dimension::Bytes,
            bytes,
            limits::MAX_BATCH_BYTES,
        )?;
        let pack = super::EvidencePack::read(payload)?;
        objects = objects.saturating_add(pack.records().len());
        limits::enforce(
            limits::LimitKind::BatchTooLarge,
            None,
            limits::Dimension::Objects,
            objects,
            limits::MAX_BATCH_OBJECTS,
        )?;
    }
    Ok(())
}

fn dependency(id: &ObjectId, problem: &'static str) -> EvidenceError {
    EvidenceError::Dependency {
        object: id.clone(),
        problem,
    }
}

struct Count(usize);
impl Write for Count {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .ok_or_else(|| std::io::Error::other("evidence size overflow"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn encoded_size(value: &impl Serialize) -> Result<usize, EvidenceError> {
    let mut count = Count(0);
    serde_json::to_writer(&mut count, value)?;
    Ok(count.0)
}

/// Plan one bounded selected candidate without mutating a destination or minting ids.
/// The lookup must read one already-admitted original by id, with its original bytes.
/// All collisions and unresolved links are checked before returning any component.
///
/// # Errors
/// Refuses changed original identities, missing or wrong-kind dependencies, and
/// oversized staging, indivisible components, batches or construction metadata.
pub fn plan_components(
    selected: &[ClosureRecord<'_>],
    mut known: impl FnMut(&ObjectId) -> Result<Option<RecordEnvelope>, EvidenceError>,
) -> Result<ClosurePlan, EvidenceError> {
    let mut originals = BTreeMap::<ObjectId, usize>::new();
    let mut staging = limits::StagingBudget::default();
    let mut already_known = Vec::new();
    let mut missing = Vec::new();
    let mut sizes = BTreeMap::new();
    for (index, input) in selected.iter().enumerate() {
        let record = input.record;
        let id = record.object_id();
        if let Some(&previous) = originals.get(id) {
            if !selected[previous].record.same_record(record) {
                return Err(EvidenceError::RecordConflict(id.clone()));
            }
            let left = selected[previous]
                .dependencies
                .iter()
                .collect::<BTreeSet<_>>();
            let right = input.dependencies.iter().collect::<BTreeSet<_>>();
            if left != right {
                return Err(dependency(
                    id,
                    "dependency inventory differs for the same original",
                ));
            }
            continue;
        }
        let encoded = encoded_size(record)?;
        staging.reserve(record.canonical_bytes().len())?;
        staging.reserve(encoded)?;
        staging.reserve(encoded_size(&input.dependencies)?)?;
        originals.insert(id.clone(), index);
        sizes.insert(id.clone(), (record.canonical_bytes().len(), encoded));
        if let Some(retained) = known(id)? {
            if !retained.same_record(record) {
                return Err(EvidenceError::RecordConflict(id.clone()));
            }
            already_known.push(id.clone());
        } else {
            missing.push(id.clone());
        }
    }
    missing.sort();
    already_known.sort();
    let indices = missing
        .iter()
        .enumerate()
        .map(|(index, id)| (id.clone(), index))
        .collect::<BTreeMap<_, _>>();
    let mut edges = vec![Vec::new(); missing.len()];
    for (index, id) in missing.iter().enumerate() {
        for reference in &selected[originals[id]].dependencies {
            if let Some(&other) = originals.get(&reference.object_id) {
                if selected[other].record.kind() != reference.object_kind {
                    return Err(dependency(&reference.object_id, "dependency kind differs"));
                }
                if let Some(&target) = indices.get(&reference.object_id) {
                    edges[index].push(target);
                }
            } else {
                let retained = known(&reference.object_id)?.ok_or_else(|| {
                    dependency(&reference.object_id, "required original is unavailable")
                })?;
                if retained.object_id() != &reference.object_id
                    || retained.kind() != reference.object_kind
                {
                    return Err(dependency(&reference.object_id, "dependency kind differs"));
                }
            }
        }
        edges[index].sort_unstable();
        edges[index].dedup();
    }
    let components = atomic_components(&missing, &edges, &sizes)?;
    batch_components(components, already_known)
}

fn atomic_components(
    missing: &[ObjectId],
    edges: &[Vec<usize>],
    sizes: &BTreeMap<ObjectId, (usize, usize)>,
) -> Result<Vec<AtomicComponent>, EvidenceError> {
    let groups = strongly_connected(edges);
    let mut components = Vec::new();
    // Kosaraju yields dependents first for these edges; reversing gives a
    // dependency order. Independent order is stable because nodes/edges are sorted.
    for group in groups.into_iter().rev() {
        let mut objects = group
            .into_iter()
            .map(|index| missing[index].clone())
            .collect::<Vec<_>>();
        objects.sort();
        let canonical_bytes = objects.iter().map(|id| sizes[id].0).sum();
        let encoded_bytes = objects.iter().map(|id| sizes[id].1).sum();
        limits::enforce(
            limits::LimitKind::ComponentTooLarge,
            objects.first(),
            limits::Dimension::Objects,
            objects.len(),
            limits::MAX_COMPONENT_OBJECTS,
        )?;
        limits::enforce(
            limits::LimitKind::ComponentTooLarge,
            objects.first(),
            limits::Dimension::Bytes,
            canonical_bytes,
            limits::MAX_COMPONENT_BYTES,
        )?;
        components.push(AtomicComponent {
            objects,
            canonical_bytes,
            encoded_bytes,
        });
    }
    Ok(components)
}

fn batch_components(
    components: Vec<AtomicComponent>,
    already_known: Vec<ObjectId>,
) -> Result<ClosurePlan, EvidenceError> {
    let mut batches = Vec::new();
    let mut batch = Vec::new();
    let (mut bytes, mut objects) = (0, 0);
    for (index, component) in components.iter().enumerate() {
        limits::enforce(
            limits::LimitKind::BatchTooLarge,
            component.objects.first(),
            limits::Dimension::Bytes,
            component.encoded_bytes,
            limits::MAX_BATCH_BYTES,
        )?;
        limits::enforce(
            limits::LimitKind::BatchTooLarge,
            component.objects.first(),
            limits::Dimension::Objects,
            component.objects.len(),
            limits::MAX_BATCH_OBJECTS,
        )?;
        if bytes + component.encoded_bytes > limits::MAX_BATCH_BYTES
            || objects + component.objects.len() > limits::MAX_BATCH_OBJECTS
        {
            batches.push(std::mem::take(&mut batch));
            bytes = 0;
            objects = 0;
        }
        batch.push(index);
        bytes += component.encoded_bytes;
        objects += component.objects.len();
    }
    if !batch.is_empty() {
        batches.push(batch);
    }
    let plan = ClosurePlan {
        components,
        batches,
        already_known,
    };
    limits::enforce(
        limits::LimitKind::PlanTooLarge,
        None,
        limits::Dimension::Bytes,
        encoded_size(&plan)?,
        limits::MAX_PLAN_BYTES,
    )?;
    Ok(plan)
}

/// Iterative DFS avoids a stack overflow for a long acyclic history.
fn strongly_connected(edges: &[Vec<usize>]) -> Vec<Vec<usize>> {
    let mut visited = vec![false; edges.len()];
    let mut order = Vec::new();
    for start in 0..edges.len() {
        if visited[start] {
            continue;
        }
        visited[start] = true;
        let mut stack = vec![(start, 0)];
        while let Some((node, next)) = stack.last_mut() {
            if let Some(&target) = edges[*node].get(*next) {
                *next += 1;
                if !visited[target] {
                    visited[target] = true;
                    stack.push((target, 0));
                }
            } else {
                order.push(*node);
                stack.pop();
            }
        }
    }
    let mut reverse = vec![Vec::new(); edges.len()];
    for (node, dependencies) in edges.iter().enumerate() {
        for &target in dependencies {
            reverse[target].push(node);
        }
    }
    visited.fill(false);
    let mut groups = Vec::new();
    for &start in order.iter().rev() {
        if visited[start] {
            continue;
        }
        visited[start] = true;
        let mut stack = vec![start];
        let mut group = Vec::new();
        while let Some(node) = stack.pop() {
            group.push(node);
            for &target in &reverse[node] {
                if !visited[target] {
                    visited[target] = true;
                    stack.push(target);
                }
            }
        }
        groups.push(group);
    }
    groups
}

#[cfg(test)]
mod tests;
