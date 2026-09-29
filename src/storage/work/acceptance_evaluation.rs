//! Host-evaluated, core-enforced acceptance evaluations on the run feed.
//!
//! The evaluator records an immutable per-criterion verdict set; storage
//! validates structure and provenance, binds it to the exact work revision,
//! run, evaluated cut, and run evidence, and completion later consults the
//! newest record. No projection table exists: the run feed is the index.

use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize};

use super::completion::feed_head;
use super::feeds::{
    append_to_work_feeds, inspect_work_request, latest_named_root_binding_on,
    latest_named_root_sighting_on, load_typed_work_object, replay_operation, request_object,
};
use super::planning::{normalize_note_text, persist_operation_result};
use super::query::{load_work_claim_optional, load_work_item, load_work_run, on_one_snapshot};
use super::{
    CanonicalObject, FeedPosition, ObjectId, SCHEMA_VERSION, SessionId,
    WorkCompletionRecoveryCause, WorkId, WorkItem, WorkRunId,
};
use crate::domain::{
    AcceptanceBasis, AcceptanceBinding, AcceptanceEvaluation, AcceptanceEvaluationMode,
    AcceptanceEvaluationPolicy, AcceptanceResult, AcceptanceStaleReason, AcceptanceVerdict,
    AssuranceLevel, CarriedFailure, CarriedFailureReviser, CarriedFailureVerdict, CompletionSeal,
    CriterionVerdict, CriterionVerdictInput, ExecutionObservation, ExecutionSourceBasis, FeedId,
    MAX_ACCEPTANCE_EVALUATION_BYTES, MAX_ACCEPTANCE_SOURCE_BASIS_BYTES,
    MAX_ACCEPTANCE_VERDICT_CITATIONS, MAX_EXECUTION_IDENTITY_BYTES, MechanicalBasis,
    NamedRootBindingEvent, NamedRootBindingKind, ProjectId, RecordAcceptanceEvaluationRequest,
    SourceRootState, VerificationEvidence, VerificationResult, WorkEvent, WorkEvidence,
    WorkLifecycle, WorkObligation, WorkPlanningAuthority, WorkTransition,
};
use crate::memory::Redactor;
use crate::storage::{CarriedFailureRefusal, EvaluationBasisMove, SqliteStore, StoreError};

/// Canonical object kind and run-feed entry kind of one evaluation.
pub(crate) const KIND: &str = "acceptance_evaluation";
const OPERATION: &str = "record_acceptance_evaluation";
const MAX_ATTEMPT_KEY_BYTES: usize = 256;

struct NamedEvaluationRoot {
    position: i64,
    event_id: ObjectId,
    event: NamedRootBindingEvent,
}

fn named_root_at_on(
    connection: &Connection,
    run_id: WorkRunId,
    through: i64,
) -> Result<Option<NamedEvaluationRoot>, StoreError> {
    let Some(claim) = load_work_claim_optional(connection, run_id)? else {
        return Ok(None);
    };
    let Some((position, event_id, event)) =
        latest_named_root_binding_on(connection, run_id, claim.claim_id, through)?
    else {
        return Ok(None);
    };
    Ok(
        (event.kind == NamedRootBindingKind::Bound).then_some(NamedEvaluationRoot {
            position: position.position,
            event_id,
            event,
        }),
    )
}
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
    pub evaluation: ObjectId,
    /// True when an identical attempt was already recorded.
    pub replayed: bool,
    pub record: AcceptanceEvaluation,
}

/// Newest evaluation on a run and whether completion may still consume it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AcceptanceEvaluationStatus {
    pub evaluation: ObjectId,
    pub record: AcceptanceEvaluation,
    /// `None` when fresh; otherwise why completion treats it as absent.
    pub stale: Option<AcceptanceStaleReason>,
    /// True when the policy requires source freshness and this read could
    /// not measure a fingerprint: the recorded basis is checked against the
    /// fingerprint `done` presents, and this read does not call it stale.
    pub source_checked_at_done: bool,
    /// The failing evaluation whose criteria were revised on this run, which
    /// the next evaluation must see and, after the executor's revision, name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub carried_failure: Option<CarriedFailure>,
}

/// What a completion would do with the newest evaluation right now.
#[derive(Debug)]
pub enum AcceptanceEvaluationReadiness {
    /// The policy is self-asserted: no evaluation is consulted.
    SelfAsserted,
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
    pub fingerprint: ObjectId,
}

/// Completion-side view of the newest evaluation.
#[derive(Clone, Debug)]
pub(super) enum AcceptanceEvaluationAssessment {
    Absent,
    Stale(AcceptanceStaleReason),
    Fresh {
        hash: ObjectId,
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
    /// Left out when absent, so every attempt recorded before the field
    /// existed keeps its fingerprint.
    #[serde(skip_serializing_if = "Option::is_none")]
    supersedes: Option<&'a ObjectId>,
}

enum Citation {
    VerificationPassed {
        kind: crate::domain::VerificationKind,
        check_fingerprint: ObjectId,
        /// The source the check ran on.
        source_basis: ExecutionSourceBasis,
        /// The execution observation that ran the check.
        producer: ObjectId,
    },
    VerificationOther,
    Environment,
    Gate {
        name: String,
        passed: bool,
    },
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
    /// identity, criteria, or citation violations;
    /// [`StoreError::AcceptanceEvaluationCarriedFailure`] when `supersedes`
    /// does not answer the failure carried on the run
    /// ([`CarriedFailureRefusal::Unacknowledged`], naming the failed record,
    /// when it is missing after the executor's revision or names another
    /// record; [`CarriedFailureRefusal::SelfAcknowledged`], naming it too,
    /// when an executor of the run names the failure its executor's revision
    /// carried; [`CarriedFailureRefusal::NothingToSupersede`] when nothing is
    /// carried); and other [`StoreError`] values for stale
    /// revisions, closed work, damaged projections, or persistence failures.
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
        let carried = carried_failure_on(&transaction, &item, run_id)?;
        admit_supersedes(
            &item,
            carried.as_ref(),
            request.supersedes.as_ref(),
            &EvaluatorStanding {
                mode: request.mode,
                session: &evaluator_session,
                holder: holder.as_ref(),
                executor: run.executor.as_ref(),
                history: &history,
            },
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
        let named_root = named_root_at_on(&transaction, run_id, cut)?;
        if let (Some(root), Some(workspace)) = (
            named_root.as_ref(),
            request
                .source_basis
                .as_ref()
                .and_then(|basis| basis.workspace_id.as_ref()),
        ) && workspace != &root.event.workspace_id
        {
            return Err(refused(
                item.work_id,
                "the evaluation declares a workspace other than the claim's named source root",
            ));
        }
        if named_root_at_on(&transaction, run_id, head)?
            .as_ref()
            .map(|root| &root.event_id)
            != named_root.as_ref().map(|root| &root.event_id)
        {
            return Err(StoreError::AcceptanceEvaluationBasisMoved {
                work: item.work_id,
                moved: EvaluationBasisMove::SourceChanged,
                reason: "the named source root changed after the evaluated cut; re-read the run and evaluate its current root".into(),
            });
        }
        if let Some(moved) = basis_moved_after(
            &transaction,
            run_id,
            cut,
            request.source_basis.as_ref(),
            named_root.as_ref(),
        )? {
            return Err(StoreError::AcceptanceEvaluationBasisMoved {
                work: item.work_id,
                moved,
                reason: match moved {
                    EvaluationBasisMove::CheckRecorded => format!(
                        "a host check was recorded after evidence basis {cut}; {}",
                        moved.remedy()
                    ),
                    EvaluationBasisMove::SourceChanged => format!(
                        "the source changed after evidence basis {cut} and this evaluation did not judge that revision; {}",
                        moved.remedy()
                    ),
                },
            });
        }
        let verdicts = bind_verdicts(&transaction, &item, run_id, &policy, cut, &request.verdicts)?;
        let judged = judged_source(
            &transaction,
            run_id,
            cut,
            request.source_basis.as_ref(),
            named_root.as_ref(),
        )?;
        require_named_root_judged_source(
            &transaction,
            run_id,
            cut,
            named_root.as_ref(),
            judged.as_ref(),
            item.work_id,
        )?;
        if let Some(stale) = stale_bound_citation(
            &transaction,
            &item,
            run_id,
            judged.as_ref(),
            cut,
            passing_citations(&verdicts),
            named_root.as_ref(),
        )? {
            return Err(refused(
                item.work_id,
                stale.refusal(&item, judged.as_ref(), cut)?,
            ));
        }
        let receipt = append_evaluation(&transaction, &item, run_id, request, &attempt, verdicts)?;
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
        // The item, the policy and the newest record are compared, so they
        // come from one commit.
        on_one_snapshot(&self.connection, |connection| {
            let item = load_work_item(connection, work_id)?;
            let run_id = match item.active_run_id {
                Some(run_id) => run_id,
                // Same connection, so the same open snapshot.
                None => match self.latest_work_run(work_id)? {
                    Some(run) => run.run_id,
                    None => return Ok(None),
                },
            };
            let policy = SqliteStore::load_acceptance_evaluation_policy_on(connection)?;
            let Some((hash, record)) = latest_on(connection, run_id)? else {
                return Ok(None);
            };
            let source = source_fingerprint.map_or(SourceCheck::Unmeasured, |fingerprint| {
                SourceCheck::AtCompletion(Some(fingerprint))
            });
            let stale = staleness(connection, &item, run_id, &policy, &record, source)?;
            let carried_failure = carried_failure_on(connection, &item, run_id)?;
            Ok(Some(AcceptanceEvaluationStatus {
                carried_failure,
                evaluation: hash,
                source_checked_at_done: policy.require_source_freshness
                    && record.source_basis.is_some()
                    && matches!(source, SourceCheck::Unmeasured),
                record,
                stale,
            }))
        })
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
        // The policy, the item and the newest record are compared, so they
        // come from one commit; a concurrent revision and re-evaluation must
        // not make the pre-check refuse a store that was consistent.
        on_one_snapshot(&self.connection, |connection| {
            Self::acceptance_evaluation_readiness_on(
                connection,
                work_id,
                run_id,
                source_fingerprint,
            )
        })
    }

    fn acceptance_evaluation_readiness_on(
        connection: &Connection,
        work_id: WorkId,
        run_id: WorkRunId,
        source_fingerprint: Option<&str>,
    ) -> Result<AcceptanceEvaluationReadiness, StoreError> {
        let policy = SqliteStore::load_acceptance_evaluation_policy_on(connection)?;
        if policy.is_self_asserted() {
            return Ok(AcceptanceEvaluationReadiness::SelfAsserted);
        }
        let item = load_work_item(connection, work_id)?;
        Ok(
            match assess_on(connection, &item, run_id, &policy, source_fingerprint)? {
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

    /// Whether `evidence_id` is host-minted verification or environment
    /// evidence on `run_id`: typed evidence an evaluator cites by its full
    /// record id rather than by a note/gate locator.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the projection cannot be read.
    pub(crate) fn host_minted_run_evidence(
        &self,
        run_id: WorkRunId,
        evidence_id: &ObjectId,
    ) -> Result<bool, StoreError> {
        Ok(self.connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM work_run_evidence
                 WHERE run_id = ?1 AND evidence_id = ?2
                   AND evidence_kind IN ('verification', 'environment')
             )",
            params![run_id.0.to_string(), evidence_id.as_str()],
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
    /// `None` for a self-asserted seal.
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
        supersedes: request.supersedes.as_ref(),
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
        None => format!("content:{}", fingerprint.key()),
    };
    Ok(AttemptIdentity {
        key,
        fingerprint: fingerprint.key().clone(),
    })
}

/// Freezes and appends one admitted evaluation of `item` on `run_id`, with
/// `verdicts` bound at `request.evaluated_through`, and persists the receipt
/// an exact resend replays. Every admission check has already passed; only
/// the byte cap is checked here, on the frozen bytes before any write, so a
/// refused record leaves the feed and the newest record untouched.
fn append_evaluation(
    transaction: &Transaction<'_>,
    item: &WorkItem,
    run_id: WorkRunId,
    request: &RecordAcceptanceEvaluationRequest,
    attempt: &AttemptIdentity,
    verdicts: Vec<CriterionVerdict>,
) -> Result<AcceptanceEvaluationReceipt, StoreError> {
    let cut = request.evaluated_through;
    let record = AcceptanceEvaluation {
        schema_version: SCHEMA_VERSION,
        project_id: item.project_id.clone(),
        root_id: item.root_id,
        work_id: item.work_id,
        run_id,
        work_revision: item.revision,
        work_revision_hash: CanonicalObject::freeze(item)?.key().clone(),
        criteria: item.acceptance.clone(),
        evaluated_cut: FeedPosition {
            feed: FeedId::RunExecution(run_id),
            position: cut,
        },
        evidence_basis: run_evidence_through(transaction, run_id, cut)?,
        source_basis: request.source_basis.clone(),
        named_root_binding: named_root_at_on(transaction, run_id, cut)?.map(|root| root.event_id),
        mode: request.mode,
        evaluator: request.evaluator.clone(),
        execution_identity: request.execution_identity.clone(),
        parent_session: request.parent_session.clone(),
        evaluator_model: request.evaluator_model.clone(),
        verdicts,
        attempt_key: attempt.key.clone(),
        created_at: request.recorded_at,
        supersedes: request.supersedes.clone(),
    };
    let object = CanonicalObject::mint(&record)?;
    if object.bytes().len() > MAX_ACCEPTANCE_EVALUATION_BYTES {
        return Err(refused(
            item.work_id,
            format!(
                "the evaluation would be {} canonical bytes, over the {MAX_ACCEPTANCE_EVALUATION_BYTES} byte cap; shorten the rationales",
                object.bytes().len()
            ),
        ));
    }
    SqliteStore::insert_object(transaction, KIND, &object)?;
    append_to_work_feeds(
        transaction,
        &item.project_id,
        item.root_id,
        Some(run_id),
        None,
        KIND,
        &object,
    )?;
    let receipt = AcceptanceEvaluationReceipt {
        evaluation: object.key().clone(),
        replayed: false,
        record,
    };
    persist_operation_result(
        transaction,
        OPERATION,
        &attempt.key,
        &attempt.fingerprint,
        &receipt,
    )?;
    Ok(receipt)
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
/// and derives exactly the sealed acceptance vector. Self-asserted seals bind none.
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
        "SELECT object_id FROM work_feed_entries
         WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND object_kind = 'work_event'
         ORDER BY position",
    )?;
    let rows = statement
        .query_map(params![run_id.0.to_string()], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut holders = Vec::new();
    for stored in rows {
        let hash =
            ObjectId::from_stored(stored.clone()).ok_or(StoreError::InvalidStoredKey(stored))?;
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
    if policy.is_self_asserted() {
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
        let binding = item
            .acceptance_bindings
            .iter()
            .find(|binding| binding.criterion == index + 1);
        if let Some(binding) = binding
            && input.verdict == AcceptanceVerdict::Pass
            && input.basis != AcceptanceBasis::Observed
        {
            return Err(refused(
                item.work_id,
                format!(
                    "criterion {} is bound to {} verification: a pass needs an observed basis citing host-minted verification evidence of that kind with a passed result, never judgment or an asserted gate",
                    index + 1,
                    super::planning::encode_state(binding.requirement.check_kind)?
                ),
            ));
        }
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
                if let Some(binding) = binding {
                    let matches = matches!(&citation, Citation::VerificationPassed { kind, check_fingerprint, .. }
                        if *kind == binding.requirement.check_kind
                            && binding
                                .requirement
                                .check_fingerprint
                                .as_ref()
                                .is_none_or(|required| required == check_fingerprint));
                    if !matches {
                        return Err(refused(
                            item.work_id,
                            format!(
                                "criterion {} is bound to {} verification; {hash} is not passed host-minted verification evidence of that kind",
                                index + 1,
                                super::planning::encode_state(binding.requirement.check_kind)?
                            ),
                        ));
                    }
                }
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
            Citation::VerificationPassed { .. } => Ok(()),
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
    hash: &ObjectId,
) -> Result<Option<Citation>, StoreError> {
    let row: Option<(String, Option<String>)> = connection
        .query_row(
            "SELECT evidence_kind, verification_result FROM work_run_evidence
             WHERE run_id = ?1 AND evidence_id = ?2",
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
                Citation::VerificationPassed {
                    kind: evidence.check_kind,
                    check_fingerprint: evidence.check_fingerprint,
                    source_basis: evidence.source_basis,
                    producer: evidence.producer_observation,
                }
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
    hash: &ObjectId,
) -> Result<Option<i64>, StoreError> {
    Ok(connection
        .query_row(
            "SELECT position FROM work_feed_entries
             WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND object_id = ?2
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
) -> Result<Vec<ObjectId>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT object_id FROM work_feed_entries
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
            ObjectId::from_stored(stored.clone()).ok_or(StoreError::InvalidStoredKey(stored))
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
        "SELECT object_id FROM work_feed_entries
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
            ObjectId::from_stored(stored.clone()).ok_or(StoreError::InvalidStoredKey(stored))?;
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
) -> Result<Option<(ObjectId, AcceptanceEvaluation)>, StoreError> {
    let stored: Option<String> = connection
        .query_row(
            "SELECT object_id FROM work_feed_entries
             WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND object_kind = ?2
             ORDER BY position DESC LIMIT 1",
            params![run_id.0.to_string(), KIND],
            |row| row.get(0),
        )
        .optional()?;
    let Some(stored) = stored else {
        return Ok(None);
    };
    let hash = ObjectId::from_stored(stored.clone()).ok_or(StoreError::InvalidStoredKey(stored))?;
    let record: AcceptanceEvaluation = load_typed_work_object(connection, &hash, KIND)?;
    if record.run_id != run_id {
        return Err(StoreError::InvalidWorkProjection(
            "acceptance evaluation is bound to another run".into(),
        ));
    }
    Ok(Some((hash, record)))
}

/// The failure carried on `run_id`, when the newest evaluation on the run does
/// not pass everywhere, in one of two cases:
/// - It names a failure it superseded. That failure, the root of the naming
///   chain, stays carried whatever the item's criteria are now: naming a
///   failure with a verdict that does not pass accepts no revision, so only a
///   passing evaluation that names it ends the carry.
/// - It names nothing. It is itself carried when a revision after it changed
///   the criteria it judged or their verification bindings, and the item's
///   criteria and bindings still differ from them. Both are followed through
///   each revision's snapshot, so a revision of other fields carries nothing
///   and a revision back to the judged contract ends the carry.
///
/// Its other staleness reasons do not matter: a check or source change after
/// the failure is the executor's ordinary next step, not a reason to forget
/// it. The reviser is the executor when any criteria-changing revision since
/// the carried failure was made under the run's claim, or by its executor or
/// a session that holds or held the run.
/// The carry belongs to the item's active run: once completion, disposal or
/// detachment ends that run, nothing is carried.
pub(super) fn carried_failure_on(
    connection: &Connection,
    item: &WorkItem,
    run_id: WorkRunId,
) -> Result<Option<CarriedFailure>, StoreError> {
    if item.active_run_id != Some(run_id) {
        return Ok(None);
    }
    let Some((newest, newest_record)) = latest_on(connection, run_id)? else {
        return Ok(None);
    };
    if newest_record.first_blocking().is_none() {
        return Ok(None);
    }
    let newest_position = citation_position(connection, run_id, &newest)?.ok_or_else(|| {
        StoreError::InvalidWorkProjection(format!(
            "acceptance evaluation {newest} is not on its run feed"
        ))
    })?;
    let newest_bindings = judged_bindings(
        connection,
        item,
        run_id,
        &newest,
        &newest_record,
        newest_position,
    )?;
    let (evaluation, record, position) = carried_anchor(
        connection,
        run_id,
        newest.clone(),
        newest_record.clone(),
        newest_position,
    )?;
    let judged_bindings = if evaluation == newest {
        newest_bindings.clone()
    } else {
        judged_bindings(connection, item, run_id, &evaluation, &record, position)?
    };
    // A blocking evaluation that names the failure accepts no revision, so the
    // failure it named stays carried whatever the item's contract is now.
    // Only when it names nothing does rewording back to the contract it
    // judged end the carry.
    if newest_record.supersedes.is_none()
        && record.criteria == item.acceptance
        && judged_bindings == item.acceptance_bindings
    {
        return Ok(None);
    }
    let mut statement = connection.prepare(
        "SELECT object_id FROM work_feed_entries
         WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND object_kind = 'work_event'
           AND position > ?2
         ORDER BY position",
    )?;
    let rows = statement
        .query_map(params![run_id.0.to_string(), position], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut criteria = record.criteria.clone();
    let mut bindings = judged_bindings.clone();
    // Each revision that changed the criteria or their bindings: whether its
    // claim authority makes it the executor's, and the session that made it.
    let mut revisions: Vec<(bool, Option<SessionId>)> = Vec::new();
    for stored in rows {
        let id =
            ObjectId::from_stored(stored.clone()).ok_or(StoreError::InvalidStoredKey(stored))?;
        let event: WorkEvent = load_typed_work_object(connection, &id, "work_event")?;
        let WorkTransition::Revised { authority } = &event.transition else {
            continue;
        };
        if event.work_id != item.work_id
            || (event.work.acceptance == criteria && event.work.acceptance_bindings == bindings)
        {
            continue;
        }
        criteria.clone_from(&event.work.acceptance);
        bindings.clone_from(&event.work.acceptance_bindings);
        revisions.push((
            matches!(authority, WorkPlanningAuthority::Claim { .. }),
            event.actor.session_id.clone(),
        ));
    }
    // Every revision of an item with an active run lands on that run's feed,
    // so the revisions after the evaluation must lead to the item as it is.
    // When they do not, the projection is damaged; reading nothing carried
    // would silently lift the requirement to name the failure.
    if criteria != item.acceptance || bindings != item.acceptance_bindings {
        return Err(StoreError::InvalidWorkProjection(format!(
            "the revisions after acceptance evaluation {evaluation} on its run feed do not lead to the item's current criteria"
        )));
    }
    let claim = load_work_claim_optional(connection, run_id)?;
    let run = load_work_run(connection, run_id)?;
    let history = run_holder_history(connection, run_id)?;
    let executor = revisions.iter().any(|(under_claim, session)| {
        *under_claim
            || session.as_ref().is_some_and(|session| {
                history.contains(session)
                    || claim.as_ref().is_some_and(|claim| claim.holder == *session)
                    || run.executor.as_ref() == Some(session)
            })
    });
    let newest_judged_bindings = (evaluation != newest).then_some(newest_bindings);
    Ok(Some(CarriedFailure {
        evaluation,
        newest_judged_bindings,
        revised_by: if executor {
            CarriedFailureReviser::Executor
        } else {
            CarriedFailureReviser::Planner
        },
        judged_revision: record.work_revision,
        judged_bindings,
        blocking: record
            .verdicts
            .iter()
            .enumerate()
            .filter(|(_, verdict)| verdict.verdict != AcceptanceVerdict::Pass)
            .map(|(index, verdict)| CarriedFailureVerdict {
                criterion: index + 1,
                verdict: verdict.verdict,
                rationale: verdict.rationale.clone(),
            })
            .collect(),
        judged_criteria: record.criteria,
    }))
}

/// The verification bindings an evaluation's criteria had when it judged
/// them, read from the newest snapshot of the item before it on the run feed.
/// Dropping or changing a binding weakens a criterion as surely as rewording
/// it. Every run starts with a work event, and every revision lands on the
/// run feed, so a missing or mismatched snapshot is a damaged projection,
/// never a reason to stop comparing bindings.
fn judged_bindings(
    connection: &Connection,
    item: &WorkItem,
    run_id: WorkRunId,
    evaluation: &ObjectId,
    record: &AcceptanceEvaluation,
    position: i64,
) -> Result<Vec<AcceptanceBinding>, StoreError> {
    Ok(
        work_snapshot_before(connection, run_id, item.work_id, position)?
            .filter(|work| work.acceptance == record.criteria)
            .ok_or_else(|| {
                StoreError::InvalidWorkProjection(format!(
                    "acceptance evaluation {evaluation} has no snapshot of the criteria it judged on its run feed"
                ))
            })?
            .acceptance_bindings,
    )
}

/// The failing evaluation a blocking record carries, with its run-feed
/// position: the record itself, or, when it names a failure it superseded,
/// the root of that chain. Admission lets a record name only the failure
/// carried when it was recorded, so every link names an earlier failing
/// record on the same run feed; anything else is a damaged projection.
fn carried_anchor(
    connection: &Connection,
    run_id: WorkRunId,
    mut evaluation: ObjectId,
    mut record: AcceptanceEvaluation,
    mut position: i64,
) -> Result<(ObjectId, AcceptanceEvaluation, i64), StoreError> {
    while let Some(named) = record.supersedes.clone() {
        let named_position = citation_position(connection, run_id, &named)?
            .filter(|named_position| *named_position < position)
            .ok_or_else(|| {
                StoreError::InvalidWorkProjection(format!(
                    "acceptance evaluation {evaluation} supersedes {named}, which is not earlier on its run feed"
                ))
            })?;
        let named_record: AcceptanceEvaluation = load_typed_work_object(connection, &named, KIND)?;
        if named_record.first_blocking().is_none() {
            return Err(StoreError::InvalidWorkProjection(format!(
                "acceptance evaluation {evaluation} supersedes {named}, which did not fail"
            )));
        }
        evaluation = named;
        record = named_record;
        position = named_position;
    }
    Ok((evaluation, record, position))
}

/// The item as the newest work event on the run feed before `position`
/// recorded it: every revision of an item with an active run lands on that
/// feed, so this is the item as it stood at that point.
fn work_snapshot_before(
    connection: &Connection,
    run_id: WorkRunId,
    work_id: WorkId,
    position: i64,
) -> Result<Option<WorkItem>, StoreError> {
    let stored: Option<String> = connection
        .query_row(
            "SELECT object_id FROM work_feed_entries
             WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND object_kind = 'work_event'
               AND position < ?2
             ORDER BY position DESC LIMIT 1",
            params![run_id.0.to_string(), position],
            |row| row.get(0),
        )
        .optional()?;
    let Some(stored) = stored else {
        return Ok(None);
    };
    let id = ObjectId::from_stored(stored.clone()).ok_or(StoreError::InvalidStoredKey(stored))?;
    let event: WorkEvent = load_typed_work_object(connection, &id, "work_event")?;
    Ok((event.work_id == work_id).then_some(event.work))
}

/// Who submits an evaluation, against who executes its run: enough to tell
/// whether an executor of the run is acknowledging a failure itself.
struct EvaluatorStanding<'a> {
    mode: AcceptanceEvaluationMode,
    session: &'a SessionId,
    holder: Option<&'a SessionId>,
    executor: Option<&'a SessionId>,
    history: &'a [SessionId],
}

impl EvaluatorStanding<'_> {
    /// A `same_session` evaluation, or one whose session holds, held or
    /// executes the run. The session decides, not the mode label: a
    /// `sub_agent` that shares an executor's session is that executor, and its
    /// asserted execution identity plays no part. Identity admission already
    /// confines `same_session` to the session that holds or executes the run;
    /// the mode is named here so the rule does not lean on that.
    fn is_an_executor(&self) -> bool {
        self.mode == AcceptanceEvaluationMode::SameSession
            || self.holder == Some(self.session)
            || self.executor == Some(self.session)
            || self.history.contains(self.session)
    }
}

/// Admits an evaluation's `supersedes` against the failure carried on its
/// run. After the executor's revision the evaluation must name the carried
/// failure, from an evaluator that is not an executor of the run; after a
/// planner's alone it may name it; with none carried it must not.
fn admit_supersedes(
    item: &WorkItem,
    carried: Option<&CarriedFailure>,
    supersedes: Option<&ObjectId>,
    evaluator: &EvaluatorStanding<'_>,
) -> Result<(), StoreError> {
    let refuse = |refusal: CarriedFailureRefusal, failed: Option<&ObjectId>, reason: String| {
        StoreError::AcceptanceEvaluationCarriedFailure {
            work: item.work_id,
            refusal,
            failed: failed.cloned(),
            reason: format!("{reason}; {}", refusal.remedy()),
        }
    };
    let Some(carried) = carried else {
        return supersedes.map_or(Ok(()), |named| {
            Err(refuse(
                CarriedFailureRefusal::NothingToSupersede,
                None,
                format!(
                    "it supersedes {named}, but no failing evaluation's criteria were revised on this run"
                ),
            ))
        });
    };
    match supersedes {
        // The executor that revised the criteria its failure judged may not
        // judge its own revision: someone else accepts it.
        Some(named)
            if *named == carried.evaluation
                && carried.revised_by == CarriedFailureReviser::Executor
                && evaluator.is_an_executor() =>
        {
            Err(refuse(
                CarriedFailureRefusal::SelfAcknowledged,
                Some(&carried.evaluation),
                format!(
                    "the run's executor revised the criteria that evaluation {} failed, and this {} evaluation comes from an executor of the run",
                    carried.evaluation,
                    evaluator.mode.word()
                ),
            ))
        }
        Some(named) if *named == carried.evaluation => Ok(()),
        Some(named) => Err(refuse(
            CarriedFailureRefusal::Unacknowledged,
            Some(&carried.evaluation),
            format!(
                "it supersedes {named}, but the failure carried on this run is evaluation {}",
                carried.evaluation
            ),
        )),
        // A planner's revision alone is shown to the evaluation, which need
        // not name it.
        None if carried.revised_by == CarriedFailureReviser::Planner => Ok(()),
        None => Err(refuse(
            CarriedFailureRefusal::Unacknowledged,
            Some(&carried.evaluation),
            format!(
                "the run's executor revised the criteria that evaluation {} failed",
                carried.evaluation
            ),
        )),
    }
}

/// The newest evaluation entry on the run feed at or before `cut`: the one a
/// seal binding that cut must name. Entries after the cut are excluded.
pub(super) fn newest_evaluation_through(
    connection: &Connection,
    run_id: WorkRunId,
    cut: i64,
) -> Result<Option<ObjectId>, StoreError> {
    let stored: Option<String> = connection
        .query_row(
            "SELECT object_id FROM work_feed_entries
             WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND object_kind = ?2
               AND position <= ?3
             ORDER BY position DESC LIMIT 1",
            params![run_id.0.to_string(), KIND, cut],
            |row| row.get(0),
        )
        .optional()?;
    stored
        .map(|stored| {
            ObjectId::from_stored(stored.clone()).ok_or(StoreError::InvalidStoredKey(stored))
        })
        .transpose()
}

/// What, if anything, the host recorded on the run after the evaluated cut
/// that the evaluation did not see.
///
/// A source change voids the evaluation, unless it left the source at the
/// revision the evaluation declared it judged: then the evaluator saw that
/// change, and neither it nor the obligation it opened counts. The declared
/// fingerprint is the host's source revision, as it reports it on turn
/// observations; a declared workspace must match too.
///
/// The source can also move without a reported change, as when a check runs
/// after someone else's edit. So when the newest sighting after the cut
/// shows the source at a revision other than the judged one, the evaluation
/// is void too, whatever that sighting claims. The judged revision is the
/// declared one, or else the revision the run was last seen at when the
/// basis was cut; with neither, there is nothing to compare. The revision
/// fingerprints the full content, so it is compared whatever workspace
/// reported it.
///
/// Only an execution observation is a sighting of the source: the host lists
/// a turn's observations in the order it saw them, each at the revision the
/// source had then. A turn reported after the cut may still hold sightings
/// from before the evaluation, such as a check that ran before the edit the
/// evaluator judged. So the newest sighting decides: a later quiet sighting
/// of the judged revision, or a reported change to the declared revision,
/// puts the source back where it was judged. Verification and environment
/// records describe a check and carry the content basis that check ran on,
/// which is its producer's and may predate the cut, so neither counts here
/// as a sighting; a check that a bound pass cites is held to the judged
/// source separately.
///
/// Any other host check (a verification, an environment record, an
/// obligation opened or resolved) asks for a re-read and resubmission. A
/// move the evaluator did not see wins over a check.
fn basis_moved_after(
    connection: &Connection,
    run_id: WorkRunId,
    position: i64,
    declared: Option<&crate::domain::AcceptanceSourceBasis>,
    root: Option<&NamedEvaluationRoot>,
) -> Result<Option<EvaluationBasisMove>, StoreError> {
    let judged =
        judged_source(connection, run_id, position, declared, root)?.map(|judged| judged.revision);
    let mut statement = connection.prepare(
        "SELECT object_kind, object_id FROM work_feed_entries
         WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND position > ?2
         ORDER BY position",
    )?;
    let rows = statement
        .query_map(params![run_id.0.to_string(), position], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut seen = std::collections::BTreeSet::new();
    let mut check = false;
    let mut quiet_move = false;
    for (kind, stored) in rows {
        if !MUTATION_KINDS.contains(&kind.as_str()) {
            continue;
        }
        let hash =
            ObjectId::from_stored(stored.clone()).ok_or(StoreError::InvalidStoredKey(stored))?;
        match kind.as_str() {
            "execution_observation" => {
                let observation: ExecutionObservation =
                    load_typed_work_object(connection, &hash, "execution_observation")?;
                if root.is_some_and(|root| {
                    observation.source_basis.as_ref().is_some_and(|basis| {
                        basis.workspace_id != root.event.workspace_id
                            || basis.source_root_generation != Some(root.event.generation)
                            || basis.source_root_state != Some(SourceRootState::Named)
                    })
                }) {
                    continue;
                }
                if !observation.source_changed {
                    // The newest sighting decides where the source is.
                    if let (Some(judged_at), Some(sighting)) =
                        (judged.as_deref(), observation.source_basis.as_ref())
                    {
                        quiet_move = sighting.source_revision != judged_at;
                    }
                    continue;
                }
                if !judged_revision(declared, &observation) {
                    return Ok(Some(EvaluationBasisMove::SourceChanged));
                }
                // The reported change left the source at the declared
                // revision, so any sighting of another revision before it
                // is older than that change.
                quiet_move = false;
                seen.insert(hash);
            }
            "work_obligation" => {
                let obligation: WorkObligation =
                    load_typed_work_object(connection, &hash, "work_obligation")?;
                if !seen.contains(&obligation.triggering_observation) {
                    check = true;
                }
            }
            _ => check = true,
        }
    }
    if quiet_move {
        return Ok(Some(EvaluationBasisMove::SourceChanged));
    }
    Ok(check.then_some(EvaluationBasisMove::CheckRecorded))
}

/// The revision the run was last seen at, at or before `through`: that of
/// the newest execution observation there that carries one. Verification
/// and environment records are left out: they carry the basis of the check
/// they describe, not where the source was when they were recorded.
fn revision_seen_through(
    connection: &Connection,
    run_id: WorkRunId,
    through: i64,
    root: Option<&NamedEvaluationRoot>,
) -> Result<Option<String>, StoreError> {
    if let Some(root) = root {
        return Ok(latest_named_root_sighting_on(
            connection,
            run_id,
            &root.event.workspace_id,
            root.event.generation,
            through,
            false,
        )?
        .and_then(|(_, observation)| observation.source_basis.map(|basis| basis.source_revision)));
    }
    Ok(connection
        .query_row(
            "SELECT json_extract(object.canonical_json, '$.source_basis.source_revision')
             FROM work_feed_entries entry
             JOIN objects object ON object.object_id = entry.object_id
             WHERE entry.feed_kind = 'run_execution' AND entry.feed_id = ?1
               AND entry.position <= ?2
               AND entry.object_kind = 'execution_observation'
               AND json_extract(object.canonical_json, '$.source_basis.source_revision')
                   IS NOT NULL
             ORDER BY entry.position DESC LIMIT 1",
            params![run_id.0.to_string(), through],
            |row| row.get::<_, String>(0),
        )
        .optional()?)
}

/// The source an evaluation judged: the revision it declared, in the
/// workspace it declared when it named one, or else the revision the run was
/// last seen at through its cut.
struct JudgedSource {
    revision: String,
    workspace: Option<String>,
    /// Whether the evaluation declared this source rather than taking the
    /// run's newest sighting.
    declared: bool,
}

impl JudgedSource {
    /// Whether a check that ran on `basis` checked this source.
    fn checked_by(&self, basis: &ExecutionSourceBasis) -> bool {
        basis.source_revision == self.revision
            && self
                .workspace
                .as_ref()
                .is_none_or(|workspace| *workspace == basis.workspace_id)
    }
}

/// The source the evaluation cut at `through` judged, or `None` when it
/// declared none and the run carries no revision through the cut. A
/// declaration always wins: an older sighting never stands in for the tree
/// the evaluator says it judged.
fn judged_source(
    connection: &Connection,
    run_id: WorkRunId,
    through: i64,
    declared: Option<&crate::domain::AcceptanceSourceBasis>,
    root: Option<&NamedEvaluationRoot>,
) -> Result<Option<JudgedSource>, StoreError> {
    Ok(match declared {
        Some(declared) => Some(JudgedSource {
            revision: declared.fingerprint.clone(),
            workspace: root
                .map(|root| root.event.workspace_id.clone())
                .or_else(|| declared.workspace_id.clone()),
            declared: true,
        }),
        None => {
            revision_seen_through(connection, run_id, through, root)?.map(|revision| JudgedSource {
                revision,
                workspace: root.map(|root| root.event.workspace_id.clone()),
                declared: false,
            })
        }
    })
}

fn require_named_root_judged_source(
    connection: &Connection,
    run_id: WorkRunId,
    through: i64,
    root: Option<&NamedEvaluationRoot>,
    judged: Option<&JudgedSource>,
    work_id: WorkId,
) -> Result<(), StoreError> {
    let Some(root) = root else {
        return Ok(());
    };
    // A root the host has not yet sighted anchors no evaluation: its first
    // sighting could show any source.
    let Some(latest) = revision_seen_through(connection, run_id, through, Some(root))? else {
        return Err(refused(
            work_id,
            "the named root has no sighting yet; capture that root, then evaluate it",
        ));
    };
    if judged.map(|judged| judged.revision.as_str()) != Some(latest.as_str()) {
        return Err(refused(
            work_id,
            "the evaluated source does not match the named root's newest sighting; capture and evaluate that root",
        ));
    }
    Ok(())
}

/// Each passing verdict's one-based criterion position and citations.
fn passing_citations(
    verdicts: &[CriterionVerdict],
) -> impl Iterator<Item = (usize, &[ObjectId])> + '_ {
    verdicts
        .iter()
        .enumerate()
        .filter(|(_, verdict)| verdict.verdict == AcceptanceVerdict::Pass)
        .map(|(index, verdict)| (index + 1, verdict.evidence.as_slice()))
}

/// A citation of a pass on a bound criterion that does not show its check
/// ran on the source the evaluation judged, and why.
struct StaleCitation {
    criterion: usize,
    citation: ObjectId,
    cause: StaleCause,
}

enum StaleCause {
    /// The check ran on another source than the judged one.
    OtherSource(ExecutionSourceBasis),
    /// The check ran on the judged revision, but by the cut the run had moved
    /// away from it.
    MovedAfter { checked: String, moved: Moved },
    /// The citation is not a passed check, or no judged source exists to
    /// match. A pass admitted here never reaches this: `bind_verdicts` admits
    /// only passed checks for a bound criterion, and each check's producer is
    /// a sighting on the run, with a revision, before the check. Only a record
    /// written some other way can.
    Unverifiable,
}

/// How the run left the revision a check ran on.
enum Moved {
    /// Its newest sighting after the check is at this other revision.
    To(String),
    /// It reported a change that carries no revision, which may have moved
    /// the source anywhere.
    Unrevised,
}

impl StaleCitation {
    /// Why record admission refuses the evaluation.
    fn refusal(
        &self,
        item: &WorkItem,
        judged: Option<&JudgedSource>,
        cut: i64,
    ) -> Result<String, StoreError> {
        let kind = item
            .acceptance_bindings
            .iter()
            .find(|binding| binding.criterion == self.criterion)
            .map(|binding| super::planning::encode_state(binding.requirement.check_kind))
            .transpose()?
            .unwrap_or_default();
        let (criterion, citation) = (self.criterion, &self.citation);
        Ok(match (&self.cause, judged) {
            (StaleCause::OtherSource(checked), Some(judged)) => {
                let (ran, evaluated) = if checked.source_revision == judged.revision {
                    (
                        format!("in workspace {}", checked.workspace_id),
                        format!(
                            "workspace {}",
                            judged.workspace.as_deref().unwrap_or_default()
                        ),
                    )
                } else {
                    (
                        format!("on source revision {}", checked.source_revision),
                        format!("revision {}", judged.revision),
                    )
                };
                let declaration = if judged.declared {
                    ", or, when the declared fingerprint is not the source revision the host reports, declare that revision"
                } else {
                    ""
                };
                format!(
                    "criterion {criterion} is bound to {kind} verification, and {citation} ran {ran}, not the {evaluated} this evaluation judged; run the check on the current source, then evaluate again citing it{declaration}"
                )
            }
            (StaleCause::MovedAfter { checked, moved }, _) => {
                let moved = match moved {
                    Moved::To(seen) => format!("was last seen at revision {seen}"),
                    Moved::Unrevised => "reported a source change without a revision".to_owned(),
                };
                format!(
                    "criterion {criterion} is bound to {kind} verification, and {citation} ran on source revision {checked}, but the run {moved} after it, before evidence basis {cut}; run the check on the current source, then evaluate again citing it"
                )
            }
            _ => format!(
                "criterion {criterion} is bound to {kind} verification, and {citation} cannot be shown to be a passed check of the source this evaluation judged; run the check on the current source, then evaluate again citing it"
            ),
        })
    }
}

/// The first citation of a pass on a bound criterion that does not show its
/// check ran on the source the evaluation judged, or `None` when every one
/// does.
///
/// A bound criterion rests on a typed check of the work as it was judged, so
/// the check must have run on that source, and the source must still be there
/// at the cut `through`: a passed check of an earlier revision says nothing
/// about a later edit the source still holds, even one an older declaration
/// leaves out. The obligation path does not
/// always catch such a check at completion (a binding whose obligation was
/// waived is never matched to the run's latest change), so record admission
/// (R5) applies this rule, and completion applies it again to the record it
/// consumes, which covers a record admitted before the rule existed.
fn stale_bound_citation<'a>(
    connection: &Connection,
    item: &WorkItem,
    run_id: WorkRunId,
    judged: Option<&JudgedSource>,
    through: i64,
    passes: impl IntoIterator<Item = (usize, &'a [ObjectId])>,
    root: Option<&NamedEvaluationRoot>,
) -> Result<Option<StaleCitation>, StoreError> {
    for (criterion, citations) in passes {
        if !item
            .acceptance_bindings
            .iter()
            .any(|binding| binding.criterion == criterion)
        {
            continue;
        }
        for citation in citations {
            let cause = match classify_citation(connection, run_id, citation)? {
                Some(Citation::VerificationPassed {
                    source_basis,
                    producer,
                    ..
                }) => {
                    // Under a named root both the check and the observation
                    // that produced it must be in the root's workspace and
                    // generation, and both must follow the binding on the run
                    // feed, as the obligation matcher requires.
                    let same_named_root = match root {
                        None => true,
                        Some(root) => {
                            let in_root = |basis: &ExecutionSourceBasis| {
                                basis.workspace_id == root.event.workspace_id
                                    && basis.source_root_generation == Some(root.event.generation)
                                    && basis.source_root_state == Some(SourceRootState::Named)
                            };
                            let producer_basis = load_typed_work_object::<ExecutionObservation>(
                                connection,
                                &producer,
                                "execution_observation",
                            )?
                            .source_basis;
                            in_root(&source_basis)
                                && producer_basis.as_ref().is_some_and(in_root)
                                && citation_position(connection, run_id, &producer)?
                                    .is_some_and(|position| position > root.position)
                                && citation_position(connection, run_id, citation)?
                                    .is_some_and(|position| position > root.position)
                        }
                    };
                    match judged {
                        Some(judged) if judged.checked_by(&source_basis) && same_named_root => {
                            match moved_after_check(
                                connection,
                                run_id,
                                citation,
                                &producer,
                                &source_basis.source_revision,
                                through,
                                root,
                            )? {
                                None => continue,
                                Some(moved) => StaleCause::MovedAfter {
                                    checked: source_basis.source_revision,
                                    moved,
                                },
                            }
                        }
                        Some(_) => StaleCause::OtherSource(source_basis),
                        None => StaleCause::Unverifiable,
                    }
                }
                _ => StaleCause::Unverifiable,
            };
            return Ok(Some(StaleCitation {
                criterion,
                citation: citation.clone(),
                cause,
            }));
        }
    }
    Ok(None)
}

/// Whether the run left `checked`, the revision `producer` ran the check
/// `citation` on, between that check and `through`, inclusive, read as F3
/// reads the source after a cut. The newest execution observation there
/// that carries a revision decides where the source is, whatever change it
/// reports: the revision fingerprints the full content, so a move and its
/// revert leave the check standing. A reported change that carries no
/// revision may have moved the source anywhere, and no later sighting
/// clears it. `None` when the source is still where the check ran.
fn moved_after_check(
    connection: &Connection,
    run_id: WorkRunId,
    citation: &ObjectId,
    producer: &ObjectId,
    checked: &str,
    through: i64,
    root: Option<&NamedEvaluationRoot>,
) -> Result<Option<Moved>, StoreError> {
    // The checkpoint admits a check only with a producer on the same run.
    let ran = citation_position(connection, run_id, producer)?.ok_or_else(|| {
        StoreError::InvalidWorkProjection(format!(
            "verification evidence {citation} names producer observation {producer}, which is not on its run feed"
        ))
    })?;
    let run = run_id.0.to_string();
    let unrevised: bool = connection.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM work_feed_entries entry
             JOIN objects object ON object.object_id = entry.object_id
             WHERE entry.feed_kind = 'run_execution' AND entry.feed_id = ?1
               AND entry.position > ?2 AND entry.position <= ?3
               AND entry.object_kind = 'execution_observation'
               AND json_extract(object.canonical_json, '$.source_basis.source_revision') IS NULL
               AND json_extract(object.canonical_json, '$.source_changed') = 1
         )",
        params![run, ran, through],
        |row| row.get(0),
    )?;
    if unrevised {
        return Ok(Some(Moved::Unrevised));
    }
    let newest: Option<String> = connection
        .query_row(
            "SELECT json_extract(object.canonical_json, '$.source_basis.source_revision')
             FROM work_feed_entries entry
             JOIN objects object ON object.object_id = entry.object_id
             WHERE entry.feed_kind = 'run_execution' AND entry.feed_id = ?1
               AND entry.position > ?2 AND entry.position <= ?3
               AND entry.object_kind = 'execution_observation'
               AND json_extract(object.canonical_json, '$.source_basis.source_revision')
                   IS NOT NULL
               AND (?4 IS NULL OR (
                   json_extract(object.canonical_json, '$.source_basis.workspace_id') = ?4
                   AND json_extract(object.canonical_json, '$.source_basis.source_root_generation') = ?5
                   AND json_extract(object.canonical_json, '$.source_basis.source_root_state') = 'named'
               ))
             ORDER BY entry.position DESC LIMIT 1",
            params![
                run,
                ran,
                through,
                root.map(|root| root.event.workspace_id.as_str()),
                root.map(|root| root.event.generation),
            ],
            |row| row.get(0),
        )
        .optional()?;
    Ok(newest.filter(|revision| revision != checked).map(Moved::To))
}

/// Whether a source change left the source at the revision the evaluation
/// declared it judged, in the declared workspace when one was named.
fn judged_revision(
    declared: Option<&crate::domain::AcceptanceSourceBasis>,
    observation: &ExecutionObservation,
) -> bool {
    let (Some(declared), Some(basis)) = (declared, observation.source_basis.as_ref()) else {
        return false;
    };
    declared.fingerprint == basis.source_revision
        && declared
            .workspace_id
            .as_ref()
            .is_none_or(|workspace| *workspace == basis.workspace_id)
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
        || record.work_revision_hash != *CanonicalObject::freeze(item)?.key()
        || record.criteria != item.acceptance
    {
        return Ok(Some(AcceptanceStaleReason::Revision));
    }
    let current_root = named_root_at_on(connection, run_id, i64::MAX)?;
    if record.named_root_binding.as_ref() != current_root.as_ref().map(|root| &root.event_id) {
        return Ok(Some(AcceptanceStaleReason::Mutation));
    }
    let evaluated_root = named_root_at_on(connection, run_id, record.evaluated_cut.position)?;
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
    if basis_moved_after(
        connection,
        run_id,
        record.evaluated_cut.position,
        record.source_basis.as_ref(),
        evaluated_root.as_ref(),
    )?
    .is_some()
    {
        return Ok(Some(AcceptanceStaleReason::Mutation));
    }
    let judged = judged_source(
        connection,
        run_id,
        record.evaluated_cut.position,
        record.source_basis.as_ref(),
        evaluated_root.as_ref(),
    )?;
    if let Some(root) = evaluated_root.as_ref() {
        let latest = revision_seen_through(
            connection,
            run_id,
            record.evaluated_cut.position,
            Some(root),
        )?;
        if latest.is_none()
            || latest.as_deref() != judged.as_ref().map(|judged| judged.revision.as_str())
        {
            return Ok(Some(AcceptanceStaleReason::Source));
        }
    }
    if stale_bound_citation(
        connection,
        item,
        run_id,
        judged.as_ref(),
        record.evaluated_cut.position,
        passing_citations(&record.verdicts),
        evaluated_root.as_ref(),
    )?
    .is_some()
    {
        return Ok(Some(AcceptanceStaleReason::VerificationSource));
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

/// Completion-side assessment of the newest evaluation on `run_id`, under an
/// evaluated policy. An item with no acceptance criteria is refused first:
/// no evaluation of it can exist, so no recovery cause could name one.
pub(super) fn assess_on(
    connection: &Connection,
    item: &WorkItem,
    run_id: WorkRunId,
    policy: &AcceptanceEvaluationPolicy,
    source_fingerprint: Option<&str>,
) -> Result<AcceptanceEvaluationAssessment, StoreError> {
    if item.acceptance.is_empty() {
        return Err(StoreError::AcceptanceCriteriaRequired { work: item.work_id });
    }
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
