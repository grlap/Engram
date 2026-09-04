//! Deterministic work-graph recovery snapshots.
//!
//! The snapshot is a planning-and-history artifact, not canonical-object
//! interchange and not execution recovery. SQLite assembles the body from one
//! read transaction; this module owns the substrate-neutral file vocabulary.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::{
    ActorContext, CanonicalObject, ChildRequirement, GateEvidenceRecord, ObjectHash, ProjectId,
    Sensitivity, WorkBlockerKind, WorkEvidenceKind, WorkId, WorkItem, WorkItemKind, WorkLifecycle,
    WorkOrigin, WorkSourceSnapshot, storage::StoreError,
};

mod json_input;

/// Current pre-release work-graph snapshot schema.
pub const WORK_GRAPH_SNAPSHOT_SCHEMA_VERSION: u16 = 1;

/// Work-feed and project-memory heads captured by one snapshot transaction.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkGraphSnapshotCut {
    pub work_feed: i64,
    pub project_memory: i64,
}

/// Placeholder counts retained by section rather than hidden as absence.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkGraphSnapshotRedactedCounts {
    pub items: usize,
    pub blockers: usize,
    pub sources: usize,
    pub records: usize,
    pub memories: usize,
}

/// Exact row counts mirrored into the snapshot manifest.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkGraphSnapshotSectionCounts {
    pub items: usize,
    pub blockers: usize,
    pub sources: usize,
    pub records: usize,
    pub memories: usize,
}

/// Summary fields that the body and manifest must carry verbatim.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkGraphSnapshotSummary {
    pub schema_version: u16,
    pub format_fingerprint: ObjectHash,
    pub project_id: ProjectId,
    pub as_of: WorkGraphSnapshotCut,
    pub widened: bool,
    pub widening_reason: Option<String>,
    pub redacted: WorkGraphSnapshotRedactedCounts,
    pub redactor_status: String,
    pub section_counts: WorkGraphSnapshotSectionCounts,
}

/// One planning item with execution authority deliberately removed.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkGraphSnapshotItem {
    pub work_id: WorkId,
    #[serde(rename = "ref")]
    pub short_ref: String,
    pub root_id: WorkId,
    pub parent_id: Option<WorkId>,
    pub child_requirement: ChildRequirement,
    pub title: String,
    pub outcome: String,
    pub acceptance: Vec<String>,
    pub kind: WorkItemKind,
    pub priority: i32,
    pub labels: Vec<String>,
    pub origin: WorkOrigin,
    pub source_snapshot_id: Option<ObjectHash>,
    pub lifecycle: WorkLifecycle,
    pub prerequisites: Vec<WorkId>,
    pub superseded_by: Option<WorkId>,
    pub assigned_to: Option<String>,
    pub deferred_until: Option<DateTime<Utc>>,
    pub disposal_reason: Option<String>,
}

pub(crate) fn restored_item_basis_matches(
    snapshot: &WorkGraphSnapshotItem,
    project_id: &ProjectId,
    item: &WorkItem,
) -> bool {
    &item.project_id == project_id
        && item.work_id == snapshot.work_id
        && item.short_ref == snapshot.short_ref
        && item.root_id == snapshot.root_id
        && item.parent_id == snapshot.parent_id
        && item.child_requirement == snapshot.child_requirement
        && item.title == snapshot.title
        && item.outcome == snapshot.outcome
        && item.acceptance == snapshot.acceptance
        && item.kind == snapshot.kind
        && item.priority == snapshot.priority
        && item.labels == snapshot.labels
        && item.origin == snapshot.origin
        && item.source_snapshot_id == snapshot.source_snapshot_id
        && item.lifecycle == snapshot.lifecycle
        && item.superseded_by == snapshot.superseded_by
        && item.assigned_to == snapshot.assigned_to
        && item.deferred_until == snapshot.deferred_until
        && item.restored
        && item.revision == 1
        && item.active_run_id.is_none()
}

/// One active planning blocker.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkGraphSnapshotBlocker {
    pub work_id: WorkId,
    pub blocker_id: String,
    pub kind: WorkBlockerKind,
    pub detail: String,
    pub created_by: ActorContext,
    pub created_at: DateTime<Utc>,
}

/// A cited source object retained as its verified canonical JSON value.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkGraphSnapshotSource {
    pub hash: ObjectHash,
    pub canonical_json: Value,
}

/// Gate details carried by a native history note without evidence authority.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkGraphSnapshotGate {
    pub name: String,
    pub passed: bool,
    pub failed: Vec<String>,
    pub evidence_ref: Option<String>,
}

/// One note-like evidence summary in a native history layer.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkGraphSnapshotNote {
    pub evidence_kind: WorkEvidenceKind,
    pub summary: String,
    pub refs: Vec<String>,
    pub gate: Option<WorkGraphSnapshotGate>,
    pub actor: ActorContext,
    pub recorded_at: DateTime<Utc>,
}

/// Compact audited transition stripped of live execution authority.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkGraphSnapshotEvent {
    pub kind: String,
    pub work_revision: i64,
    pub occurred_at: DateTime<Utc>,
    pub reason: Option<String>,
    pub lifecycle: Option<WorkLifecycle>,
    pub related_work_id: Option<WorkId>,
    pub related_work_revision: Option<i64>,
    pub actor: ActorContext,
}

/// Human completion proof retained in a history layer, never as a seal.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkGraphSnapshotCompletion {
    pub summary: String,
    pub completed_at: DateTime<Utc>,
    pub actor: ActorContext,
}

/// History payload that is safe to recreate as one inert restored record.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkGraphSnapshotHistory {
    pub notes: Vec<WorkGraphSnapshotNote>,
    pub events: Vec<WorkGraphSnapshotEvent>,
    pub completion: Option<WorkGraphSnapshotCompletion>,
}

/// One ordered history generation for an item.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkGraphSnapshotRecordPayload {
    /// Canonical restored records will be carried verbatim once load ships.
    Restored {
        object_hash: ObjectHash,
        canonical_json: Value,
    },
    Native {
        history: Box<WorkGraphSnapshotHistory>,
    },
}

/// A history layer ordered by item short ref and generation index.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
pub struct WorkGraphSnapshotRecord {
    pub work_id: WorkId,
    pub generation_index: usize,
    #[serde(flatten)]
    pub payload: WorkGraphSnapshotRecordPayload,
}

/// Immutable, inert history layer recreated by `graph load`.
///
/// The record deliberately carries no run, claim, feed, or completion-seal
/// identity. Its hash binds the project, planning item, relation cut,
/// generation, and history independently of the importing store.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RestoredRecord {
    pub schema_version: u16,
    pub project_id: ProjectId,
    pub work_id: WorkId,
    pub generation_index: usize,
    pub item: WorkGraphSnapshotItem,
    pub relations: RestoredRelationBasis,
    pub history: WorkGraphSnapshotHistory,
}

/// Planning relation cut bound into one inert restored history generation.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RestoredRelationBasis {
    pub prerequisites: Vec<WorkId>,
    pub blockers: Vec<WorkGraphSnapshotBlocker>,
}

/// One attributed late finding whose immutable authority basis is a restored
/// completion record rather than a native run or completion seal.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct RestoredWorkEvidence {
    pub schema_version: u16,
    pub work_id: WorkId,
    pub restored_record: ObjectHash,
    pub sequence: i64,
    pub summary: String,
    pub refs: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<GateEvidenceRecord>,
    pub actor: ActorContext,
    pub created_at: DateTime<Utc>,
}

/// Typed text that is either present or retained as an inert placeholder.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkGraphSnapshotText {
    Present { value: String },
    Redacted { sensitivity: Sensitivity },
}

/// Active project memory or a permanently reserved tombstone.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkGraphSnapshotMemoryState {
    Active {
        body: WorkGraphSnapshotText,
        sensitivity: Sensitivity,
        remembered_at: DateTime<Utc>,
        actor: ActorContext,
    },
    Tombstone {
        retired_at: DateTime<Utc>,
        actor: ActorContext,
    },
}

/// One key-ordered project-memory entry.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
pub struct WorkGraphSnapshotMemory {
    pub key: String,
    #[serde(flatten)]
    pub state: WorkGraphSnapshotMemoryState,
}

/// Canonical deterministic snapshot body.
/// This save-side DTO is deliberately serialize-only. The load slice owns a
/// strict parser that rejects unknown flattened members before constructing it.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
pub struct WorkGraphSnapshotBody {
    #[serde(flatten)]
    pub summary: WorkGraphSnapshotSummary,
    pub items: Vec<WorkGraphSnapshotItem>,
    pub blockers: Vec<WorkGraphSnapshotBlocker>,
    pub sources: Vec<WorkGraphSnapshotSource>,
    pub records: Vec<WorkGraphSnapshotRecord>,
    pub memories: Vec<WorkGraphSnapshotMemory>,
}

/// Human/adapter preflight whose summary must equal the body summary.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
pub struct WorkGraphSnapshotManifest {
    pub exported_at: DateTime<Utc>,
    pub exporting_build: String,
    pub body_sha256: ObjectHash,
    #[serde(flatten)]
    pub summary: WorkGraphSnapshotSummary,
}

/// One human-readable JSON document written by `engram graph save`.
#[derive(Clone, Debug, Eq, JsonSchema, PartialEq, Serialize)]
pub struct WorkGraphSnapshotDocument {
    pub body: WorkGraphSnapshotBody,
    pub manifest: WorkGraphSnapshotManifest,
}

/// Disclosure destination recorded before save bytes leave the process.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkGraphSnapshotDestinationKind {
    DefaultFile,
    File,
    Stdout,
}

/// Immutable source-store record of one attempted snapshot disclosure.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkGraphSnapshotSavedEvent {
    /// `UUIDv7` identity for this disclosure attempt. It prevents two attempts
    /// at the same cut and timestamp from collapsing into one canonical row.
    pub attempt_id: String,
    pub schema_version: u16,
    pub project_id: ProjectId,
    pub as_of: WorkGraphSnapshotCut,
    pub widened: bool,
    pub widening_reason: Option<String>,
    pub redacted: WorkGraphSnapshotRedactedCounts,
    pub body_sha256: ObjectHash,
    pub destination_kind: WorkGraphSnapshotDestinationKind,
    pub actor: ActorContext,
    pub attempted_at: DateTime<Utc>,
}

/// Immutable destination-store audit of one successful snapshot load.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkGraphSnapshotLoadedEvent {
    pub attempt_id: String,
    pub schema_version: u16,
    pub project_id: ProjectId,
    pub as_of: WorkGraphSnapshotCut,
    pub exporting_build: String,
    pub widened: bool,
    pub widening_reason: Option<String>,
    pub redacted: WorkGraphSnapshotRedactedCounts,
    pub body_sha256: ObjectHash,
    pub actor: ActorContext,
    pub loaded_at: DateTime<Utc>,
}

/// Exact validation result shared by dry-run and committed load receipts.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WorkGraphSnapshotLoadPreview {
    pub body_sha256: ObjectHash,
    pub summary: WorkGraphSnapshotSummary,
    pub lifecycle_counts: WorkGraphSnapshotLifecycleCounts,
    pub refs: Vec<String>,
    pub placeholder_memories: Vec<String>,
    pub completed_by_record: Vec<String>,
}

/// Exact item counts by durable planning lifecycle.
#[derive(Clone, Debug, Default, Eq, PartialEq, Serialize)]
pub struct WorkGraphSnapshotLifecycleCounts {
    pub proposed: usize,
    pub open: usize,
    pub completed: usize,
    pub cancelled: usize,
    pub superseded: usize,
}

/// Result of either a dry-run or a committed graph load.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WorkGraphSnapshotLoadResult {
    pub loaded: bool,
    pub preview: WorkGraphSnapshotLoadPreview,
}

/// Snapshot bytes and identity returned only after the save audit commits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkGraphSnapshotExport {
    pub document: WorkGraphSnapshotDocument,
    pub body_sha256: ObjectHash,
    pub redactor_status: String,
}

/// Compares snapshot JSON without the two intentionally varying manifest fields.
/// Malformed JSON and duplicate members are never equivalent, even byte-for-byte.
pub fn graph_snapshot_files_are_equivalent(left: &[u8], right: &[u8]) -> bool {
    fn without_varying_manifest_fields(bytes: &[u8]) -> Option<Value> {
        let mut value = json_input::parse(bytes).ok()?;
        let manifest = value.get_mut("manifest")?.as_object_mut()?;
        manifest.get("exported_at")?.as_str()?;
        manifest.get("exporting_build")?.as_str()?;
        manifest.remove("exported_at")?;
        manifest.remove("exporting_build")?;
        Some(value)
    }
    matches!(
        (without_varying_manifest_fields(left), without_varying_manifest_fields(right)),
        (Some(left), Some(right)) if left == right
    )
}

/// Strictly parses one snapshot document, including the flattened container
/// members whose save-side DTOs intentionally do not derive `Deserialize`.
///
/// # Errors
///
/// Returns [`StoreError`] when the JSON is malformed or any member or scalar
/// representation would be discarded or rewritten by the compiled format.
pub fn parse_work_graph_snapshot_document(
    bytes: &[u8],
) -> Result<WorkGraphSnapshotDocument, StoreError> {
    let original = json_input::parse(bytes)?;
    let parsed: StrictDocument = serde_json::from_value(original.clone())?;
    let document: WorkGraphSnapshotDocument = parsed.into();
    if serde_json::to_value(&document)? != original {
        return Err(StoreError::InvalidGraphSnapshot(
            "snapshot contains a member or scalar representation not preserved by this build"
                .into(),
        ));
    }
    Ok(document)
}

/// Reads only the build-discrimination fields before the current build's
/// strict document decoder sees the rest of the payload. A future build may
/// add members that this build cannot decode, but it must still receive the
/// one generic different-build refusal rather than a misleading corruption
/// error.
pub(crate) fn preflight_work_graph_snapshot_build(bytes: &[u8]) -> Result<(), StoreError> {
    let value = json_input::parse(bytes)?;
    let body = value
        .get("body")
        .and_then(Value::as_object)
        .ok_or_else(|| StoreError::InvalidGraphSnapshot("snapshot body is missing".into()))?;
    let schema_version = body
        .get("schema_version")
        .and_then(Value::as_u64)
        .and_then(|value| u16::try_from(value).ok())
        .ok_or_else(|| {
            StoreError::InvalidGraphSnapshot("snapshot schema version is invalid".into())
        })?;
    let format_fingerprint: ObjectHash =
        serde_json::from_value(body.get("format_fingerprint").cloned().ok_or_else(|| {
            StoreError::InvalidGraphSnapshot("snapshot format fingerprint is missing".into())
        })?)
        .map_err(|error| StoreError::InvalidGraphSnapshot(error.to_string()))?;
    if schema_version != WORK_GRAPH_SNAPSHOT_SCHEMA_VERSION
        || format_fingerprint != work_graph_snapshot_format_fingerprint()?
    {
        return Err(StoreError::GraphDifferentBuild);
    }
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictDocument {
    body: StrictBody,
    manifest: StrictManifest,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictBody {
    schema_version: u16,
    format_fingerprint: ObjectHash,
    project_id: ProjectId,
    as_of: WorkGraphSnapshotCut,
    widened: bool,
    widening_reason: Option<String>,
    redacted: WorkGraphSnapshotRedactedCounts,
    redactor_status: String,
    section_counts: WorkGraphSnapshotSectionCounts,
    items: Vec<WorkGraphSnapshotItem>,
    blockers: Vec<WorkGraphSnapshotBlocker>,
    sources: Vec<WorkGraphSnapshotSource>,
    records: Vec<StrictRecord>,
    memories: Vec<StrictMemory>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct StrictManifest {
    exported_at: DateTime<Utc>,
    exporting_build: String,
    body_sha256: ObjectHash,
    schema_version: u16,
    format_fingerprint: ObjectHash,
    project_id: ProjectId,
    as_of: WorkGraphSnapshotCut,
    widened: bool,
    widening_reason: Option<String>,
    redacted: WorkGraphSnapshotRedactedCounts,
    redactor_status: String,
    section_counts: WorkGraphSnapshotSectionCounts,
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
enum StrictRecord {
    Restored {
        work_id: WorkId,
        generation_index: usize,
        object_hash: ObjectHash,
        canonical_json: Value,
    },
    Native {
        work_id: WorkId,
        generation_index: usize,
        history: Box<WorkGraphSnapshotHistory>,
    },
}

#[derive(Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum StrictMemory {
    Active {
        key: String,
        body: WorkGraphSnapshotText,
        sensitivity: Sensitivity,
        remembered_at: DateTime<Utc>,
        actor: ActorContext,
    },
    Tombstone {
        key: String,
        retired_at: DateTime<Utc>,
        actor: ActorContext,
    },
}

impl StrictBody {
    fn summary(&self) -> WorkGraphSnapshotSummary {
        WorkGraphSnapshotSummary {
            schema_version: self.schema_version,
            format_fingerprint: self.format_fingerprint.clone(),
            project_id: self.project_id.clone(),
            as_of: self.as_of.clone(),
            widened: self.widened,
            widening_reason: self.widening_reason.clone(),
            redacted: self.redacted.clone(),
            redactor_status: self.redactor_status.clone(),
            section_counts: self.section_counts.clone(),
        }
    }
}

impl StrictManifest {
    fn summary(&self) -> WorkGraphSnapshotSummary {
        WorkGraphSnapshotSummary {
            schema_version: self.schema_version,
            format_fingerprint: self.format_fingerprint.clone(),
            project_id: self.project_id.clone(),
            as_of: self.as_of.clone(),
            widened: self.widened,
            widening_reason: self.widening_reason.clone(),
            redacted: self.redacted.clone(),
            redactor_status: self.redactor_status.clone(),
            section_counts: self.section_counts.clone(),
        }
    }
}

impl From<StrictRecord> for WorkGraphSnapshotRecord {
    fn from(value: StrictRecord) -> Self {
        match value {
            StrictRecord::Restored {
                work_id,
                generation_index,
                object_hash,
                canonical_json,
            } => Self {
                work_id,
                generation_index,
                payload: WorkGraphSnapshotRecordPayload::Restored {
                    object_hash,
                    canonical_json,
                },
            },
            StrictRecord::Native {
                work_id,
                generation_index,
                history,
            } => Self {
                work_id,
                generation_index,
                payload: WorkGraphSnapshotRecordPayload::Native { history },
            },
        }
    }
}

impl From<StrictMemory> for WorkGraphSnapshotMemory {
    fn from(value: StrictMemory) -> Self {
        match value {
            StrictMemory::Active {
                key,
                body,
                sensitivity,
                remembered_at,
                actor,
            } => Self {
                key,
                state: WorkGraphSnapshotMemoryState::Active {
                    body,
                    sensitivity,
                    remembered_at,
                    actor,
                },
            },
            StrictMemory::Tombstone {
                key,
                retired_at,
                actor,
            } => Self {
                key,
                state: WorkGraphSnapshotMemoryState::Tombstone { retired_at, actor },
            },
        }
    }
}

impl From<StrictDocument> for WorkGraphSnapshotDocument {
    fn from(value: StrictDocument) -> Self {
        let body_summary = value.body.summary();
        let manifest_summary = value.manifest.summary();
        Self {
            body: WorkGraphSnapshotBody {
                summary: body_summary,
                items: value.body.items,
                blockers: value.body.blockers,
                sources: value.body.sources,
                records: value.body.records.into_iter().map(Into::into).collect(),
                memories: value.body.memories.into_iter().map(Into::into).collect(),
            },
            manifest: WorkGraphSnapshotManifest {
                exported_at: value.manifest.exported_at,
                exporting_build: value.manifest.exporting_build,
                body_sha256: value.manifest.body_sha256,
                summary: manifest_summary,
            },
        }
    }
}

/// Returns the exporting build label carried only by the non-canonical manifest.
#[must_use]
pub fn work_graph_snapshot_exporting_build() -> String {
    format!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"))
}

/// Derives the format identity from the definition compiled with this build.
///
/// # Errors
///
/// Returns [`StoreError`] if the compiled definition cannot be canonicalized.
pub fn work_graph_snapshot_format_fingerprint() -> Result<ObjectHash, StoreError> {
    Ok(
        CanonicalObject::freeze(&compiled_work_graph_snapshot_format()?)?
            .hash()
            .clone(),
    )
}

fn compiled_work_graph_snapshot_format() -> Result<Value, StoreError> {
    let mut document_schema =
        serde_json::to_value(schemars::schema_for!(WorkGraphSnapshotDocument))?;
    let mut source_object_schema = serde_json::to_value(schemars::schema_for!(WorkSourceSnapshot))?;
    let mut restored_record_schema = serde_json::to_value(schemars::schema_for!(RestoredRecord))?;
    strip_schema_annotations(&mut document_schema);
    strip_schema_annotations(&mut source_object_schema);
    strip_schema_annotations(&mut restored_record_schema);
    Ok(json!({
        "schema_version": WORK_GRAPH_SNAPSHOT_SCHEMA_VERSION,
        "schemas": {
            "document": document_schema,
            "source_canonical_json": source_object_schema,
            "restored_record_canonical_json": restored_record_schema
        },
        "sections": ["items", "blockers", "sources", "records", "memories"],
        "record_rules": {
            "restored": "verbatim_canonical_json",
            "native": "planning_history_without_execution_authority"
        },
        "ordering": {
            "items": ["ref"],
            "blockers": ["item_ref", "blocker_id"],
            "sources": ["hash"],
            "records": ["item_ref", "generation_index"],
            "memories": ["key"]
        },
        "redaction": {
            "restricted": "placeholder_unless_widened",
            "secret_ref": "writer_asserted_body_carried_verbatim"
        }
    }))
}

fn strip_schema_annotations(value: &mut Value) {
    let Value::Object(schema) = value else {
        return;
    };
    for key in ["$schema", "title", "description", "examples"] {
        schema.remove(key);
    }
    for key in [
        "additionalItems",
        "additionalProperties",
        "contains",
        "contentSchema",
        "else",
        "if",
        "items",
        "not",
        "propertyNames",
        "then",
        "unevaluatedItems",
        "unevaluatedProperties",
    ] {
        if let Some(child) = schema.get_mut(key) {
            strip_schema_annotations(child);
        }
    }
    for key in ["allOf", "anyOf", "oneOf", "prefixItems"] {
        if let Some(Value::Array(children)) = schema.get_mut(key) {
            for child in children {
                strip_schema_annotations(child);
            }
        }
    }
    for key in [
        "$defs",
        "definitions",
        "dependentSchemas",
        "patternProperties",
        "properties",
    ] {
        if let Some(Value::Object(children)) = schema.get_mut(key) {
            for child in children.values_mut() {
                strip_schema_annotations(child);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[derive(JsonSchema, Serialize)]
    struct NumericShape {
        value: i32,
        optional: Option<String>,
    }

    #[derive(JsonSchema, Serialize)]
    struct TextShape {
        value: String,
        optional: Option<String>,
    }

    #[derive(JsonSchema, Serialize)]
    struct RequiredShape {
        value: i32,
        optional: String,
    }

    #[derive(JsonSchema, Serialize)]
    struct NumericTitleShape {
        title: i32,
    }

    #[derive(JsonSchema, Serialize)]
    struct TextTitleShape {
        title: String,
    }

    fn schema_hash<T: JsonSchema>() -> ObjectHash {
        let mut schema = serde_json::to_value(schemars::schema_for!(T)).unwrap();
        strip_schema_annotations(&mut schema);
        CanonicalObject::freeze(&schema).unwrap().hash().clone()
    }

    #[test]
    fn format_schema_identity_tracks_types_and_nullability() {
        assert_ne!(schema_hash::<NumericShape>(), schema_hash::<TextShape>());
        assert_ne!(
            schema_hash::<NumericShape>(),
            schema_hash::<RequiredShape>()
        );
        assert_ne!(
            schema_hash::<NumericTitleShape>(),
            schema_hash::<TextTitleShape>()
        );
    }

    #[test]
    fn format_definition_carries_the_restored_record_schema() {
        let format = compiled_work_graph_snapshot_format().unwrap();
        let schema = &format["schemas"]["restored_record_canonical_json"];
        assert!(schema.is_object());
        assert!(
            schema
                .pointer("/properties/item")
                .is_some_and(Value::is_object)
        );
        assert!(
            schema
                .pointer("/properties/relations")
                .is_some_and(Value::is_object)
        );
    }
}
