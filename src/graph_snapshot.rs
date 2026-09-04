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
    ActorContext, CanonicalObject, ChildRequirement, ObjectHash, ProjectId, Sensitivity,
    WorkBlockerKind, WorkEvidenceKind, WorkId, WorkItemKind, WorkLifecycle, WorkOrigin,
    WorkSourceSnapshot, storage::StoreError,
};

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

/// Snapshot bytes and identity returned only after the save audit commits.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct WorkGraphSnapshotExport {
    pub document: WorkGraphSnapshotDocument,
    pub body_sha256: ObjectHash,
    pub redactor_status: String,
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
    strip_schema_annotations(&mut document_schema);
    strip_schema_annotations(&mut source_object_schema);
    Ok(json!({
        "schema_version": WORK_GRAPH_SNAPSHOT_SCHEMA_VERSION,
        "schemas": {
            "document": document_schema,
            "source_canonical_json": source_object_schema,
            "restored_record_canonical_json": "verified canonical object value"
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
}
