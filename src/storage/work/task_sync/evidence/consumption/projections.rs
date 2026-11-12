//! Rebuildable native membership and consumed-decision indexes.

use rusqlite::{Connection, OptionalExtension, params};

use super::{ConsumedProof, ImportedEvidenceConsumption, ObjectId, Result, invalid, load_on};
use crate::domain::{NamedRootBindingEvent, SCHEMA_VERSION};
use crate::storage::work::EvidenceProjectionRow;
use crate::task_sync::evidence::consumption::CONSUMPTION_KIND;

pub(in crate::storage::work) fn expected_projection_on(
    connection: &Connection,
    id: &ObjectId,
) -> Result<EvidenceProjectionRow> {
    let record = load_on(connection, id)?;
    let (_, bytes) = super::object_on(connection, &record.named_root)?
        .filter(|(kind, _)| kind == "named_root_binding")
        .ok_or_else(|| invalid("consumption named-root original is missing"))?;
    let root: NamedRootBindingEvent = serde_json::from_slice(&bytes)?;
    let binding = &record.request.binding;
    if root.schema_version != SCHEMA_VERSION
        || root.project_id != record.project_id
        || root.work_id != binding.work_id
        || root.run_id != binding.run_id
        || root.claim_id != binding.claim_id
    {
        return Err(invalid("consumption named-root binding differs"));
    }
    Ok(EvidenceProjectionRow {
        work_id: binding.work_id.0.to_string(),
        run_id: binding.run_id.0.to_string(),
        evidence_kind: "imported".into(),
        workspace_id: Some(root.workspace_id),
        source_revision: None,
        producer_session_id: Some(record.session_id.0),
        producer_observation_id: None,
        check_fingerprint: None,
        verification_result: None,
        observed_at_ms: Some(record.recorded_at.timestamp_millis()),
        environment_fingerprint: None,
        environment_evidence_id: None,
        components_json: None,
    })
}

pub(super) fn record_consumed_on(
    connection: &Connection,
    record: &ImportedEvidenceConsumption,
) -> Result<()> {
    if matches!(record.proof, ConsumedProof::Verification { .. }) {
        let changed = connection.execute(
            "UPDATE work_task_sync_evidence_applicability
             SET consumed_by=COALESCE(consumed_by,?2) WHERE decision_id=?1",
            params![
                record.request.decision.as_str(),
                record.request.consumption.as_str()
            ],
        )?;
        if changed != 1 {
            return Err(invalid(
                "consumed verification decision projection is missing",
            ));
        }
    }
    Ok(())
}

fn native_uses(connection: &Connection) -> Result<Vec<(ObjectId, ImportedEvidenceConsumption)>> {
    // The local run feed is essential: the same kind imported as inert history
    // must not activate a consumption or acquire local work membership.
    let mut statement = connection.prepare(
        "SELECT original.object_id FROM objects original
         JOIN work_feed_entries entry ON entry.object_id=original.object_id
         WHERE original.object_kind=?1 AND entry.feed_kind='run_execution'
         ORDER BY entry.feed_id,entry.position",
    )?;
    let mut records = Vec::new();
    for row in statement.query_map([CONSUMPTION_KIND], |row| row.get::<_, String>(0))? {
        let id = super::super::stored_id(row?).map_err(super::super::store_error)?;
        let record = load_on(connection, &id)?;
        records.push((id, record));
    }
    Ok(records)
}

pub(crate) fn rebuild(connection: &Connection) -> Result<()> {
    connection.execute(
        "UPDATE work_task_sync_evidence_applicability SET consumed_by=NULL",
        [],
    )?;
    for (_, record) in native_uses(connection)? {
        record_consumed_on(connection, &record)?;
    }
    Ok(())
}

pub(crate) fn verify(connection: &Connection) -> Result<()> {
    let mut expected = std::collections::BTreeMap::new();
    for (id, record) in native_uses(connection)? {
        expected_projection_on(connection, &id)?;
        if matches!(record.proof, ConsumedProof::Verification { .. }) {
            expected.entry(record.request.decision).or_insert(id);
        }
    }
    let mut statement = connection
        .prepare("SELECT decision_id,consumed_by FROM work_task_sync_evidence_applicability")?;
    for row in statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Option<String>>(1)?))
    })? {
        let (decision, consumed) = row?;
        let decision = super::super::stored_id(decision).map_err(super::super::store_error)?;
        if consumed.as_deref() != expected.get(&decision).map(ObjectId::as_str) {
            return Err(invalid(
                "consumed-decision projection differs from canonical local uses",
            ));
        }
    }
    // A known canonical local use also needs its actual native evidence row.
    for (id, _) in native_uses(connection)? {
        let projected: Option<String> = connection
            .query_row(
                "SELECT evidence_kind FROM work_run_evidence WHERE evidence_id=?1",
                [id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        if projected.as_deref() != Some("imported") {
            return Err(invalid("native consumption evidence projection is missing"));
        }
    }
    Ok(())
}
