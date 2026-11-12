//! Covering metadata for one canonical resolved delta; task documents stay in
//! their original event and never enter this historical comparison read.

use super::{
    Connection, Deserialize, ObjectId, OptionalExtension, PREDICATE, StoreError, canonical_invalid,
    require,
};
use crate::{
    ActorContext,
    task_sync::{
        delta::DeltaIntent,
        ledger::{OutgoingFailure, RetainedCommit},
    },
};

pub(super) const EXPRESSION: &str = "CASE
    WHEN json_extract(canonical_json,'$.transition.transition')='outgoing_reconciled'
      AND json_extract(canonical_json,'$.transition.reconciliation.step')='delta_resolved'
    THEN json_object(
      'schema',json_extract(canonical_json,'$.schema_version'),
      'binding',json_extract(canonical_json,'$.binding'),
      'sequence',json_extract(canonical_json,'$.sequence'),
      'predecessor',json_extract(canonical_json,'$.predecessor'),
      'actor',json_extract(canonical_json,'$.actor'),
      'prepared',json_extract(canonical_json,'$.transition.prepared'),
      'failure',json_extract(canonical_json,'$.transition.reconciliation.failure'),
      'resolved',json_extract(canonical_json,'$.transition.reconciliation.resolved'),
      'commit',json_extract(canonical_json,'$.transition.reconciliation.commit'),
      'record_bytes',length(canonical_json)) END";

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(in crate::storage::work::task_sync) struct ResolutionMetadata {
    schema: i64,
    binding: ObjectId,
    pub sequence: u64,
    pub predecessor: ObjectId,
    pub actor: ActorContext,
    pub prepared: ObjectId,
    pub failure: OutgoingFailure,
    pub resolved: Box<DeltaIntent>,
    pub commit: Box<RetainedCommit>,
    record_bytes: usize,
}

pub(in crate::storage::work::task_sync) fn select_sql() -> String {
    format!(
        "SELECT {EXPRESSION} FROM objects INDEXED BY objects_task_sync_membership_checkpoint WHERE object_id=?1 AND {PREDICATE}"
    )
}

pub(in crate::storage::work::task_sync) fn delta_resolution_on(
    connection: &Connection,
    id: &ObjectId,
    binding: &ObjectId,
) -> Result<ResolutionMetadata, StoreError> {
    let bytes: Option<String> = connection
        .query_row(&select_sql(), [id.as_str()], |row| row.get(0))
        .optional()?
        .flatten();
    let metadata: ResolutionMetadata = serde_json::from_str(
        &bytes.ok_or_else(|| canonical_invalid("delta resolution metadata is missing"))?,
    )
    .map_err(|_| canonical_invalid("delta resolution metadata is invalid"))?;
    require(
        metadata.schema == 1
            && metadata.binding == *binding
            && metadata.record_bytes <= super::super::super::super::ledger::MAX_RECORD_BYTES,
        "canonical delta resolution binding, schema or byte bound differs",
    )?;
    Ok(metadata)
}
