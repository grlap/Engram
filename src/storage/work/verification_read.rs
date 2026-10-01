//! Every host verification of one criterion's bound kind on an item's active
//! run, read from one snapshot at a pinned run-feed cut, in feed order. The
//! run's feed is the enumeration authority: each candidate is checked against
//! its run projection and its producer observation, and a damaged record is
//! an error, never a missing candidate. It writes nothing and computes no
//! freshness, applicability or satisfaction.

use std::fmt::Write as _;

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::execution::work_evidence_kind_on;
use super::feeds::{current_run_feed_cut_on, load_typed_work_object};
use super::query::{load_work_item, load_work_run};
use crate::ObjectId;
use crate::domain::{
    ACCEPTANCE_VERIFICATION_READ_PAGE_BYTES, ACCEPTANCE_VERIFICATION_READ_PAGE_ROWS,
    AcceptanceBindingProducer, AcceptanceBindingReadBasis, AcceptanceBindingVerification,
    AcceptanceVerificationPage, AcceptanceVerificationReadRefusal as Refusal, ExecutionObservation,
    ProjectId, RootExecutionId, VerificationEvidence, VerificationRequirement, WorkEvidenceKind,
    WorkId, WorkRunId,
};
use crate::storage::StoreError;

const CURSOR_PREFIX: &str = "avr1-";
const MAX_CURSOR_BYTES: usize = 4_096;

/// One page request: the item, the revision, run and cut the caller expects,
/// the one-based criterion, and the continuation of an earlier page, if any.
pub(crate) struct VerificationReadRequest<'a> {
    pub work_id: WorkId,
    pub expected_work_revision: i64,
    pub run_id: WorkRunId,
    pub run_cut: i64,
    pub criterion: usize,
    pub after: Option<&'a str>,
}

/// What a continuation pins: the whole basis, the criterion and its
/// requirement, the candidate count, and the last candidate already
/// returned by its rank, position and record. It is a validated position,
/// not a capability: a token that passes every check against the current
/// snapshot is a read under the current credentials.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Cursor {
    project_id: ProjectId,
    work_id: WorkId,
    work_revision: i64,
    run_id: WorkRunId,
    run_cut: i64,
    criterion: usize,
    requirement: VerificationRequirement,
    total: usize,
    through: usize,
    last_position: i64,
    last_record: ObjectId,
}

fn refused(refusal: Refusal, reason: impl Into<String>) -> StoreError {
    StoreError::AcceptanceVerificationReadRefused {
        refusal,
        reason: reason.into(),
    }
}

fn damaged(reason: String) -> StoreError {
    StoreError::InvalidWorkProjection(reason)
}

/// Reads one page for `project_id`. The caller has checked the control
/// credentials on the same snapshot.
pub(in crate::storage) fn read_acceptance_verifications_on(
    connection: &Connection,
    project_id: &ProjectId,
    request: &VerificationReadRequest<'_>,
) -> Result<AcceptanceVerificationPage, StoreError> {
    let cursor = request.after.map(decode_cursor).transpose()?;
    if let Some(cursor) = &cursor
        && (cursor.project_id != *project_id
            || cursor.work_id != request.work_id
            || cursor.work_revision != request.expected_work_revision
            || cursor.run_id != request.run_id
            || cursor.run_cut != request.run_cut
            || cursor.criterion != request.criterion)
    {
        return Err(refused(
            Refusal::CursorBasisMismatch,
            "the continuation was made for another item, revision, run, cut or criterion",
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
    if request.run_cut != head {
        return Err(refused(
            Refusal::StaleCut,
            format!(
                "the run feed is at {head}, not the requested cut {}; read the bindings again for the current cut",
                request.run_cut
            ),
        ));
    }
    if request.criterion == 0 || request.criterion > item.acceptance.len() {
        return Err(refused(
            Refusal::InvalidCriterion,
            format!(
                "criterion {} is not one of the item's {} authored criteria",
                request.criterion,
                item.acceptance.len()
            ),
        ));
    }
    let requirement = item
        .acceptance_bindings
        .iter()
        .find(|binding| binding.criterion == request.criterion)
        .map(|binding| binding.requirement.clone());
    if let Some(cursor) = &cursor
        && requirement.as_ref() != Some(&cursor.requirement)
    {
        return Err(refused(
            Refusal::CursorBasisMismatch,
            "the continuation was made for another requirement",
        ));
    }
    let basis = AcceptanceBindingReadBasis {
        project_id: project_id.clone(),
        work_id: item.work_id,
        work_revision: item.revision,
        run_id: request.run_id,
        run_cut: head,
    };
    let Some(requirement) = requirement else {
        return Ok(AcceptanceVerificationPage {
            basis,
            criterion: request.criterion,
            requirement: None,
            total: 0,
            earlier: 0,
            shown: 0,
            omitted: 0,
            rows: Vec::new(),
            continuation: None,
        });
    };
    require_fed_verifications_on(connection, request.run_id)?;
    let through = cursor.as_ref().map_or(0, |cursor| cursor.through);
    let scan = CandidateScan {
        connection,
        basis: &basis,
        root_execution_id: run.root_execution_id,
        requirement: &requirement,
    }
    .scan(through)?;
    if let Some(cursor) = &cursor {
        let boundary = scan
            .boundary
            .as_ref()
            .map(|row| (row.position, &row.record));
        if cursor.total != scan.total
            || boundary != Some((cursor.last_position, &cursor.last_record))
        {
            return Err(refused(
                Refusal::InvalidCursor,
                "the continuation does not name a candidate boundary of this read; read again from the first page",
            ));
        }
    }
    fit_page(
        &basis,
        request.criterion,
        &requirement,
        scan.total,
        through,
        scan.rows,
    )
}

/// The feed is the enumeration authority, so every verification the run's
/// evidence projection holds must have its run-feed entry: one without would
/// silently drop out of the candidates. That is a damaged store.
fn require_fed_verifications_on(
    connection: &Connection,
    run_id: WorkRunId,
) -> Result<(), StoreError> {
    let unfed = connection
        .query_row(
            "SELECT projection.evidence_id
             FROM work_run_evidence projection
             LEFT JOIN work_feed_entries entry
               ON entry.feed_kind = 'run_execution'
              AND entry.feed_id = projection.run_id
              AND entry.object_id = projection.evidence_id
              AND entry.object_kind = 'verification_evidence'
             WHERE projection.run_id = ?1
               AND projection.evidence_kind = 'verification'
               AND entry.object_id IS NULL
             ORDER BY projection.evidence_id
             LIMIT 1",
            params![run_id.0.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if let Some(evidence) = unfed {
        return Err(damaged(format!(
            "verification {evidence} on run {run_id:?} has no run-feed entry"
        )));
    }
    Ok(())
}

/// The candidates counted at the cut: all of them by number, the rows after
/// `through` up to one page, and the candidate at rank `through` itself.
struct Scanned {
    total: usize,
    boundary: Option<AcceptanceBindingVerification>,
    rows: Vec<AcceptanceBindingVerification>,
}

struct CandidateScan<'a> {
    connection: &'a Connection,
    basis: &'a AcceptanceBindingReadBasis,
    /// The basis run's root execution, which every candidate and its
    /// producer must name.
    root_execution_id: RootExecutionId,
    requirement: &'a VerificationRequirement,
}

impl CandidateScan<'_> {
    /// Counts every verification of the bound kind on the run at or before
    /// the cut, keeping only the boundary and one page of rows in memory.
    fn scan(&self, through: usize) -> Result<Scanned, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT position, object_id FROM work_feed_entries
             WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND position <= ?2
               AND object_kind = 'verification_evidence'
             ORDER BY position",
        )?;
        let entries = statement.query_map(
            params![self.basis.run_id.0.to_string(), self.basis.run_cut],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )?;
        let mut scanned = Scanned {
            total: 0,
            boundary: None,
            rows: Vec::new(),
        };
        for entry in entries {
            let (position, stored) = entry?;
            let id = ObjectId::from_stored(stored.clone())
                .ok_or(StoreError::InvalidStoredKey(stored))?;
            let evidence: VerificationEvidence =
                load_typed_work_object(self.connection, &id, "verification_evidence")?;
            if evidence.check_kind != self.requirement.check_kind {
                continue;
            }
            let row = self.candidate(&id, position, evidence)?;
            scanned.total += 1;
            let rank = scanned.total;
            if rank == through {
                scanned.boundary = Some(row);
            } else if rank > through && rank <= through + ACCEPTANCE_VERIFICATION_READ_PAGE_ROWS {
                scanned.rows.push(row);
            }
        }
        Ok(scanned)
    }

    /// One candidate, after checking that it and its producer belong to the
    /// basis project, item, root execution and run, that it agrees with its
    /// run projection and its producer, and that the producer is an
    /// execution observation on the run's feed before it.
    fn candidate(
        &self,
        id: &ObjectId,
        position: i64,
        evidence: VerificationEvidence,
    ) -> Result<AcceptanceBindingVerification, StoreError> {
        if position <= 0
            || evidence.project_id != self.basis.project_id
            || evidence.binding.run_id != self.basis.run_id
            || evidence.binding.work_id != self.basis.work_id
            || evidence.binding.root_execution_id != self.root_execution_id
        {
            return Err(damaged(format!("verification {id} crosses its run")));
        }
        // Agreement with the run's evidence projection, which also checks the
        // record's binding to its producer observation.
        match work_evidence_kind_on(self.connection, self.basis.run_id, id) {
            Ok(WorkEvidenceKind::Verification) => {}
            Ok(_) => {
                return Err(damaged(format!(
                    "verification {id} is projected as another kind of evidence"
                )));
            }
            Err(StoreError::InvalidWork(reason)) => return Err(damaged(reason)),
            Err(error) => return Err(error),
        }
        let producer_id = &evidence.producer_observation;
        let producer: ExecutionObservation =
            load_typed_work_object(self.connection, producer_id, "execution_observation")?;
        if producer.project_id != self.basis.project_id
            || producer.binding.run_id != self.basis.run_id
            || producer.binding.work_id != self.basis.work_id
            || producer.binding.root_execution_id != self.root_execution_id
        {
            return Err(damaged(format!(
                "producer {producer_id} of verification {id} crosses its run"
            )));
        }
        let producer_position = self.producer_position(producer_id, id, position)?;
        Ok(AcceptanceBindingVerification {
            record: id.clone(),
            position,
            check_kind: evidence.check_kind,
            check_fingerprint: evidence.check_fingerprint,
            result: evidence.result,
            source_basis: evidence.source_basis,
            producer: AcceptanceBindingProducer {
                record: producer_id.clone(),
                position: producer_position,
                outcome: producer.outcome,
            },
        })
    }

    /// The producer's run-feed position, read from its own feed entry, which
    /// must record it as an execution observation at a position after the
    /// feed's start and before its verification's.
    fn producer_position(
        &self,
        producer_id: &ObjectId,
        id: &ObjectId,
        position: i64,
    ) -> Result<i64, StoreError> {
        let entry = self
            .connection
            .query_row(
                "SELECT position, object_kind FROM work_feed_entries
                 WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND object_id = ?2",
                params![self.basis.run_id.0.to_string(), producer_id.as_str()],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )
            .optional()?;
        match entry {
            Some((at, kind)) if kind == "execution_observation" && 0 < at && at < position => {
                Ok(at)
            }
            Some((at, kind)) => Err(damaged(format!(
                "producer {producer_id} of verification {id} is recorded on the run feed as {kind} at {at}, not as an execution observation before {position}"
            ))),
            None => Err(damaged(format!(
                "producer {producer_id} of verification {id} has no run-feed entry"
            ))),
        }
    }
}

/// The largest page of up to the row limit that fits the byte limit. A
/// first row that does not fit alone is refused, never clipped or dropped.
fn fit_page(
    basis: &AcceptanceBindingReadBasis,
    criterion: usize,
    requirement: &VerificationRequirement,
    total: usize,
    through: usize,
    mut rows: Vec<AcceptanceBindingVerification>,
) -> Result<AcceptanceVerificationPage, StoreError> {
    loop {
        let shown = rows.len();
        let omitted = total - through - shown;
        let continuation = match rows.last() {
            Some(last) if omitted > 0 => Some(encode_cursor(&Cursor {
                project_id: basis.project_id.clone(),
                work_id: basis.work_id,
                work_revision: basis.work_revision,
                run_id: basis.run_id,
                run_cut: basis.run_cut,
                criterion,
                requirement: requirement.clone(),
                total,
                through: through + shown,
                last_position: last.position,
                last_record: last.record.clone(),
            })?),
            _ => None,
        };
        let page = AcceptanceVerificationPage {
            basis: basis.clone(),
            criterion,
            requirement: Some(requirement.clone()),
            total,
            earlier: through,
            shown,
            omitted,
            rows,
            continuation,
        };
        if serde_json::to_vec(&page)?.len() <= ACCEPTANCE_VERIFICATION_READ_PAGE_BYTES {
            return Ok(page);
        }
        if shown <= 1 {
            return Err(refused(
                Refusal::PageTooLarge,
                format!(
                    "candidate {} does not fit one {ACCEPTANCE_VERIFICATION_READ_PAGE_BYTES}-byte page",
                    through + 1
                ),
            ));
        }
        rows = page.rows;
        rows.pop();
    }
}

fn encode_cursor(cursor: &Cursor) -> Result<String, StoreError> {
    let mut token = CURSOR_PREFIX.to_owned();
    for byte in serde_json::to_vec(cursor)? {
        write!(token, "{byte:02x}").map_err(|_| damaged("cursor encoding failed".into()))?;
    }
    if token.len() > MAX_CURSOR_BYTES {
        return Err(damaged(
            "an acceptance verification cursor exceeds its bound".into(),
        ));
    }
    Ok(token)
}

fn decode_cursor(token: &str) -> Result<Cursor, StoreError> {
    let invalid = || {
        refused(
            Refusal::InvalidCursor,
            "the continuation is not a position this read can resume at; read again from the first page",
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
    if cursor.run_cut < 0
        || cursor.criterion == 0
        || cursor.through == 0
        || cursor.through >= cursor.total
        || cursor.last_position <= 0
        || cursor.last_position > cursor.run_cut
    {
        return Err(invalid());
    }
    Ok(cursor)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::VerificationKind;

    fn cursor() -> Cursor {
        Cursor {
            project_id: ProjectId("project-guards".into()),
            work_id: WorkId::new(),
            work_revision: 2,
            run_id: WorkRunId::new(),
            run_cut: 9,
            criterion: 3,
            requirement: VerificationRequirement {
                check_kind: VerificationKind::Test,
                check_fingerprint: None,
            },
            total: 12,
            through: 8,
            last_position: 7,
            last_record: ObjectId::from_canonical_bytes(b"the last candidate returned"),
        }
    }

    fn basis() -> AcceptanceBindingReadBasis {
        AcceptanceBindingReadBasis {
            project_id: ProjectId("project-guards".into()),
            work_id: WorkId::new(),
            work_revision: 2,
            run_id: WorkRunId::new(),
            run_cut: 200,
        }
    }

    /// A candidate whose source basis holds `width` bytes of workspace id,
    /// at run-feed position `position`.
    fn row(position: i64, width: usize) -> AcceptanceBindingVerification {
        AcceptanceBindingVerification {
            record: ObjectId::from_canonical_bytes(format!("record {position}").as_bytes()),
            position,
            check_kind: VerificationKind::Test,
            check_fingerprint: ObjectId::from_canonical_bytes(b"cargo test"),
            result: crate::domain::VerificationResult::Passed,
            source_basis: crate::domain::ExecutionSourceBasis {
                workspace_id: "w".repeat(width),
                source_revision: "revision".into(),
                source_root_generation: None,
                source_root_state: None,
            },
            producer: AcceptanceBindingProducer {
                record: ObjectId::from_canonical_bytes(format!("producer {position}").as_bytes()),
                position: position - 1,
                outcome: crate::domain::ExecutionOutcome::Succeeded,
            },
        }
    }

    // A page keeps whole rows in order within the byte limit, dropping rows
    // from its end and carrying them to the next page; a row that cannot fit
    // alone is refused, never clipped.
    #[test]
    fn a_page_fits_whole_rows_or_refuses_the_first() {
        let basis = basis();
        let requirement = cursor().requirement;
        let rows = |width| (1..=8).map(|n| row(n * 10, width)).collect::<Vec<_>>();
        let small = fit_page(&basis, 1, &requirement, 8, 0, rows(8)).expect("small rows");
        assert_eq!(
            (small.shown, small.omitted, small.continuation),
            (8, 0, None)
        );

        let page = fit_page(&basis, 1, &requirement, 12, 2, rows(4_000)).expect("large rows");
        assert!(page.shown >= 1 && page.shown < 8, "{}", page.shown);
        assert_eq!(page.earlier, 2);
        assert_eq!(page.omitted, 12 - 2 - page.shown);
        assert!(
            serde_json::to_vec(&page).expect("bytes").len()
                <= ACCEPTANCE_VERIFICATION_READ_PAGE_BYTES
        );
        let last = page.rows.last().expect("a row");
        let resumed =
            decode_cursor(page.continuation.as_deref().expect("continuation")).expect("cursor");
        assert_eq!(resumed.through, 2 + page.shown);
        assert_eq!(
            (resumed.last_position, &resumed.last_record),
            (last.position, &last.record)
        );
        assert_eq!(page.rows, rows(4_000)[..page.shown]);

        assert!(matches!(
            fit_page(&basis, 1, &requirement, 3, 0, vec![row(10, 17_000)]),
            Err(StoreError::AcceptanceVerificationReadRefused {
                refusal: Refusal::PageTooLarge,
                ..
            })
        ));
    }

    fn invalid(token: &str) -> bool {
        matches!(
            decode_cursor(token),
            Err(StoreError::AcceptanceVerificationReadRefused {
                refusal: Refusal::InvalidCursor,
                ..
            })
        )
    }

    // A cursor round-trips, stays within its bound, and anything that is not
    // its exact lowercase encoding, or names an impossible position, is
    // refused as invalid.
    #[test]
    fn a_cursor_round_trips_and_refuses_an_impossible_position() {
        let token = encode_cursor(&cursor()).expect("encode");
        assert!(token.starts_with(CURSOR_PREFIX));
        assert!(token.len() <= MAX_CURSOR_BYTES);
        let decoded = decode_cursor(&token).expect("decode");
        assert_eq!(
            (decoded.through, decoded.last_position, decoded.criterion),
            (8, 7, 3)
        );
        assert!(invalid(&token.to_uppercase()));
        assert!(invalid(&token.replacen(CURSOR_PREFIX, "abr1-", 1)));
        assert!(invalid(&format!("{token}0")));
        for impossible in [
            Cursor {
                criterion: 0,
                ..cursor()
            },
            Cursor {
                through: 0,
                ..cursor()
            },
            Cursor {
                through: 12,
                ..cursor()
            },
            Cursor {
                last_position: 10,
                ..cursor()
            },
            Cursor {
                last_position: 0,
                ..cursor()
            },
            Cursor {
                run_cut: -1,
                ..cursor()
            },
        ] {
            assert!(invalid(&encode_cursor(&impossible).expect("encode")));
        }
    }
}
