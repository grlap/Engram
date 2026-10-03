//! Local SQLite object store and integrity verification.

mod acceptance_binding_read;
mod acceptance_verification_read;
mod control_inspection;
mod control_runtime;
mod control_support;
mod doctor;
mod graph_snapshot;
pub mod migration;
mod named_root_read;
mod named_root_sighting_read;
mod objects_tasks;
mod open_schema;
#[cfg(test)]
pub(crate) use open_schema::CopyProbePoint;
pub use open_schema::{
    CopyInterrupt, LiveAuthority, RestoreCopyReport, VerifiedStoreCopy, installed_sidecar_problem,
};
mod policy_admin;
mod project_memory;
mod schema_diagnostics;
mod task_memory;
mod unadmitted_observation;
mod work;
pub(crate) use work::BindingReadRequest;
pub(crate) use work::VerificationReadRequest;
pub(crate) use work::acceptance_attempt_identity;
pub(crate) use work::validate_work_plan;
pub use work::{
    AcceptanceEvaluationReadiness, AcceptanceEvaluationReceipt, AcceptanceEvaluationStatus,
    RecordedLanding, WorkObligationCompletionAction,
};
pub(crate) use work::{AssessedAcceptanceEvaluation, SourceObservationRecord};
pub(crate) use work::{criteria_without_evidence_link, unlinked_criteria_owe_bound_check};

pub(crate) use project_memory::validate_context_generation;

pub(crate) const DECOMPOSE_PROTOCOL_OPERATION: &str = "work_propose:decompose";

/// The protocol operation of a complete atomic plan. Stored as the operation
/// of its protocol attempts and hashed into every plan's receipt key, so its
/// value can never change while a store holds plans.
pub(crate) const PLAN_PROTOCOL_OPERATION: &str = "work_propose:plan";

/// The core operation a plan's receipt is stored under. Stored values; never
/// change it.
pub(crate) const PLAN_CORE_OPERATION: &str = "propose_work_plan";

pub(crate) const DECOMPOSITION_RETRY_REMEDY: &str = "inspect the parent and its existing children; reuse the already-created child when present; add new work only for a genuinely different child intent";

pub use control_inspection::ControlSessionInspection;
pub use schema_diagnostics::{
    StoreOpenRefusalKind, running_schema_reference, store_open_refusal_kind, store_schema_reference,
};

/// The most held items an implicit-target refusal names; the rest are
/// counted in `more`.
pub(crate) const IMPLICIT_TARGET_HELD_SHOWN: usize = 3;

/// A word that named no item, the focus it would have acted on, and the
/// items this session holds instead, by short ref.
#[derive(Debug)]
pub struct ImplicitTargetConflict {
    pub operation: String,
    pub focus: String,
    /// What can be done on the focus, so the refusal offers a command that
    /// works there.
    pub focus_state: ImplicitFocusState,
    /// At most a few held items; `more` counts the rest.
    pub held: Vec<String>,
    pub more: usize,
}

/// The state of the focus a refused bare word would have acted on.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ImplicitFocusState {
    /// Open, and no live claim holds it: this session may claim it.
    Unclaimed,
    /// Open, and another session holds it.
    HeldElsewhere,
    /// Not open: completed, cancelled, superseded or proposed.
    NotOpen,
}

impl ImplicitFocusState {
    /// The value as the refusal's details spell it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unclaimed => "unclaimed",
            Self::HeldElsewhere => "held_elsewhere",
            Self::NotOpen => "not_open",
        }
    }
}

impl std::fmt::Display for ImplicitTargetConflict {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            formatter,
            "{} named no item, and the focus {} is not held by this session, which holds {}",
            self.operation,
            self.focus,
            self.held.join(", ")
        )?;
        if self.more > 0 {
            write!(formatter, " and {} more", self.more)?;
        }
        write!(formatter, "; nothing was recorded; name the item")
    }
}

pub(crate) const PENDING_HANDOFF_REFUSAL: &str =
    "a live handoff offer blocks this operation; cancel the offer, or let it be accepted or expire";

pub(crate) fn parent_not_open_remedy(lifecycle: crate::domain::WorkLifecycle) -> &'static str {
    match lifecycle {
        crate::domain::WorkLifecycle::Proposed => {
            "the parent is proposed, not open; inspect it before adding children"
        }
        crate::domain::WorkLifecycle::Open => "inspect the parent before retrying",
        crate::domain::WorkLifecycle::Completed
        | crate::domain::WorkLifecycle::Cancelled
        | crate::domain::WorkLifecycle::Superseded => {
            "file an independent root follow-up or add under an open ancestor"
        }
    }
}

#[cfg(test)]
mod test_support;

#[cfg(test)]
pub(crate) use work::test_support::source_mutation_from_basis;
#[cfg(test)]
pub(crate) use work::test_support::{
    HostCheck, assessed_verification_fixture, bound_verification_refusal_fixture,
    stale_deciding_refusal_fixture, unreported_move_fixture, verification_note_fixture,
};

#[cfg(test)]
pub(crate) mod concurrent_commit;

#[cfg(test)]
use control_runtime::resolve_verification_environment_on;
use control_support::{normalize_control_policy_actor, normalize_control_policy_idempotency_key};
use project_memory::{
    derived_project_memory_state_on, derived_project_memory_state_rows_on,
    lookup_project_memory_on, project_memory_state_on, validate_keyed_project_memory_shape,
    validate_stored_project_memory_key,
};
use task_memory::{fts_query, normalize_project_memory_query};

pub(crate) use work::RequiredChildSuccessor;
pub(crate) use work::SelectedStatusNote;
pub(crate) use work::WorkDiscoveryRow;
pub(crate) use work::{
    AssessmentBoundary, RecordedObligationEnd, VerificationAssessment,
    VerificationObligationAssessment,
};
pub(crate) use work::{VerificationFacts, WorkNoteRecord};
pub(crate) use work::{WorkEvidenceProjectionSummary, WorkObligationRecord};
pub(crate) use work::{
    WorkRecordAddress, WorkRecordContent, WorkRecordFamily, WorkRecordIndex, WorkRecordKind,
    WorkRecordOrder,
};

pub(crate) use work::{
    CompleteWorkStorageResult, CompletionRecoverySnapshot, StageWorkSessionDelivery,
    WorkNoteCapture, checkpoint_run_feed_end, normalize_completion_acceptance_shape,
};

pub(crate) fn admit_session_id(session: &crate::SessionId) -> Result<(), StoreError> {
    session
        .validate_admitted()
        .map_err(|error| StoreError::InvalidWork(error.as_str().into()))
}

pub(crate) fn admit_session_id_text(value: &str) -> Result<(), StoreError> {
    crate::domain::validate_session_id_length(value)
        .map_err(|error| StoreError::InvalidWork(error.as_str().into()))
}

pub(crate) fn admit_live_actor_session(
    actor: &crate::domain::ActorContext,
) -> Result<(), StoreError> {
    if let Some(session) = actor.session_id.as_ref() {
        admit_session_id(session)?;
    }
    Ok(())
}

pub(crate) const PROCESS_DEFAULT_WORK_SESSION_NAMESPACE: &str = "local-process-";
pub(crate) const PROCESS_DEFAULT_WORK_SESSION_PREFIX: &str = "local-process-v1-";
pub(crate) const PROCESS_DEFAULT_WORK_SESSION_RETENTION_SECONDS: i64 = 7 * 24 * 60 * 60;
pub(crate) const PROCESS_DEFAULT_WORK_SESSION_REUSE_REFUSAL: &str = "process-default work session cannot be reused; run without --session-id to receive a fresh process default";

#[cfg(test)]
pub(crate) use work::{
    reset_work_catalog_count_queries, reset_work_event_decode_count,
    reset_work_item_projection_decode_count, work_catalog_count_queries, work_event_decode_count,
    work_item_projection_decode_count,
};

use std::{
    collections::{HashMap, HashSet},
    path::Path,
    time::Duration,
};

#[cfg(test)]
use std::cell::Cell;

#[cfg(test)]
type TestTableColumn = (i64, String, String, i64, Option<String>, i64);

#[cfg(test)]
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct TestDatabaseShapeSnapshot {
    schema: Vec<(String, String, String, i64, Option<String>)>,
    table_info: Vec<(String, Vec<TestTableColumn>)>,
    rows: Vec<(String, Vec<Vec<u8>>)>,
}

#[cfg(test)]
pub(crate) fn test_database_shape_snapshot(
    connection: &Connection,
) -> Result<TestDatabaseShapeSnapshot, rusqlite::Error> {
    let schema = connection
        .prepare(
            "SELECT type, name, tbl_name, rootpage, sql
             FROM sqlite_master ORDER BY type, name",
        )?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let table_names = schema
        .iter()
        .filter_map(|(kind, name, _, _, _)| (kind == "table").then_some(name.clone()))
        .collect::<Vec<_>>();
    let mut table_info = Vec::with_capacity(table_names.len());
    let mut rows = Vec::with_capacity(table_names.len());
    for table in table_names {
        let quoted = table.replace('"', "\"\"");
        let info = connection
            .prepare(&format!("PRAGMA table_info(\"{quoted}\")"))?
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        table_info.push((table.clone(), info));

        let mut statement = connection.prepare(&format!("SELECT * FROM \"{quoted}\""))?;
        let column_count = statement.column_count();
        let mut table_rows = statement
            .query_map([], |row| {
                let mut encoded = Vec::new();
                for index in 0..column_count {
                    let value = match row.get_ref(index)? {
                        rusqlite::types::ValueRef::Null => vec![0],
                        rusqlite::types::ValueRef::Integer(value) => {
                            let mut bytes = vec![1];
                            bytes.extend_from_slice(&value.to_be_bytes());
                            bytes
                        }
                        rusqlite::types::ValueRef::Real(value) => {
                            let mut bytes = vec![2];
                            bytes.extend_from_slice(&value.to_bits().to_be_bytes());
                            bytes
                        }
                        rusqlite::types::ValueRef::Text(value) => {
                            let mut bytes = vec![3];
                            bytes.extend_from_slice(value);
                            bytes
                        }
                        rusqlite::types::ValueRef::Blob(value) => {
                            let mut bytes = vec![4];
                            bytes.extend_from_slice(value);
                            bytes
                        }
                    };
                    encoded.extend_from_slice(&(value.len() as u64).to_be_bytes());
                    encoded.extend_from_slice(&value);
                }
                Ok(encoded)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        table_rows.sort();
        rows.push((table, table_rows));
    }
    Ok(TestDatabaseShapeSnapshot {
        schema,
        table_info,
        rows,
    })
}

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;

#[cfg(test)]
use crate::domain::{DeltaItem, MemoryRecord, TaskDelta};
use crate::{
    CanonicalObject, ObjectId,
    control::effective_mediated_effects,
    domain::{
        ActorContext, AssuranceLevel, Authority, CONTROL_SCHEMA_VERSION, ChangeCursor,
        ControlAssurance, ControlEpochs, ControlPolicy, ControlSessionBinding,
        ControlSessionStatus, ControlTurnBeginDecision, ControlTurnCheckpointDecision,
        ControlTurnDecision, ControlWorkBinding, Delivery, EffectClass, EnvironmentComponents,
        EnvironmentEvidence, EnvironmentEvidenceInput, EnvironmentEvidenceReference,
        ExecutionObservation, ExecutionObservationInput, ExecutionObservationReference,
        ExecutionOutcome, ForgetProjectMemoryRequest, HostPathPolicy, IssuedTurnGrant,
        MAX_PROJECT_MEMORY_BODY_BYTES, MAX_PROJECT_MEMORY_KEY_BYTES,
        MAX_PROJECT_MEMORY_QUERY_BYTES, MAX_PROJECT_MEMORY_QUERY_TOKENS, MemoryAssertionEvent,
        MemoryId, MemoryKind, MemoryStatus, MemorySummary, MemoryVersion, NamedRootBindingEvent,
        NamedRootBindingKind, NamedRootBindingReceipt, NamedRootEndReason, NoteReceipt,
        NoteRequest, NoteVisibility, OBLIGATION_RULE_SET_SCHEMA_VERSION, ObligationRuleSet,
        OpenWorkObligation, ParticipantMembership, ProjectId, ProjectMemoryFull, ProjectMemoryList,
        ProjectMemoryListRow, ProjectMemoryMutationReceipt, ProjectPolicyAuthorityDecision,
        ProjectPolicyEpoch, ProjectPolicyOperation, RememberProjectMemoryRequest, SCHEMA_VERSION,
        Scope, Sensitivity, SessionId, SessionPhase, TaskAdmissionEpoch, TaskId, TurnBeginDecision,
        TurnBeginReceipt, TurnBeginSnapshot, TurnCheckpointDecision, TurnCheckpointEvent,
        TurnCheckpointReceipt, TurnCheckpointSnapshot, TurnDecision, TurnEvaluationInput,
        TurnGrantState, TurnGrantSupersession, TurnGrantSupersessionReason, TurnIntent,
        TurnNextIntent, VerificationEvidence, VerificationEvidenceInput, VerificationKind,
        VerificationResult, WorkCompletionRecoveryCause, WorkReferenceCandidate,
    },
    memory::{DevelopmentNoopRedactor, Redactor, activation_policy, classify_note},
    schema::{
        CONTROL_POLICY_AUTHORITY_SCHEMA_VERSION,
        CONTROL_POLICY_OPERATION_FINGERPRINT_SCHEMA_VERSION, CONTROL_POLICY_SCHEMA_VERSION,
        CONTROL_POLICY_STATE_SCHEMA_VERSION,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SchemaOwner {
    Core,
    Work,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SchemaDurability {
    Durable,
    Rebuildable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct SchemaDefinition {
    object_type: String,
    name: String,
    sql: String,
}

// `project_memory_advertisements` is discardable delivery bookkeeping rather
// than canonical state: explicit projection repair may drop it and cause one
// harmless reannouncement. `project_memory_state` is reconstructed from
// verified project-memory versions and assertion events.
const CORE_REBUILDABLE_SCHEMA_OBJECTS: &[(&str, &str)] = &[
    ("table", "object_fts"),
    ("index", "objects_memory_assertion_version"),
    ("index", "objects_project_memory_key"),
    ("index", "objects_project_memory_root"),
    ("index", "objects_graph_snapshot_audit"),
    ("index", "objects_graph_snapshot_load_audit"),
    ("index", "memory_heads_scope"),
    ("index", "memory_heads_work_scope"),
    ("table", "project_memory_state"),
    ("table", "project_memory_advertisements"),
    ("index", "control_changes_task_cursor"),
    ("index", "control_sessions_work_run"),
];

const DIFFERENT_BUILD_STORE_MESSAGE: &str = "the store schema is not recognized by this Engram build; use the Engram build that owns this store; this build cannot convert its schema";

static CURRENT_SCHEMA_REFERENCE: std::sync::OnceLock<Vec<SchemaDefinition>> =
    std::sync::OnceLock::new();

std::thread_local! {
    static BUILDING_SCHEMA_REFERENCE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

struct SchemaReferenceBuildGuard;

impl SchemaReferenceBuildGuard {
    fn enter() -> Result<Self, StoreError> {
        let already_building = BUILDING_SCHEMA_REFERENCE.with(|state| state.replace(true));
        if already_building {
            return Err(StoreError::InvalidControlProjection(
                "recursive current-schema reference construction".into(),
            ));
        }
        Ok(Self)
    }
}

impl Drop for SchemaReferenceBuildGuard {
    fn drop(&mut self) {
        BUILDING_SCHEMA_REFERENCE.with(|state| state.set(false));
    }
}

#[cfg(test)]
fn building_schema_reference() -> bool {
    BUILDING_SCHEMA_REFERENCE.with(std::cell::Cell::get)
}

fn normalized_schema_definition(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(super) fn different_build_store_error() -> StoreError {
    StoreError::DifferentBuildSchema
}

pub(crate) fn is_different_build_store_error(error: &StoreError) -> bool {
    matches!(error, StoreError::DifferentBuildSchema)
}

pub(super) fn require_current_schema_marker(stored: i64, current: i64) -> Result<(), StoreError> {
    if stored == current {
        Ok(())
    } else {
        Err(different_build_store_error())
    }
}

fn stored_schema_definitions(connection: &Connection) -> Result<Vec<SchemaDefinition>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT type, name, sql
         FROM sqlite_schema
         WHERE substr(name, 1, 7) COLLATE NOCASE != 'sqlite_' AND sql IS NOT NULL
         ORDER BY type, name",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(SchemaDefinition {
            object_type: row.get(0)?,
            name: row.get(1)?,
            sql: normalized_schema_definition(&row.get::<_, String>(2)?),
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

fn current_schema_reference() -> Result<&'static [SchemaDefinition], StoreError> {
    if let Some(reference) = CURRENT_SCHEMA_REFERENCE.get() {
        return Ok(reference);
    }
    let guard = SchemaReferenceBuildGuard::enter()?;
    let store = SqliteStore::open_in_memory_with_host_path_identity(None)?;
    let reference = stored_schema_definitions(&store.connection)?;
    drop(store);
    drop(guard);
    let _ = CURRENT_SCHEMA_REFERENCE.set(reference);
    CURRENT_SCHEMA_REFERENCE
        .get()
        .map(Vec::as_slice)
        .ok_or_else(|| {
            StoreError::InvalidControlProjection(
                "current-schema reference was not initialized".into(),
            )
        })
}

fn schema_object_matches_owner(definition: &SchemaDefinition, owner: SchemaOwner) -> bool {
    let work_owned = work::owns_schema_object(&definition.name);
    matches!(
        (owner, work_owned),
        (SchemaOwner::Core, false) | (SchemaOwner::Work, true)
    )
}

fn schema_object_matches_durability(
    definition: &SchemaDefinition,
    durability: SchemaDurability,
) -> bool {
    let rebuildable = if work::owns_schema_object(&definition.name) {
        work::is_rebuildable_schema_object(&definition.object_type, &definition.name)
    } else {
        (definition.object_type == "table" && is_fts_schema_object(&definition.name, "object_fts"))
            || CORE_REBUILDABLE_SCHEMA_OBJECTS
                .contains(&(definition.object_type.as_str(), definition.name.as_str()))
    };
    matches!(
        (durability, rebuildable),
        (SchemaDurability::Durable, false) | (SchemaDurability::Rebuildable, true)
    )
}

fn is_fts_schema_object(name: &str, table: &str) -> bool {
    name == table
        || name.strip_prefix(table).is_some_and(|suffix| {
            matches!(
                suffix,
                "_data" | "_idx" | "_content" | "_docsize" | "_config"
            )
        })
}

fn orphan_fts_schema_issue(
    reference: &[SchemaDefinition],
    actual: &[SchemaDefinition],
    owner: SchemaOwner,
) -> Option<String> {
    let table = match owner {
        SchemaOwner::Core => "object_fts",
        SchemaOwner::Work => "work_catalog_fts",
    };
    let shadow = actual.iter().find(|definition| {
        definition.object_type == "table"
            && definition.name != table
            && is_fts_schema_object(&definition.name, table)
    })?;
    let recognized_parent = reference
        .iter()
        .find(|definition| definition.object_type == "table" && definition.name == table)
        .is_some_and(|definition| actual.contains(definition));
    if recognized_parent {
        None
    } else {
        Some(format!(
            "shadow table {} has no recognized FTS owner {table}",
            shadow.name
        ))
    }
}

pub(super) fn current_schema_definition_issue(
    connection: &Connection,
    owner: SchemaOwner,
    durability: SchemaDurability,
) -> Result<Option<String>, StoreError> {
    let reference = current_schema_reference()?;
    let actual = stored_schema_definitions(connection)?;
    if durability == SchemaDurability::Durable
        && let Some(issue) = orphan_fts_schema_issue(reference, &actual, owner)
    {
        return Ok(Some(issue));
    }
    let expected = reference
        .iter()
        .filter(|definition| schema_object_matches_owner(definition, owner))
        .filter(|definition| schema_object_matches_durability(definition, durability))
        .cloned()
        .collect::<Vec<_>>();
    let actual = actual
        .into_iter()
        .filter(|definition| schema_object_matches_owner(definition, owner))
        .filter(|definition| schema_object_matches_durability(definition, durability))
        .collect::<Vec<_>>();
    if actual == expected {
        return Ok(None);
    }

    for definition in &expected {
        match actual
            .iter()
            .find(|candidate| candidate.name == definition.name)
        {
            None => {
                return Ok(Some(format!(
                    "missing required {} {}",
                    definition.object_type, definition.name
                )));
            }
            Some(candidate) if candidate != definition => {
                return Ok(Some(format!(
                    "{} {} has a different definition",
                    definition.object_type, definition.name
                )));
            }
            Some(_) => {}
        }
    }
    let expected_names = expected
        .iter()
        .map(|definition| definition.name.as_str())
        .collect::<HashSet<_>>();
    if let Some(unexpected) = actual
        .iter()
        .find(|definition| !expected_names.contains(definition.name.as_str()))
    {
        return Ok(Some(format!(
            "unexpected {} {}",
            unexpected.object_type, unexpected.name
        )));
    }
    Ok(Some(
        "schema definitions differ from the current build".into(),
    ))
}

fn stored_schema_definition(
    connection: &Connection,
    object: &str,
) -> Result<Option<(String, String, String)>, StoreError> {
    connection
        .query_row(
            "SELECT type, name, sql FROM sqlite_schema WHERE name = ?1",
            [object],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(StoreError::from)
}

pub(super) fn drop_schema_object(connection: &Connection, name: &str) -> Result<bool, StoreError> {
    let Some((object_type, _, _)) = stored_schema_definition(connection, name)? else {
        return Ok(false);
    };
    let drop_kind = match object_type.as_str() {
        "table" => "TABLE",
        "index" => "INDEX",
        "trigger" => "TRIGGER",
        "view" => "VIEW",
        other => {
            return Err(StoreError::InvalidControlProjection(format!(
                "cannot replace schema object {name} with unsupported type {other}"
            )));
        }
    };
    let quoted = name.replace('"', "\"\"");
    connection.execute_batch(&format!("DROP {drop_kind} \"{quoted}\";"))?;
    Ok(true)
}

#[derive(Serialize)]
struct NoteIntentFingerprint<'a> {
    project_id: &'a crate::domain::ProjectId,
    task_id: Option<TaskId>,
    work_id: Option<crate::domain::WorkId>,
    prose: &'a str,
    visibility: NoteVisibility,
    kind: Option<crate::domain::MemoryKind>,
    authority: Option<crate::domain::Authority>,
    sensitivity: Option<Sensitivity>,
    title: Option<&'a str>,
    tags: &'a [String],
    evidence: &'a [ObjectId],
    refs: &'a [String],
    actor: &'a ActorContext,
}

#[derive(Serialize)]
struct NoteIntentKey<'a> {
    project_id: &'a crate::domain::ProjectId,
    actor_id: &'a str,
    session_id: Option<&'a SessionId>,
    caller_key: &'a str,
}

pub(crate) struct BeginWorkProtocolAttempt<'a, T, B> {
    pub(crate) project_id: &'a crate::domain::ProjectId,
    pub(crate) session_id: &'a SessionId,
    pub(crate) operation: &'a str,
    pub(crate) idempotency_key: &'a str,
    pub(crate) intent: &'a T,
    pub(crate) basis: &'a B,
    pub(crate) now: DateTime<Utc>,
}

pub(crate) struct BeginGateWorkProtocolAttempt<'a, B> {
    pub(crate) project_id: &'a crate::domain::ProjectId,
    pub(crate) session_id: &'a SessionId,
    pub(crate) basis: &'a B,
    pub(crate) now: DateTime<Utc>,
}

#[derive(Serialize)]
struct TurnIntentFingerprint<'a> {
    control_schema_version: u16,
    session_id: &'a SessionId,
    task_id: Option<TaskId>,
    intent: &'a TurnIntent,
}

#[derive(Serialize)]
struct ControlSessionBindFingerprint<'a> {
    control_schema_version: u16,
    project_id: &'a crate::domain::ProjectId,
    external_ref: &'a str,
    title: &'a str,
    session_id: &'a SessionId,
    actor: &'a ActorContext,
    assurance: ControlAssurance,
    mediated_effects: &'a [EffectClass],
    #[serde(skip_serializing_if = "Option::is_none")]
    work_binding: Option<&'a ControlWorkBinding>,
    capability_map_revision: i64,
    idempotency_key: &'a str,
}

#[derive(Serialize)]
struct ControlTurnBeginFingerprint<'a> {
    control_schema_version: u16,
    session_id: &'a SessionId,
    grant_id: &'a str,
    delivery_tokens: &'a [String],
    idempotency_key: &'a str,
}

#[derive(Serialize)]
struct ControlTurnCheckpointFingerprint<'a> {
    control_schema_version: u16,
    session_id: &'a SessionId,
    grant_id: &'a str,
    next_intent: TurnNextIntent,
    #[serde(skip_serializing_if = "execution_observations_are_empty")]
    observations: &'a [ExecutionObservationInput],
    #[serde(skip_serializing_if = "verification_evidence_inputs_are_empty")]
    verification_evidence: &'a [VerificationEvidenceInput],
    #[serde(skip_serializing_if = "environment_evidence_inputs_are_empty")]
    environment_evidence: &'a [EnvironmentEvidenceInput],
    idempotency_key: &'a str,
}

#[derive(Serialize)]
struct NamedRootBindingFingerprint<'a> {
    control_schema_version: u16,
    session_id: &'a SessionId,
    claim_id: &'a crate::domain::WorkClaimId,
    claim_fence: i64,
    workspace_id: &'a str,
    generation: i64,
    named_at: DateTime<Utc>,
    kind: NamedRootBindingKind,
    end_reason: Option<NamedRootEndReason>,
    idempotency_key: &'a str,
}

fn execution_observations_are_empty(value: &&[ExecutionObservationInput]) -> bool {
    value.is_empty()
}

fn verification_evidence_inputs_are_empty(value: &&[VerificationEvidenceInput]) -> bool {
    value.is_empty()
}

fn environment_evidence_inputs_are_empty(value: &&[EnvironmentEvidenceInput]) -> bool {
    value.is_empty()
}

#[derive(Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
#[allow(
    clippy::enum_variant_names,
    reason = "each variant names the audited policy operation it fingerprints"
)]
enum ControlPolicyOperationFingerprint<'a> {
    SetRequiredAssurance {
        fingerprint_schema_version: u16,
        idempotency_key: &'a str,
        required_assurance: ControlAssurance,
        authorized_by: &'a ActorContext,
        expected_policy: Option<&'a ObjectId>,
    },
    SetObligationRuleSet {
        fingerprint_schema_version: u16,
        idempotency_key: &'a str,
        obligation_rule_set: &'a ObjectId,
        authorized_by: &'a ActorContext,
        expected_policy: Option<&'a ObjectId>,
    },
    SetAcceptanceEvaluation {
        fingerprint_schema_version: u16,
        idempotency_key: &'a str,
        acceptance_evaluation: &'a crate::domain::AcceptanceEvaluationPolicy,
        authorized_by: &'a ActorContext,
        expected_policy: Option<&'a ObjectId>,
    },
}

/// What moved a run past the evidence basis an acceptance evaluation read
/// through.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum EvaluationBasisMove {
    /// A host check was recorded after the basis: a verification, an
    /// environment record, or an obligation opened or resolved. Re-read the
    /// item, take the check into account, and submit again.
    CheckRecorded,
    /// The source changed after the basis, and not to the revision the
    /// evaluation declared it judged. The evaluation is void; request a new
    /// one.
    SourceChanged,
}

pub use crate::domain::DecidingObservation;

/// What a stale-evaluation recovery carries beside its cause, never inside
/// it: the cause's words stay as they are, and this context is read from the
/// same snapshot that decided the cause. Every field is `None` for another
/// cause.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct StaleRecoveryContext {
    /// The source observation that decided a source move after the newest
    /// evaluation's cut.
    pub deciding_observation: Option<Box<DecidingObservation>>,
    pub source: Option<Box<crate::domain::AcceptanceSourceRecoveryCause>>,
}

impl DecidingObservation {
    /// One line naming the observation beside the evaluated revision. Stored
    /// text is shown with control characters escaped, so the line stays one
    /// line.
    #[must_use]
    pub fn sentence(&self) -> String {
        let field = |value: Option<&str>| value.map_or_else(|| "not recorded".to_owned(), one_line);
        format!(
            "The deciding source observation is at run-feed position {}: workspace {}, revision {}, reported by session {}, observed {} (recorded {}); the evaluation {} revision {}{}.",
            self.position,
            field(self.workspace.as_deref()),
            field(self.revision.as_deref()),
            one_line(&self.reporting_session.0),
            self.observed_at
                .map_or_else(|| "at a time not recorded".to_owned(), |at| at.to_rfc3339()),
            self.recorded_at.to_rfc3339(),
            if self.evaluated_revision_declared {
                "declared"
            } else {
                "judged"
            },
            self.evaluated_revision
                .as_deref()
                .map_or_else(|| "not known".to_owned(), one_line),
            if self.evaluated_revision_declared {
                ""
            } else {
                " at its cut"
            },
        )
    }
}

/// Stored text shown in the refusal sentence: control characters escaped, so
/// the sentence stays one line, and at most this many bytes of it, so the
/// whole message stays well within what a host relays.
const MAX_SENTENCE_FIELD_BYTES: usize = 300;

/// A host that relays the refusal treats stderr holding this phrase as a
/// locked store and retries; stored text must never make the refusal read so.
const LOCKED_STORE_PHRASE: &str = "database is locked";

fn one_line(value: &str) -> String {
    // Whether the field could read as the phrase once a renderer lowercases
    // it and collapses its whitespace, as the CLI's text refusal does. Such a
    // field writes every whitespace character as a visible escape, which no
    // normalization joins back into the phrase; the bound below counts those
    // escapes, so the guard never lengthens the sentence past it.
    let guard = value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
        .contains(LOCKED_STORE_PHRASE);
    let mut shown = String::new();
    for character in value.chars() {
        let escaped: String = if crate::domain::is_unsafe_rendered_text_char(character) {
            character.escape_default().collect()
        } else if guard && character.is_whitespace() {
            character.escape_unicode().collect()
        } else {
            character.to_string()
        };
        if shown.len() + escaped.len() > MAX_SENTENCE_FIELD_BYTES {
            shown.push_str("… (");
            shown.push_str(&value.len().to_string());
            shown.push_str(" bytes stored)");
            return shown;
        }
        shown.push_str(&escaped);
    }
    shown
}

/// Serialized JSON in which no string spells the locked-store phrase, while
/// every string decodes to the same text: the spaces inside the phrase are
/// written as `\u0020` escapes. The CLI writes every JSON receipt this way,
/// and a refusal on stderr that carries host-recorded text; never a real lock
/// error, which must still read as one.
#[must_use]
pub fn json_without_locked_store_phrase(json: &str) -> String {
    let lower = json.to_ascii_lowercase();
    let mut out = String::with_capacity(json.len());
    let mut from = 0;
    while let Some(found) = lower[from..].find(LOCKED_STORE_PHRASE) {
        let start = from + found;
        let end = start + LOCKED_STORE_PHRASE.len();
        out.push_str(&json[from..start]);
        out.push_str(&json[start..end].replace(' ', "\\u0020"));
        from = end;
    }
    out.push_str(&json[from..]);
    out
}

impl crate::domain::StaleVerificationSource {
    /// One sentence naming the record that decided a stale verification
    /// beside the check's own source. Each stored field is escaped and
    /// bounded on its own, so the comparison always survives.
    #[must_use]
    pub fn sentence(&self) -> String {
        use crate::domain::StaleSourceDecider;
        let field = |value: Option<&str>| value.map_or_else(|| "not recorded".to_owned(), one_line);
        let kind = if self.source_changed == Some(false) {
            "a sighting"
        } else {
            "a change"
        };
        let check = format!(
            "the check ran on revision {} in workspace {}",
            one_line(&self.verification_revision),
            one_line(&self.verification_workspace)
        );
        match self.decider {
            StaleSourceDecider::LatestChange => format!(
                "The deciding source record is the run's latest source change at run-feed position {}: {kind}, workspace {}, revision {}; {check}.",
                self.position,
                field(self.workspace.as_deref()),
                field(self.revision.as_deref()),
            ),
            StaleSourceDecider::RootSighting => format!(
                "The deciding source record is the named root's newest sighting at run-feed position {}: {kind}, workspace {}, revision {}; {check}.",
                self.position,
                field(self.workspace.as_deref()),
                field(self.revision.as_deref()),
            ),
            StaleSourceDecider::RootBinding => format!(
                "The deciding source record is the named root's binding at run-feed position {}: workspace {}, generation {}; {check}, not of that root's workspace and generation or not after its binding.",
                self.position,
                field(self.workspace.as_deref()),
                self.root_generation.map_or_else(
                    || "not recorded".to_owned(),
                    |generation| generation.to_string()
                ),
            ),
            StaleSourceDecider::MeasuredSighting => format!(
                "The deciding source record is the newest measured sighting after an unadmitted change, at run-feed position {}: workspace {}, revision {}; {check}.",
                self.position,
                field(self.workspace.as_deref()),
                field(self.revision.as_deref()),
            ),
        }
    }
}

/// The refusal message's addition: one sentence naming the deciding
/// observation, after the unchanged reason, or nothing.
fn deciding_observation_suffix(observation: Option<&DecidingObservation>) -> String {
    observation.map_or_else(String::new, |observation| {
        format!(" {}", observation.sentence())
    })
}

impl EvaluationBasisMove {
    /// What the evaluator does next. The refusal message and the MCP details
    /// both carry this text, because a host may relay only the message.
    #[must_use]
    pub const fn remedy(self) -> &'static str {
        match self {
            Self::CheckRecorded => "re-read show, take the new check into account and submit again",
            Self::SourceChanged => {
                "the evaluation is void, so request a new evaluation of the current source"
            }
        }
    }
}

/// Why an acceptance evaluation's `supersedes` does not match the carried
/// failure on its run.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum CarriedFailureRefusal {
    /// The run's executor revised a failing evaluation's criteria, and the
    /// evaluation does not name that failing evaluation; or it names another
    /// record than the failure carried.
    Unacknowledged,
    /// The evaluation names the failure the run's executor revised, but an
    /// executor of the run submits it: a `same_session` evaluation, or one
    /// whose session holds, held or executes the run.
    SelfAcknowledged,
    /// The evaluation names a failure to supersede, but no failing
    /// evaluation's criteria were revised on this run.
    NothingToSupersede,
}

impl CarriedFailureRefusal {
    /// Stable reason word, under the `acceptance_evaluation_refused` code.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Unacknowledged => "carried_failure_unacknowledged",
            Self::SelfAcknowledged => "carried_failure_self_acknowledged",
            Self::NothingToSupersede => "nothing_to_supersede",
        }
    }

    /// What the evaluator does next. The refusal message and the MCP details
    /// both carry this text, because a host may relay only the message.
    #[must_use]
    pub const fn remedy(self) -> &'static str {
        match self {
            Self::Unacknowledged => {
                "show the evaluator the failed verdicts and the criteria and their bindings before and after the revision, have it judge whether the revised criteria still deliver the requested outcome, and submit with --supersedes RECORD_ID naming the failed evaluation; after a revision by the run's executor, that evaluator must be one that never held the run"
            }
            Self::SelfAcknowledged => {
                "have an evaluator that never held this run name the failure: an independent_session evaluation, or a sub_agent under its own host-issued session, widening the acceptance-evaluation policy, or changing or clearing the task's evaluation mode, if they allow neither; or, while no later failing evaluation has named the failure, revise the criteria and their bindings back to the ones it judged"
            }
            Self::NothingToSupersede => {
                "submit without --supersedes: no failing evaluation's criteria were revised on this run"
            }
        }
    }
}

/// A section a partial memory revise named that its basis revision lacks,
/// with the sections it has.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct MissingMemorySection {
    pub key: String,
    pub revision: u64,
    pub section: String,
    pub sections: Vec<String>,
}

/// Errors at the immutable storage boundary.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("failed to parse JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("SQLite operation failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("project store is not initialized; run `engram init` explicitly before reading it")]
    StoreNotInitialized,
    #[error("{DIFFERENT_BUILD_STORE_MESSAGE}")]
    DifferentBuildSchema,
    #[error("immutable object collision at {0}")]
    ImmutableCollision(ObjectId),
    #[error("object {hash} is stored as kind {stored:?}, not {requested:?}")]
    ObjectKindMismatch {
        hash: ObjectId,
        stored: String,
        requested: String,
    },
    #[error("stored record id or content fingerprint is invalid: {0}")]
    InvalidStoredKey(String),
    #[error("note idempotency key {0:?} was reused for different content")]
    NoteIdempotencyConflict(String),
    #[error("note prose must not be empty")]
    EmptyNote,
    #[error("pre-write redaction refused capture: {0}")]
    RedactionRefused(String),
    #[error("memory projection contains invalid data: {0}")]
    InvalidMemoryProjection(String),
    #[error("task projection contains invalid data: {0}")]
    InvalidTaskProjection(String),
    #[error("session {0:?} has no control scope binding")]
    NoActiveTask(String),
    #[error("session {session:?} is not a participant of task {task:?}")]
    TaskAccessDenied { task: TaskId, session: String },
    #[error("memory {0} does not exist or its schema is not active")]
    MemoryNotFound(ObjectId),
    #[error("caller is not authorized to read memory {0}")]
    MemoryAccessDenied(ObjectId),
    #[error("project memory key {0:?} already exists")]
    ProjectMemoryExists(String),
    #[error(
        "project memory key {key:?} changed: expected revision {expected}, current revision {current}"
    )]
    ProjectMemoryRevisionConflict {
        key: String,
        expected: u64,
        current: u64,
    },
    #[error("project memory key {key:?} has no revision {revision}; current revision is {current}")]
    ProjectMemoryRevisionNotFound {
        key: String,
        revision: u64,
        current: u64,
    },
    #[error(
        "project memory key {:?} revision {} has no section {:?}; its sections are {:?}",
        .0.key, .0.revision, .0.section, .0.sections
    )]
    ProjectMemorySectionNotFound(Box<MissingMemorySection>),
    #[error("project memory key {0:?} is permanently retired")]
    ProjectMemoryRetired(String),
    #[error("project memory key {0:?} was not found")]
    ProjectMemoryNotFound(String),
    #[error("the asserted actor/session binding for project memories is absent or inconsistent")]
    ProjectMemoryBindingInvalid,
    #[error("project memory input is invalid: {0}")]
    InvalidProjectMemory(String),
    #[error("control session input is invalid: {0}")]
    InvalidControlSession(String),
    #[error("named-root binding refused: {0}")]
    NamedRootBindingRefused(String),
    /// The requested run and claim are unknown or do not belong together;
    /// never a named-root state.
    #[error("named-root read refused: {0}")]
    NamedRootReadRefused(String),
    /// An `execution_observe` request is malformed or out of bounds; nothing
    /// was recorded.
    #[error("execution observation is invalid: {0}")]
    ExecutionObservationInvalid(String),
    /// An `execution_observe` request names a binding, cut or root basis the
    /// store's history does not hold; nothing was recorded.
    #[error("execution observation basis does not match the store: {0}")]
    ExecutionObservationBasisMismatch(String),
    /// An `execution_observe` request asked for source-change accounting
    /// under a policy basis that is not the project's current policy epoch,
    /// policy and obligation rule set; nothing was recorded.
    #[error("execution observation policy basis is not the project's current policy: {0}")]
    ExecutionObservationPolicyBasisMismatch(String),
    /// A word named no item while the session's focus is not an item it
    /// holds and it holds others: it would otherwise act on an item the
    /// caller did not mean. Nothing was recorded.
    #[error("{0}")]
    WorkImplicitTargetConflict(Box<ImplicitTargetConflict>),
    /// A host's read of an item's acceptance bindings was refused for a
    /// typed reason; never an empty result.
    #[error("acceptance binding read refused: {reason}")]
    AcceptanceBindingReadRefused {
        refusal: crate::domain::AcceptanceBindingReadRefusal,
        reason: String,
    },
    /// A host's read of a named root's initial sighting was refused for a
    /// typed reason; never a finding about the root.
    #[error("named root sighting read refused: {reason}")]
    NamedRootSightingReadRefused {
        refusal: crate::domain::NamedRootSightingReadRefusal,
        reason: String,
    },
    /// A host's read of one criterion's candidate verifications was refused
    /// for a typed reason; never an empty result.
    #[error("acceptance verification read refused: {reason}")]
    AcceptanceVerificationReadRefused {
        refusal: crate::domain::AcceptanceVerificationReadRefusal,
        reason: String,
    },
    #[error(
        "the project root's filesystem identity is unresolved, so path intents are refused; pass --host-path-policy case_fold|case_sensitive or set ENGRAM_HOST_PATH_POLICY"
    )]
    HostPathIdentityUnresolved,
    #[error("session {0:?} has no host-private control binding")]
    ControlSessionNotBound(String),
    #[error("routing token does not match control session {0:?}")]
    ControlSessionTokenMismatch(String),
    #[error("host control connection for session {0:?} was superseded")]
    ControlConnectionSuperseded(String),
    #[error("control session bind key {0:?} was reused for a different intent")]
    ControlSessionBindConflict(String),
    #[error("turn request key {0:?} was reused for a different intent")]
    ControlTurnIdempotencyConflict(String),
    #[error("control operation {operation} key {key:?} was reused for a different intent")]
    ControlOperationIdempotencyConflict { operation: String, key: String },
    #[error("control work binding for {work:?} is stale; reread the live claim and rebind")]
    ControlWorkBindingStale { work: crate::domain::WorkId },
    #[error("execution observation {observation_id:?} is outside the turn grant scope")]
    ControlGrantScopeMismatch { observation_id: String },
    #[error("execution observation {observation_id:?} does not match the bound work scope")]
    ControlObservationScopeMismatch { observation_id: String },
    #[error("verification producer observation {0:?} cannot be resolved for this checkpoint")]
    VerificationProducerObservationNotFound(String),
    #[error("environment fingerprint does not match the canonical component identity")]
    EnvironmentFingerprintMismatch,
    #[error("environment evidence {0:?} cannot be resolved for this checkpoint")]
    EnvironmentEvidenceNotFound(String),
    #[error("environment evidence {0:?} does not match the verification run/source basis")]
    EnvironmentBasisMismatch(String),
    #[error("turn grant {0:?} does not exist")]
    ControlTurnGrantNotFound(String),
    #[error("control projection contains invalid data: {0}")]
    InvalidControlProjection(String),
    #[error("active control policy changed: expected {expected}, current policy is {current}")]
    ControlPolicyConflict {
        expected: ObjectId,
        current: ObjectId,
    },
    #[error("acceptance evaluation for {work:?} was refused: {reason}")]
    AcceptanceEvaluationRefused {
        work: crate::domain::WorkId,
        reason: String,
    },
    /// Additive admission context; Display preserves the generic refusal bytes.
    #[error("acceptance evaluation for {work:?} was refused: {reason}")]
    AcceptanceEvaluationAdmissionRefused {
        work: crate::domain::WorkId,
        reason: String,
        cause: Box<crate::domain::AcceptanceEvaluationAdmissionCause>,
    },
    /// An evaluation's `supersedes` does not match the failure carried on its
    /// run: `refusal` names which way, and `failed` the carried failing
    /// evaluation when there is one.
    #[error("acceptance evaluation for {work:?} was refused ({}): {reason}", refusal.word())]
    AcceptanceEvaluationCarriedFailure {
        work: crate::domain::WorkId,
        refusal: CarriedFailureRefusal,
        failed: Option<ObjectId>,
        reason: String,
    },
    /// The run moved past the evidence basis an evaluation read through.
    /// `moved` says whether a re-read and resubmission can still stand or
    /// the evaluation is void.
    #[error(
        "acceptance evaluation for {work:?} was refused: {reason}{}",
        deciding_observation_suffix(observation.as_deref())
    )]
    AcceptanceEvaluationBasisMoved {
        work: crate::domain::WorkId,
        moved: EvaluationBasisMove,
        reason: String,
        /// The source observation that decided a source move, when one did.
        observation: Option<Box<DecidingObservation>>,
    },
    #[error("local work item {0:?} does not exist")]
    WorkNotFound(crate::domain::WorkId),
    #[error("local work input is invalid: {0}")]
    InvalidWork(String),
    #[error(
        "work reference {reference:?} is ambiguous; use a full work id for one of {candidates:?}; {more} additional candidates omitted"
    )]
    WorkReferenceAmbiguous {
        reference: String,
        candidates: Vec<WorkReferenceCandidate>,
        more: usize,
    },
    #[error("work projection contains invalid data: {0}")]
    InvalidWorkProjection(String),
    #[error(
        "graph_destination_not_empty: destination project already contains work or project memory"
    )]
    GraphDestinationNotEmpty,
    #[error(
        "graph_project_mismatch: snapshot project {snapshot:?} does not match destination {destination:?}"
    )]
    GraphProjectMismatch {
        snapshot: ProjectId,
        destination: ProjectId,
    },
    #[error("snapshot format differs from this Engram build; use the build that wrote the file")]
    GraphDifferentBuild,
    #[error("graph_snapshot_corrupt: {0}")]
    InvalidGraphSnapshot(String),
    #[error(
        "work revision changed for {work:?}: expected {expected}, current revision is {current}"
    )]
    WorkRevisionConflict {
        work: crate::domain::WorkId,
        expected: i64,
        current: i64,
    },
    #[error("work operation {operation} key {key:?} was reused for a different intent")]
    WorkOperationIdempotencyConflict { operation: String, key: String },
    #[error("decomposition retry refused for {parent_ref}: {reason}")]
    WorkDecompositionRetryConflict {
        parent_ref: String,
        reason: &'static str,
    },
    #[error("work completion dependency graph would contain a cycle")]
    WorkDependencyCycle,
    #[error("work prerequisite {0:?} is already completed; no edge is needed")]
    WorkPrerequisiteAlreadySatisfied(crate::domain::WorkId),
    #[error("work {0:?} is not open for this operation")]
    WorkNotOpen(crate::domain::WorkId),
    #[error(
        "cannot add beneath {lifecycle:?} work; {}", parent_not_open_remedy(*.lifecycle)
    )]
    WorkParentNotOpen {
        parent: crate::domain::WorkId,
        lifecycle: crate::domain::WorkLifecycle,
    },
    #[error(
        "a peer may propose only optional children without prerequisites beneath held work; ask the parent holder to add required children or prerequisites"
    )]
    WorkPeerDecompositionRefused { parent: crate::domain::WorkId },
    #[error("reject refused for {child_ref}: {reason}; {remedy}")]
    WorkRejectRefused {
        child_ref: String,
        parent_ref: Option<String>,
        reason: &'static str,
        remedy: Box<str>,
    },
    #[error("detach refused: {reason}; {remedy}")]
    WorkDetachRefused {
        work_id: crate::domain::WorkId,
        reason: String,
        remedy: String,
    },
    #[error("listing continuation refused: {reason}")]
    WorkCatalogCursorInvalid { reason: String },
    #[error("show continuation refused: {reason}")]
    WorkShowCursorInvalid { reason: String },
    #[error("note reference refused: {reason}")]
    WorkNoteReferenceInvalid {
        reason: String,
        candidates: Vec<String>,
        more: usize,
    },
    #[error(
        "note body is {bytes} UTF-8 bytes; limit is {limit}; carry bulk content as a reference"
    )]
    WorkNoteTooLarge { bytes: usize, limit: usize },
    #[error("work {work:?} is claimed by session {holder} until {expires_at}")]
    WorkClaimHeld {
        work: crate::domain::WorkId,
        holder: String,
        expires_at: i64,
    },
    #[error("claim authority for work {work:?} is stale or does not match the holder")]
    WorkClaimMismatch { work: crate::domain::WorkId },
    #[error("claim for work {work:?} lapsed at {expired_at}")]
    WorkClaimLapsed {
        work: crate::domain::WorkId,
        expired_at: DateTime<Utc>,
    },
    /// A holder with no recorded contribution released without a reason.
    #[error(
        "release of work {work:?} needs a reason: this session recorded no contribution, and the reason is recorded as the attributed waiver of its missing contribution"
    )]
    WorkReleaseWaiverRequired { work: crate::domain::WorkId },
    #[error("completion for work {work:?} was refused: {reason}")]
    WorkCompletionRefused {
        work: crate::domain::WorkId,
        reason: String,
    },
    #[error("completion for work {work:?} was refused: {reason}")]
    WorkBoundVerificationRefused {
        work: crate::domain::WorkId,
        reason: String,
        cause: Box<crate::domain::WorkBoundVerificationCause>,
    },
    /// Completion under an evaluated acceptance policy on an item that has no
    /// acceptance criteria: there is nothing an evaluation could judge.
    #[error(
        "completion for work {work:?} was refused: the item has no acceptance criteria, and the project policy requires an acceptance evaluation, which needs at least one criterion; the host also refuses to evaluate an item without criteria, so add criteria first"
    )]
    AcceptanceCriteriaRequired { work: crate::domain::WorkId },
    #[error("evidence link refused: {reason}")]
    WorkCriterionLinkInvalid {
        criterion: Option<usize>,
        reason: &'static str,
    },
    #[error("completion for work {work:?} requires recovery: {cause:?}")]
    WorkCompletionRecoveryRequired {
        work: crate::domain::WorkId,
        cause: WorkCompletionRecoveryCause,
        /// Beside the cause, so the message above keeps its words.
        context: Box<StaleRecoveryContext>,
    },
    #[error("completion for work {work:?} has open work obligations")]
    OpenWorkObligations {
        work: crate::domain::WorkId,
        obligations: Vec<OpenWorkObligation>,
        omitted_count: usize,
    },
}

/// Diagnostic identity of the read snapshot, not a replay or authority token.
/// Captured inside verification's transaction, never refreshed after the scan.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IntegritySnapshot {
    /// Actual rows in `objects`, excluding additional projection checks.
    pub object_count: usize,
    /// Named project-feed heads in this store, ordered by project id.
    pub project_feed_heads: Vec<crate::domain::FeedPosition>,
}

/// Result of verifying immutable history and projections in one read snapshot.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IntegrityReport {
    pub snapshot: IntegritySnapshot,
    pub checked_objects: usize,
    pub invalid_objects: Vec<String>,
    pub checked_graph_snapshot_audits: usize,
    pub invalid_graph_snapshot_audits: Vec<String>,
    pub checked_control_records: usize,
    pub invalid_control_records: Vec<String>,
    pub checked_work_records: usize,
    pub invalid_work_records: Vec<String>,
}

/// One invalid binding found by diagnostics-only control-policy recovery.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ControlPolicyRecoveryFinding {
    /// Stable projection identity suitable for operator correlation.
    pub record: String,
    /// Exact verification failure; this is guidance, never a repair action.
    pub detail: String,
}

/// Read-only control-policy report for a store that ordinary open refuses.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ControlPolicyRecoveryReport {
    pub checked_control_records: usize,
    pub invalid_control_records: Vec<ControlPolicyRecoveryFinding>,
    /// Recovery deliberately restores verified bytes; it never selects or
    /// rewrites a policy head on the operator's behalf.
    pub guidance: String,
}

/// Operator-facing summary of the currently enforceable control envelope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlDiagnostics {
    pub control_schema_version: u16,
    pub active_policy: ObjectId,
    pub policy_epoch: ProjectPolicyEpoch,
    pub required_assurance: ControlAssurance,
    pub supported_effects: Vec<EffectClass>,
    pub obligation_rule_set: ObjectId,
    /// Which evaluator modes may record verdicts and what backs a pass; a
    /// host reads this to select a mode the store will admit.
    pub acceptance_evaluation: crate::domain::AcceptanceEvaluationPolicy,
    pub unenforced_effects: Vec<EffectClass>,
    pub active_sessions: usize,
    pub issued_turns: usize,
    pub begun_turns: usize,
    pub action_gating_available: bool,
    pub authority_mediation_available: bool,
    pub action_outcome_tracking_available: bool,
}

/// Validated policy facts for host readiness, without live-history counters.
#[derive(Clone, Debug, Serialize)]
pub struct ReadinessControlPolicy {
    pub schema_version: u16,
    pub policy: ObjectId,
    pub epoch: i64,
    pub required_assurance: ControlAssurance,
    pub obligation_rules: ObjectId,
    pub acceptance_evaluation: crate::domain::AcceptanceEvaluationPolicy,
    pub supported_effects: Vec<EffectClass>,
}

/// Existing-store admission facts, not an exhaustive integrity report or grant.
#[derive(Clone, Debug)]
pub struct StoreReadiness {
    pub work_schema_version: i64,
    pub stored_host_path_policy: Option<HostPathPolicy>,
    pub control: ReadinessControlPolicy,
}

/// Operator-facing receipt for one idempotent project control-policy update.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ControlPolicyUpdateReceipt {
    pub changed: bool,
    pub active_policy: ObjectId,
    pub previous_policy: Option<ObjectId>,
    pub authority: ObjectId,
    pub policy_epoch: ProjectPolicyEpoch,
    pub previous_required_assurance: ControlAssurance,
    pub required_assurance: ControlAssurance,
    pub activated_at: DateTime<Utc>,
}

/// Operator-facing receipt for one immutable acceptance-evaluation policy activation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AcceptanceEvaluationPolicyUpdateReceipt {
    pub changed: bool,
    pub active_policy: ObjectId,
    pub previous_policy: Option<ObjectId>,
    pub authority: ObjectId,
    pub policy_epoch: ProjectPolicyEpoch,
    pub previous_acceptance_evaluation: crate::domain::AcceptanceEvaluationPolicy,
    pub acceptance_evaluation: crate::domain::AcceptanceEvaluationPolicy,
    pub activated_at: DateTime<Utc>,
}

/// Operator-facing receipt for one immutable obligation rule-set activation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ObligationRuleSetUpdateReceipt {
    pub changed: bool,
    pub active_policy: ObjectId,
    pub previous_policy: Option<ObjectId>,
    pub authority: ObjectId,
    pub policy_epoch: ProjectPolicyEpoch,
    pub previous_rule_set: Option<ObjectId>,
    pub obligation_rule_set: ObjectId,
    pub activated_at: DateTime<Utc>,
}

/// One ordered entry in a task's local change feed, read back by tests.
#[cfg(test)]
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskChange {
    pub cursor: ChangeCursor,
    pub task_id: TaskId,
    pub object_kind: String,
    pub object_id: ObjectId,
}

type MemorySummaryRow = (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    Option<String>,
    String,
    String,
    String,
    i64,
);

#[derive(Debug, Eq, PartialEq)]
struct MemoryHeadProjectionRow {
    memory_id: String,
    version_id: String,
    assertion_id: String,
    schema_version: i64,
    status: String,
    scope_kind: String,
    project_id: String,
    task_id: Option<String>,
    work_id: Option<String>,
    agent_id: Option<String>,
    memory_kind: String,
    authority: String,
    delivery: String,
    sensitivity: String,
    title: String,
    body: String,
    created_at_ms: i64,
}

struct PreparedNote {
    version: MemoryVersion,
    assertion: MemoryAssertionEvent,
    version_object: CanonicalObject,
    assertion_object: CanonicalObject,
}

struct StoredProjectMemory {
    version_id: ObjectId,
    version: MemoryVersion,
    assertion: MemoryAssertionEvent,
}

struct PreparedProjectMemory {
    version: MemoryVersion,
    assertion: MemoryAssertionEvent,
    version_object: CanonicalObject,
    assertion_object: CanonicalObject,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MemoryProjectionMode {
    Live,
    Replay,
}

#[derive(Clone, Debug)]
pub(crate) struct ProjectMemoryAdvertisement {
    pub count: usize,
    pub changed: bool,
    /// The call supplied a context generation that no recorded memories
    /// listing of the session carries.
    pub generation_unlisted: bool,
    change_position: i64,
}

/// The project-memory position that a listing's own snapshot read.
#[derive(Clone, Copy, Debug)]
pub(crate) struct ProjectMemoryListingCut {
    change_position: i64,
}

const PROJECT_MEMORY_LIST_LIMIT: usize = 20;
const MAX_PROJECT_MEMORY_ADVERTISEMENTS_PER_PROJECT: i64 = 1_024;
const PROJECT_MEMORY_FIRST_LINE_BYTES: usize = 160;
const MAX_CONTEXT_GENERATION_BYTES: usize = 256;
const MAX_PROJECT_MEMORY_ATTRIBUTION_TEXT_BYTES: usize = 4_096;
const MAX_PROJECT_MEMORY_PROVENANCE_LINKS: usize = 32;
const MAX_PROJECT_MEMORY_ATTRIBUTION_BYTES: usize = 64 * 1_024;

const MAX_EXECUTION_OBSERVATIONS_PER_CHECKPOINT: usize = 64;
const MAX_VERIFICATION_EVIDENCE_PER_CHECKPOINT: usize = 16;
const MAX_ENVIRONMENT_EVIDENCE_PER_CHECKPOINT: usize = 4;
const MAX_TYPED_EVIDENCE_SUMMARY_BYTES: usize = 4 * 1_024;
const MAX_TYPED_EVIDENCE_REFS: usize = 64;
const MAX_TYPED_EVIDENCE_REF_BYTES: usize = 1_024;
const MAX_TASK_CHANGE_OBJECT_BYTES: usize = 64 * 1_024;
const BUILTIN_CONTROL_GRANT_TTL_SECONDS: i64 = 30;
const MAX_CONTROL_POLICY_PROVENANCE_LINKS: usize = 32;
const MAX_CONTROL_POLICY_ATTRIBUTION_BYTES: usize = 64 * 1_024;
const MAX_CONTROL_POLICY_AUTHORITY_BYTES: usize = 72 * 1_024;
const MAX_CONTROL_POLICY_OPERATION_INTENT_BYTES: usize = 96 * 1_024;
const MAX_CONTROL_POLICY_OPERATION_RESULT_BYTES: usize = 16 * 1_024;
const MAX_CONTROL_POLICY_IDEMPOTENCY_KEY_BYTES: usize = 512;

#[cfg(test)]
thread_local! {
    static CONTROL_POLICY_VERSION_LOAD_COUNT: Cell<usize> = const { Cell::new(0) };
    static FAIL_COLD_SCHEMA_AFTER_DDL: Cell<bool> = const { Cell::new(false) };
}

#[cfg(test)]
fn reset_control_policy_version_load_count() {
    CONTROL_POLICY_VERSION_LOAD_COUNT.set(0);
}

#[cfg(test)]
fn control_policy_version_load_count() -> usize {
    CONTROL_POLICY_VERSION_LOAD_COUNT.get()
}

#[cfg(test)]
fn fail_cold_schema_after_ddl() -> bool {
    FAIL_COLD_SCHEMA_AFTER_DDL.replace(false)
}

struct StoredControlSession {
    project_id: crate::domain::ProjectId,
    task_id: TaskId,
    work_binding: Option<ControlWorkBinding>,
    session_id: SessionId,
    routing_token: String,
    actor: ActorContext,
    bind_key: String,
    bind_intent_hash: String,
    phase: SessionPhase,
    assurance: ControlAssurance,
    mediated_effects: Vec<EffectClass>,
    epochs: ControlEpochs,
    capability_map_revision: i64,
    revision: i64,
    open_grant_id: Option<String>,
}

/// One `control_sessions` row as stored. `confirmed_cursor`,
/// `tentative_cursor` and `blocking_watermark` are retained columns that no
/// decision reads; the loader still checks their bounds.
struct RawControlSession {
    project_id: String,
    task_id: String,
    root_execution_id: Option<String>,
    work_id: Option<String>,
    run_id: Option<String>,
    work_revision: Option<i64>,
    claim_id: Option<String>,
    claim_fence: Option<i64>,
    routing_token: String,
    actor_json: Vec<u8>,
    bind_key: String,
    bind_intent_hash: String,
    bind_intent_json: Vec<u8>,
    phase: String,
    assurance: String,
    mediated_effects_json: String,
    confirmed_cursor: i64,
    tentative_cursor: Option<i64>,
    project_policy_epoch: i64,
    task_admission_epoch: i64,
    blocking_watermark: i64,
    capability_map_revision: i64,
    revision: i64,
    open_grant_id: Option<String>,
}

struct ControlPolicyProjection {
    state_schema_version: i64,
    policy_id: ObjectId,
    authority_id: ObjectId,
    epoch: ProjectPolicyEpoch,
    required_assurance: ControlAssurance,
    supported_effects: Vec<EffectClass>,
    grant_ttl_seconds: i64,
    obligation_rule_set: ObjectId,
    activated_at: DateTime<Utc>,
}

struct InitialControlPolicy {
    required_assurance: ControlAssurance,
    authorized_by: ActorContext,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OpenWriteNeed {
    Current,
    NeedsWrite,
}

struct StoredTurnGrant {
    grant: IssuedTurnGrant,
    state: TurnGrantState,
}

struct StoredControlTurnResult {
    sequence: i64,
    session_id: String,
    task_id: String,
    idempotency_key: String,
    intent_hash: String,
    intent_json: Vec<u8>,
    decision_hash: String,
    decision_json: Vec<u8>,
}

struct StoredControlGrantRow {
    grant_id: String,
    session_id: String,
    task_id: String,
    request_key: String,
    grant_json: Vec<u8>,
    state: String,
    issued_at_ms: i64,
    expires_at_ms: i64,
}

struct PendingTurnGrantSupersession {
    grant_id: String,
    request_key: String,
}

struct StoredTurnGrantSupersession {
    superseded_grant_id: String,
    session_id: String,
    task_id: String,
    replacement_request_key: String,
    replacement_decision_hash: String,
    supersession_json: Vec<u8>,
    superseded_at_ms: i64,
}

struct StoredControlOperation {
    sequence: i64,
    session_id: String,
    operation: String,
    idempotency_key: String,
    intent_hash: String,
    intent_json: Vec<u8>,
    result_json: Vec<u8>,
}

struct StoredControlPolicyOperation {
    sequence: i64,
    operation: String,
    idempotency_key: String,
    intent_hash: String,
    intent_json: Vec<u8>,
    result_json: Vec<u8>,
}

impl IntegrityReport {
    /// Whether every stored object passed canonicalization and digest checks.
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.invalid_objects.is_empty()
            && self.invalid_graph_snapshot_audits.is_empty()
            && self.invalid_control_records.is_empty()
            && self.invalid_work_records.is_empty()
    }
}

/// What an integrity label may say about a record that did not decode: the
/// shape of the problem and where it is, never a value from the record, since
/// labels reach the operator's terminal. A member the record's format has no
/// place for, or one it lacks, is named; any other decoding error is reduced
/// to its category and position. `None` for a failure other than decoding.
pub(crate) fn undecodable_record_reason(error: &StoreError) -> Option<String> {
    let StoreError::Json(error) = error else {
        return None;
    };
    Some(undecodable_json_reason(error))
}

/// [`undecodable_record_reason`] for a decoding error itself.
pub(crate) fn undecodable_json_reason(error: &serde_json::Error) -> String {
    let message = error.to_string();
    let named_member = ["unknown field `", "missing field `"]
        .into_iter()
        .find_map(|prefix| {
            let member = message.strip_prefix(prefix)?.split_once('`')?.0;
            Some(format!("{prefix}{member}`"))
        });
    let shape = named_member.unwrap_or_else(|| {
        match error.classify() {
            serde_json::error::Category::Syntax => "malformed JSON",
            serde_json::error::Category::Eof => "truncated JSON",
            serde_json::error::Category::Data => "a field of the wrong type or value",
            serde_json::error::Category::Io => "unreadable JSON",
        }
        .to_owned()
    });
    format!("{shape} at line {} column {}", error.line(), error.column())
}

/// `label` for a record that failed to load, with the decoding reason when
/// that is why; see [`undecodable_record_reason`].
pub(crate) fn decode_failure_label(label: String, error: &StoreError) -> String {
    match undecodable_record_reason(error) {
        Some(reason) => format!("{label}:{reason}"),
        None => label,
    }
}

impl ControlPolicyRecoveryReport {
    /// Whether the active selector and every reachable policy record verify.
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.invalid_control_records.is_empty()
    }
}

/// Human-readable form of a host path policy for diagnostics and refusals.
#[must_use]
pub fn describe_host_path_policy(policy: HostPathPolicy) -> String {
    format!(
        "{}, windows alias rules {}",
        if policy.case_fold_paths {
            "case_fold"
        } else {
            "case_sensitive"
        },
        if policy.windows_alias_rules {
            "on"
        } else {
            "off"
        }
    )
}

/// What a verified backup copy contains. A backup is a full copy of the
/// store, including host-private state and private scratch, so it is exactly as
/// sensitive as the store itself.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BackupManifest {
    pub path: std::path::PathBuf,
    /// SHA-256 of the backup file bytes after verification.
    pub file_sha256: String,
    pub file_bytes: u64,
    pub checked_objects: usize,
    pub checked_control_records: usize,
    pub checked_work_records: usize,
    pub created_at: DateTime<Utc>,
}

/// A sibling path only this process will use, for staging a file before it
/// is published under its final name.
fn unique_sibling_path(path: &Path, label: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let name = path.file_name().map_or_else(
        || "store".into(),
        |name| name.to_string_lossy().into_owned(),
    );
    path.with_file_name(format!(
        ".{name}.{label}-{}-{nanos}.tmp",
        std::process::id()
    ))
}

/// Publishes a staged file under `target` without replacing anything: the
/// final name is created exclusively, the staged bytes are copied in and
/// flushed, and the staged file is removed. An existing `target` is an error
/// and leaves both files untouched; a failure while writing removes the
/// partial target so a retry is not blocked by it.
fn publish_without_replacing(staged: &Path, target: &Path) -> Result<(), StoreError> {
    let io_error = |what: &str, error: std::io::Error| {
        StoreError::InvalidWork(format!("cannot {what} {}: {error}", target.display()))
    };
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                StoreError::InvalidWork(format!(
                    "backup target {} already exists",
                    target.display()
                ))
            } else {
                io_error("create", error)
            }
        })?;
    let written = (|| -> std::io::Result<()> {
        let mut input = std::fs::File::open(staged)?;
        std::io::copy(&mut input, &mut out)?;
        out.sync_all()
    })();
    drop(out);
    if let Err(error) = written {
        let _ = std::fs::remove_file(target);
        return Err(io_error("write", error));
    }
    remove_store_files(staged).map_err(|error| io_error("clean up after", error))?;
    Ok(())
}

/// Installs a verified staged copy as `target` without replacing anything;
/// hosts use it for a restore into an absent store.
///
/// # Errors
///
/// Returns [`StoreError`] when `target` already exists or the copy fails.
pub fn install_store_copy_without_replacing(
    staged: &Path,
    target: &Path,
) -> Result<(), StoreError> {
    publish_without_replacing(staged, target)
}

/// The log sidecars SQLite may keep beside a store file.
/// `database` with `suffix` appended to its file name, byte for byte.
pub(crate) fn sidecar(database: &Path, suffix: &str) -> std::path::PathBuf {
    let mut name = database.as_os_str().to_os_string();
    name.push(suffix);
    std::path::PathBuf::from(name)
}

/// The write-ahead log, its shared-memory file and the rollback journal a
/// database may have beside it.
pub(crate) fn store_sidecars(path: &Path) -> [std::path::PathBuf; 3] {
    [
        sidecar(path, "-wal"),
        sidecar(path, "-shm"),
        sidecar(path, "-journal"),
    ]
}

/// Removes a store file and any log sidecars an open may have left beside it.
fn remove_store_files(path: &Path) -> std::io::Result<()> {
    std::fs::remove_file(path)?;
    for sidecar in store_sidecars(path) {
        if sidecar.exists() {
            std::fs::remove_file(&sidecar)?;
        }
    }
    Ok(())
}

/// A `file:` URI that opens `path` as an immutable database: SQLite then
/// reads the file bytes alone and never touches or creates log sidecars.
fn immutable_uri(path: &Path) -> Result<String, StoreError> {
    let absolute = std::path::absolute(path).map_err(|error| {
        StoreError::InvalidWork(format!("cannot resolve {}: {error}", path.display()))
    })?;
    let mut text = absolute.to_string_lossy().replace('\\', "/");
    if let Some(stripped) = text.strip_prefix("//?/") {
        text = stripped.to_owned();
    }
    let encoded = text
        .chars()
        .map(|character| match character {
            '%' => "%25".to_owned(),
            '?' => "%3F".to_owned(),
            '#' => "%23".to_owned(),
            other => other.to_string(),
        })
        .collect::<String>();
    Ok(format!(
        "file:///{}?immutable=1",
        encoded.trim_start_matches('/')
    ))
}

fn enum_name<T: Serialize>(value: T) -> Result<String, StoreError> {
    serde_json::to_value(value)?
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| StoreError::InvalidMemoryProjection("enum did not serialize as text".into()))
}

fn parse_enum<T: DeserializeOwned>(value: &str) -> Result<T, StoreError> {
    serde_json::from_value(serde_json::Value::String(value.to_owned())).map_err(StoreError::Json)
}

/// V1's canonical local persistence backend.
pub struct SqliteStore {
    connection: Connection,
    /// Local-work schema generation this connection opened and understands.
    /// Every work mutation compares it with durable metadata inside the write
    /// transaction so a process with a non-current view cannot write.
    work_schema_version: i64,
    /// The project root's filesystem identity for this opener. `None` means
    /// unresolved: reads and work proceed, path intents fail closed.
    host_path_policy: Option<HostPathPolicy>,
}

#[cfg(test)]
pub(crate) use work::{
    AdmissionTransportFixture, SourceRecoveryTransportFixture, admission_transport_fixture,
    long_declared_source, long_presented_source, source_recovery_transport_fixture,
};
