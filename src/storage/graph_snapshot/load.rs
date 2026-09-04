use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use super::{
    REDACTED_MEMORY_PLACEHOLDER, RESTORED_MEMORY_SOURCE, RESTORED_REDACTED_MEMORY_SOURCE,
    SqliteStore, StoreError, inspect_serialized_strings, validate_snapshot_audit_actor_shape,
};
use crate::{
    ActorContext, Authority, CanonicalObject, Delivery, MemoryId, MemoryKind, MemoryStatus,
    MemoryVersion, ObjectHash, ProjectId, Redactor, RestoredRecord, RestoredRelationBasis, Scope,
    Sensitivity, WorkBlocker, WorkGraphSnapshotDocument, WorkGraphSnapshotHistory,
    WorkGraphSnapshotLifecycleCounts, WorkGraphSnapshotLoadPreview, WorkGraphSnapshotLoadResult,
    WorkGraphSnapshotLoadedEvent, WorkGraphSnapshotMemoryState, WorkGraphSnapshotRecordPayload,
    WorkGraphSnapshotRedactedCounts, WorkGraphSnapshotText, WorkId, WorkItem, WorkLifecycle,
    WorkOrigin, WorkSourceSnapshot,
    domain::{
        MAX_PROJECT_MEMORY_BODY_BYTES, MemoryAssertionEvent, SourceSnapshot,
        normalize_gate_evidence_input,
    },
    graph_snapshot::preflight_work_graph_snapshot_build,
    parse_work_graph_snapshot_document, work_graph_snapshot_format_fingerprint,
};

struct PreparedLoad {
    document: WorkGraphSnapshotDocument,
    body_object: CanonicalObject,
    records: Vec<(RestoredRecord, CanonicalObject)>,
    preview: WorkGraphSnapshotLoadPreview,
}

pub(super) fn validate_generated_snapshot_document(
    document: &WorkGraphSnapshotDocument,
) -> Result<(), StoreError> {
    prepare_load(document.clone(), &document.body.summary.project_id).map(drop)
}

impl SqliteStore {
    /// Validates and optionally atomically recreates one work-graph snapshot.
    ///
    /// # Errors
    ///
    /// Returns a typed graph refusal when the destination is nonempty, the
    /// file belongs to another project/build, or any carried relation,
    /// history layer, source, memory, or manifest binding is invalid.
    pub fn load_work_graph_snapshot<R: Redactor>(
        &mut self,
        project_id: &ProjectId,
        actor: &ActorContext,
        bytes: &[u8],
        dry_run: bool,
        loaded_at: DateTime<Utc>,
        redactor: &R,
    ) -> Result<WorkGraphSnapshotLoadResult, StoreError> {
        if !graph_destination_is_empty_on(&self.connection, project_id)? {
            return Err(StoreError::GraphDestinationNotEmpty);
        }
        validate_snapshot_audit_actor_shape(actor).map_err(StoreError::InvalidGraphSnapshot)?;
        preflight_work_graph_snapshot_build(bytes)?;
        let raw_document: serde_json::Value = serde_json::from_slice(bytes)?;
        inspect_serialized_strings(redactor, &raw_document)?;
        inspect_serialized_strings(redactor, &serde_json::to_value(actor)?)?;
        let document = parse_work_graph_snapshot_document(bytes)
            .map_err(|error| StoreError::InvalidGraphSnapshot(error.to_string()))?;
        let prepared = prepare_load(document, project_id)?;
        if dry_run {
            return Ok(WorkGraphSnapshotLoadResult {
                loaded: false,
                preview: prepared.preview,
            });
        }

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !graph_destination_is_empty_on(&transaction, project_id)? {
            return Err(StoreError::GraphDestinationNotEmpty);
        }
        insert_prepared_load_on(&transaction, project_id, actor, loaded_at, &prepared)?;
        transaction.commit()?;
        Ok(WorkGraphSnapshotLoadResult {
            loaded: true,
            preview: prepared.preview,
        })
    }
}

fn prepare_load(
    document: WorkGraphSnapshotDocument,
    project_id: &ProjectId,
) -> Result<PreparedLoad, StoreError> {
    if &document.body.summary.project_id != project_id {
        return Err(StoreError::GraphProjectMismatch {
            snapshot: document.body.summary.project_id.clone(),
            destination: project_id.clone(),
        });
    }
    if document.body.summary.schema_version != crate::WORK_GRAPH_SNAPSHOT_SCHEMA_VERSION
        || document.body.summary.format_fingerprint != work_graph_snapshot_format_fingerprint()?
    {
        return Err(StoreError::GraphDifferentBuild);
    }
    if document.manifest.summary != document.body.summary {
        return Err(corrupt("manifest summary differs from the canonical body"));
    }
    validate_summary(&document)?;
    if document.body.summary.widened != document.body.summary.widening_reason.is_some() {
        return Err(corrupt("widening flag and reason disagree"));
    }
    let body_object = CanonicalObject::freeze(&document.body)?;
    if document.manifest.body_sha256 != *body_object.hash() {
        return Err(corrupt(
            "manifest body digest differs from the canonical body",
        ));
    }
    let counts = &document.body.summary.section_counts;
    if counts.items != document.body.items.len()
        || counts.blockers != document.body.blockers.len()
        || counts.sources != document.body.sources.len()
        || counts.records != document.body.records.len()
        || counts.memories != document.body.memories.len()
    {
        return Err(corrupt("manifest section counts differ from the body"));
    }
    validate_section_order(&document)?;

    validate_items_and_relations(&document)?;
    validate_sources(&document)?;
    let records = validate_and_materialize_records(&document)?;
    validate_lifecycle_proofs(&document, &records)?;
    validate_memories(&document)?;

    let mut lifecycle_counts = WorkGraphSnapshotLifecycleCounts::default();
    for item in &document.body.items {
        match item.lifecycle {
            WorkLifecycle::Proposed => lifecycle_counts.proposed += 1,
            WorkLifecycle::Open => lifecycle_counts.open += 1,
            WorkLifecycle::Completed => lifecycle_counts.completed += 1,
            WorkLifecycle::Cancelled => lifecycle_counts.cancelled += 1,
            WorkLifecycle::Superseded => lifecycle_counts.superseded += 1,
        }
    }
    let refs = document
        .body
        .items
        .iter()
        .map(|item| item.short_ref.clone())
        .collect();
    let placeholder_memories = document
        .body
        .memories
        .iter()
        .filter_map(|memory| match &memory.state {
            WorkGraphSnapshotMemoryState::Active {
                sensitivity: Sensitivity::Restricted,
                ..
            } => Some(memory.key.clone()),
            _ => None,
        })
        .collect();
    let completed_by_record = document
        .body
        .items
        .iter()
        .filter(|item| item.lifecycle == WorkLifecycle::Completed)
        .map(|item| item.short_ref.clone())
        .collect();
    let preview = WorkGraphSnapshotLoadPreview {
        body_sha256: body_object.hash().clone(),
        summary: document.body.summary.clone(),
        lifecycle_counts,
        refs,
        placeholder_memories,
        completed_by_record,
    };
    Ok(PreparedLoad {
        document,
        body_object,
        records,
        preview,
    })
}

fn validate_summary(document: &WorkGraphSnapshotDocument) -> Result<(), StoreError> {
    if document.body.summary.as_of.work_feed < 0 || document.body.summary.as_of.project_memory < 0 {
        return Err(corrupt("snapshot cut contains a negative position"));
    }
    let widening_reason =
        super::validate_widening_reason(document.body.summary.widening_reason.as_deref())
            .map_err(|_| corrupt("snapshot widening reason is invalid"))?;
    if widening_reason != document.body.summary.widening_reason {
        return Err(corrupt("snapshot widening reason is not canonical"));
    }
    super::validate_snapshot_audit_text(
        &document.body.summary.redactor_status,
        "snapshot redactor status",
    )
    .map_err(corrupt)?;
    super::validate_snapshot_audit_text(
        &document.manifest.exporting_build,
        "snapshot exporting build",
    )
    .map_err(corrupt)?;
    let memory_redactions = document
        .body
        .memories
        .iter()
        .filter(|memory| {
            matches!(
                memory.state,
                WorkGraphSnapshotMemoryState::Active {
                    body: WorkGraphSnapshotText::Redacted { .. },
                    ..
                }
            )
        })
        .count();
    let actual_redactions = WorkGraphSnapshotRedactedCounts {
        memories: memory_redactions,
        ..WorkGraphSnapshotRedactedCounts::default()
    };
    if document.body.summary.redacted != actual_redactions {
        return Err(corrupt(
            "snapshot redacted counts differ from the typed placeholders",
        ));
    }
    Ok(())
}

fn validate_section_order(document: &WorkGraphSnapshotDocument) -> Result<(), StoreError> {
    if document
        .body
        .items
        .windows(2)
        .any(|pair| pair[0].short_ref >= pair[1].short_ref)
    {
        return Err(corrupt("work items are not strictly ordered by ref"));
    }
    let refs = document
        .body
        .items
        .iter()
        .map(|item| (item.work_id, item.short_ref.as_str()))
        .collect::<HashMap<_, _>>();
    if document.body.blockers.windows(2).any(|pair| {
        let left_ref = refs.get(&pair[0].work_id).copied().unwrap_or("");
        let right_ref = refs.get(&pair[1].work_id).copied().unwrap_or("");
        (left_ref, pair[0].blocker_id.as_str()) >= (right_ref, pair[1].blocker_id.as_str())
    }) {
        return Err(corrupt(
            "work blockers are not strictly ordered by item ref and blocker id",
        ));
    }
    if document
        .body
        .sources
        .windows(2)
        .any(|pair| pair[0].hash.as_str() >= pair[1].hash.as_str())
    {
        return Err(corrupt("source snapshots are not strictly ordered by hash"));
    }
    let record_key = |record: &crate::WorkGraphSnapshotRecord| {
        (
            refs.get(&record.work_id).copied().unwrap_or(""),
            record.generation_index,
        )
    };
    if document
        .body
        .records
        .windows(2)
        .any(|pair| record_key(&pair[0]) >= record_key(&pair[1]))
    {
        return Err(corrupt(
            "restored records are not strictly ordered by item ref and generation",
        ));
    }
    if document
        .body
        .memories
        .windows(2)
        .any(|pair| pair[0].key >= pair[1].key)
    {
        return Err(corrupt("project memories are not strictly ordered by key"));
    }
    Ok(())
}

fn validate_items_and_relations(document: &WorkGraphSnapshotDocument) -> Result<(), StoreError> {
    let items = &document.body.items;
    let mut ids = HashSet::with_capacity(items.len());
    let mut refs = HashSet::with_capacity(items.len());
    for item in items {
        if !ids.insert(item.work_id) {
            return Err(corrupt("duplicate work id"));
        }
        if !refs.insert(item.short_ref.as_str()) {
            return Err(corrupt("duplicate work ref"));
        }
        validate_snapshot_item_shape(item)?;
    }
    let mut blocker_ids = HashSet::new();
    for blocker in &document.body.blockers {
        if !ids.contains(&blocker.work_id) || !blocker_ids.insert(blocker.blocker_id.as_str()) {
            return Err(corrupt("duplicate or dangling blocker"));
        }
        validate_text(&blocker.blocker_id, "blocker id")?;
        validate_text(&blocker.detail, "blocker detail")?;
        validate_actor(&blocker.created_by)?;
    }
    for item in items {
        if !ids.contains(&item.root_id)
            || item.parent_id.is_some_and(|id| !ids.contains(&id))
            || item.prerequisites.iter().any(|id| !ids.contains(id))
            || item.superseded_by.is_some_and(|id| !ids.contains(&id))
        {
            return Err(corrupt("dangling work relation"));
        }
        if let Some(parent) = item.parent_id {
            let parent = items
                .iter()
                .find(|candidate| candidate.work_id == parent)
                .ok_or_else(|| corrupt("dangling parent"))?;
            if parent.root_id != item.root_id {
                return Err(corrupt("child crosses its root binding"));
            }
        }
    }
    validate_hierarchy_limits(items)?;
    validate_combined_graph(items)
}

fn validate_snapshot_item_shape(item: &crate::WorkGraphSnapshotItem) -> Result<(), StoreError> {
    validate_text(&item.short_ref, "work ref")?;
    let simple = item.work_id.0.simple().to_string();
    let expected_ref = format!("w-{}", simple.get(20..).unwrap_or(&simple));
    if item.short_ref != expected_ref {
        return Err(corrupt("work ref does not match its work id"));
    }
    validate_text(&item.title, "work title")?;
    validate_text(&item.outcome, "work outcome")?;
    validate_string_set(&item.acceptance, "acceptance")?;
    validate_string_set(&item.labels, "labels")?;
    if !(0..=4).contains(&item.priority) {
        return Err(corrupt("work priority is outside 0 through 4"));
    }
    if item.parent_id.is_none() != (item.work_id == item.root_id) {
        return Err(corrupt("work root and parent bindings disagree"));
    }
    if matches!(item.origin, WorkOrigin::Local) != item.source_snapshot_id.is_none() {
        return Err(corrupt("work origin and source binding disagree"));
    }
    if let Some(value) = item.assigned_to.as_deref() {
        validate_text(value, "assignee")?;
    }
    match item.lifecycle {
        WorkLifecycle::Cancelled => {
            if item.disposal_reason.is_none() || item.superseded_by.is_some() {
                return Err(corrupt("cancelled work has invalid disposal proof"));
            }
        }
        WorkLifecycle::Superseded => {
            if item.disposal_reason.is_none() || item.superseded_by.is_none() {
                return Err(corrupt("superseded work has invalid disposal proof"));
            }
        }
        WorkLifecycle::Proposed | WorkLifecycle::Open | WorkLifecycle::Completed => {
            if item.disposal_reason.is_some() || item.superseded_by.is_some() {
                return Err(corrupt("non-disposed work carries disposal proof"));
            }
        }
    }
    if let Some(reason) = item.disposal_reason.as_deref() {
        validate_text(reason, "disposal reason")?;
    }
    Ok(())
}

fn validate_hierarchy_limits(items: &[crate::WorkGraphSnapshotItem]) -> Result<(), StoreError> {
    let by_id = items
        .iter()
        .map(|item| (item.work_id, item))
        .collect::<HashMap<_, _>>();
    for item in items {
        let mut depth = 0_u32;
        let mut cursor = item.parent_id;
        let mut visited = HashSet::new();
        while let Some(parent) = cursor {
            if !visited.insert(parent) {
                return Err(corrupt("parent hierarchy contains a cycle"));
            }
            depth += 1;
            if depth > super::super::work::MAX_WORK_DEPTH {
                return Err(corrupt("parent hierarchy exceeds the work depth limit"));
            }
            cursor = by_id.get(&parent).and_then(|parent| parent.parent_id);
        }
    }
    for root in items.iter().filter(|item| item.parent_id.is_none()) {
        let open_descendants = items
            .iter()
            .filter(|item| {
                item.root_id == root.work_id
                    && item.work_id != root.work_id
                    && matches!(
                        item.lifecycle,
                        WorkLifecycle::Open | WorkLifecycle::Proposed
                    )
            })
            .count();
        if open_descendants > super::super::work::MAX_OPEN_WORK_DESCENDANTS as usize {
            return Err(corrupt("open descendant count exceeds the work limit"));
        }
    }
    Ok(())
}

fn validate_combined_graph(items: &[crate::WorkGraphSnapshotItem]) -> Result<(), StoreError> {
    let mut graph = HashMap::<WorkId, Vec<WorkId>>::new();
    for item in items {
        graph.entry(item.work_id).or_default();
        if item.child_requirement == crate::ChildRequirement::Required
            && let Some(parent) = item.parent_id
        {
            graph.entry(parent).or_default().push(item.work_id);
        }
        graph
            .entry(item.work_id)
            .or_default()
            .extend(item.prerequisites.iter().copied());
        if let Some(successor) = item.superseded_by {
            graph.entry(item.work_id).or_default().push(successor);
        }
    }
    let mut incoming = graph
        .keys()
        .copied()
        .map(|id| (id, 0_usize))
        .collect::<HashMap<_, _>>();
    for targets in graph.values() {
        for target in targets {
            *incoming.entry(*target).or_default() += 1;
        }
    }
    let mut ready = incoming
        .iter()
        .filter_map(|(id, count)| (*count == 0).then_some(*id))
        .collect::<Vec<_>>();
    let mut removed = 0;
    while let Some(id) = ready.pop() {
        removed += 1;
        for target in graph.get(&id).into_iter().flatten() {
            let count = incoming
                .get_mut(target)
                .ok_or_else(|| corrupt("graph target has no node"))?;
            *count -= 1;
            if *count == 0 {
                ready.push(*target);
            }
        }
    }
    if removed != incoming.len() {
        return Err(corrupt("combined work graph contains a cycle"));
    }
    Ok(())
}

fn validate_sources(document: &WorkGraphSnapshotDocument) -> Result<(), StoreError> {
    let expected = document
        .body
        .items
        .iter()
        .filter_map(|item| item.source_snapshot_id.clone())
        .collect::<HashSet<_>>();
    let mut seen = HashSet::new();
    for source in &document.body.sources {
        if !seen.insert(source.hash.clone()) {
            return Err(corrupt("duplicate source hash"));
        }
        let object = CanonicalObject::freeze(&source.canonical_json)?;
        if object.hash() != &source.hash {
            return Err(corrupt("source hash differs from its canonical JSON"));
        }
        let snapshot: WorkSourceSnapshot = serde_json::from_value(source.canonical_json.clone())
            .map_err(|_| corrupt("source canonical JSON has an invalid shape"))?;
        let typed = CanonicalObject::freeze(&snapshot)?;
        if typed.hash() != &source.hash || typed.bytes() != object.bytes() {
            return Err(corrupt(
                "source canonical JSON is not exactly preserved by its typed shape",
            ));
        }
        super::super::work::validate_work_source_snapshot_for_restore(&snapshot)?;
    }
    if expected != seen {
        return Err(corrupt("source section differs from item source bindings"));
    }
    Ok(())
}

fn validate_and_materialize_records(
    document: &WorkGraphSnapshotDocument,
) -> Result<Vec<(RestoredRecord, CanonicalObject)>, StoreError> {
    let item_ids = document
        .body
        .items
        .iter()
        .map(|item| item.work_id)
        .collect::<HashSet<_>>();
    let items = document
        .body
        .items
        .iter()
        .map(|item| (item.work_id, item))
        .collect::<HashMap<_, _>>();
    let mut keys = HashSet::new();
    let mut records = Vec::with_capacity(document.body.records.len());
    for record in &document.body.records {
        if !item_ids.contains(&record.work_id)
            || !keys.insert((record.work_id, record.generation_index))
        {
            return Err(corrupt("duplicate or dangling restored record"));
        }
        let (restored, object) = match &record.payload {
            WorkGraphSnapshotRecordPayload::Restored {
                object_hash,
                canonical_json,
            } => {
                let object = CanonicalObject::freeze(canonical_json)?;
                if object.hash() != object_hash {
                    return Err(corrupt("restored record hash differs from canonical JSON"));
                }
                let restored: RestoredRecord = serde_json::from_value(canonical_json.clone())
                    .map_err(|_| corrupt("restored record canonical JSON has an invalid shape"))?;
                let typed = CanonicalObject::freeze(&restored)?;
                if typed.hash() != object_hash || typed.bytes() != object.bytes() {
                    return Err(corrupt(
                        "restored record canonical JSON is not exactly preserved by its typed shape",
                    ));
                }
                (restored, object)
            }
            WorkGraphSnapshotRecordPayload::Native { history } => {
                let item = items
                    .get(&record.work_id)
                    .ok_or_else(|| corrupt("restored record names an unknown work item"))?;
                let restored = RestoredRecord {
                    schema_version: crate::WORK_GRAPH_SNAPSHOT_SCHEMA_VERSION,
                    project_id: document.body.summary.project_id.clone(),
                    work_id: record.work_id,
                    generation_index: record.generation_index,
                    item: (*item).clone(),
                    relations: restored_relation_basis(document, record.work_id)?,
                    history: (**history).clone(),
                };
                let object = CanonicalObject::freeze(&restored)?;
                (restored, object)
            }
        };
        if restored.schema_version != crate::WORK_GRAPH_SNAPSHOT_SCHEMA_VERSION
            || restored.project_id != document.body.summary.project_id
            || restored.work_id != record.work_id
            || restored.generation_index != record.generation_index
            || restored.item.work_id != record.work_id
        {
            return Err(corrupt(
                "restored record disagrees with its section binding",
            ));
        }
        validate_history(&restored.history, &item_ids)?;
        validate_restored_relation_basis(&restored.relations, restored.work_id, &item_ids)?;
        validate_inherited_record(&restored, &item_ids, &items)?;
        records.push((restored, object));
    }
    let mut per_item = HashMap::<WorkId, Vec<usize>>::new();
    for (record, _) in &records {
        per_item
            .entry(record.work_id)
            .or_default()
            .push(record.generation_index);
    }
    for item_id in item_ids {
        let generations = per_item
            .get_mut(&item_id)
            .ok_or_else(|| corrupt("work item has no restored history record"))?;
        generations.sort_unstable();
        if generations
            .iter()
            .copied()
            .enumerate()
            .any(|(expected, actual)| expected != actual)
        {
            return Err(corrupt(
                "restored record generations are not dense from zero",
            ));
        }
    }
    Ok(records)
}

fn validate_inherited_record(
    record: &RestoredRecord,
    item_ids: &HashSet<WorkId>,
    current_items: &HashMap<WorkId, &crate::WorkGraphSnapshotItem>,
) -> Result<(), StoreError> {
    validate_snapshot_item_shape(&record.item)?;
    let current = current_items
        .get(&record.work_id)
        .ok_or_else(|| corrupt("restored record names an unknown work item"))?;
    if record.item.short_ref != current.short_ref
        || record.item.root_id != current.root_id
        || record.item.parent_id != current.parent_id
        || record.item.child_requirement != current.child_requirement
        || record.item.origin != current.origin
        || record.item.source_snapshot_id != current.source_snapshot_id
    {
        return Err(corrupt(
            "restored record changes immutable work planning identity",
        ));
    }
    if !item_ids.contains(&record.item.root_id)
        || record
            .item
            .parent_id
            .is_some_and(|id| !item_ids.contains(&id))
        || record
            .item
            .prerequisites
            .iter()
            .any(|id| !item_ids.contains(id))
        || record
            .item
            .superseded_by
            .is_some_and(|id| !item_ids.contains(&id))
    {
        return Err(corrupt("restored record item has a dangling relation"));
    }
    let mut prerequisites = record.item.prerequisites.clone();
    prerequisites.sort_by_key(|id| id.0);
    prerequisites.dedup();
    let mut relation_prerequisites = record.relations.prerequisites.clone();
    relation_prerequisites.sort_by_key(|id| id.0);
    relation_prerequisites.dedup();
    if prerequisites != record.item.prerequisites
        || relation_prerequisites != record.relations.prerequisites
        || prerequisites != relation_prerequisites
    {
        return Err(corrupt(
            "restored record item and relation prerequisites disagree",
        ));
    }
    if record.history.completion.is_some() != (record.item.lifecycle == WorkLifecycle::Completed) {
        return Err(corrupt(
            "restored record lifecycle disagrees with its completion proof",
        ));
    }
    validate_disposal_proof(&record.item, &record.history)?;
    Ok(())
}

fn validate_disposal_proof(
    item: &crate::WorkGraphSnapshotItem,
    history: &WorkGraphSnapshotHistory,
) -> Result<(), StoreError> {
    let terminal = matches!(
        item.lifecycle,
        WorkLifecycle::Cancelled | WorkLifecycle::Superseded
    );
    // Check both directions. An earlier disposal cannot describe this layer
    // after a later lifecycle transition, nor can a trailing disposal coexist
    // with a live/completed item. Empty layers may carry restored late notes.
    let latest_lifecycle_event = history.events.iter().rev().find(|event| {
        matches!(
            event.kind.as_str(),
            "created" | "reopened" | "completed" | "disposed"
        )
    });
    if !terminal {
        if latest_lifecycle_event.is_some_and(|event| event.kind == "disposed") {
            return Err(corrupt(
                "disposal history disagrees with a non-terminal layer",
            ));
        }
        return Ok(());
    }
    let disposal = history
        .events
        .last()
        .filter(|event| event.kind == "disposed")
        .ok_or_else(|| corrupt("terminal history layer has no final disposal event"))?;
    if disposal.lifecycle != Some(item.lifecycle)
        || disposal.reason != item.disposal_reason
        || disposal.related_work_id != item.superseded_by
    {
        return Err(corrupt(
            "terminal history layer disagrees with its latest disposal event",
        ));
    }
    Ok(())
}

fn validate_restored_relation_basis(
    relations: &RestoredRelationBasis,
    work_id: WorkId,
    item_ids: &HashSet<WorkId>,
) -> Result<(), StoreError> {
    let mut prerequisites = HashSet::new();
    for prerequisite in &relations.prerequisites {
        if prerequisite == &work_id
            || !item_ids.contains(prerequisite)
            || !prerequisites.insert(*prerequisite)
        {
            return Err(corrupt(
                "restored relation basis has a duplicate, self, or dangling prerequisite",
            ));
        }
    }
    let mut blocker_ids = HashSet::new();
    for blocker in &relations.blockers {
        if blocker.work_id != work_id || !blocker_ids.insert(blocker.blocker_id.as_str()) {
            return Err(corrupt(
                "restored relation basis has a duplicate or cross-item blocker",
            ));
        }
        validate_text(&blocker.blocker_id, "restored blocker id")?;
        validate_text(&blocker.detail, "restored blocker detail")?;
        validate_actor(&blocker.created_by)?;
    }
    Ok(())
}

fn validate_history(
    history: &WorkGraphSnapshotHistory,
    item_ids: &HashSet<WorkId>,
) -> Result<(), StoreError> {
    for note in &history.notes {
        validate_text(&note.summary, "history note")?;
        validate_string_set(&note.refs, "history refs")?;
        validate_actor(&note.actor)?;
        if let Some(gate) = &note.gate {
            if note.evidence_kind != crate::WorkEvidenceKind::Generic {
                return Err(corrupt("typed gate is not generic work evidence"));
            }
            let normalized = normalize_gate_evidence_input(
                &gate.name,
                &gate.failed,
                gate.evidence_ref.as_deref(),
            )
            .map_err(|_| corrupt("gate fields do not satisfy the live input contract"))?;
            if normalized.name != gate.name
                || normalized.failed != gate.failed
                || normalized.evidence_ref != gate.evidence_ref
                || gate.passed != gate.failed.is_empty()
            {
                return Err(corrupt("gate fields are not canonical and consistent"));
            }
        }
    }
    for event in &history.events {
        if event.work_revision <= 0 {
            return Err(corrupt("history event revision must be positive"));
        }
        if let Some(reason) = event.reason.as_deref() {
            validate_text(reason, "history event reason")?;
        }
        if event
            .related_work_id
            .is_some_and(|id| !item_ids.contains(&id))
            || event
                .related_work_revision
                .is_some_and(|revision| revision <= 0)
        {
            return Err(corrupt("history event has a dangling or invalid relation"));
        }
        validate_event_shape(event)?;
        validate_actor(&event.actor)?;
    }
    if let Some(completion) = &history.completion {
        validate_text(&completion.summary, "completion summary")?;
        validate_actor(&completion.actor)?;
    }
    Ok(())
}

fn validate_event_shape(event: &crate::WorkGraphSnapshotEvent) -> Result<(), StoreError> {
    let no_relation = || {
        event.lifecycle.is_none()
            && event.related_work_id.is_none()
            && event.related_work_revision.is_none()
    };
    let valid = match event.kind.as_str() {
        "created"
        | "decomposed"
        | "revised"
        | "handoff_expired"
        | "evidence_added"
        | "memory_captured"
        | "typed_evidence_added"
        | "completed" => event.reason.is_none() && no_relation(),
        "claimed" => no_relation(),
        "blocked" | "unblocked" | "released" | "checkpointed" | "handoff_offered"
        | "handoff_cancelled" | "handed_off" | "reopened" => {
            event.reason.is_some() && no_relation()
        }
        "prerequisite_added" | "prerequisite_removed" => {
            event.reason.is_none()
                && event.lifecycle.is_none()
                && event.related_work_id.is_some()
                && event.related_work_revision.is_none()
        }
        "disposed" => {
            event.reason.is_some()
                && event.related_work_revision.is_none()
                && matches!(
                    (event.lifecycle, event.related_work_id),
                    (Some(WorkLifecycle::Cancelled), None)
                        | (Some(WorkLifecycle::Superseded), Some(_))
                )
        }
        "required_child_waived" => {
            event.reason.is_some()
                && event.lifecycle.is_none()
                && event.related_work_id.is_some()
                && event.related_work_revision.is_some()
        }
        _ => false,
    };
    if valid {
        Ok(())
    } else {
        Err(corrupt(
            "history event has an unknown or inconsistent shape",
        ))
    }
}

fn validate_lifecycle_proofs(
    document: &WorkGraphSnapshotDocument,
    records: &[(RestoredRecord, CanonicalObject)],
) -> Result<(), StoreError> {
    for item in &document.body.items {
        let newest = records
            .iter()
            .filter(|(record, _)| record.work_id == item.work_id)
            .max_by_key(|(record, _)| record.generation_index)
            .map(|(record, _)| record)
            .ok_or_else(|| corrupt("work item has no newest restored record"))?;
        let completed = newest.history.completion.is_some();
        if completed != (item.lifecycle == WorkLifecycle::Completed) {
            return Err(corrupt(
                "work lifecycle disagrees with restored completion proof",
            ));
        }
        if newest.relations != restored_relation_basis(document, item.work_id)? {
            return Err(corrupt(
                "work relations disagree with the newest restored record",
            ));
        }
        if newest.project_id != document.body.summary.project_id || newest.item != *item {
            return Err(corrupt(
                "work item planning state disagrees with the newest restored record",
            ));
        }
    }
    Ok(())
}

fn restored_relation_basis(
    document: &WorkGraphSnapshotDocument,
    work_id: WorkId,
) -> Result<RestoredRelationBasis, StoreError> {
    let item = document
        .body
        .items
        .iter()
        .find(|item| item.work_id == work_id)
        .ok_or_else(|| corrupt("restored record names an unknown work item"))?;
    let mut prerequisites = item.prerequisites.clone();
    prerequisites.sort_by_key(|id| id.0);
    prerequisites.dedup();
    if prerequisites.len() != item.prerequisites.len() {
        return Err(corrupt("work item duplicates a prerequisite"));
    }
    let mut blockers = document
        .body
        .blockers
        .iter()
        .filter(|blocker| blocker.work_id == work_id)
        .cloned()
        .collect::<Vec<_>>();
    blockers.sort_by(|left, right| left.blocker_id.cmp(&right.blocker_id));
    if blockers
        .windows(2)
        .any(|pair| pair[0].blocker_id == pair[1].blocker_id)
    {
        return Err(corrupt("work item duplicates a blocker"));
    }
    Ok(RestoredRelationBasis {
        prerequisites,
        blockers,
    })
}

fn validate_memories(document: &WorkGraphSnapshotDocument) -> Result<(), StoreError> {
    let mut keys = HashSet::new();
    for memory in &document.body.memories {
        if !keys.insert(memory.key.as_str()) {
            return Err(corrupt("duplicate project-memory key"));
        }
        super::super::validate_stored_project_memory_key(&memory.key)
            .map_err(|_| corrupt("project-memory key is invalid"))?;
        match &memory.state {
            WorkGraphSnapshotMemoryState::Active {
                body,
                sensitivity,
                actor,
                ..
            } => {
                validate_actor(actor)?;
                match body {
                    WorkGraphSnapshotText::Present { value } => {
                        if *sensitivity == Sensitivity::Restricted && !document.body.summary.widened
                        {
                            return Err(corrupt(
                                "restricted memory plaintext requires a widened snapshot",
                            ));
                        }
                        validate_text(value, "project-memory body")?;
                        if value.len() > MAX_PROJECT_MEMORY_BODY_BYTES {
                            return Err(corrupt("project-memory body exceeds the live limit"));
                        }
                    }
                    WorkGraphSnapshotText::Redacted {
                        sensitivity: redacted,
                    } if redacted == sensitivity && *sensitivity == Sensitivity::Restricted => {}
                    WorkGraphSnapshotText::Redacted { .. } => {
                        return Err(corrupt("memory placeholder sensitivity is invalid"));
                    }
                }
            }
            WorkGraphSnapshotMemoryState::Tombstone { actor, .. } => validate_actor(actor)?,
        }
    }
    Ok(())
}

fn validate_actor(actor: &ActorContext) -> Result<(), StoreError> {
    validate_snapshot_audit_actor_shape(actor).map_err(corrupt)
}

fn validate_text(value: &str, label: &str) -> Result<(), StoreError> {
    if value.is_empty()
        || value.trim() != value
        || value
            .chars()
            .any(|ch| ch != '\n' && ch != '\t' && crate::domain::is_unsafe_rendered_text_char(ch))
    {
        return Err(corrupt(format!(
            "{label} is empty, unnormalized, or unsafe"
        )));
    }
    Ok(())
}

fn validate_string_set(values: &[String], label: &str) -> Result<(), StoreError> {
    for value in values {
        validate_text(value, label)?;
    }
    let mut normalized = values.to_vec();
    normalized.sort();
    normalized.dedup();
    if normalized != values {
        return Err(corrupt(format!("{label} are not sorted and unique")));
    }
    Ok(())
}

#[allow(
    clippy::too_many_lines,
    reason = "one transaction inserts every validated snapshot section and its load audit"
)]
fn insert_prepared_load_on(
    transaction: &Transaction<'_>,
    project_id: &ProjectId,
    actor: &ActorContext,
    loaded_at: DateTime<Utc>,
    prepared: &PreparedLoad,
) -> Result<(), StoreError> {
    transaction.execute_batch("PRAGMA defer_foreign_keys = ON")?;
    for source in &prepared.document.body.sources {
        let object = CanonicalObject::freeze(&source.canonical_json)?;
        SqliteStore::insert_object(transaction, "work_source_snapshot", &object)?;
    }
    for snapshot in &prepared.document.body.items {
        let item = WorkItem {
            schema_version: crate::schema::SCHEMA_VERSION,
            project_id: project_id.clone(),
            work_id: snapshot.work_id,
            short_ref: snapshot.short_ref.clone(),
            root_id: snapshot.root_id,
            parent_id: snapshot.parent_id,
            child_requirement: snapshot.child_requirement,
            title: snapshot.title.clone(),
            outcome: snapshot.outcome.clone(),
            acceptance: snapshot.acceptance.clone(),
            kind: snapshot.kind,
            priority: snapshot.priority,
            labels: snapshot.labels.clone(),
            assigned_to: snapshot.assigned_to.clone(),
            deferred_until: snapshot.deferred_until,
            origin: snapshot.origin,
            source_snapshot_id: snapshot.source_snapshot_id.clone(),
            lifecycle: snapshot.lifecycle,
            revision: 1,
            active_run_id: None,
            restored: true,
            superseded_by: snapshot.superseded_by,
            created_by: actor.clone(),
            created_at: loaded_at,
            updated_at: loaded_at,
        };
        transaction.execute(
            "INSERT INTO work_items (
                 work_id, project_id, short_ref, root_id, parent_id,
                 child_requirement, lifecycle, priority, assigned_to,
                 deferred_until_ms, revision, active_run_id, superseded_by,
                 source_snapshot_hash, latest_event_hash, created_at_ms,
                 updated_at_ms, item_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, 1, NULL,
                       ?11, ?12, NULL, ?13, ?13, ?14)",
            params![
                item.work_id.0.to_string(),
                item.project_id.0,
                item.short_ref,
                item.root_id.0.to_string(),
                item.parent_id.map(|id| id.0.to_string()),
                super::super::work::encode_state_for_restore(item.child_requirement)?,
                super::super::work::encode_state_for_restore(item.lifecycle)?,
                item.priority,
                item.assigned_to,
                item.deferred_until.map(|value| value.timestamp_millis()),
                item.superseded_by.map(|id| id.0.to_string()),
                item.source_snapshot_id.as_ref().map(ObjectHash::as_str),
                loaded_at.timestamp_millis(),
                serde_json::to_vec(&item)?,
            ],
        )?;
    }
    for snapshot in &prepared.document.body.items {
        let item = super::super::work::load_work_item_projection_for_restore(
            transaction,
            snapshot.work_id,
        )?;
        super::super::work::refresh_work_catalog_for_restore(transaction, &item)?;
    }

    let mut newest_record = HashMap::<WorkId, ObjectHash>::new();
    for (record, object) in &prepared.records {
        SqliteStore::insert_object(transaction, "work_restored_record", object)?;
        transaction.execute(
            "INSERT INTO work_restored_records (work_id, generation_index, record_hash)
             VALUES (?1, ?2, ?3)",
            params![
                record.work_id.0.to_string(),
                i64::try_from(record.generation_index)
                    .map_err(|_| corrupt("record generation exceeds SQLite range"))?,
                object.hash().as_str(),
            ],
        )?;
        newest_record.insert(record.work_id, object.hash().clone());
    }
    for item in &prepared.document.body.items {
        let anchor = newest_record
            .get(&item.work_id)
            .ok_or_else(|| corrupt("work item has no restored-record anchor"))?;
        for prerequisite in &item.prerequisites {
            transaction.execute(
                "INSERT INTO work_prerequisites (work_id, prerequisite_id, event_hash)
                 VALUES (?1, ?2, ?3)",
                params![
                    item.work_id.0.to_string(),
                    prerequisite.0.to_string(),
                    anchor.as_str(),
                ],
            )?;
        }
    }
    for blocker in &prepared.document.body.blockers {
        let anchor = newest_record
            .get(&blocker.work_id)
            .ok_or_else(|| corrupt("blocker work has no restored-record anchor"))?;
        let restored = WorkBlocker {
            blocker_id: blocker.blocker_id.clone(),
            work_id: blocker.work_id,
            kind: blocker.kind,
            detail: blocker.detail.clone(),
            created_by: blocker.created_by.clone(),
            created_at: blocker.created_at,
        };
        transaction.execute(
            "INSERT INTO work_blockers (
                 blocker_id, work_id, state, blocker_json,
                 created_event_hash, cleared_event_hash
             ) VALUES (?1, ?2, 'active', ?3, ?4, NULL)",
            params![
                restored.blocker_id,
                restored.work_id.0.to_string(),
                serde_json::to_vec(&restored)?,
                anchor.as_str(),
            ],
        )?;
    }
    for snapshot in &prepared.document.body.items {
        let item = super::super::work::load_work_item_projection_for_restore(
            transaction,
            snapshot.work_id,
        )?;
        super::super::work::refresh_work_catalog_for_restore(transaction, &item)?;
    }

    for memory in &prepared.document.body.memories {
        insert_memory_on(
            transaction,
            project_id,
            prepared.body_object.hash(),
            memory,
            loaded_at,
        )?;
    }
    SqliteStore::rebuild_project_memory_state_on(transaction)?;

    let loaded = WorkGraphSnapshotLoadedEvent {
        attempt_id: uuid::Uuid::now_v7().to_string(),
        schema_version: crate::WORK_GRAPH_SNAPSHOT_SCHEMA_VERSION,
        project_id: project_id.clone(),
        as_of: prepared.document.body.summary.as_of.clone(),
        exporting_build: prepared.document.manifest.exporting_build.clone(),
        widened: prepared.document.body.summary.widened,
        widening_reason: prepared.document.body.summary.widening_reason.clone(),
        redacted: WorkGraphSnapshotRedactedCounts {
            memories: prepared.preview.placeholder_memories.len(),
            ..WorkGraphSnapshotRedactedCounts::default()
        },
        body_sha256: prepared.body_object.hash().clone(),
        actor: actor.clone(),
        loaded_at,
    };
    let object = CanonicalObject::freeze(&loaded)?;
    SqliteStore::insert_object(transaction, "work_graph_snapshot_loaded", &object)?;
    Ok(())
}

fn restored_memory_body(
    body: &WorkGraphSnapshotText,
    sensitivity: Sensitivity,
) -> (String, &'static str) {
    match body {
        WorkGraphSnapshotText::Present { value } if sensitivity != Sensitivity::Restricted => {
            (value.clone(), RESTORED_MEMORY_SOURCE)
        }
        WorkGraphSnapshotText::Present { .. } | WorkGraphSnapshotText::Redacted { .. } => (
            REDACTED_MEMORY_PLACEHOLDER.into(),
            RESTORED_REDACTED_MEMORY_SOURCE,
        ),
    }
}

fn insert_memory_on(
    transaction: &Transaction<'_>,
    project_id: &ProjectId,
    body_hash: &ObjectHash,
    memory: &crate::WorkGraphSnapshotMemory,
    loaded_at: DateTime<Utc>,
) -> Result<(), StoreError> {
    let (body, sensitivity, remembered_at, actor, status, assertion_at, source_ref) =
        match &memory.state {
            WorkGraphSnapshotMemoryState::Active {
                body,
                sensitivity,
                remembered_at,
                actor,
            } => {
                let (body, source_ref) = restored_memory_body(body, *sensitivity);
                (
                    body,
                    *sensitivity,
                    *remembered_at,
                    actor.clone(),
                    MemoryStatus::Active,
                    *remembered_at,
                    source_ref,
                )
            }
            WorkGraphSnapshotMemoryState::Tombstone { retired_at, actor } => (
                REDACTED_MEMORY_PLACEHOLDER.into(),
                Sensitivity::Internal,
                *retired_at,
                actor.clone(),
                MemoryStatus::Tombstoned,
                *retired_at,
                RESTORED_MEMORY_SOURCE,
            ),
        };
    let memory_id = restored_memory_id(body_hash, &memory.key);
    let version = MemoryVersion {
        schema_version: crate::schema::SCHEMA_VERSION,
        memory_id,
        project_key: Some(memory.key.clone()),
        parents: Vec::new(),
        kind: MemoryKind::Episode,
        authority: Authority::Soft,
        delivery: Delivery::OnDemand,
        scope: Scope::Project {
            project: project_id.clone(),
        },
        title: format!("Project memory {}", memory.key),
        body,
        structured_value: None,
        tags: vec!["project-memory".into()],
        evidence: Vec::new(),
        refs: Vec::new(),
        source_snapshot: Some(SourceSnapshot {
            source_ref: source_ref.into(),
            fingerprint: body_hash.to_string(),
            observed_at: loaded_at,
        }),
        confidence: None,
        sensitivity,
        classification_reason: "restored project episode".into(),
        delivery_override_reason: None,
        valid_from: None,
        valid_until: None,
        review_by: None,
        last_verified: None,
        actor: actor.clone(),
        created_at: remembered_at,
    };
    let version_object = CanonicalObject::freeze(&version)?;
    let assertion = MemoryAssertionEvent {
        schema_version: crate::schema::SCHEMA_VERSION,
        memory_id,
        version: version_object.hash().clone(),
        status,
        policy_reason: match status {
            MemoryStatus::Active => "restored project episode is active immediately",
            MemoryStatus::Tombstoned => "restored project-memory tombstone",
            _ => unreachable!("load emits only active or tombstoned memory"),
        }
        .into(),
        actor,
        created_at: assertion_at,
    };
    let assertion_object = CanonicalObject::freeze(&assertion)?;
    SqliteStore::insert_project_memory_version_object(
        transaction,
        &version_object,
        project_id,
        &memory.key,
    )?;
    SqliteStore::insert_object(transaction, "memory_assertion_event", &assertion_object)?;
    SqliteStore::apply_memory_projection(
        transaction,
        version_object.hash(),
        assertion_object.hash(),
        &version,
        &assertion,
        super::super::MemoryProjectionMode::Live,
    )?;
    Ok(())
}

fn restored_memory_id(body_hash: &ObjectHash, key: &str) -> MemoryId {
    let mut digest = Sha256::new();
    digest.update(b"engram-restored-project-memory-v1\0");
    digest.update(body_hash.as_str().as_bytes());
    digest.update([0]);
    digest.update(key.as_bytes());
    let bytes = digest.finalize();
    let mut uuid_bytes = [0_u8; 16];
    uuid_bytes.copy_from_slice(&bytes[..16]);
    MemoryId(uuid::Builder::from_custom_bytes(uuid_bytes).into_uuid())
}

fn graph_destination_is_empty_on(
    connection: &Connection,
    project_id: &ProjectId,
) -> Result<bool, StoreError> {
    let has_work = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_items WHERE project_id = ?1 LIMIT 1)",
        [project_id.0.as_str()],
        |row| row.get::<_, bool>(0),
    )?;
    let has_memory = connection.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM memory_heads INDEXED BY memory_heads_scope
             WHERE project_id = ?1 AND scope_kind = 'project'
             LIMIT 1
         )",
        [project_id.0.as_str()],
        |row| row.get::<_, bool>(0),
    )?;
    let has_work_events = connection.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM objects
             WHERE object_kind = 'work_event'
               AND json_extract(canonical_json, '$.project_id') = ?1
             LIMIT 1
         )",
        [project_id.0.as_str()],
        |row| row.get::<_, bool>(0),
    )?;
    Ok(!has_work && !has_memory && !has_work_events)
}

fn corrupt(detail: impl Into<String>) -> StoreError {
    StoreError::InvalidGraphSnapshot(detail.into())
}
