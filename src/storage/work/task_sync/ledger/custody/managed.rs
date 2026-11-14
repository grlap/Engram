//! Small indexed checkpoint metadata; ordinary custody never decodes its
//! historical artifact list or bootstrap task documents.

use super::super::{StoreError, view::read};
use crate::ObjectId;
use crate::task_sync::{
    delta::{CheckpointReason, OriginQualifiedCut},
    ledger::require,
    retention::audit::{AuditMode, journal::Journal},
};
use rusqlite::{Connection, OptionalExtension};
use serde::Deserialize;
use std::path::Path;

const PREDICATE: &str = "object_kind='task_sync_exchange_event' AND json_valid(canonical_json) AND json_type(canonical_json,'$.transition.checkpoint')='object'";
const EXPRESSION: &str = "json_object('schema',json_extract(canonical_json,'$.schema_version'),'binding',json_extract(canonical_json,'$.binding'),'reason',json_extract(canonical_json,'$.transition.checkpoint.reason'),'cut',json(json_extract(canonical_json,'$.transition.checkpoint.local_cut')))";

pub(in crate::storage::work::task_sync) fn create_schema(
    connection: &Connection,
) -> Result<(), StoreError> {
    connection.execute_batch(&format!("CREATE INDEX IF NOT EXISTS objects_task_sync_custody_checkpoint ON objects(object_id,{EXPRESSION}) WHERE {PREDICATE};"))?;
    Ok(())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Metadata {
    schema: u32,
    binding: ObjectId,
    reason: CheckpointReason,
    cut: OriginQualifiedCut,
}

pub(super) fn check_on(
    connection: &Connection,
    root: &Path,
    journal: &Journal,
    budget: &mut read::ReadBudget,
) -> Result<(), StoreError> {
    let indexed: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM sqlite_schema WHERE type='index' AND name='objects_task_sync_custody_checkpoint')",
        [], |row| row.get(0),
    )?;
    require(
        indexed,
        "managed custody checkpoint index missing; repair projections explicitly",
    )?;
    let acknowledgement = &journal.checkpoint;
    let binding = read::binding_on(connection, &acknowledgement.binding, budget)?;
    require(
        binding.repository.git_directory == root
            && binding.store.origin.as_ref() == Some(&acknowledgement.origin)
            && binding.store.restore_occurrence == acknowledgement.restore_occurrence
            && acknowledgement.mode == AuditMode::Managed,
        "managed custody checkpoint repository or origin differs; run named recovery",
    )?;
    read::check_head(connection, &acknowledgement.binding)?;
    let head: Option<String> = connection.query_row(
        "SELECT last_checkpoint_id FROM work_task_sync_exchange_heads WHERE binding_id=?1",
        [acknowledgement.binding.as_str()],
        |row| row.get(0),
    )?;
    require(
        head.as_deref() == Some(acknowledgement.checkpoint.as_str()),
        "managed custody checkpoint is not current; repair projections or run named recovery",
    )?;
    let bytes: Option<String> = connection.query_row(&format!(
        "SELECT {EXPRESSION} FROM objects INDEXED BY objects_task_sync_custody_checkpoint WHERE object_id=?1 AND {PREDICATE}"),
        [acknowledgement.checkpoint.as_str()], |row| row.get(0)).optional()?;
    let metadata: Metadata = serde_json::from_str(&bytes.ok_or_else(|| {
        super::super::super::invalid(
            "managed custody checkpoint metadata missing; repair projections explicitly",
        )
    })?)
    .map_err(|_| {
        super::super::super::invalid(
            "managed custody checkpoint metadata invalid; repair projections explicitly",
        )
    })?;
    require(
        metadata.schema == 1
            && metadata.binding == acknowledgement.binding
            && metadata.reason == CheckpointReason::ManagedAudit,
        "managed custody checkpoint canonical scope differs; run named recovery",
    )?;
    metadata.cut.validate(&acknowledgement.binding, &binding)
}
