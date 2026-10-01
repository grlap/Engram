//! What satisfied each bound acceptance criterion of an item on its active
//! run, read from one snapshot at a pinned run-feed cut. It selects each
//! binding's obligation as completion does and reports that obligation's
//! recorded resolution with its original verification and producer. It
//! writes nothing and computes no freshness.

use std::collections::HashSet;
use std::fmt::Write as _;

use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};

use super::WorkObligationRecord;
use super::completion::{binding_obligation, load_work_obligation_records_on};
use super::feeds::{
    current_run_feed_cut_on, load_typed_work_object, run_feed_position_for_object_on,
};
use super::query::{load_work_item, load_work_run};
use crate::ObjectId;
use crate::domain::{
    ACCEPTANCE_BINDING_READ_PAGE_BYTES, ACCEPTANCE_BINDING_READ_PAGE_ROWS,
    AcceptanceBindingObligation, AcceptanceBindingPage, AcceptanceBindingProducer,
    AcceptanceBindingReadBasis, AcceptanceBindingReadBinding,
    AcceptanceBindingReadRefusal as Refusal, AcceptanceBindingResolution,
    AcceptanceBindingResolutionKind, AcceptanceBindingRow, AcceptanceBindingSatisfaction,
    AcceptanceBindingVerification, ExecutionObservation, FeedId, FeedPosition, ProjectId,
    VerificationEvidence, WorkId, WorkItem, WorkObligationResolution, WorkObligationState,
    WorkRunId,
};
use crate::storage::StoreError;

const CURSOR_PREFIX: &str = "abr1-";
const MAX_CURSOR_BYTES: usize = 4_096;

/// One page request: the item, the revision and run the caller expects, and
/// the continuation of an earlier page, if any.
pub(crate) struct BindingReadRequest<'a> {
    pub work_id: WorkId,
    pub expected_work_revision: i64,
    pub run_id: WorkRunId,
    pub after: Option<&'a str>,
}

/// What a continuation pins: the basis of the first page and the last
/// criterion already returned.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    project_id: ProjectId,
    work_id: WorkId,
    work_revision: i64,
    run_id: WorkRunId,
    run_cut: i64,
    total: usize,
    through: usize,
}

fn refused(refusal: Refusal, reason: impl Into<String>) -> StoreError {
    StoreError::AcceptanceBindingReadRefused {
        refusal,
        reason: reason.into(),
    }
}

fn damaged(reason: String) -> StoreError {
    StoreError::InvalidWorkProjection(reason)
}

/// Reads one page for `project_id`. The caller has checked the control
/// credentials on the same snapshot.
pub(in crate::storage) fn read_acceptance_bindings_on(
    connection: &Connection,
    project_id: &ProjectId,
    request: &BindingReadRequest<'_>,
) -> Result<AcceptanceBindingPage, StoreError> {
    let cursor = request.after.map(decode_cursor).transpose()?;
    if let Some(cursor) = &cursor
        && (cursor.project_id != *project_id
            || cursor.work_id != request.work_id
            || cursor.work_revision != request.expected_work_revision
            || cursor.run_id != request.run_id)
    {
        return Err(refused(
            Refusal::CursorBasisMismatch,
            "the continuation was issued for another item, revision or run",
        ));
    }
    let item = match load_work_item(connection, request.work_id) {
        Err(StoreError::WorkNotFound(_)) => {
            return Err(refused(
                Refusal::UnknownWork,
                "the store holds no item with this id",
            ));
        }
        other => other?,
    };
    if item.project_id != *project_id {
        return Err(refused(
            Refusal::WrongProject,
            "the item belongs to another project",
        ));
    }
    if item.revision != request.expected_work_revision {
        return Err(refused(
            Refusal::WrongRevision,
            format!(
                "the item is at revision {}, not {}",
                item.revision, request.expected_work_revision
            ),
        ));
    }
    if item.active_run_id != Some(request.run_id) {
        return Err(refused(
            Refusal::WrongRun,
            "the run is not the item's active run",
        ));
    }
    let run = load_work_run(connection, request.run_id)?;
    if run.work_id != item.work_id {
        return Err(damaged(format!(
            "active run {:?} of work {:?} names another item",
            run.run_id, item.work_id
        )));
    }
    let head = current_run_feed_cut_on(connection, request.run_id)?.position;
    let total = item.acceptance.len();
    let through = match &cursor {
        None => 0,
        Some(cursor) => {
            if cursor.run_cut != head {
                return Err(refused(
                    Refusal::StaleCut,
                    format!(
                        "the run feed is at {head}, not the continuation's cut {}; read again from the first page",
                        cursor.run_cut
                    ),
                ));
            }
            if cursor.total != total {
                return Err(refused(
                    Refusal::CursorBasisMismatch,
                    "the continuation counts another criterion set",
                ));
            }
            cursor.through
        }
    };
    let basis = AcceptanceBindingReadBasis {
        project_id: project_id.clone(),
        work_id: item.work_id,
        work_revision: item.revision,
        run_id: request.run_id,
        run_cut: head,
    };
    let records = load_work_obligation_records_on(connection, request.run_id, None)?;
    require_projected_obligations_on(connection, &basis, &records)?;
    let reader = RowReader {
        connection,
        item: &item,
        basis: &basis,
        records: &records,
    };
    let candidates = (through + 1..=total)
        .take(ACCEPTANCE_BINDING_READ_PAGE_ROWS)
        .map(|criterion| reader.row(criterion))
        .collect::<Result<Vec<_>, _>>()?;
    fit_page(&basis, total, through, candidates)
}

/// The largest page of up to the row limit that fits the byte limit. A
/// first row that does not fit alone is refused, never clipped or dropped.
fn fit_page(
    basis: &AcceptanceBindingReadBasis,
    total: usize,
    through: usize,
    mut rows: Vec<AcceptanceBindingRow>,
) -> Result<AcceptanceBindingPage, StoreError> {
    loop {
        let shown = rows.len();
        let omitted = total - through - shown;
        let continuation = if omitted == 0 {
            None
        } else {
            Some(encode_cursor(&Cursor {
                project_id: basis.project_id.clone(),
                work_id: basis.work_id,
                work_revision: basis.work_revision,
                run_id: basis.run_id,
                run_cut: basis.run_cut,
                total,
                through: through + shown,
            })?)
        };
        let page = AcceptanceBindingPage {
            basis: basis.clone(),
            total,
            earlier: through,
            shown,
            omitted,
            rows,
            continuation,
        };
        if serde_json::to_vec(&page)?.len() <= ACCEPTANCE_BINDING_READ_PAGE_BYTES {
            return Ok(page);
        }
        if shown <= 1 {
            return Err(refused(
                Refusal::PageTooLarge,
                format!(
                    "criterion {} does not fit one {ACCEPTANCE_BINDING_READ_PAGE_BYTES}-byte page",
                    through + 1
                ),
            ));
        }
        rows = page.rows;
        rows.pop();
    }
}

/// Selection reads the run's obligation projection rows, so every
/// obligation record on the run's feed at or before the cut must have its
/// row. A lost definition row would read as no obligation, or let an older
/// definition answer for the criterion; a lost resolution would read as
/// open. Either is a damaged store.
fn require_projected_obligations_on(
    connection: &Connection,
    basis: &AcceptanceBindingReadBasis,
    records: &[WorkObligationRecord],
) -> Result<(), StoreError> {
    let definitions: HashSet<&ObjectId> =
        records.iter().map(|record| &record.definition_id).collect();
    let resolutions: HashSet<&ObjectId> = records
        .iter()
        .filter_map(|record| record.resolution_id.as_ref())
        .collect();
    let recorded = connection
        .prepare(
            "SELECT object_kind, object_id FROM work_feed_entries
             WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND position <= ?2
               AND object_kind IN ('work_obligation', 'work_obligation_resolution')",
        )?
        .query_map(params![basis.run_id.0.to_string(), basis.run_cut], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (kind, stored) in recorded {
        let id =
            ObjectId::from_stored(stored.clone()).ok_or(StoreError::InvalidStoredKey(stored))?;
        let projected = if kind == "work_obligation" {
            definitions.contains(&id)
        } else {
            resolutions.contains(&id)
        };
        if !projected {
            return Err(damaged(format!(
                "{kind} {id} on run {:?} has no obligation projection",
                basis.run_id
            )));
        }
    }
    Ok(())
}

/// A run-feed position the read reports, which must lie at or before the
/// read cut.
fn within_cut(
    basis: &AcceptanceBindingReadBasis,
    position: i64,
    object: &ObjectId,
) -> Result<(), StoreError> {
    if position > basis.run_cut {
        return Err(damaged(format!(
            "record {object} lies at run-feed position {position}, past the read cut {}",
            basis.run_cut
        )));
    }
    Ok(())
}

/// A position recorded inside a record, which must name the basis run's feed
/// and lie at or before the read cut.
fn recorded_position(
    basis: &AcceptanceBindingReadBasis,
    at: &FeedPosition,
    owner: &ObjectId,
) -> Result<i64, StoreError> {
    if at.feed != FeedId::RunExecution(basis.run_id) {
        return Err(damaged(format!(
            "record {owner} names a position on another feed"
        )));
    }
    within_cut(basis, at.position, owner)?;
    Ok(at.position)
}

struct RowReader<'a> {
    connection: &'a Connection,
    item: &'a WorkItem,
    basis: &'a AcceptanceBindingReadBasis,
    records: &'a [WorkObligationRecord],
}

impl RowReader<'_> {
    fn row(&self, criterion: usize) -> Result<AcceptanceBindingRow, StoreError> {
        let Some(binding) = self
            .item
            .acceptance_bindings
            .iter()
            .find(|binding| binding.criterion == criterion)
        else {
            return Ok(AcceptanceBindingRow {
                criterion,
                binding: None,
            });
        };
        let obligation = binding_obligation(self.records, binding)
            .map(|record| self.obligation(record))
            .transpose()?;
        Ok(AcceptanceBindingRow {
            criterion,
            binding: Some(AcceptanceBindingReadBinding {
                requirement: binding.requirement.clone(),
                obligation,
            }),
        })
    }

    /// The run-feed position of `object`, which must lie at or before the
    /// read cut.
    fn position(&self, object: &ObjectId) -> Result<i64, StoreError> {
        let position =
            run_feed_position_for_object_on(self.connection, self.basis.run_id, object)?.position;
        within_cut(self.basis, position, object)?;
        Ok(position)
    }

    fn recorded_position(&self, at: &FeedPosition, owner: &ObjectId) -> Result<i64, StoreError> {
        recorded_position(self.basis, at, owner)
    }

    fn obligation(
        &self,
        record: &WorkObligationRecord,
    ) -> Result<AcceptanceBindingObligation, StoreError> {
        let obligation = &record.obligation;
        if obligation.run_id != self.basis.run_id || obligation.work_id != self.item.work_id {
            return Err(damaged(format!(
                "obligation {} crosses its run",
                obligation.obligation_id.0
            )));
        }
        let resolution = match (record.state, &record.resolution, &record.resolution_id) {
            (WorkObligationState::Open, None, None) => None,
            (state, Some(event), Some(id)) if state != WorkObligationState::Open => {
                Some(self.resolution(record, &event.resolution, id)?)
            }
            _ => {
                return Err(damaged(format!(
                    "obligation {} state disagrees with its resolution",
                    obligation.obligation_id.0
                )));
            }
        };
        Ok(AcceptanceBindingObligation {
            obligation_id: obligation.obligation_id,
            definition: record.definition_id.clone(),
            work_revision: obligation.work_revision,
            rule: obligation.rule.clone(),
            triggering_observation: obligation.triggering_observation.clone(),
            trigger_position: self
                .recorded_position(&obligation.trigger_position, &record.definition_id)?,
            definition_position: self.position(&record.definition_id)?,
            state: record.state,
            resolution,
        })
    }

    fn resolution(
        &self,
        record: &WorkObligationRecord,
        resolution: &WorkObligationResolution,
        id: &ObjectId,
    ) -> Result<AcceptanceBindingResolution, StoreError> {
        let (kind, satisfaction) = match resolution {
            WorkObligationResolution::Satisfied {
                evidence,
                evaluated_cut,
            } => (
                AcceptanceBindingResolutionKind::Satisfied,
                Some(AcceptanceBindingSatisfaction {
                    evaluated_cut: self.recorded_position(evaluated_cut, id)?,
                    verification: self.verification(evidence)?,
                }),
            ),
            WorkObligationResolution::Waived { .. } => {
                (AcceptanceBindingResolutionKind::Waived, None)
            }
            WorkObligationResolution::Displaced { .. } => {
                (AcceptanceBindingResolutionKind::Displaced, None)
            }
        };
        let agrees = matches!(
            (record.state, kind),
            (
                WorkObligationState::Satisfied,
                AcceptanceBindingResolutionKind::Satisfied
            ) | (
                WorkObligationState::Waived,
                AcceptanceBindingResolutionKind::Waived
            ) | (
                WorkObligationState::Displaced,
                AcceptanceBindingResolutionKind::Displaced
            )
        );
        if !agrees {
            return Err(damaged(format!(
                "obligation {} state disagrees with its resolution {id}",
                record.obligation.obligation_id.0
            )));
        }
        Ok(AcceptanceBindingResolution {
            record: id.clone(),
            position: self.position(id)?,
            kind,
            satisfaction,
        })
    }

    fn verification(&self, id: &ObjectId) -> Result<AcceptanceBindingVerification, StoreError> {
        let evidence: VerificationEvidence =
            load_typed_work_object(self.connection, id, "verification_evidence")?;
        if evidence.project_id != self.basis.project_id
            || evidence.binding.run_id != self.basis.run_id
        {
            return Err(damaged(format!("verification {id} crosses its run")));
        }
        let producer_id = &evidence.producer_observation;
        let producer: ExecutionObservation =
            load_typed_work_object(self.connection, producer_id, "execution_observation")?;
        if producer.project_id != self.basis.project_id
            || producer.binding.run_id != self.basis.run_id
        {
            return Err(damaged(format!(
                "producer {producer_id} of verification {id} crosses its run"
            )));
        }
        Ok(AcceptanceBindingVerification {
            record: id.clone(),
            position: self.position(id)?,
            check_kind: evidence.check_kind,
            check_fingerprint: evidence.check_fingerprint,
            result: evidence.result,
            source_basis: evidence.source_basis,
            producer: AcceptanceBindingProducer {
                record: producer_id.clone(),
                position: self.position(producer_id)?,
                outcome: producer.outcome,
            },
        })
    }
}

fn encode_cursor(cursor: &Cursor) -> Result<String, StoreError> {
    let mut token = CURSOR_PREFIX.to_owned();
    for byte in serde_json::to_vec(cursor)? {
        write!(token, "{byte:02x}").map_err(|_| damaged("cursor encoding failed".into()))?;
    }
    if token.len() > MAX_CURSOR_BYTES {
        return Err(damaged(
            "an acceptance binding cursor exceeds its bound".into(),
        ));
    }
    Ok(token)
}

fn decode_cursor(token: &str) -> Result<Cursor, StoreError> {
    let invalid = || {
        refused(
            Refusal::InvalidCursor,
            "the continuation is not one this read issued; read again from the first page",
        )
    };
    if token.len() > MAX_CURSOR_BYTES {
        return Err(invalid());
    }
    let hex = token.strip_prefix(CURSOR_PREFIX).ok_or_else(invalid)?;
    if hex.len() % 2 != 0
        || !hex
            .bytes()
            .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
    {
        return Err(invalid());
    }
    let bytes = hex
        .as_bytes()
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| {
            std::str::from_utf8(pair)
                .ok()
                .and_then(|pair| u8::from_str_radix(pair, 16).ok())
                .ok_or_else(invalid)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let cursor: Cursor = serde_json::from_slice(&bytes).map_err(|_| invalid())?;
    if cursor.run_cut < 0 || cursor.through == 0 || cursor.through >= cursor.total {
        return Err(invalid());
    }
    Ok(cursor)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn basis(run_cut: i64) -> AcceptanceBindingReadBasis {
        AcceptanceBindingReadBasis {
            project_id: ProjectId("project-guards".into()),
            work_id: WorkId::new(),
            work_revision: 1,
            run_id: WorkRunId::new(),
            run_cut,
        }
    }

    fn record() -> ObjectId {
        ObjectId::from_canonical_bytes(b"a record the read reports")
    }

    fn is_damaged(result: &Result<i64, StoreError>) -> bool {
        matches!(result, Err(StoreError::InvalidWorkProjection(_)))
    }

    // A position is reported only on the basis run's feed and at or before
    // its cut; anything else is a damaged store, never a refusal.
    #[test]
    fn a_reported_position_lies_on_the_basis_run_at_or_before_its_cut() {
        let basis = basis(7);
        let on_run = |position| FeedPosition {
            feed: FeedId::RunExecution(basis.run_id),
            position,
        };
        assert_eq!(
            recorded_position(&basis, &on_run(7), &record()).ok(),
            Some(7)
        );
        assert_eq!(
            recorded_position(&basis, &on_run(3), &record()).ok(),
            Some(3)
        );
        assert!(is_damaged(&recorded_position(
            &basis,
            &on_run(8),
            &record()
        )));
        let other_run = FeedPosition {
            feed: FeedId::RunExecution(WorkRunId::new()),
            position: 2,
        };
        assert!(is_damaged(&recorded_position(
            &basis,
            &other_run,
            &record()
        )));
        assert!(within_cut(&basis, 7, &record()).is_ok());
        assert!(matches!(
            within_cut(&basis, 8, &record()),
            Err(StoreError::InvalidWorkProjection(_))
        ));
    }

    // A cursor names a pinned basis and a boundary inside the criterion set;
    // its encoding round-trips and anything else is not one this read issued.
    #[test]
    fn a_cursor_round_trips_and_refuses_what_it_did_not_issue() {
        let pinned = basis(4);
        let cursor = Cursor {
            project_id: pinned.project_id.clone(),
            work_id: pinned.work_id,
            work_revision: pinned.work_revision,
            run_id: pinned.run_id,
            run_cut: pinned.run_cut,
            total: 12,
            through: 8,
        };
        let token = encode_cursor(&cursor).expect("encode");
        let decoded = decode_cursor(&token).expect("decode");
        assert_eq!(decoded.through, 8);
        assert_eq!(decoded.run_cut, 4);
        assert!(matches!(
            decode_cursor(&token.to_uppercase()),
            Err(StoreError::AcceptanceBindingReadRefused {
                refusal: Refusal::InvalidCursor,
                ..
            })
        ));
    }
}
