//! `evaluate`: host-submitted acceptance verdicts recorded under the ambient
//! session's attributed identity. The core validates and binds; nothing here
//! calls a model or judges relevance.
//!
//! The response is bounded by construction: the actual representation is
//! measured before the commit with the same envelope it will have afterwards,
//! and verdict rows beyond the budget are omitted with an exact count rather
//! than truncated silently or failed after the record exists.

use chrono::{DateTime, Utc};
use serde_json::json;

use super::{
    LocalWorkService, MAX_AGENT_WORK_RESPONSE_BYTES, WorkEvaluateInput, WorkEvaluateResult,
    WorkEvaluationBlocking, WorkEvaluationProjection, WorkEvaluationVerdictRow,
    WorkMutationReceipt, compact_text, ensure_agent_response_budget,
};
use crate::domain::{
    AcceptanceBasis, AcceptanceEvaluation, AcceptanceEvaluationMode, AcceptanceSourceBasis,
    AcceptanceVerdict, CriterionVerdictInput, EvaluatorModel, MAX_EVALUATOR_MODEL_SEGMENT_BYTES,
    RecordAcceptanceEvaluationRequest, SessionId, WorkItem, WorkRunId,
};
use crate::{DevelopmentNoopRedactor, ObjectId, SqliteStore, StoreError};

/// Bytes the service preflight leaves free for the `evaluate` word's own
/// JSON envelope on top of the fitted service result, so the word never
/// needs a post-commit refusal. The word receipt is the shared projection
/// (already inside the service envelope) plus these word-only fields, each
/// bounded by an admitted limit and counted at its worst-case compact JSON
/// escaping. Two of them admit interior control characters (a stored title
/// is only trimmed and its summary compaction keeps interior bytes; a
/// session id is only length-checked), and a control byte escapes to six
/// bytes (``); the others are hex, enum words, numbers, instants, or
/// the display pseudonym, which escape one to one: `operation` (~22),
/// `work` (`short_ref` 14, `title` at most `MAX_SUMMARY_BYTES` = 192 →
/// 1152 escaped, `lifecycle`, `revision`; ~1250), `claim` (`holder` label
/// "you" or a 29-byte pseudonym, allowed 64, plus an RFC 3339 instant;
/// ~150), the `hash` and `replayed` extras on the evaluation block (~90),
/// `obligations` counts (~45), `omissions` (at most one entry per `next`
/// section, seven; ~490), `full_detail` (~50), an empty `reminders` list and
/// one retained `next` command (~70), and `effective_session_id` at the
/// 64-byte session bound → 384 escaped (~410): about 2.5 KiB, under this
/// reserve with margin. Reminders and the extra `next` commands are shed by
/// the word before the reserve is relied upon, and the word's last resort
/// is a minimal measured provenance receipt. The verbs tests pin this
/// derivation by constructing that maximal envelope from a real receipt
/// with control-character fields, by pinning the minimal receipt's size,
/// and by measuring a real word receipt against its service result.
pub const EVALUATE_WORD_RESERVE: usize = 3072;

const SERVICE_ENVELOPE_BUDGET: usize = MAX_AGENT_WORK_RESPONSE_BYTES - EVALUATE_WORD_RESERVE;

impl LocalWorkService {
    /// Records one acceptance evaluation on the targeted item's active run.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the target or revision basis is stale, a
    /// word is unknown, a citation is malformed, the core refuses the record
    /// for policy, identity, criteria, provenance, or size reasons, or the
    /// row-free response envelope cannot fit the agent budget (checked
    /// before anything is recorded).
    pub(crate) fn work_evaluate_on(
        &self,
        input: &WorkEvaluateInput,
        now: DateTime<Utc>,
    ) -> Result<WorkEvaluateResult, StoreError> {
        let mut store = self.store_at(now)?;
        let target = self.resolve_target(&store, input.work_ref.as_deref())?;
        let basis = self.protocol_basis(&store, true, false, target, now)?;
        // An independent evaluator does not hold the item; its evaluation
        // leaves its focus where it was.
        self.focus_if_held(&mut store, target, basis.claim.as_ref(), now)?;
        let work = basis.focused_work.clone().ok_or_else(|| {
            StoreError::InvalidWorkProjection("evaluation attempt has no bound focused work".into())
        })?;
        // The run an attempt binds: the active run, or the latest run when
        // the item has since completed, so an exact resend can still find
        // the committed attempt. Fresh writes are admitted only further down.
        let run_id = match work.active_run_id {
            Some(run_id) => run_id,
            None => store
                .latest_work_run(work.work_id)?
                .map(|run| run.run_id)
                .ok_or_else(|| StoreError::AcceptanceEvaluationRefused {
                    work: work.work_id,
                    reason: "the item has no active run to evaluate".into(),
                })?,
        };
        let mode = AcceptanceEvaluationMode::parse(&input.mode).ok_or_else(|| {
            StoreError::InvalidWork(format!(
                "unknown evaluation mode {:?}; use same_session, sub_agent, or independent_session",
                input.mode
            ))
        })?;
        // A citation is what the agent saw: a note/gate locator from `show
        // --notes --gates`, resolved exactly as `done --link` resolves it
        // (observations, inherited members, and other runs refuse), or the
        // full hash of host-minted verification or environment evidence on
        // this run, which no locator window prints.
        let index = store.work_record_index(
            &self.project_id,
            work.work_id,
            crate::storage::WorkRecordKind::NotesWithGates,
        )?;
        let resolve_citation = |criterion: usize, value: &str| -> Result<ObjectId, StoreError> {
            if let Ok(hash) = value.parse::<ObjectId>()
                && store.host_minted_run_evidence(run_id, &hash)?
            {
                return Ok(hash);
            }
            store
                .resolve_criterion_evidence(
                    &self.project_id,
                    work.work_id,
                    run_id,
                    criterion,
                    value,
                    &index,
                )
                .map_err(|error| match error {
                    StoreError::WorkCriterionLinkInvalid { reason, .. } => {
                        StoreError::AcceptanceEvaluationRefused {
                            work: work.work_id,
                            reason: format!("criterion {criterion} cites {value}: {reason}"),
                        }
                    }
                    other => other,
                })
        };
        let verdicts = input
            .verdicts
            .iter()
            .map(|verdict| {
                Ok(CriterionVerdictInput {
                    criterion: verdict.criterion,
                    verdict: AcceptanceVerdict::parse(&verdict.verdict).ok_or_else(|| {
                        StoreError::InvalidWork(format!(
                            "unknown verdict {:?} for criterion {}; use pass, fail, insufficient_evidence, or needs_human",
                            verdict.verdict, verdict.criterion
                        ))
                    })?,
                    basis: AcceptanceBasis::parse(&verdict.basis).ok_or_else(|| {
                        StoreError::InvalidWork(format!(
                            "unknown basis {:?} for criterion {}; use observed, asserted, judgment, or human_required",
                            verdict.basis, verdict.criterion
                        ))
                    })?,
                    rationale: verdict.rationale.clone(),
                    evidence: verdict
                        .evidence
                        .iter()
                        .map(|value| resolve_citation(verdict.criterion, value))
                        .collect::<Result<Vec<_>, StoreError>>()?,
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        let evaluator_model = input
            .model
            .as_deref()
            .map(parse_evaluator_model)
            .transpose()?;
        let request = RecordAcceptanceEvaluationRequest {
            project_id: self.project_id.clone(),
            work_id: work.work_id,
            // The submitted basis, not the current revision: an exact resend
            // must produce the identity it produced when it was recorded.
            expected_work_revision: input.acceptance_basis,
            evaluated_through: input.evidence_basis,
            mode,
            execution_identity: input.execution_identity.clone(),
            parent_session: input.parent_session.clone().map(SessionId),
            evaluator_model,
            source_basis: input.source_fingerprint.clone().map(|fingerprint| {
                AcceptanceSourceBasis {
                    workspace_id: None,
                    fingerprint,
                }
            }),
            verdicts,
            evaluator: self.actor("work_evaluate", "record an acceptance evaluation"),
            attempt_key: input.attempt.clone(),
            recorded_at: now,
        };
        let full_detail = format!("engram work show {} --full", work.short_ref);

        // Preflight the actual representation before the commit. The item
        // receipt, guidance, and obligation page do not change with the
        // record, so the row prefix that fits now fits afterwards too; the
        // placeholder hash has the length of every real hash, and the attempt
        // key is the exact one the core will record.
        let attempt = crate::storage::acceptance_attempt_identity(&request, run_id)?;
        // An exact resend recovers the committed attempt before any fresh
        // write admission, whatever the item's revision or lifecycle is now;
        // a contradicting payload under the same explicit key conflicts here.
        if let Some(receipt) = store.replay_acceptance_evaluation(&attempt)? {
            let mut result = self.assemble(
                &store,
                &work,
                receipt.evaluation,
                true,
                project_record(&receipt.record, full_detail),
                now,
            )?;
            fit_projection(&mut result)?;
            ensure_agent_response_budget(&result, "work_evaluate")?;
            return Ok(result);
        }
        if work.revision != input.acceptance_basis {
            return Err(StoreError::WorkRevisionConflict {
                work: work.work_id,
                expected: input.acceptance_basis,
                current: work.revision,
            });
        }
        if work.active_run_id != Some(run_id) {
            return Err(StoreError::AcceptanceEvaluationRefused {
                work: work.work_id,
                reason: "the item has no active run to evaluate".into(),
            });
        }
        let preview = preview_projection(&request, &work, run_id, full_detail.clone(), attempt.key);
        let placeholder = ObjectId::from_canonical_bytes(b"acceptance evaluation preflight");
        let mut preflight = self.assemble(&store, &work, placeholder, false, preview, now)?;
        if !fit_projection(&mut preflight)? {
            return Err(StoreError::InvalidWorkProjection(
                "work_evaluate response envelope exceeds the agent protocol limit before any verdict row; nothing was recorded"
                    .into(),
            ));
        }
        let visible = preflight.projection.verdicts.len();

        let receipt = store.record_acceptance_evaluation(&request, &DevelopmentNoopRedactor)?;
        let mut projection = project_record(&receipt.record, full_detail);
        projection.verdicts.truncate(visible);
        projection.verdicts_omitted = projection.verdicts_total - projection.verdicts.len();
        let mut result = self.assemble(
            &store,
            &work,
            receipt.evaluation,
            receipt.replayed,
            projection,
            now,
        )?;
        // The envelope was measured before the commit; shedding here only
        // covers drift and keeps every count exact.
        fit_projection(&mut result)?;
        ensure_agent_response_budget(&result, "work_evaluate")?;
        Ok(result)
    }

    fn assemble(
        &self,
        store: &SqliteStore,
        work: &WorkItem,
        evaluation: ObjectId,
        replayed: bool,
        projection: WorkEvaluationProjection,
        now: DateTime<Utc>,
    ) -> Result<WorkEvaluateResult, StoreError> {
        // The compact mutation receipt keeps scalar facts only; the bounded
        // projection travels beside it as typed data.
        let mutation = WorkMutationReceipt {
            work_id: work.work_id,
            work_ref: work.short_ref.clone(),
            revision: work.revision,
            control_binding: None,
            result: json!({
                "evaluation": evaluation.as_str(),
                "replayed": replayed,
            }),
        };
        let update = self.work_update_result(
            store,
            "evaluate",
            work.work_id,
            serde_json::to_value(&mutation)?,
            now,
        )?;
        Ok(WorkEvaluateResult {
            operation: update.operation,
            receipt: update.receipt,
            evaluation,
            replayed,
            projection,
            obligations: update.obligations,
            obligation_page: update.obligation_page,
            allowed_next: update.allowed_next,
        })
    }
}

/// Keeps the longest verdict-row prefix whose whole response fits the
/// service share of the agent budget (the word's reserve stays free),
/// counting the rest as omitted. Returns whether the row-free envelope
/// itself fits.
fn fit_projection(result: &mut WorkEvaluateResult) -> Result<bool, StoreError> {
    let fits = |result: &WorkEvaluateResult| -> Result<bool, StoreError> {
        Ok(serde_json::to_vec(result)?.len() < SERVICE_ENVELOPE_BUDGET)
    };
    if fits(result)? {
        return Ok(true);
    }
    let rows = std::mem::take(&mut result.projection.verdicts);
    let total = result.projection.verdicts_total;
    let (mut lower, mut upper) = (0, rows.len());
    while lower < upper {
        let probe = lower + (upper - lower).div_ceil(2);
        result.projection.verdicts = rows[..probe].to_vec();
        result.projection.verdicts_omitted = total - probe;
        if fits(result)? {
            lower = probe;
        } else {
            upper = probe - 1;
        }
    }
    result.projection.verdicts = rows[..lower].to_vec();
    result.projection.verdicts_omitted = total - lower;
    fits(result)
}

/// The projection the response would carry, built from the request before
/// the record exists: the same rows and counts the record will produce.
pub(super) fn preview_projection(
    request: &RecordAcceptanceEvaluationRequest,
    work: &WorkItem,
    run_id: WorkRunId,
    full_detail: String,
    attempt_key: String,
) -> WorkEvaluationProjection {
    let mut inputs = request.verdicts.iter().collect::<Vec<_>>();
    inputs.sort_by_key(|verdict| verdict.criterion);
    let rows = inputs
        .iter()
        .map(|verdict| {
            let mut evidence = verdict.evidence.clone();
            evidence.sort();
            evidence.dedup();
            (
                verdict.criterion,
                verdict.verdict,
                verdict.basis,
                evidence.len(),
                work.acceptance
                    .get(verdict.criterion.wrapping_sub(1))
                    .map(String::as_str)
                    .unwrap_or_default(),
            )
        })
        .collect::<Vec<_>>();
    projection_from_rows(
        request.mode,
        work.revision,
        run_id,
        request.evaluated_through,
        &rows,
        request
            .source_basis
            .as_ref()
            .map(|basis| basis.fingerprint.clone()),
        attempt_key,
        full_detail,
    )
}

/// The projection of a recorded evaluation.
fn project_record(record: &AcceptanceEvaluation, full_detail: String) -> WorkEvaluationProjection {
    let rows = record
        .verdicts
        .iter()
        .enumerate()
        .map(|(index, verdict)| {
            (
                index + 1,
                verdict.verdict,
                verdict.basis,
                verdict.evidence.len(),
                verdict.criterion.as_str(),
            )
        })
        .collect::<Vec<_>>();
    projection_from_rows(
        record.mode,
        record.work_revision,
        record.run_id,
        record.evaluated_cut.position,
        &rows,
        record
            .source_basis
            .as_ref()
            .map(|basis| basis.fingerprint.clone()),
        record.attempt_key.clone(),
        full_detail,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "one projection is assembled from the same facts before and after the record exists"
)]
fn projection_from_rows(
    mode: AcceptanceEvaluationMode,
    work_revision: i64,
    run_id: WorkRunId,
    evaluated_cut: i64,
    rows: &[(usize, AcceptanceVerdict, AcceptanceBasis, usize, &str)],
    source_fingerprint: Option<String>,
    attempt_key: String,
    full_detail: String,
) -> WorkEvaluationProjection {
    let blocking = [
        AcceptanceVerdict::Fail,
        AcceptanceVerdict::InsufficientEvidence,
        AcceptanceVerdict::NeedsHuman,
    ]
    .into_iter()
    .find_map(|kind| {
        rows.iter()
            .find(|(_, verdict, _, _, _)| *verdict == kind)
            .map(
                |(position, verdict, _, _, criterion)| WorkEvaluationBlocking {
                    position: *position,
                    verdict: *verdict,
                    criterion: compact_text(criterion),
                },
            )
    });
    WorkEvaluationProjection {
        mode,
        work_revision,
        run_id,
        evaluated_cut,
        verdicts_total: rows.len(),
        verdicts_omitted: 0,
        verdicts: rows
            .iter()
            .map(
                |(position, verdict, basis, citations, _)| WorkEvaluationVerdictRow {
                    position: *position,
                    verdict: *verdict,
                    basis: *basis,
                    citations: *citations,
                },
            )
            .collect(),
        passed: rows
            .iter()
            .filter(|(_, verdict, _, _, _)| *verdict == AcceptanceVerdict::Pass)
            .count(),
        blocking,
        source_fingerprint,
        attempt_key,
        full_detail,
    }
}

/// `PROVIDER/MODEL[@VERSION]` as structured, asserted metadata. The segment
/// bounds are the domain's; the core checks them again at its write boundary.
fn parse_evaluator_model(value: &str) -> Result<EvaluatorModel, StoreError> {
    let invalid = || {
        StoreError::InvalidWork(format!(
            "evaluator model {value:?} must be PROVIDER/MODEL or PROVIDER/MODEL@VERSION with non-empty segments of at most {MAX_EVALUATOR_MODEL_SEGMENT_BYTES} bytes"
        ))
    };
    let (provider, rest) = value.split_once('/').ok_or_else(invalid)?;
    let (model, version) = match rest.split_once('@') {
        Some((model, version)) => (model, Some(version)),
        None => (rest, None),
    };
    let parsed = EvaluatorModel {
        provider: provider.trim().to_owned(),
        model: model.trim().to_owned(),
        version: version.map(|text| text.trim().to_owned()),
    };
    parsed.validate().map_err(|_| invalid())?;
    Ok(parsed)
}

#[cfg(test)]
mod tests;
