//! One verification record assessed against one obligation at one run-feed
//! cut: the named-root prefilter storage reads from the store, then control's
//! position, source-context and typed-matcher decision. The satisfaction path,
//! the doctor's replay of a satisfied resolution and the reconstruction read
//! share it, so what a record satisfies when stored is what a read explains.

use rusqlite::{Connection, OptionalExtension, params};
use serde::Serialize;

use super::super::feeds::{
    current_run_feed_cut_on, latest_source_mutation_on, load_typed_work_object,
    run_feed_position_for_object_on,
};
use super::named_root::{NamedRootContext, named_root_context_on, obligation_matches_named_root};
use super::{
    OBLIGATION_COLUMNS, ObjectId, SqliteStore, StoreError, encode_state,
    load_work_obligation_record_on, obligation_projection_row,
};
use crate::control::{
    ObligationAssessment, ObligationSatisfactionInput, ObligationSkip,
    acceptance_binding_criterion, assess_obligation_satisfaction,
};
use crate::domain::{
    BuiltinObligationRuleRef, ExecutionObservation, FeedPosition, VerificationEvidence,
    VerificationKind, WorkId, WorkObligation, WorkObligationId, WorkObligationResolution,
    WorkRunId,
};

/// Everything the satisfaction decision reads for one verification record at
/// one cut, loaded once.
pub(super) struct VerificationAtCut {
    evidence: VerificationEvidence,
    producer: ExecutionObservation,
    evidence_position: i64,
    producer_position: i64,
    cut: FeedPosition,
    root: Option<NamedRootContext>,
    latest_mutation: Option<(i64, ExecutionObservation)>,
}

impl VerificationAtCut {
    /// The record's context at `cut`: the claim's named root and the latest
    /// source mutation as they stood there, and the record's and its
    /// producer's run-feed positions.
    pub(super) fn load_on(
        connection: &Connection,
        evidence: VerificationEvidence,
        evidence_id: &ObjectId,
        cut: FeedPosition,
    ) -> Result<Self, StoreError> {
        let run_id = evidence.binding.run_id;
        let evidence_position =
            run_feed_position_for_object_on(connection, run_id, evidence_id)?.position;
        let root =
            named_root_context_on(connection, run_id, evidence.binding.claim_id, cut.position)?;
        let latest_mutation = if let Some(root) = &root {
            root.latest_mutation.clone()
        } else {
            latest_source_mutation_on(connection, run_id, cut.position)?
        };
        let producer = load_typed_work_object::<ExecutionObservation>(
            connection,
            &evidence.producer_observation,
            "execution_observation",
        )?;
        let producer_position =
            run_feed_position_for_object_on(connection, run_id, &evidence.producer_observation)?
                .position;
        Ok(Self {
            evidence,
            producer,
            evidence_position,
            producer_position,
            cut,
            root,
            latest_mutation,
        })
    }

    /// Whether the record satisfies `obligation` at this cut, or why not. An
    /// obligation the named root holds as foreign or displaced is left out
    /// before matching.
    pub(super) fn assess_on(
        &self,
        connection: &Connection,
        obligation: &WorkObligation,
    ) -> Result<ObligationAssessment, StoreError> {
        if !obligation_matches_named_root(connection, obligation, self.root.as_ref())? {
            return Ok(ObligationAssessment::Skipped(
                ObligationSkip::ForeignOrDisplaced,
            ));
        }
        Ok(assess_obligation_satisfaction(
            &ObligationSatisfactionInput {
                evidence: &self.evidence,
                producer: &self.producer,
                latest_mutation: self
                    .latest_mutation
                    .as_ref()
                    .map(|(position, mutation)| (mutation, *position)),
                named_root: self.root.as_ref().map(NamedRootContext::match_input),
                evidence_position: self.evidence_position,
                producer_position: Some(self.producer_position),
                evaluated_cut: &self.cut,
            },
            obligation,
        ))
    }
}

/// What the store holds about one obligation's end, whatever a reconstruction
/// says: a recorded resolution is a fact, never recomputed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum RecordedObligationEnd {
    Open,
    SatisfiedByThisRecord,
    SatisfiedByAnotherRecord,
    Waived,
    Displaced,
}

/// One candidate obligation: its identity, the reconstructed assessment of
/// the record against it, and its recorded end.
#[derive(Clone, Debug)]
pub(crate) struct VerificationObligationAssessment {
    pub obligation_id: WorkObligationId,
    pub rule: BuiltinObligationRuleRef,
    pub check_kind: VerificationKind,
    pub pinned: bool,
    /// The one-based acceptance criterion a binding rule requires.
    pub criterion: Option<usize>,
    pub trigger_position: i64,
    pub assessment: ObligationAssessment,
    pub recorded: RecordedObligationEnd,
}

/// The last obligation a page showed, in trigger-position and id order.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) struct AssessmentBoundary {
    pub trigger_position: i64,
    pub obligation_id: WorkObligationId,
}

/// One page of the obligations of the record's check kind on its run, each
/// assessed as the store stood when the record was appended, under the
/// current matching rules, with exact counts over all of them.
#[derive(Clone, Debug)]
pub(crate) struct VerificationAssessment {
    pub run_id: WorkRunId,
    /// The record's own run-feed position.
    pub record_position: i64,
    /// The run-feed position every assessment reads at: the record's own
    /// event, the head when satisfaction ran for it.
    pub cut_position: i64,
    /// The run feed's head at this read; a continuation binds it.
    pub head_position: i64,
    /// Every obligation of the record's check kind on the run.
    pub total: usize,
    /// Those up to and including the boundary the page continues from.
    pub earlier: usize,
    /// False when the boundary names no obligation of that kind on the run.
    pub boundary_found: bool,
    pub rows: Vec<VerificationObligationAssessment>,
}

impl SqliteStore {
    /// Reconstructs, for one native verification record of `work`, what it
    /// satisfies at its own run-feed position: at most `limit` obligations of
    /// its check kind after `after`, and only those are loaded and assessed.
    /// `None` when the id is not such a record of this item. Reads only; the
    /// caller owns the read snapshot.
    pub(crate) fn verification_assessment(
        &self,
        work: WorkId,
        evidence_id: &ObjectId,
        after: Option<AssessmentBoundary>,
        limit: usize,
    ) -> Result<Option<VerificationAssessment>, StoreError> {
        let connection = &self.connection;
        let kind = connection
            .query_row(
                "SELECT object_kind FROM objects WHERE object_id = ?1",
                [evidence_id.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if kind.as_deref() != Some("verification_evidence") {
            return Ok(None);
        }
        let evidence = load_typed_work_object::<VerificationEvidence>(
            connection,
            evidence_id,
            "verification_evidence",
        )?;
        if evidence.binding.work_id != work {
            return Ok(None);
        }
        let run_id = evidence.binding.run_id;
        let record_position = run_feed_position_for_object_on(connection, run_id, evidence_id)?;
        // The record's event follows it in the same append; satisfaction read
        // the run feed with that event as its head.
        let cut = FeedPosition {
            feed: record_position.feed.clone(),
            position: record_position.position + 1,
        };
        let head_position = current_run_feed_cut_on(connection, run_id)?.position;
        let check_kind = encode_state(evidence.check_kind)?;
        let run = run_id.0.to_string();
        let boundary =
            after.map(|after| (after.trigger_position, after.obligation_id.0.to_string()));
        let count = |filter: &str, parameters: &[&dyn rusqlite::ToSql]| {
            let count = connection.query_row(
                &format!(
                    "SELECT COUNT(*) FROM work_run_obligations
                     WHERE run_id = ?1 AND check_kind = ?2 {filter}"
                ),
                parameters,
                |row| row.get::<_, i64>(0),
            )?;
            usize::try_from(count)
                .map_err(|_| StoreError::InvalidWorkProjection("negative obligation count".into()))
        };
        let total = count("", &[&run, &check_kind])?;
        let (earlier, boundary_found) = if let Some((position, id)) = &boundary {
            let parameters: [&dyn rusqlite::ToSql; 4] = [&run, &check_kind, position, id];
            (
                count(
                    "AND (trigger_position < ?3 OR (trigger_position = ?3 AND obligation_id <= ?4))",
                    &parameters,
                )?,
                count(
                    "AND trigger_position = ?3 AND obligation_id = ?4",
                    &parameters,
                )? == 1,
            )
        } else {
            (0, true)
        };
        let mut statement = connection.prepare(&format!(
            "SELECT {OBLIGATION_COLUMNS} FROM work_run_obligations
             WHERE run_id = ?1 AND check_kind = ?2
               AND (?3 IS NULL OR trigger_position > ?3
                    OR (trigger_position = ?3 AND obligation_id > ?4))
             ORDER BY trigger_position, obligation_id
             LIMIT ?5"
        ))?;
        let page = statement
            .query_map(
                params![
                    run,
                    check_kind,
                    boundary.as_ref().map(|(position, _)| *position),
                    boundary.as_ref().map(|(_, id)| id.as_str()),
                    i64::try_from(limit).unwrap_or(i64::MAX),
                ],
                obligation_projection_row,
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let at_cut = VerificationAtCut::load_on(connection, evidence, evidence_id, cut.clone())?;
        let mut rows = Vec::new();
        for row in &page {
            let record = load_work_obligation_record_on(connection, row)?;
            let assessment = if record.obligation.trigger_position.position > cut.position {
                // Opened after the record: it did not exist to be matched or
                // held against the record's root.
                ObligationAssessment::Skipped(ObligationSkip::NotYetDefined)
            } else if record
                .resolution_position
                .as_ref()
                .is_some_and(|resolved| resolved.position <= cut.position)
            {
                ObligationAssessment::Skipped(ObligationSkip::AlreadyClosed)
            } else {
                at_cut.assess_on(connection, &record.obligation)?
            };
            let recorded = match record.resolution.as_ref().map(|event| &event.resolution) {
                None => RecordedObligationEnd::Open,
                Some(WorkObligationResolution::Satisfied { evidence, .. }) => {
                    if evidence == evidence_id {
                        RecordedObligationEnd::SatisfiedByThisRecord
                    } else {
                        RecordedObligationEnd::SatisfiedByAnotherRecord
                    }
                }
                Some(WorkObligationResolution::Waived { .. }) => RecordedObligationEnd::Waived,
                Some(WorkObligationResolution::Displaced { .. }) => {
                    RecordedObligationEnd::Displaced
                }
            };
            let obligation = record.obligation;
            rows.push(VerificationObligationAssessment {
                obligation_id: obligation.obligation_id,
                criterion: acceptance_binding_criterion(&obligation.rule),
                pinned: obligation.requirement.check_fingerprint.is_some(),
                check_kind: obligation.requirement.check_kind,
                trigger_position: obligation.trigger_position.position,
                rule: obligation.rule,
                assessment,
                recorded,
            });
        }
        Ok(Some(VerificationAssessment {
            run_id,
            record_position: record_position.position,
            cut_position: cut.position,
            head_position,
            total,
            earlier,
            boundary_found,
            rows,
        }))
    }
}
