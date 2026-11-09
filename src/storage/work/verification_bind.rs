//! Doctor's check of a bind an earlier build stored. The `verification_bind`
//! operation is gone: nothing writes a bound record now, and the records and
//! receipts the live stores hold are read as stored.

use rusqlite::{OptionalExtension, params};

use super::super::StoreError;
use super::feeds::load_typed_work_object;
use crate::ObjectId;
use crate::domain::{
    BoundVerificationTarget, SessionId, VerificationBindTarget, VerificationEvidence,
};

#[cfg(test)]
mod tests;
#[cfg(test)]
pub(crate) use tests::bound_assessment_fixture;

/// Whether a stored bind receipt names, per requested target in order, the
/// record the bind wrote for it: a bound verification of `original` on that
/// target's binding, with its sighting, criteria and the request's
/// measurement, recorded by `session_id`, at the receipt's run position.
pub(in crate::storage) fn bound_receipt_matches_on(
    connection: &rusqlite::Connection,
    session_id: &SessionId,
    original: &ObjectId,
    measurement: &crate::domain::BindMeasurement,
    targets: &[VerificationBindTarget],
    bound: &[BoundVerificationTarget],
) -> Result<bool, StoreError> {
    if targets.len() != bound.len() {
        return Ok(false);
    }
    for (target, bound) in targets.iter().zip(bound) {
        let kind: Option<String> = connection
            .query_row(
                "SELECT object_kind FROM objects WHERE object_id = ?1",
                [bound.verification.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        if kind.as_deref() != Some("verification_evidence") {
            return Ok(false);
        }
        let evidence: VerificationEvidence =
            load_typed_work_object(connection, &bound.verification, "verification_evidence")?;
        let Some(source) = &evidence.bound_from else {
            return Ok(false);
        };
        let position = connection
            .query_row(
                "SELECT position FROM work_feed_entries
                 WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND object_id = ?2",
                params![
                    target.binding.run_id.0.to_string(),
                    bound.verification.as_str()
                ],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        if bound.work_id != target.binding.work_id
            || bound.run_id != target.binding.run_id
            || evidence.binding != target.binding
            || &evidence.session_id != session_id
            || &source.verification != original
            || source.sighting != target.sighting.observation
            || Some(target.sighting.source_revision.as_str())
                != evidence.source_basis.legacy_revision()
            || &source.measurement != measurement
            || source.criteria != target.criteria
            || position != Some(bound.run_position)
        {
            return Ok(false);
        }
    }
    Ok(true)
}
