//! Host-evaluated, core-enforced acceptance evaluations on the run feed.
//!
//! The evaluator records an immutable per-criterion verdict set; storage
//! validates structure and provenance, binds it to the exact work revision,
//! run, evaluated cut, and run evidence, and completion later consults the
//! newest record. No projection table exists: the run feed is the index.

use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};

use super::completion::feed_head;
use super::feeds::{
    append_to_work_feeds, inspect_work_request, load_typed_work_object, replay_operation,
    request_object,
};
use super::planning::{normalize_note_text, persist_operation_result};
use super::query::{load_work_claim_optional, load_work_item, load_work_run};
use super::{
    CanonicalObject, FeedPosition, ObjectHash, SCHEMA_VERSION, SessionId,
    WorkCompletionRecoveryCause, WorkId, WorkItem, WorkRunId,
};
use crate::domain::{
    AcceptanceBasis, AcceptanceEvaluation, AcceptanceEvaluationMode, AcceptanceEvaluationPolicy,
    AcceptanceResult, AcceptanceStaleReason, AcceptanceVerdict, AssuranceLevel, CompletionSeal,
    CriterionVerdict, CriterionVerdictInput, ExecutionObservation, FeedId,
    MAX_ACCEPTANCE_EVALUATION_BYTES, MAX_ACCEPTANCE_SOURCE_BASIS_BYTES,
    MAX_ACCEPTANCE_VERDICT_CITATIONS, MAX_EXECUTION_IDENTITY_BYTES, MechanicalBasis, ProjectId,
    RecordAcceptanceEvaluationRequest, VerificationEvidence, VerificationResult, WorkEvent,
    WorkEvidence, WorkLifecycle, WorkTransition,
};
use crate::memory::Redactor;
use crate::storage::{SqliteStore, StoreError};

/// Canonical object kind and run-feed entry kind of one evaluation.
pub(crate) const KIND: &str = "acceptance_evaluation";
const OPERATION: &str = "record_acceptance_evaluation";
const MAX_ATTEMPT_KEY_BYTES: usize = 256;
/// Feed entry kinds that describe host-observed workspace or check changes;
/// later notes, gates, observations, and evaluations never appear here.
const MUTATION_KINDS: &[&str] = &[
    "execution_observation",
    "verification_evidence",
    "environment_evidence",
    "work_obligation",
    "work_obligation_resolution",
];

/// Result of recording one acceptance evaluation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AcceptanceEvaluationReceipt {
    pub evaluation: ObjectHash,
    /// True when an identical attempt was already recorded.
    pub replayed: bool,
    pub record: AcceptanceEvaluation,
}

/// Newest evaluation on a run and whether completion may still consume it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AcceptanceEvaluationStatus {
    pub evaluation: ObjectHash,
    pub record: AcceptanceEvaluation,
    /// `None` when fresh; otherwise why completion treats it as absent.
    pub stale: Option<AcceptanceStaleReason>,
    /// True when the policy requires source freshness and this read could
    /// not measure a fingerprint: the recorded basis is checked against the
    /// fingerprint `done` presents, and this read does not call it stale.
    pub source_checked_at_done: bool,
}

/// What a completion would do with the newest evaluation right now.
#[derive(Debug)]
pub enum AcceptanceEvaluationReadiness {
    /// The policy is legacy: no evaluation is consulted.
    Legacy,
    /// A fresh, all-pass evaluation completion would consume; the seal will
    /// carry its citations.
    Ready(Box<AcceptanceEvaluation>),
    /// The typed recovery completion would raise.
    Blocked(WorkCompletionRecoveryCause),
}

/// How a completion-time source fingerprint enters a freshness check.
#[derive(Clone, Copy, Debug)]
pub(super) enum SourceCheck<'a> {
    /// A read: nothing has been measured for a completion, so a recorded
    /// basis is pending rather than stale.
    Unmeasured,
    /// A completion attempt presenting this fingerprint, if any.
    AtCompletion(Option<&'a str>),
}

/// The attempt key one request resolves to on a run, and the content
/// fingerprint a same-key resend is checked against. The service computes
/// the same identity before its preflight so the projected key is the
/// recorded one.
pub(crate) struct AttemptIdentity {
    pub key: String,
    pub fingerprint: ObjectHash,
}

/// Completion-side view of the newest evaluation.
#[derive(Clone, Debug)]
pub(super) enum AcceptanceEvaluationAssessment {
    Absent,
    Stale(AcceptanceStaleReason),
    Fresh {
        hash: ObjectHash,
        evaluation: Box<AcceptanceEvaluation>,
    },
}

/// Content-derived attempt identity: everything the evaluator decided, and
/// nothing that changes on an identical resend. The run is part of it so a
/// resend after a new run started is a new attempt, never a replay of the
/// old run's record.
#[derive(Serialize)]
struct AttemptFingerprint<'a> {
    schema_version: u16,
    project_id: &'a ProjectId,
    session_id: Option<&'a SessionId>,
    work_id: WorkId,
    run_id: WorkRunId,
    expected_work_revision: i64,
    evaluated_through: i64,
    mode: AcceptanceEvaluationMode,
    execution_identity: Option<&'a str>,
    parent_session: Option<&'a SessionId>,
    evaluator_model: Option<&'a crate::domain::EvaluatorModel>,
    source_basis: Option<&'a crate::domain::AcceptanceSourceBasis>,
    verdicts: &'a [CriterionVerdictInput],
}

enum Citation {
    VerificationPassed,
    VerificationOther,
    Environment,
    Gate { name: String, passed: bool },
    Note,
}

fn refused(work: WorkId, reason: impl Into<String>) -> StoreError {
    StoreError::AcceptanceEvaluationRefused {
        work,
        reason: reason.into(),
    }
}

impl SqliteStore {
    /// Records one immutable acceptance evaluation on the item's active run.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::AcceptanceEvaluationRefused`] for policy, mode,
    /// identity, criteria, or citation violations, and other [`StoreError`]
    /// values for stale revisions, closed work, or persistence failures.
    /// Nothing is appended on refusal.
    pub fn record_acceptance_evaluation<R: Redactor>(
        &mut self,
        request: &RecordAcceptanceEvaluationRequest,
        redactor: &R,
    ) -> Result<AcceptanceEvaluationReceipt, StoreError> {
        inspect_work_request(redactor, request, &request.evaluator)?;
        crate::storage::admit_live_actor_session(&request.evaluator)?;
        let evaluator_session = request
            .evaluator
            .session_id
            .clone()
            .ok_or_else(|| refused(request.work_id, "the evaluator must carry a session id"))?;
        validate_request_shape(request)?;
        let transaction = self.begin_work_mutation()?;
        let item = load_work_item(&transaction, request.work_id)?;
        if item.project_id != request.project_id {
            return Err(refused(item.work_id, "the item belongs to another project"));
        }
        // The attempt identity binds the run the item is on now or, for a
        // resend after that run ended, its latest run, so a replay answers
        // exactly the attempt it recorded.
        let attempt_run = match item.active_run_id {
            Some(run_id) => run_id,
            None => latest_run_id_on(&transaction, item.work_id)?
                .ok_or_else(|| refused(item.work_id, "the item has no active run to evaluate"))?,
        };
        let attempt = attempt_identity(request, attempt_run)?;
        if let Some(mut receipt) = replay_operation::<AcceptanceEvaluationReceipt>(
            &transaction,
            OPERATION,
            &attempt.key,
            &attempt.fingerprint,
        )? {
            transaction.commit()?;
            receipt.replayed = true;
            return Ok(receipt);
        }
        if item.lifecycle != WorkLifecycle::Open {
            return Err(StoreError::WorkNotOpen(item.work_id));
        }
        if item.revision != request.expected_work_revision {
            return Err(StoreError::WorkRevisionConflict {
                work: item.work_id,
                expected: request.expected_work_revision,
                current: item.revision,
            });
        }
        let run_id = item
            .active_run_id
            .ok_or_else(|| refused(item.work_id, "the item has no active run to evaluate"))?;
        let run = load_work_run(&transaction, run_id)?;
        let claim = load_work_claim_optional(&transaction, run_id)?;
        let policy = SqliteStore::load_acceptance_evaluation_policy_on(&transaction)?;
        admit_mode(&item, &policy, request.mode)?;
        let holder = claim.as_ref().map(|claim| claim.holder.clone());
        let history = run_holder_history(&transaction, run_id)?;
        admit_identity(
            &item,
            request,
            &evaluator_session,
            holder.as_ref(),
            run.executor.as_ref(),
            &history,
        )?;
        // The evaluator names the run-feed position it read through. Anything
        // the host observed after that point means the verdicts describe a
        // superseded state, so the record is refused rather than silently
        // bound to the current head.
        let head = feed_head(&transaction, &FeedId::RunExecution(run_id))?;
        let cut = request.evaluated_through;
        if cut < 0 || cut > head {
            return Err(refused(
                item.work_id,
                format!(
                    "evidence basis {cut} is not a position on this run's feed (head {head}); re-read show"
                ),
            ));
        }
        if mutation_after(&transaction, run_id, cut)? {
            return Err(refused(
                item.work_id,
                format!(
                    "the run changed after evidence basis {cut} (a host-observed mutation or check); re-read show and evaluate the current state"
                ),
            ));
        }
        let verdicts = bind_verdicts(&transaction, &item, run_id, &policy, cut, &request.verdicts)?;
        let evaluated_cut = FeedPosition {
            feed: FeedId::RunExecution(run_id),
            position: cut,
        };
        let evidence_basis = run_evidence_through(&transaction, run_id, cut)?;
        let work_revision_hash = CanonicalObject::freeze(&item)?.hash().clone();
        let record = AcceptanceEvaluation {
            schema_version: SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            root_id: item.root_id,
            work_id: item.work_id,
            run_id,
            work_revision: item.revision,
            work_revision_hash,
            criteria: item.acceptance.clone(),
            evaluated_cut,
            evidence_basis,
            source_basis: request.source_basis.clone(),
            mode: request.mode,
            evaluator: request.evaluator.clone(),
            execution_identity: request.execution_identity.clone(),
            parent_session: request.parent_session.clone(),
            evaluator_model: request.evaluator_model.clone(),
            verdicts,
            attempt_key: attempt.key.clone(),
            created_at: request.recorded_at,
        };
        let object = CanonicalObject::mint(&record)?;
        // The explicit cap is checked on the frozen bytes before any write:
        // a refused record leaves the feed and the newest record untouched.
        if object.bytes().len() > MAX_ACCEPTANCE_EVALUATION_BYTES {
            return Err(refused(
                item.work_id,
                format!(
                    "the evaluation would be {} canonical bytes, over the {MAX_ACCEPTANCE_EVALUATION_BYTES} byte cap; shorten the rationales",
                    object.bytes().len()
                ),
            ));
        }
        SqliteStore::insert_object(&transaction, KIND, &object)?;
        append_to_work_feeds(
            &transaction,
            &item.project_id,
            item.root_id,
            Some(run_id),
            None,
            KIND,
            &object,
        )?;
        let receipt = AcceptanceEvaluationReceipt {
            evaluation: object.hash().clone(),
            replayed: false,
            record,
        };
        persist_operation_result(
            &transaction,
            OPERATION,
            &attempt.key,
            &attempt.fingerprint,
            &receipt,
        )?;
        transaction.commit()?;
        Ok(receipt)
    }

    /// The newest evaluation on the item's active (or latest) run, with the
    /// freshness completion would apply now. `source_fingerprint` is the
    /// host-measured value a completion would present; a read that measured
    /// none leaves a recorded source basis pending rather than stale.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the item, run, policy, or record cannot be
    /// read.
    pub fn acceptance_evaluation_status(
        &self,
        work_id: WorkId,
        source_fingerprint: Option<&str>,
    ) -> Result<Option<AcceptanceEvaluationStatus>, StoreError> {
        let item = load_work_item(&self.connection, work_id)?;
        let run_id = match item.active_run_id {
            Some(run_id) => run_id,
            None => match self.latest_work_run(work_id)? {
                Some(run) => run.run_id,
                None => return Ok(None),
            },
        };
        let policy = SqliteStore::load_acceptance_evaluation_policy_on(&self.connection)?;
        let Some((hash, record)) = latest_on(&self.connection, run_id)? else {
            return Ok(None);
        };
        let source = source_fingerprint.map_or(SourceCheck::Unmeasured, |fingerprint| {
            SourceCheck::AtCompletion(Some(fingerprint))
        });
        let stale = staleness(&self.connection, &item, run_id, &policy, &record, source)?;
        Ok(Some(AcceptanceEvaluationStatus {
            evaluation: hash,
            source_checked_at_done: policy.require_source_freshness
                && record.source_basis.is_some()
                && matches!(source, SourceCheck::Unmeasured),
            record,
            stale,
        }))
    }

    /// What a completion would do with the newest evaluation right now, read
    /// without a write: the same assessment `complete_work` repeats inside
    /// its transaction. A caller can refuse before recording any capture,
    /// and learns the evaluation whose citations the seal will carry.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the item, policy, run, or record cannot be
    /// read consistently; that refusal is as strict as completion's own.
    pub(crate) fn acceptance_evaluation_readiness(
        &self,
        work_id: WorkId,
        run_id: WorkRunId,
        source_fingerprint: Option<&str>,
    ) -> Result<AcceptanceEvaluationReadiness, StoreError> {
        let policy = SqliteStore::load_acceptance_evaluation_policy_on(&self.connection)?;
        if policy.is_legacy() {
            return Ok(AcceptanceEvaluationReadiness::Legacy);
        }
        let item = load_work_item(&self.connection, work_id)?;
        Ok(
            match assess_on(&self.connection, &item, run_id, &policy, source_fingerprint)? {
                AcceptanceEvaluationAssessment::Absent => AcceptanceEvaluationReadiness::Blocked(
                    WorkCompletionRecoveryCause::MissingAcceptanceEvaluation {
                        criterion: item.acceptance.first().cloned().unwrap_or_default(),
                    },
                ),
                AcceptanceEvaluationAssessment::Stale(reason) => {
                    AcceptanceEvaluationReadiness::Blocked(
                        WorkCompletionRecoveryCause::AcceptanceEvaluationStale { reason },
                    )
                }
                AcceptanceEvaluationAssessment::Fresh { evaluation, .. } => {
                    match blocking_cause(&evaluation) {
                        Some(cause) => AcceptanceEvaluationReadiness::Blocked(cause),
                        None => AcceptanceEvaluationReadiness::Ready(evaluation),
                    }
                }
            },
        )
    }

    /// Whether `hash` is host-minted verification or environment evidence on
    /// `run_id`: typed evidence an evaluator cites by its full hash rather
    /// than by a note/gate locator.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the projection cannot be read.
    pub(crate) fn host_minted_run_evidence(
        &self,
        run_id: WorkRunId,
        hash: &ObjectHash,
    ) -> Result<bool, StoreError> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM work_run_evidence
                 WHERE run_id = ?1 AND evidence_hash = ?2
                   AND evidence_kind IN ('verification', 'environment')
             )",
            params![run_id.0.to_string(), hash.as_str()],
            |row| row.get(0),
        )?)
    }

    /// The committed receipt for an attempt identity, when one exists: the
    /// same lookup the record path performs, without a write, so a caller
    /// can recover an exact resend before admitting a fresh write.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::WorkOperationIdempotencyConflict`] when the
    /// explicit key exists with a different payload, or another
    /// [`StoreError`] when the receipt cannot be read.
    pub(crate) fn replay_acceptance_evaluation(
        &mut self,
        attempt: &AttemptIdentity,
    ) -> Result<Option<AcceptanceEvaluationReceipt>, StoreError> {
        let transaction = self.connection.transaction()?;
        let replayed = replay_operation::<AcceptanceEvaluationReceipt>(
            &transaction,
            OPERATION,
            &attempt.key,
            &attempt.fingerprint,
        )?;
        drop(transaction);
        Ok(replayed.map(|mut receipt| {
            receipt.replayed = true;
            receipt
        }))
    }

    /// The evaluation a completion seal binds, validated against the seal;
    /// `None` for a legacy seal.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::InvalidWorkProjection`] when the binding does
    /// not hold, or another [`StoreError`] when it cannot be read.
    pub(crate) fn completion_seal_evaluation(
        &self,
        seal: &CompletionSeal,
    ) -> Result<Option<AcceptanceEvaluation>, StoreError> {
        validate_completion_seal_acceptance_evaluation_on(&self.connection, seal)
    }
}

/// The attempt identity of one request on `run_id`. Explicit keys are scoped
/// to the work item and its run, so a host may reuse per-task counters
/// across items and restart them on a new run; keyless attempts derive
/// their identity from the content, which names the run too.
///
/// # Errors
///
/// Returns [`StoreError::AcceptanceEvaluationRefused`] for an empty or
/// oversized explicit key.
pub(crate) fn attempt_identity(
    request: &RecordAcceptanceEvaluationRequest,
    run_id: WorkRunId,
) -> Result<AttemptIdentity, StoreError> {
    let fingerprint = request_object(&AttemptFingerprint {
        schema_version: SCHEMA_VERSION,
        project_id: &request.project_id,
        session_id: request.evaluator.session_id.as_ref(),
        work_id: request.work_id,
        run_id,
        expected_work_revision: request.expected_work_revision,
        evaluated_through: request.evaluated_through,
        mode: request.mode,
        execution_identity: request.execution_identity.as_deref(),
        parent_session: request.parent_session.as_ref(),
        evaluator_model: request.evaluator_model.as_ref(),
        source_basis: request.source_basis.as_ref(),
        verdicts: &request.verdicts,
    })?;
    let key = match request.attempt_key.as_deref().map(str::trim) {
        Some(key) if key.is_empty() || key.len() > MAX_ATTEMPT_KEY_BYTES => {
            return Err(refused(
                request.work_id,
                format!(
                    "an explicit attempt key must contain from 1 through {MAX_ATTEMPT_KEY_BYTES} bytes"
                ),
            ));
        }
        Some(key) => format!("explicit:{}:{}:{key}", request.work_id.0, run_id.0),
        None => format!("content:{}", fingerprint.hash()),
    };
    Ok(AttemptIdentity {
        key,
        fingerprint: fingerprint.hash().clone(),
    })
}

/// The latest run of an item by generation, when any exists.
fn latest_run_id_on(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Option<WorkRunId>, StoreError> {
    let stored: Option<String> = connection
        .query_row(
            "SELECT run_id FROM work_runs WHERE work_id = ?1 ORDER BY generation DESC LIMIT 1",
            [work_id.0.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    stored
        .map(|value| {
            value.parse().map(WorkRunId).map_err(|_| {
                StoreError::InvalidWorkProjection(format!("work run id {value} is not a UUID"))
            })
        })
        .transpose()
}

/// The seal-to-evaluation binding every seal consumer checks: a bound
/// evaluation exists as a canonical object, names the sealed work and run,
/// sits on the run feed at or before the completion cut, passes everywhere,
/// and derives exactly the sealed acceptance vector. Legacy seals bind none.
///
/// # Errors
///
/// Returns [`StoreError::InvalidWorkProjection`] for a binding that does not
/// hold, or another [`StoreError`] when the object cannot be read.
pub(crate) fn validate_completion_seal_acceptance_evaluation_on(
    connection: &Connection,
    seal: &CompletionSeal,
) -> Result<Option<AcceptanceEvaluation>, StoreError> {
    let Some(hash) = &seal.acceptance_evaluation else {
        return Ok(None);
    };
    let invalid = |reason: &str| {
        StoreError::InvalidWorkProjection(format!(
            "completion seal acceptance evaluation {hash} {reason}"
        ))
    };
    let evaluation: AcceptanceEvaluation = load_typed_work_object(connection, hash, KIND)?;
    if evaluation.work_id != seal.work_id || evaluation.run_id != seal.run_id {
        return Err(invalid("is bound to another work item or run"));
    }
    match citation_position(connection, seal.run_id, hash)? {
        Some(position) if position <= seal.completion_cut.position => {}
        _ => {
            return Err(invalid(
                "is not on the sealed run feed at or before the completion cut",
            ));
        }
    }
    if evaluation.first_blocking().is_some() {
        return Err(invalid("does not pass every criterion"));
    }
    // Completion consumes the newest evaluation at its cut; a seal that binds
    // an older record while a newer one sits before the cut is forged, even
    // when the older record passes and derives the same vector.
    if newest_evaluation_through(connection, seal.run_id, seal.completion_cut.position)?.as_ref()
        != Some(hash)
    {
        return Err(invalid(
            "is not the newest evaluation on the run feed at the completion cut",
        ));
    }
    let assurance = seal
        .acceptance
        .first()
        .map_or(AssuranceLevel::Asserted, |result| result.assurance);
    if derive_acceptance_results(&evaluation, assurance) != seal.acceptance {
        return Err(invalid("does not derive the sealed acceptance vector"));
    }
    Ok(Some(evaluation))
}

fn validate_request_shape(request: &RecordAcceptanceEvaluationRequest) -> Result<(), StoreError> {
    let work = request.work_id;
    if let Some(model) = &request.evaluator_model
        && let Err(reason) = model.validate()
    {
        return Err(refused(work, format!("evaluator model: {reason}")));
    }
    if request.verdicts.is_empty() {
        return Err(refused(
            work,
            "an evaluation needs one verdict per criterion",
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for verdict in &request.verdicts {
        if verdict.criterion == 0 || !seen.insert(verdict.criterion) {
            return Err(refused(
                work,
                "verdict positions are one-based and each criterion appears exactly once",
            ));
        }
        if verdict.rationale.trim().is_empty() {
            return Err(refused(
                work,
                format!("criterion {} needs a rationale", verdict.criterion),
            ));
        }
        if verdict.evidence.len() > MAX_ACCEPTANCE_VERDICT_CITATIONS {
            return Err(refused(
                work,
                format!(
                    "criterion {} cites more than {MAX_ACCEPTANCE_VERDICT_CITATIONS} objects",
                    verdict.criterion
                ),
            ));
        }
        if verdict.verdict == AcceptanceVerdict::Pass {
            if verdict.evidence.is_empty() {
                return Err(refused(
                    work,
                    format!(
                        "criterion {} passes without a citation; a pass needs at least one relevant run evidence citation",
                        verdict.criterion
                    ),
                ));
            }
            if verdict.basis == AcceptanceBasis::HumanRequired {
                return Err(refused(
                    work,
                    format!(
                        "criterion {} cannot pass on a human_required basis",
                        verdict.criterion
                    ),
                ));
            }
        }
    }
    match request.mode {
        AcceptanceEvaluationMode::SubAgent => {
            let (Some(identity), Some(parent)) = (
                request
                    .execution_identity
                    .as_deref()
                    .filter(|identity| !identity.trim().is_empty()),
                request.parent_session.as_ref(),
            ) else {
                return Err(refused(
                    work,
                    "sub_agent mode needs a distinct execution identity and the attested parent session",
                ));
            };
            // Both are asserted identifiers stored in the immutable record:
            // bounded like every other identifier on it.
            if identity.len() > MAX_EXECUTION_IDENTITY_BYTES
                || identity.chars().any(char::is_control)
            {
                return Err(refused(
                    work,
                    format!(
                        "the execution identity must be at most {MAX_EXECUTION_IDENTITY_BYTES} bytes with no control characters"
                    ),
                ));
            }
            if let Err(error) = crate::storage::admit_session_id(parent) {
                return Err(refused(work, format!("parent session: {error}")));
            }
        }
        AcceptanceEvaluationMode::SameSession | AcceptanceEvaluationMode::IndependentSession => {
            if request.execution_identity.is_some() || request.parent_session.is_some() {
                return Err(refused(
                    work,
                    "execution identity and parent session belong to sub_agent mode only",
                ));
            }
        }
    }
    if let Some(basis) = &request.source_basis {
        if basis.fingerprint.trim().is_empty() {
            return Err(refused(work, "a source fingerprint must not be blank"));
        }
        if basis.fingerprint.len() > MAX_ACCEPTANCE_SOURCE_BASIS_BYTES
            || basis
                .workspace_id
                .as_ref()
                .is_some_and(|workspace| workspace.len() > MAX_ACCEPTANCE_SOURCE_BASIS_BYTES)
        {
            return Err(refused(
                work,
                format!(
                    "a source fingerprint or workspace id must not exceed {MAX_ACCEPTANCE_SOURCE_BASIS_BYTES} bytes"
                ),
            ));
        }
    }
    Ok(())
}

/// Every session that ever held the run, from its immutable history: claim,
/// renewal, recovery, and handoff transitions on the run feed. The mutable
/// claim row forgets former holders; this does not.
fn run_holder_history(
    connection: &Connection,
    run_id: WorkRunId,
) -> Result<Vec<SessionId>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT object_hash FROM work_feed_entries
         WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND object_kind = 'work_event'
         ORDER BY position",
    )?;
    let rows = statement
        .query_map(params![run_id.0.to_string()], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut holders = Vec::new();
    for stored in rows {
        let hash =
            ObjectHash::from_stored(stored.clone()).ok_or(StoreError::InvalidStoredHash(stored))?;
        let event: WorkEvent = load_typed_work_object(connection, &hash, "work_event")?;
        if event.run_id != Some(run_id) {
            continue;
        }
        if let Some(claim) = &event.claim {
            holders.push(claim.holder.clone());
        }
        match &event.transition {
            WorkTransition::Claimed { claim, .. } | WorkTransition::ClaimRenewed { claim } => {
                holders.push(claim.holder.clone());
            }
            WorkTransition::HandedOff { from, to, .. } => {
                holders.push(from.clone());
                holders.push(to.clone());
            }
            _ => {}
        }
    }
    holders.sort_by(|left, right| left.0.cmp(&right.0));
    holders.dedup();
    Ok(holders)
}

fn admit_mode(
    item: &WorkItem,
    policy: &AcceptanceEvaluationPolicy,
    mode: AcceptanceEvaluationMode,
) -> Result<(), StoreError> {
    if policy.is_legacy() {
        return Err(refused(
            item.work_id,
            "the project policy does not enable acceptance evaluation; completion stays self-asserted",
        ));
    }
    if !policy.allows(mode) {
        return Err(refused(
            item.work_id,
            format!(
                "mode {} is not allowed by the project policy; allowed: {}",
                mode.word(),
                policy
                    .allowed_modes
                    .iter()
                    .map(|mode| mode.word())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
        ));
    }
    if let Some(selected) = item.evaluation_mode
        && selected != mode
    {
        return Err(refused(
            item.work_id,
            format!(
                "this task selects mode {}; evaluate in that mode or revise the task",
                selected.word()
            ),
        ));
    }
    Ok(())
}

fn admit_identity(
    item: &WorkItem,
    request: &RecordAcceptanceEvaluationRequest,
    evaluator_session: &SessionId,
    holder: Option<&SessionId>,
    executor: Option<&SessionId>,
    history: &[SessionId],
) -> Result<(), StoreError> {
    let executing = |session: &SessionId| holder == Some(session) || executor == Some(session);
    match request.mode {
        AcceptanceEvaluationMode::SameSession => {
            if !executing(evaluator_session) {
                return Err(refused(
                    item.work_id,
                    "same_session evaluation must come from the session that holds or executes the run",
                ));
            }
        }
        AcceptanceEvaluationMode::SubAgent => {
            let parent = request.parent_session.as_ref().ok_or_else(|| {
                refused(
                    item.work_id,
                    "sub_agent mode needs the attested parent session",
                )
            })?;
            if !executing(parent) {
                return Err(refused(
                    item.work_id,
                    "sub_agent parent session must hold or execute the run",
                ));
            }
        }
        AcceptanceEvaluationMode::IndependentSession => {
            if executing(evaluator_session) || history.contains(evaluator_session) {
                return Err(refused(
                    item.work_id,
                    "independent_session evaluation must come from a session that neither holds nor executes the run, now or at any earlier point of this run",
                ));
            }
        }
    }
    Ok(())
}

fn bind_verdicts(
    connection: &Connection,
    item: &WorkItem,
    run_id: WorkRunId,
    policy: &AcceptanceEvaluationPolicy,
    cut: i64,
    inputs: &[CriterionVerdictInput],
) -> Result<Vec<CriterionVerdict>, StoreError> {
    let criteria = &item.acceptance;
    if inputs.len() != criteria.len()
        || inputs
            .iter()
            .any(|verdict| verdict.criterion > criteria.len())
    {
        return Err(refused(
            item.work_id,
            format!(
                "verdicts must cover exactly the {} current criteria by one-based position; re-read show",
                criteria.len()
            ),
        ));
    }
    let mut by_position: Vec<Option<&CriterionVerdictInput>> = vec![None; criteria.len()];
    for verdict in inputs {
        by_position[verdict.criterion - 1] = Some(verdict);
    }
    let mut bound = Vec::with_capacity(criteria.len());
    for (index, criterion) in criteria.iter().enumerate() {
        let input = by_position[index].ok_or_else(|| {
            refused(
                item.work_id,
                format!("criterion {} has no verdict", index + 1),
            )
        })?;
        let mut evidence = input.evidence.clone();
        evidence.sort();
        evidence.dedup();
        for hash in &evidence {
            let citation = classify_citation(connection, run_id, hash)?.ok_or_else(|| {
                refused(
                    item.work_id,
                    format!(
                        "criterion {} cites {hash}, which is not evidence on this run",
                        index + 1
                    ),
                )
            })?;
            match citation_position(connection, run_id, hash)? {
                Some(position) if position <= cut => {}
                _ => {
                    return Err(refused(
                        item.work_id,
                        format!(
                            "criterion {} cites {hash}, which lies beyond evidence basis {cut}; re-read show",
                            index + 1
                        ),
                    ));
                }
            }
            if input.verdict == AcceptanceVerdict::Pass {
                admit_pass_citation(item.work_id, index + 1, input.basis, policy, &citation)?;
            }
        }
        bound.push(CriterionVerdict {
            criterion: criterion.clone(),
            verdict: input.verdict,
            basis: input.basis,
            rationale: normalize_note_text(&input.rationale, "rationale")?,
            evidence,
        });
    }
    Ok(bound)
}

fn admit_pass_citation(
    work: WorkId,
    position: usize,
    basis: AcceptanceBasis,
    policy: &AcceptanceEvaluationPolicy,
    citation: &Citation,
) -> Result<(), StoreError> {
    match basis {
        AcceptanceBasis::Observed => match citation {
            Citation::VerificationPassed => Ok(()),
            _ => Err(refused(
                work,
                format!(
                    "criterion {position}: an observed pass requires host-minted verification evidence with a passed result"
                ),
            )),
        },
        AcceptanceBasis::Asserted => {
            if policy.mechanical_basis == MechanicalBasis::Observed {
                return Err(refused(
                    work,
                    format!(
                        "criterion {position}: the project policy requires observed check evidence for a mechanical pass; agent gate records are not observed builds"
                    ),
                ));
            }
            match citation {
                Citation::Gate { passed: true, .. } => Ok(()),
                _ => Err(refused(
                    work,
                    format!(
                        "criterion {position}: an asserted pass requires a gate record with no failure labels"
                    ),
                )),
            }
        }
        AcceptanceBasis::Judgment => Ok(()),
        AcceptanceBasis::HumanRequired => Err(refused(
            work,
            format!("criterion {position} cannot pass on a human_required basis"),
        )),
    }
}

fn classify_citation(
    connection: &Connection,
    run_id: WorkRunId,
    hash: &ObjectHash,
) -> Result<Option<Citation>, StoreError> {
    let row: Option<(String, Option<String>)> = connection
        .query_row(
            "SELECT evidence_kind, verification_result FROM work_run_evidence
             WHERE run_id = ?1 AND evidence_hash = ?2",
            params![run_id.0.to_string(), hash.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((kind, result)) = row else {
        return Ok(None);
    };
    Ok(Some(match kind.as_str() {
        "verification" => {
            // The canonical object decides; the projection column is a
            // rebuildable index that must carry exactly the canonical result
            // word, so a missing value or any other variant refuses.
            let evidence: VerificationEvidence =
                load_typed_work_object(connection, hash, "verification_evidence")?;
            let expected = super::planning::encode_state(evidence.result)?;
            if result.as_deref() != Some(expected.as_str()) {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "verification evidence {hash} projection result disagrees with its canonical record"
                )));
            }
            if evidence.result == VerificationResult::Passed {
                Citation::VerificationPassed
            } else {
                Citation::VerificationOther
            }
        }
        "environment" => Citation::Environment,
        _ => {
            let evidence: WorkEvidence = load_typed_work_object(connection, hash, "work_evidence")?;
            match evidence.gate {
                Some(gate) => Citation::Gate {
                    passed: gate.failed.is_empty(),
                    name: gate.name,
                },
                None => Citation::Note,
            }
        }
    }))
}

/// The run-feed position of one cited object, when it is on this run.
fn citation_position(
    connection: &Connection,
    run_id: WorkRunId,
    hash: &ObjectHash,
) -> Result<Option<i64>, StoreError> {
    Ok(connection
        .query_row(
            "SELECT position FROM work_feed_entries
             WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND object_hash = ?2
             ORDER BY position LIMIT 1",
            params![run_id.0.to_string(), hash.as_str()],
            |row| row.get(0),
        )
        .optional()?)
}

/// Every evidence object on the run feed at or before `cut`, in feed order:
/// the selection the evaluator could have read.
fn run_evidence_through(
    connection: &Connection,
    run_id: WorkRunId,
    cut: i64,
) -> Result<Vec<ObjectHash>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT object_hash FROM work_feed_entries
         WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND position <= ?2
           AND object_kind IN ('work_evidence', 'verification_evidence', 'environment_evidence')
         ORDER BY position",
    )?;
    let rows = statement
        .query_map(params![run_id.0.to_string(), cut], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter()
        .map(|stored| {
            ObjectHash::from_stored(stored.clone()).ok_or(StoreError::InvalidStoredHash(stored))
        })
        .collect()
}

/// Gate names the passing verdicts rely on.
fn cited_gate_names(
    connection: &Connection,
    run_id: WorkRunId,
    record: &AcceptanceEvaluation,
) -> Result<Vec<String>, StoreError> {
    let mut names = Vec::new();
    for verdict in record
        .verdicts
        .iter()
        .filter(|verdict| verdict.verdict == AcceptanceVerdict::Pass)
    {
        for hash in &verdict.evidence {
            if let Some(Citation::Gate { name, .. }) = classify_citation(connection, run_id, hash)?
            {
                names.push(name);
            }
        }
    }
    names.sort();
    names.dedup();
    Ok(names)
}

/// Whether a relied-on gate has a newer record after the evaluated cut. A
/// newer record replaces the one the verdict cited whatever its result: the
/// evaluator cannot freeze an older observation of the same check.
fn gate_superseded_after(
    connection: &Connection,
    run_id: WorkRunId,
    position: i64,
    names: &[String],
) -> Result<bool, StoreError> {
    if names.is_empty() {
        return Ok(false);
    }
    let mut statement = connection.prepare(
        "SELECT object_hash FROM work_feed_entries
         WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND position > ?2
           AND object_kind = 'work_evidence'
         ORDER BY position",
    )?;
    let rows = statement
        .query_map(params![run_id.0.to_string(), position], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for stored in rows {
        let hash =
            ObjectHash::from_stored(stored.clone()).ok_or(StoreError::InvalidStoredHash(stored))?;
        let evidence: WorkEvidence = load_typed_work_object(connection, &hash, "work_evidence")?;
        if evidence.gate.is_some_and(|gate| names.contains(&gate.name)) {
            return Ok(true);
        }
    }
    Ok(false)
}

/// Newest evaluation entry on the run execution feed.
pub(super) fn latest_on(
    connection: &Connection,
    run_id: WorkRunId,
) -> Result<Option<(ObjectHash, AcceptanceEvaluation)>, StoreError> {
    let stored: Option<String> = connection
        .query_row(
            "SELECT object_hash FROM work_feed_entries
             WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND object_kind = ?2
             ORDER BY position DESC LIMIT 1",
            params![run_id.0.to_string(), KIND],
            |row| row.get(0),
        )
        .optional()?;
    let Some(stored) = stored else {
        return Ok(None);
    };
    let hash =
        ObjectHash::from_stored(stored.clone()).ok_or(StoreError::InvalidStoredHash(stored))?;
    let record: AcceptanceEvaluation = load_typed_work_object(connection, &hash, KIND)?;
    if record.run_id != run_id {
        return Err(StoreError::InvalidWorkProjection(
            "acceptance evaluation is bound to another run".into(),
        ));
    }
    Ok(Some((hash, record)))
}

/// The newest evaluation entry on the run feed at or before `cut`: the one a
/// seal binding that cut must name. Entries after the cut are excluded.
pub(super) fn newest_evaluation_through(
    connection: &Connection,
    run_id: WorkRunId,
    cut: i64,
) -> Result<Option<ObjectHash>, StoreError> {
    let stored: Option<String> = connection
        .query_row(
            "SELECT object_hash FROM work_feed_entries
             WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND object_kind = ?2
               AND position <= ?3
             ORDER BY position DESC LIMIT 1",
            params![run_id.0.to_string(), KIND, cut],
            |row| row.get(0),
        )
        .optional()?;
    stored
        .map(|stored| {
            ObjectHash::from_stored(stored.clone()).ok_or(StoreError::InvalidStoredHash(stored))
        })
        .transpose()
}

/// Whether a host-observed mutation followed the evaluated cut.
fn mutation_after(
    connection: &Connection,
    run_id: WorkRunId,
    position: i64,
) -> Result<bool, StoreError> {
    let mut statement = connection.prepare(
        "SELECT object_kind, object_hash FROM work_feed_entries
         WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND position > ?2
         ORDER BY position",
    )?;
    let rows = statement
        .query_map(params![run_id.0.to_string(), position], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (kind, stored) in rows {
        if !MUTATION_KINDS.contains(&kind.as_str()) {
            continue;
        }
        if kind == "execution_observation" {
            let hash = ObjectHash::from_stored(stored.clone())
                .ok_or(StoreError::InvalidStoredHash(stored))?;
            let observation: ExecutionObservation =
                load_typed_work_object(connection, &hash, "execution_observation")?;
            if !observation.source_changed {
                continue;
            }
        }
        return Ok(true);
    }
    Ok(false)
}

fn staleness(
    connection: &Connection,
    item: &WorkItem,
    run_id: WorkRunId,
    policy: &AcceptanceEvaluationPolicy,
    record: &AcceptanceEvaluation,
    source: SourceCheck<'_>,
) -> Result<Option<AcceptanceStaleReason>, StoreError> {
    if record.run_id != run_id {
        return Ok(Some(AcceptanceStaleReason::Run));
    }
    if record.work_revision != item.revision
        || record.work_revision_hash != *CanonicalObject::freeze(item)?.hash()
        || record.criteria != item.acceptance
    {
        return Ok(Some(AcceptanceStaleReason::Revision));
    }
    // Every effective requirement is re-read from the current policy: a
    // strengthened mechanical basis retires asserted passes, and a pinned or
    // disallowed mode retires the whole record.
    if !policy.allows(record.mode)
        || item
            .evaluation_mode
            .is_some_and(|selected| selected != record.mode)
        || (policy.mechanical_basis == MechanicalBasis::Observed
            && record.verdicts.iter().any(|verdict| {
                verdict.verdict == AcceptanceVerdict::Pass
                    && verdict.basis == AcceptanceBasis::Asserted
            }))
    {
        return Ok(Some(AcceptanceStaleReason::Policy));
    }
    // Independence is a relationship to the run, rechecked when the record
    // is consumed: an evaluator that has since taken the run by handoff or
    // recovery would otherwise consume its own earlier judgment.
    if record.mode == AcceptanceEvaluationMode::IndependentSession {
        let claim = load_work_claim_optional(connection, run_id)?;
        let run = load_work_run(connection, run_id)?;
        let history = run_holder_history(connection, run_id)?;
        let independent = record.evaluator.session_id.as_ref().is_some_and(|session| {
            claim.as_ref().is_none_or(|claim| claim.holder != *session)
                && run.executor.as_ref() != Some(session)
                && !history.contains(session)
        });
        if !independent {
            return Ok(Some(AcceptanceStaleReason::Identity));
        }
    }
    if mutation_after(connection, run_id, record.evaluated_cut.position)? {
        return Ok(Some(AcceptanceStaleReason::Mutation));
    }
    let relied_on = cited_gate_names(connection, run_id, record)?;
    if gate_superseded_after(
        connection,
        run_id,
        record.evaluated_cut.position,
        &relied_on,
    )? {
        return Ok(Some(AcceptanceStaleReason::Evidence));
    }
    if policy.require_source_freshness {
        let evaluated = record
            .source_basis
            .as_ref()
            .map(|basis| basis.fingerprint.as_str());
        match (evaluated, source) {
            // A record without a basis can never match a measurement.
            (None, _) => return Ok(Some(AcceptanceStaleReason::Source)),
            // A read measured nothing: the check is pending, not failed.
            (Some(_), SourceCheck::Unmeasured) => {}
            (Some(evaluated), SourceCheck::AtCompletion(presented))
                if presented == Some(evaluated) => {}
            (Some(_), SourceCheck::AtCompletion(_)) => {
                return Ok(Some(AcceptanceStaleReason::Source));
            }
        }
    }
    Ok(None)
}

/// Completion-side assessment of the newest evaluation on `run_id`.
pub(super) fn assess_on(
    connection: &Connection,
    item: &WorkItem,
    run_id: WorkRunId,
    policy: &AcceptanceEvaluationPolicy,
    source_fingerprint: Option<&str>,
) -> Result<AcceptanceEvaluationAssessment, StoreError> {
    let Some((hash, record)) = latest_on(connection, run_id)? else {
        return Ok(AcceptanceEvaluationAssessment::Absent);
    };
    Ok(
        match staleness(
            connection,
            item,
            run_id,
            policy,
            &record,
            SourceCheck::AtCompletion(source_fingerprint),
        )? {
            Some(reason) => AcceptanceEvaluationAssessment::Stale(reason),
            None => AcceptanceEvaluationAssessment::Fresh {
                hash,
                evaluation: Box::new(record),
            },
        },
    )
}

/// The sealed acceptance vector derived from a passing evaluation.
pub(super) fn derive_acceptance_results(
    evaluation: &AcceptanceEvaluation,
    assurance: AssuranceLevel,
) -> Vec<AcceptanceResult> {
    evaluation
        .verdicts
        .iter()
        .map(|verdict| AcceptanceResult {
            criterion: verdict.criterion.clone(),
            satisfied: verdict.verdict == AcceptanceVerdict::Pass,
            evidence: verdict.evidence.clone(),
            assurance,
            note: format!(
                "{} ({}) {}",
                verdict.verdict.word(),
                verdict.basis.word(),
                verdict.rationale
            ),
        })
        .collect()
}

/// The recovery cause for a fresh evaluation that does not pass everywhere.
pub(super) fn blocking_cause(
    evaluation: &AcceptanceEvaluation,
) -> Option<WorkCompletionRecoveryCause> {
    let blocking = evaluation.first_blocking()?;
    let criterion = blocking.criterion.clone();
    Some(match blocking.verdict {
        AcceptanceVerdict::Fail => WorkCompletionRecoveryCause::AcceptanceFailed { criterion },
        AcceptanceVerdict::InsufficientEvidence => {
            WorkCompletionRecoveryCause::AcceptanceInsufficientEvidence { criterion }
        }
        AcceptanceVerdict::NeedsHuman => {
            WorkCompletionRecoveryCause::AcceptanceNeedsHuman { criterion }
        }
        AcceptanceVerdict::Pass => return None,
    })
}

#[cfg(test)]
mod tests;
