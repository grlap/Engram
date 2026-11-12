//! Canonical comparison fields in a covering expression index, never task bodies.

use rusqlite::{Connection, OptionalExtension, params};
use serde::Deserialize;

use crate::{
    ActorContext, ObjectId,
    storage::StoreError,
    task_sync::{
        TaskSyncMembershipView,
        delta::{
            DeltaIntent, IncomingSourceAnchor, IncomingSourceBasis, MembershipSnapshot,
            OriginQualifiedCut, OutputMembership, PreparedExportAnchor, PreparedExportBasis,
        },
        ledger::{Binding, Direction, require},
    },
};

const PREDICATE: &str = "object_kind='task_sync_exchange_event' AND json_valid(canonical_json)";

mod resolution;
pub(in crate::storage::work::task_sync) use resolution::delta_resolution_on;
#[cfg(test)]
pub(in crate::storage::work::task_sync) use resolution::select_sql as resolution_sql;

// Keep creation and selected reads on exactly the same SQL expression. JSON
// extraction happens when canonical rows are indexed, not on an ordinary read.
const EXPRESSION: &str = "json_object(
    'schema',json_extract(canonical_json,'$.schema_version'),
    'binding',json_extract(canonical_json,'$.binding'),
    'transition',json_extract(canonical_json,'$.transition.transition'),
    'cut',json(coalesce(json_extract(canonical_json,'$.transition.checkpoint.local_cut'),
        json_extract(canonical_json,'$.transition.intent.local_cut'),
        json_extract(canonical_json,'$.transition.recipe.request.intent.local_cut'),
        json_extract(canonical_json,'$.transition.receipt.after_cut'))),
    'seed',json_extract(canonical_json,'$.transition.checkpoint.seed.kind'),
    'direction',coalesce(json_extract(canonical_json,'$.transition.intent.direction'),json_extract(canonical_json,'$.transition.recipe.request.intent.direction')),
    'checkpoint_source',json_extract(canonical_json,'$.transition.checkpoint.seed.snapshot'),
    'checkpoint_export',json_extract(canonical_json,'$.transition.checkpoint.seed.membership'),
    'prepared_output',coalesce(json_extract(canonical_json,'$.transition.intent.candidate.output'),json_extract(canonical_json,'$.transition.recipe.request.intent.candidate.output')),
    'source_after',coalesce(json_extract(canonical_json,'$.transition.intent.basis.source_after'),json_extract(canonical_json,'$.transition.recipe.request.intent.basis.source_after')),
    'prepared',json_extract(canonical_json,'$.transition.prepared'),
    'record_bytes',length(canonical_json))";

// Exact-request comparison can scale with that request's changed-key set.
// Keep it in a separate indexed column so ordinary membership-head reads
// never materialize a previous operation's complete intent.
const REQUEST_EXPRESSION: &str = "json_object(
    'schema',json_extract(canonical_json,'$.schema_version'),
    'binding',json_extract(canonical_json,'$.binding'),
    'sequence',json_extract(canonical_json,'$.sequence'),
    'predecessor',json_extract(canonical_json,'$.predecessor'),
    'actor',json_extract(canonical_json,'$.actor'),
    'transition',json_extract(canonical_json,'$.transition.transition'),
    'delta_intent',json(CASE WHEN json_extract(canonical_json,'$.transition.transition')='delta_prepared'
        THEN json_extract(canonical_json,'$.transition.intent') END),
    'record_bytes',length(canonical_json))";

pub(in crate::storage::work::task_sync) fn create_schema(
    connection: &Connection,
) -> Result<(), StoreError> {
    connection.execute_batch(&format!(
        "CREATE INDEX IF NOT EXISTS objects_task_sync_membership_checkpoint ON objects(object_id,{EXPRESSION},{REQUEST_EXPRESSION},{}) WHERE {PREDICATE};",
        resolution::EXPRESSION,
    ))?;
    Ok(())
}

pub(in crate::storage::work::task_sync) fn select_sql() -> String {
    format!(
        "SELECT {EXPRESSION} FROM objects INDEXED BY objects_task_sync_membership_checkpoint
        WHERE object_id=?1 AND {PREDICATE}"
    )
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    schema: i64,
    binding: ObjectId,
    transition: String,
    cut: Option<OriginQualifiedCut>,
    seed: Option<String>,
    direction: Option<Direction>,
    checkpoint_source: Option<MembershipSnapshot>,
    checkpoint_export: Option<OutputMembership>,
    prepared_output: Option<OutputMembership>,
    source_after: Option<MembershipSnapshot>,
    prepared: Option<ObjectId>,
    record_bytes: usize,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RequestMetadata {
    schema: i64,
    binding: ObjectId,
    sequence: u64,
    predecessor: Option<ObjectId>,
    actor: ActorContext,
    transition: String,
    delta_intent: Option<Box<DeltaIntent>>,
    record_bytes: usize,
}

fn request_metadata_on(
    connection: &Connection,
    id: &ObjectId,
    binding: &ObjectId,
) -> Result<RequestMetadata, StoreError> {
    let sql = format!(
        "SELECT {REQUEST_EXPRESSION} FROM objects INDEXED BY objects_task_sync_membership_checkpoint WHERE object_id=?1 AND {PREDICATE}"
    );
    let bytes: Option<String> = connection
        .query_row(&sql, [id.as_str()], |row| row.get(0))
        .optional()?;
    let metadata: RequestMetadata = serde_json::from_str(
        &bytes.ok_or_else(|| canonical_invalid("request metadata is missing"))?,
    )
    .map_err(|_| canonical_invalid("request metadata is invalid"))?;
    require(
        metadata.schema == 1
            && metadata.binding == *binding
            && metadata.record_bytes <= super::super::super::ledger::MAX_RECORD_BYTES,
        "canonical request binding, schema or byte bound differs",
    )?;
    Ok(metadata)
}

/// Exact request and predecessor metadata, without decoding the prepared task
/// payload or a checkpoint's bootstrap documents. This is a historical read,
/// not admission of a new preparation or a substitute for doctor.
pub(in crate::storage::work::task_sync) fn delta_preparation_on(
    connection: &Connection,
    prepared: &ObjectId,
    binding_id: &ObjectId,
    binding: &Binding,
) -> Result<(Box<DeltaIntent>, u64), StoreError> {
    let metadata = request_metadata_on(connection, prepared, binding_id)?;
    require(
        metadata.transition == "delta_prepared" && metadata.sequence > 1,
        "recorded request is not a delta preparation",
    )?;
    let intent = metadata
        .delta_intent
        .ok_or_else(|| canonical_invalid("delta intent is missing"))?;
    intent.validate(binding_id, binding)?;
    require(
        metadata.actor == intent.actor,
        "delta preparation attribution differs",
    )?;
    let predecessor = metadata
        .predecessor
        .ok_or_else(|| canonical_invalid("delta predecessor is missing"))?;
    let before = request_metadata_on(connection, &predecessor, binding_id)?;
    require(
        before.sequence > 0 && before.sequence.checked_add(1) == Some(metadata.sequence),
        "delta preparation predecessor sequence differs",
    )?;
    Ok((intent, metadata.sequence))
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(in crate::storage::work::task_sync) enum MembershipBasis {
    Source(Box<IncomingSourceBasis>),
    Export(Box<PreparedExportBasis>),
}

pub(in crate::storage::work::task_sync) struct MembershipHead {
    pub basis: MembershipBasis,
}

impl MembershipHead {
    pub(in crate::storage::work::task_sync) fn output(&self) -> &OutputMembership {
        match &self.basis {
            MembershipBasis::Source(source) => &source.snapshot.membership,
            MembershipBasis::Export(export) => &export.membership,
        }
    }
}

fn invalid(message: &str) -> StoreError {
    super::invalid(message)
}

fn canonical_invalid(message: &str) -> StoreError {
    StoreError::InvalidWorkProjection(format!(
        "task-sync canonical membership {message}; preserve the store and recover its canonical history"
    ))
}

fn metadata_on(
    connection: &Connection,
    id: &ObjectId,
    binding_id: &ObjectId,
) -> Result<Metadata, StoreError> {
    let bytes: Option<String> = connection
        .query_row(&select_sql(), [id.as_str()], |row| row.get(0))
        .optional()?;
    let metadata: Metadata = serde_json::from_str(
        &bytes.ok_or_else(|| canonical_invalid("head metadata is missing or malformed"))?,
    )
    .map_err(|_| canonical_invalid("head metadata is invalid"))?;
    require(
        metadata.schema == 1
            && metadata.binding == *binding_id
            && metadata.record_bytes <= super::super::super::ledger::MAX_RECORD_BYTES,
        "canonical membership head binding, schema or byte bound differs",
    )?;
    Ok(metadata)
}

fn validate_cut(
    connection: &Connection,
    cut: &OriginQualifiedCut,
    binding_id: &ObjectId,
    binding: &Binding,
) -> Result<(), StoreError> {
    cut.validate(binding_id, binding)?;
    if let Some(anchor) = &cut.anchor {
        let admitted: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM work_feed_entries WHERE feed_kind='project' AND feed_id=?1
                AND position=?2 AND object_id=?3 AND object_kind=?4)",
            params![
                binding.project.0,
                cut.position.position,
                anchor.object_id.as_str(),
                anchor.object_kind
            ],
            |row| row.get(0),
        )?;
        require(admitted, "membership head project-feed anchor differs")?;
    }
    Ok(())
}

fn validate_snapshot(snapshot: &MembershipSnapshot) -> Result<(), StoreError> {
    snapshot.commit.validate()?;
    snapshot
        .membership
        .profile
        .validate()
        .map_err(|_| invalid("source snapshot profile is invalid"))
}

pub(in crate::storage::work::task_sync) fn read_on(
    connection: &Connection,
    head: &ObjectId,
    binding_id: &ObjectId,
    binding: &Binding,
    view: TaskSyncMembershipView,
) -> Result<MembershipHead, StoreError> {
    let metadata = metadata_on(connection, head, binding_id)?;
    let cut = metadata
        .cut
        .ok_or_else(|| invalid("canonical membership cut is missing"))?;
    validate_cut(connection, &cut, binding_id, binding)?;
    let missing = || invalid("canonical head role or membership differs");
    let basis = match (view, metadata.transition.as_str()) {
        (TaskSyncMembershipView::Source, "custody_checkpoint")
            if metadata.seed.as_deref() == Some("incoming") =>
        {
            let snapshot = metadata.checkpoint_source.ok_or_else(missing)?;
            validate_snapshot(&snapshot)?;
            MembershipBasis::Source(Box::new(IncomingSourceBasis {
                anchor: IncomingSourceAnchor::Bootstrap {
                    checkpoint: head.clone(),
                },
                snapshot,
            }))
        }
        (TaskSyncMembershipView::Export, "custody_checkpoint")
            if metadata.seed.as_deref() == Some("prepared_export") =>
        {
            MembershipBasis::Export(Box::new(PreparedExportBasis {
                anchor: PreparedExportAnchor::Bootstrap {
                    checkpoint: head.clone(),
                },
                membership: metadata.checkpoint_export.ok_or_else(missing)?,
                cut: cut.clone(),
            }))
        }
        (TaskSyncMembershipView::Export, "delta_prepared")
            if metadata.direction == Some(Direction::Outgoing) =>
        {
            MembershipBasis::Export(Box::new(PreparedExportBasis {
                anchor: PreparedExportAnchor::Prepared {
                    event: head.clone(),
                },
                membership: metadata.prepared_output.ok_or_else(missing)?,
                cut: cut.clone(),
            }))
        }
        (TaskSyncMembershipView::Source, "delta_import_applied" | "planning_successor_applied") => {
            let prepared = metadata.prepared.ok_or_else(missing)?;
            let previous = metadata_on(connection, &prepared, binding_id)?;
            require(
                previous.transition
                    == if metadata.transition == "planning_successor_applied" {
                        "planning_successor_prepared"
                    } else {
                        "delta_prepared"
                    }
                    && previous.direction == Some(Direction::Incoming),
                "source outcome preparation differs",
            )?;
            let before = previous.cut.ok_or_else(missing)?;
            validate_cut(connection, &before, binding_id, binding)?;
            require(
                cut.position.position >= before.position.position
                    && (cut.position.position != before.position.position
                        || cut.anchor == before.anchor),
                "source outcome local cut differs",
            )?;
            let snapshot = previous.source_after.ok_or_else(missing)?;
            validate_snapshot(&snapshot)?;
            MembershipBasis::Source(Box::new(IncomingSourceBasis {
                anchor: IncomingSourceAnchor::Applied {
                    outcome: head.clone(),
                },
                snapshot,
            }))
        }
        _ => return Err(missing()),
    };
    let result = MembershipHead { basis };
    let output = result.output();
    output
        .profile
        .validate()
        .map_err(|_| invalid("membership profile is invalid"))?;
    require(
        output.profile.include_notes == binding.profile.include_notes
            && (view != TaskSyncMembershipView::Export || output.profile == binding.profile),
        "membership head disclosure profile differs",
    )?;
    Ok(result)
}
