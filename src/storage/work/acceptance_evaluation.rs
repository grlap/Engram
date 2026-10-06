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
    source_observation_if_accounted_on,
};
use super::planning::{normalize_note_text, persist_operation_result};
use super::query::{
    canonical_work_mark_events_for_item, load_work_claim_optional, load_work_item, load_work_run,
    on_one_snapshot,
};
use super::{
    CanonicalObject, DETACH_PROVENANCE_SOURCE, FeedPosition, ObjectId, SCHEMA_VERSION, SessionId,
    WorkCompletionRecoveryCause, WorkId, WorkItem, WorkRunId,
};
use crate::domain::{
    AcceptanceBasis, AcceptanceBinding, AcceptanceEvaluation, AcceptanceEvaluationMode,
    AcceptanceEvaluationPolicy, AcceptanceResult, AcceptanceStaleReason, AcceptanceVerdict,
    ActorContext, AssuranceLevel, CarriedFailure, CarriedFailureReviser, CarriedFailureVerdict,
    CompletionSeal, CriterionVerdict, CriterionVerdictInput, ExecutionObservation,
    ExecutionSourceBasis, FeedId, MAX_ACCEPTANCE_EVALUATION_BYTES,
    MAX_ACCEPTANCE_SOURCE_BASIS_BYTES, MAX_ACCEPTANCE_VERDICT_CITATIONS,
    MAX_EXECUTION_IDENTITY_BYTES, MechanicalBasis, NamedRootBindingEvent, NamedRootBindingKind,
    ProjectId, ProvenanceRelation, RecordAcceptanceEvaluationRequest, SourceObservation,
    SourceRootState, VerificationEvidence, VerificationResult, WorkEvent, WorkEvidence,
    WorkLifecycle, WorkObligation, WorkPlanningAuthority, WorkTransition,
};
use crate::memory::Redactor;
use crate::storage::{CarriedFailureRefusal, EvaluationBasisMove, SqliteStore, StoreError};
use crate::storage::{DecidingObservation, StaleRecoveryContext};

/// Canonical object kind and run-feed entry kind of one evaluation.
pub(crate) const KIND: &str = "acceptance_evaluation";
const OPERATION: &str = "record_acceptance_evaluation";
const MAX_ATTEMPT_KEY_BYTES: usize = 256;

/// Feed entry kinds that describe host-observed workspace or check changes;
/// later notes, gates, observations, and evaluations never appear here.
const MUTATION_KINDS: &[&str] = &[
    "execution_observation",
    super::UNADMITTED_OBSERVATION_KIND,
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
    /// The source observation that decided the move the record reads stale
    /// for, when an observation decided it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub stale_observation: Option<DecidingObservation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_recovery: Option<Box<crate::domain::AcceptanceSourceRecoveryCause>>,
    /// True when the policy requires source freshness and this read could
    /// not measure a fingerprint: the recorded basis is checked against the
    /// fingerprint `done` presents, and this read does not call it stale.
    pub source_checked_at_done: bool,
    /// The failing evaluation whose criteria were revised on this run, which
    /// the next evaluation must see and, after the executor's revision, name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub carried_failure: Option<CarriedFailure>,
    /// The blocking evaluation an evaluation through the run feed's head
    /// would be refused for: the same assessment the record transaction
    /// repeats at the submitted basis. Assessed on the item's active run
    /// only, since no evaluation is recorded without one. `None` means only
    /// that this rule does not refuse; it admits nothing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reroll: Option<Box<crate::domain::RerollAdmissionCause>>,
}

/// What a completion would do with the newest evaluation right now.
#[derive(Debug)]
pub enum AcceptanceEvaluationReadiness {
    /// The policy is self-asserted: no evaluation is consulted.
    SelfAsserted,
    /// A fresh, all-pass evaluation completion would consume; the seal will
    /// carry its citations.
    Ready(Box<AcceptanceEvaluation>),
    /// The typed recovery completion would raise, with the context read
    /// beside it.
    Blocked(WorkCompletionRecoveryCause, StaleRecoveryContext),
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
    Stale(AcceptanceStaleReason, StaleRecoveryContext),
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
    /// Returns [`StoreError::AcceptanceEvaluationAdmissionRefused`] for typed
    /// policy, mode, identity, source-root or citation violations, and
    /// [`StoreError::AcceptanceEvaluationRefused`] for remaining structural
    /// and replacement-admission refusals;
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
        let evaluator_session = IdentityShape::of_request(request)
            .evaluator()
            .cloned()
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
            &EligibilityContext {
                item: &item,
                policy: &policy,
                mode: request.mode,
                evaluator: Some(&evaluator_session),
                parent: request.parent_session.as_ref(),
            },
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
        match assess_named_root_binding(
            RootPhase::Admission,
            named_root.as_ref().map(|root| &root.event_id),
            || Ok(named_root_at_on(&transaction, run_id, head)?.map(|root| root.event_id)),
            named_root.as_ref(),
            request
                .source_basis
                .as_ref()
                .and_then(|basis| basis.workspace_id.as_deref()),
        )? {
            RootBinding::Held => {}
            RootBinding::DeclaredWorkspaceMismatch(root) => {
                return Err(admission::root_refusal(
                    item.work_id,
                    root,
                    cut,
                    EvaluationRootMismatch::DeclaredWorkspaceMismatch,
                    request.source_basis.as_ref(),
                    None,
                    "the evaluation declares a workspace other than the claim's named source root",
                ));
            }
            RootBinding::Rebound => {
                return Err(StoreError::AcceptanceEvaluationBasisMoved {
                    work: item.work_id,
                    moved: EvaluationBasisMove::SourceChanged,
                    reason: "the named source root changed after the evaluated cut; re-read the run and evaluate its current root".into(),
                    // A root binding moved, not an observed source.
                    observation: None,
                });
            }
        }
        if let Some(BasisMoveFinding { moved, observation }) = basis_moved_after(
            &transaction,
            run_id,
            cut,
            request.source_basis.as_ref(),
            named_root.as_ref(),
        )? {
            let unadmitted_change = observation
                .as_ref()
                .is_some_and(|observation| observation.source_changed && !observation.admitted);
            return Err(StoreError::AcceptanceEvaluationBasisMoved {
                work: item.work_id,
                moved,
                observation: observation.map(Box::new),
                reason: match moved {
                    EvaluationBasisMove::CheckRecorded => format!(
                        "a host check was recorded after evidence basis {cut}; {}",
                        moved.remedy()
                    ),
                    EvaluationBasisMove::SourceChanged if unadmitted_change => format!(
                        "a source change the host observed without admission was recorded after evidence basis {cut}, and whatever revision it reports this evaluation's checks did not follow it; {}",
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
            request.source_basis.as_ref(),
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
            let context = CitationContext {
                item: &item,
                run_id,
                cut,
                criterion: stale.criterion,
                citation: stale.citation.as_str(),
                position: citation_position(&transaction, run_id, &stale.citation)?,
            };
            let mut cause = context.cause(match &stale.cause {
                StaleCause::OtherSource(_) | StaleCause::OtherNaming { .. } => {
                    EvaluationCitationMismatch::WrongSource
                }
                StaleCause::MovedAfter { .. } => EvaluationCitationMismatch::SourceMovedAfterCheck,
                StaleCause::Unverifiable => EvaluationCitationMismatch::UnverifiableSource,
            });
            cause.checked_revision = match &stale.cause {
                StaleCause::OtherSource(basis) | StaleCause::OtherNaming { checked: basis, .. } => {
                    Some(basis.source_revision.clone())
                }
                StaleCause::MovedAfter { checked, .. } => Some(checked.clone()),
                StaleCause::Unverifiable => None,
            };
            cause.judged_revision = judged.as_ref().map(|source| source.revision.clone());
            cause.producer_observation.clone_from(&stale.producer);
            return Err(admission::refusal(
                item.work_id,
                stale.refusal(&item, judged.as_ref(), cut)?,
                AcceptanceEvaluationAdmissionCause::Citation(Box::new(cause)),
            ));
        }
        // Last, after every structural and basis refusal, which are the more
        // specific answers: a same-session record needs an eligible mark, or
        // no other admitted mode.
        if let Some(failure) = same_session_ineligibility(
            &transaction,
            &item,
            &policy,
            request.mode,
            &SessionStanding {
                evaluator: Some(&evaluator_session),
                holder: holder.as_ref(),
                executor: run.executor.as_ref(),
                history: &history,
            },
        )? {
            return Err(EligibilityContext {
                item: &item,
                policy: &policy,
                mode: request.mode,
                evaluator: Some(&evaluator_session),
                parent: request.parent_session.as_ref(),
            }
            .refused(failure.mismatch, failure.reason, failure.mark_author));
        }
        // And a blocking evaluation stands until something that could change
        // it lies within this one's basis.
        if let Some(cause) =
            reroll::reroll_assessment(&transaction, &item, run_id, cut, named_root.as_ref())?
        {
            return Err(admission::refusal(
                item.work_id,
                reroll::reroll_reason(&cause),
                AcceptanceEvaluationAdmissionCause::Reroll(Box::new(cause)),
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
            let (stale, context) =
                staleness_named(connection, &item, run_id, &policy, &hash, &record, source)?;
            let carried_failure = carried_failure_on(connection, &item, run_id)?;
            let reroll = if item.active_run_id == Some(run_id) {
                let head = feed_head(connection, &FeedId::RunExecution(run_id))?;
                let root = named_root_at_on(connection, run_id, head)?;
                reroll::reroll_assessment(connection, &item, run_id, head, root.as_ref())?
                    .map(Box::new)
            } else {
                None
            };
            Ok(Some(AcceptanceEvaluationStatus {
                carried_failure,
                reroll,
                stale_observation: context.deciding_observation.map(|value| *value),
                source_recovery: context.source,
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
                    StaleRecoveryContext::default(),
                ),
                AcceptanceEvaluationAssessment::Stale(reason, context) => {
                    AcceptanceEvaluationReadiness::Blocked(
                        WorkCompletionRecoveryCause::AcceptanceEvaluationStale { reason },
                        context,
                    )
                }
                AcceptanceEvaluationAssessment::Fresh { evaluation, .. } => {
                    match blocking_cause(&evaluation) {
                        Some(cause) => AcceptanceEvaluationReadiness::Blocked(
                            cause,
                            StaleRecoveryContext::default(),
                        ),
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
        let criterion = verdict.criterion;
        if let Some(fault) = verdict_fault(
            verdict.verdict,
            verdict.basis,
            &verdict.rationale,
            verdict.evidence.len(),
        ) {
            return Err(refused(
                work,
                match fault {
                    VerdictFault::BlankRationale => {
                        format!("criterion {criterion} needs a rationale")
                    }
                    VerdictFault::TooManyCitations => format!(
                        "criterion {criterion} cites more than {MAX_ACCEPTANCE_VERDICT_CITATIONS} objects"
                    ),
                    VerdictFault::PassWithoutCitation => format!(
                        "criterion {criterion} passes without a citation; a pass needs at least one relevant run evidence citation"
                    ),
                    VerdictFault::PassOnHumanRequired => {
                        format!("criterion {criterion} cannot pass on a human_required basis")
                    }
                },
            ));
        }
    }
    let shape = IdentityShape::of_request(request);
    if shape.stray_child_metadata() {
        return Err(refused(
            work,
            "execution identity and parent session belong to sub_agent mode only",
        ));
    }
    if request.mode == AcceptanceEvaluationMode::SubAgent {
        // Both are asserted identifiers stored in the immutable record:
        // bounded like every other identifier on it.
        match (
            execution_identity_fault(request.execution_identity.as_deref()),
            shape.child_parent(),
        ) {
            (Some(ExecutionIdentityFault::Missing), _) | (_, None) => {
                return Err(refused(
                    work,
                    "sub_agent mode needs a distinct execution identity and the attested parent session",
                ));
            }
            (Some(ExecutionIdentityFault::OutOfBounds), Some(_)) => {
                return Err(refused(
                    work,
                    format!(
                        "the execution identity must be at most {MAX_EXECUTION_IDENTITY_BYTES} bytes with no control characters"
                    ),
                ));
            }
            (None, Some(parent)) => {
                if let Err(error) = crate::storage::admit_session_id(parent) {
                    return Err(refused(work, format!("parent session: {error}")));
                }
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
/// obligation opened or resolved) asks for a re-read and resubmission, except
/// a passed check on the declared revision with its own environment record
/// and the obligation resolutions it satisfied (see `same_turn`). A move the
/// evaluator did not see wins over a check.
fn basis_moved_after(
    connection: &Connection,
    run_id: WorkRunId,
    position: i64,
    declared: Option<&crate::domain::AcceptanceSourceBasis>,
    root: Option<&NamedEvaluationRoot>,
) -> Result<Option<BasisMoveFinding>, StoreError> {
    let judged_source = judged_source(connection, run_id, position, declared, root)?;
    let judged = judged_source.as_ref().map(|judged| judged.revision.clone());
    let name = |at: i64, observation: &SourceObservation| DecidingObservation {
        observation: observation.record.clone(),
        position: at,
        source_changed: observation.source_changed,
        admitted: observation.admitted,
        workspace: observation
            .source_basis
            .as_ref()
            .map(|basis| basis.workspace_id.clone()),
        revision: observation
            .source_basis
            .as_ref()
            .map(|basis| basis.source_revision.clone()),
        root_generation: observation
            .source_basis
            .as_ref()
            .and_then(|basis| basis.source_root_generation),
        reporting_session: observation.reporting_session.clone(),
        observed_at: observation.observed_at,
        recorded_at: observation.recorded_at,
        evaluated_revision: judged_source.as_ref().map(|judged| judged.revision.clone()),
        evaluated_revision_declared: judged_source.as_ref().is_some_and(|judged| judged.declared),
    };
    let mut statement = connection.prepare(
        "SELECT position, object_kind, object_id FROM work_feed_entries
         WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND position > ?2
         ORDER BY position",
    )?;
    let rows = statement
        .query_map(params![run_id.0.to_string(), position], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    // Collected first: a report lands its environment records before the
    // checks that link them.
    let exempt = same_turn::ExemptChecks::after(connection, run_id, position, declared, root)?;
    let mut seen = std::collections::BTreeSet::new();
    let mut check = false;
    // The newest sighting at another revision than the judged one, while no
    // later sighting or declared change has put the source back.
    let mut quiet_move: Option<DecidingObservation> = None;
    for (at, kind, stored) in rows {
        if !MUTATION_KINDS.contains(&kind.as_str()) {
            continue;
        }
        let hash =
            ObjectId::from_stored(stored.clone()).ok_or(StoreError::InvalidStoredKey(stored))?;
        if exempt.covers(connection, &kind, &hash)? {
            continue;
        }
        match kind.as_str() {
            "execution_observation" | super::UNADMITTED_OBSERVATION_KIND => {
                // An unadmitted record that was not accounted describes no
                // source the run is held to.
                let Some(observation) = source_observation_if_accounted_on(connection, &hash)?
                else {
                    continue;
                };
                if off_named_root(root, &observation) {
                    continue;
                }
                if !observation.source_changed {
                    // The newest sighting decides where the source is.
                    if let (Some(judged_at), Some(sighting)) =
                        (judged.as_deref(), observation.source_basis.as_ref())
                    {
                        quiet_move =
                            (sighting.source_revision != judged_at).then(|| name(at, &observation));
                    }
                    continue;
                }
                // An unadmitted change is a barrier the evaluation's checks
                // did not follow, whatever revision it reports: it may describe
                // the source from before them.
                if !observation.admitted || !judged_revision(declared, &observation) {
                    return Ok(Some(BasisMoveFinding {
                        moved: EvaluationBasisMove::SourceChanged,
                        observation: Some(name(at, &observation)),
                    }));
                }
                // The reported change left the source at the declared
                // revision, so any sighting of another revision before it
                // is older than that change.
                quiet_move = None;
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
    if let Some(observation) = quiet_move {
        return Ok(Some(BasisMoveFinding {
            moved: EvaluationBasisMove::SourceChanged,
            observation: Some(observation),
        }));
    }
    Ok(check.then_some(BasisMoveFinding {
        moved: EvaluationBasisMove::CheckRecorded,
        observation: None,
    }))
}

/// A move past an evaluation's cut and, for a source move an observation
/// decided, that observation.
struct BasisMoveFinding {
    moved: EvaluationBasisMove,
    observation: Option<DecidingObservation>,
}

/// Why the record reads stale, if it does, together with the source
/// observation that decided a source move when it reads stale for one: such a
/// move reads as `Mutation`, or as `UnadmittedChange` when an accounted change
/// the host observed without admission decided it, and the observation is
/// named beside it, never as a cause.
#[allow(
    clippy::too_many_arguments,
    reason = "the record id and freshness inputs belong to one assessment"
)]
fn staleness_named(
    connection: &Connection,
    item: &WorkItem,
    run_id: WorkRunId,
    policy: &AcceptanceEvaluationPolicy,
    evaluation: &ObjectId,
    record: &AcceptanceEvaluation,
    source: SourceCheck<'_>,
) -> Result<(Option<AcceptanceStaleReason>, StaleRecoveryContext), StoreError> {
    if let Some(reason) = staleness_before_move(connection, item, run_id, policy, record)? {
        return Ok((Some(reason), StaleRecoveryContext::default()));
    }
    let evaluated_root = named_root_at_on(connection, run_id, record.evaluated_cut.position)?;
    if let Some(finding) = basis_moved_after(
        connection,
        run_id,
        record.evaluated_cut.position,
        record.source_basis.as_ref(),
        evaluated_root.as_ref(),
    )? {
        // An accounted change the host observed without admission is named
        // as such: the content need not have changed for it to void the
        // evaluation.
        let reason = match finding.observation.as_ref() {
            Some(observation) if observation.source_changed && !observation.admitted => {
                AcceptanceStaleReason::UnadmittedChange
            }
            _ => AcceptanceStaleReason::Mutation,
        };
        return Ok((
            Some(reason),
            StaleRecoveryContext {
                deciding_observation: finding.observation.map(Box::new),
                ..StaleRecoveryContext::default()
            },
        ));
    }
    staleness_after_move(
        connection,
        item,
        run_id,
        policy,
        evaluation,
        record,
        source,
        evaluated_root.as_ref(),
    )
}

/// The stale reasons judged before a move past the cut.
fn staleness_before_move(
    connection: &Connection,
    item: &WorkItem,
    run_id: WorkRunId,
    policy: &AcceptanceEvaluationPolicy,
    record: &AcceptanceEvaluation,
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
    match assess_named_root_binding(
        RootPhase::Consumption,
        record.named_root_binding.as_ref(),
        || Ok(named_root_at_on(connection, run_id, i64::MAX)?.map(|root| root.event_id)),
        None,
        None,
    )? {
        RootBinding::Held => {}
        RootBinding::Rebound => return Ok(Some(AcceptanceStaleReason::Mutation)),
        RootBinding::DeclaredWorkspaceMismatch(_) => {
            unreachable!("consumption compares no declared workspace")
        }
    }
    // Every effective requirement is re-read from the current policy: a
    // strengthened mechanical basis retires asserted passes, and a pinned or
    // disallowed mode retires the whole record.
    if assess_mode_policy(item, policy, record.mode).is_err()
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
    // An executor-affiliated record is rechecked when consumed as well: a
    // mark whose author has since taken the run, a task no one marked, or a
    // sub-agent whose own session has since taken the run no longer admits
    // it.
    if matches!(
        record.mode,
        AcceptanceEvaluationMode::SameSession | AcceptanceEvaluationMode::SubAgent
    ) {
        let claim = load_work_claim_optional(connection, run_id)?;
        let run = load_work_run(connection, run_id)?;
        let history = run_holder_history(connection, run_id)?;
        if same_session_ineligibility(
            connection,
            item,
            policy,
            record.mode,
            &SessionStanding {
                evaluator: record.evaluator.session_id.as_ref(),
                holder: claim.as_ref().map(|claim| &claim.holder),
                executor: run.executor.as_ref(),
                history: &history,
            },
        )?
        .is_some()
        {
            return Ok(Some(AcceptanceStaleReason::Policy));
        }
    }
    // A record whose shape admission refuses could not be recorded today.
    // One that reached the store by import or edit cannot complete work: an
    // identity admission refuses names no evaluator to judge, and a
    // malformed verdict list or metadata of another mode is not a record of
    // this shape at all. The earlier reasons keep their precedence.
    let shape = IdentityShape::of_record(record);
    if shape.identity_defect() {
        return Ok(Some(AcceptanceStaleReason::Identity));
    }
    if shape.stray_child_metadata() || record_verdicts_malformed(record) {
        return Ok(Some(AcceptanceStaleReason::RecordShape));
    }
    if record.mode == AcceptanceEvaluationMode::IndependentSession {
        let claim = load_work_claim_optional(connection, run_id)?;
        let run = load_work_run(connection, run_id)?;
        let history = run_holder_history(connection, run_id)?;
        let standing = SessionStanding {
            evaluator: record.evaluator.session_id.as_ref(),
            holder: claim.as_ref().map(|claim| &claim.holder),
            executor: run.executor.as_ref(),
            history: &history,
        };
        if !standing.evaluator_is_independent() {
            return Ok(Some(AcceptanceStaleReason::Identity));
        }
    }
    Ok(None)
}

/// The stale reasons judged after a move past the cut.
#[allow(
    clippy::too_many_arguments,
    reason = "the reasons after a move read the same inputs as the move itself, and the root it was judged at"
)]
fn staleness_after_move(
    connection: &Connection,
    item: &WorkItem,
    run_id: WorkRunId,
    policy: &AcceptanceEvaluationPolicy,
    evaluation: &ObjectId,
    record: &AcceptanceEvaluation,
    source: SourceCheck<'_>,
    evaluated_root: Option<&NamedEvaluationRoot>,
) -> Result<(Option<AcceptanceStaleReason>, StaleRecoveryContext), StoreError> {
    let mut source_context = crate::domain::AcceptanceSourceRecoveryCause {
        mismatch: crate::domain::AcceptanceSourceMismatch::EvaluationSourceBasisMissing,
        remedy: crate::domain::AcceptanceSourceRemedy::EvaluateCurrentSource,
        evaluation: evaluation.clone(),
        run_id,
        evaluated_cut: record.evaluated_cut.position,
        root_binding: evaluated_root.map(|root| root.event_id.clone()),
        workspace_id: evaluated_root.map(|root| root.event.workspace_id.clone()),
        declared_revision: record
            .source_basis
            .as_ref()
            .map(|basis| basis.fingerprint.clone()),
        reported_revision: None,
        expected_fingerprint: record
            .source_basis
            .as_ref()
            .map(|basis| basis.fingerprint.clone()),
        presented_fingerprint: match source {
            SourceCheck::Unmeasured => None,
            SourceCheck::AtCompletion(value) => value.map(str::to_owned),
        },
    };
    let refused_source = |source| {
        (
            Some(AcceptanceStaleReason::Source),
            StaleRecoveryContext {
                source: Some(Box::new(source)),
                ..StaleRecoveryContext::default()
            },
        )
    };
    let judged = judged_source(
        connection,
        run_id,
        record.evaluated_cut.position,
        record.source_basis.as_ref(),
        evaluated_root,
    )?;
    if let Some(root) = evaluated_root
        && let RootSource::Unconfirmed { reported } = assess_named_root_source(
            connection,
            run_id,
            record.evaluated_cut.position,
            root,
            judged.as_ref(),
            RootPhase::Consumption,
        )?
    {
        // Consumption's reading of the named root, as the source recovery
        // names it.
        let declared = judged.as_ref().is_some_and(|judged| judged.declared);
        source_context.mismatch = if declared {
            crate::domain::AcceptanceSourceMismatch::UnconfirmedDeclaration
        } else {
            crate::domain::AcceptanceSourceMismatch::UnconfirmedEvaluatedRevision
        };
        source_context.remedy = if declared {
            crate::domain::AcceptanceSourceRemedy::EndTurnReadAndRetry
        } else {
            crate::domain::AcceptanceSourceRemedy::ReadSourceAndEvaluate
        };
        source_context.reported_revision = reported;
        return Ok(refused_source(source_context));
    }
    if stale_bound_citation(
        connection,
        item,
        run_id,
        judged.as_ref(),
        record.evaluated_cut.position,
        passing_citations(&record.verdicts),
        evaluated_root,
    )?
    .is_some()
    {
        return Ok((
            Some(AcceptanceStaleReason::VerificationSource),
            StaleRecoveryContext::default(),
        ));
    }
    let relied_on = cited_gate_names(connection, run_id, record)?;
    if gate_superseded_after(
        connection,
        run_id,
        record.evaluated_cut.position,
        &relied_on,
    )? {
        return Ok((
            Some(AcceptanceStaleReason::Evidence),
            StaleRecoveryContext::default(),
        ));
    }
    if policy.require_source_freshness {
        let evaluated = record
            .source_basis
            .as_ref()
            .map(|basis| basis.fingerprint.as_str());
        match (evaluated, source) {
            // A record without a basis can never match a measurement.
            (None, _) => return Ok(refused_source(source_context)),
            // A read measured nothing: the check is pending, not failed.
            (Some(_), SourceCheck::Unmeasured) => {}
            (Some(evaluated), SourceCheck::AtCompletion(presented))
                if presented == Some(evaluated) => {}
            (Some(_), SourceCheck::AtCompletion(None)) => {
                source_context.mismatch =
                    crate::domain::AcceptanceSourceMismatch::CompletionMeasurementMissing;
                source_context.remedy =
                    crate::domain::AcceptanceSourceRemedy::MeasureSourceAndRetry;
                return Ok(refused_source(source_context));
            }
            (Some(_), SourceCheck::AtCompletion(Some(_))) => {
                source_context.mismatch =
                    crate::domain::AcceptanceSourceMismatch::CompletionFingerprintMismatch;
                return Ok(refused_source(source_context));
            }
        }
    }
    Ok((None, StaleRecoveryContext::default()))
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
        match staleness_named(
            connection,
            item,
            run_id,
            policy,
            &hash,
            &record,
            SourceCheck::AtCompletion(source_fingerprint),
        )? {
            (Some(reason), context) => AcceptanceEvaluationAssessment::Stale(reason, context),
            (None, _) => AcceptanceEvaluationAssessment::Fresh {
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

mod admission;
mod citations;
use citations::{
    Citation, bind_verdicts, citation_position, cited_gate_names, classify_citation,
    gate_superseded_after, run_evidence_through,
};
mod eligibility;
#[cfg(test)]
use eligibility::{
    MarkStep, MarkTransition, ModePolicyMismatch, mark_author, same_session_mark_author,
};
use eligibility::{
    SessionStanding, admit_identity, admit_mode, assess_mode_policy, run_holder_history,
    same_session_ineligibility,
};
mod source;
use crate::domain::{
    AcceptanceEvaluationAdmissionCause, EvaluationCitationMismatch, EvaluationEligibilityMismatch,
    EvaluationRootMismatch,
};
use admission::{CitationContext, EligibilityContext, SameSessionRefusal};
pub(in crate::storage::work) use source::check_moved_on;
#[cfg(test)]
use source::declared_not_contradicted;
pub(in crate::storage) use source::read_named_root_sighting_on;
use source::{
    NamedEvaluationRoot, RootBinding, RootPhase, RootSource, StaleCause, assess_named_root_binding,
    assess_named_root_source, judged_revision, judged_source, named_root_at_on, off_named_root,
    passing_citations, require_named_root_judged_source, revision_seen_through,
    stale_bound_citation,
};
mod history;
pub(crate) use history::{AssessedAcceptanceEvaluation, SourceObservationRecord};
mod identity_shape;
use identity_shape::{
    ExecutionIdentityFault, IdentityShape, VerdictFault, execution_identity_fault,
    record_verdicts_malformed, verdict_fault,
};
mod reroll;
mod same_turn;

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) use tests::{
    AdmissionTransportFixture, SourceRecoveryTransportFixture, admission_transport_fixture,
    long_declared_source, long_presented_source, source_recovery_transport_fixture,
};
