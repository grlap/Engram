//! `gate` and `evaluate` word handlers: recording a quality-gate observation
//! and recording an attributed acceptance evaluation on an item's active run.

use super::{
    AgentVerbs, DateTime, EvaluateInput, GateInput, MAX_AGENT_WORK_RESPONSE_BYTES, Receipt,
    StoreError, Utc, VerbError, held_suffix, json, minimal_evaluate_receipt, normalize_gate_input,
    short,
};

impl AgentVerbs {
    /// `gate`: record an observation on held open work or completed focus.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] when the input exceeds the documented bounds,
    /// text contains unsafe control/format characters, or this session does
    /// not hold the item.
    pub fn gate(&self, input: GateInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        let normalized = normalize_gate_input(&input)?;
        let GateInput {
            work_ref: target_ref,
            name,
            failed,
            evidence_ref,
        } = input;
        let view = self.target_unfocused(target_ref.as_deref(), now).map_err(|error| {
            if matches!(&error.error, StoreError::InvalidWork(reason) if reason.contains("no focused work")) {
                VerbError::from(StoreError::InvalidWork(super::super::GATE_WORK_REF_REQUIRED.into()))
            } else {
                error
            }
        })?;
        let work_ref = view.status.work.short_ref.clone();
        let passed = normalized.failed.is_empty();
        let _result = self
            .service
            .work_gate_on(
                Some(&view.status.work.work_id.0.to_string()),
                &name,
                &failed,
                evidence_ref.as_deref(),
                now,
            )
            .map_err(|error| VerbError::at(error, &work_ref))?;
        let after = self.refreshed(&view, now)?;
        let guidance = self.guidance(&after, "gate", now);
        let value = json!({"gate": {
            "name": &normalized.name,
            "passed": passed,
            "failed_count": normalized.failed.len(),
            "referenced": normalized.evidence_ref.is_some(),
        }});
        let state = if passed {
            "passed".to_owned()
        } else {
            format!("failed ({} failures)", normalized.failed.len())
        };
        let lines = vec![format!(
            "recorded gate {} {state} on {work_ref} \"{}\"{}",
            short(&normalized.name),
            short(&after.status.work.title),
            held_suffix(self.holder(&after, now), now)
        )];
        Ok(self.finish_mutation(super::super::mutation::receipt(
            &after,
            "gate",
            value,
            lines,
            guidance,
            self.holder(&after, now),
            false,
        )?))
    }

    /// `evaluate`: record one attributed acceptance evaluation on an item's
    /// active run. The core validates structure and provenance and binds the
    /// record; relevance stays the evaluator's judgment.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] when no item is targeted, the revision basis is
    /// stale, a word or citation is malformed, or the core refuses the record
    /// for policy, identity, criteria, or provenance reasons.
    pub fn evaluate(&self, input: EvaluateInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        let view = self.target_unfocused(input.work_ref.as_deref(), now).map_err(|error| {
            if matches!(&error.error, StoreError::InvalidWork(reason) if reason.contains("no focused work")) {
                VerbError::from(StoreError::InvalidWork(
                    super::super::EVALUATE_WORK_REF_REQUIRED.into(),
                ))
            } else {
                error
            }
        })?;
        let work_ref = view.status.work.short_ref.clone();
        let result = self
            .service
            .work_evaluate_on(
                &crate::WorkEvaluateInput {
                    work_ref: Some(view.status.work.work_id.0.to_string()),
                    mode: input.mode,
                    acceptance_basis: input.acceptance_basis,
                    evidence_basis: input.evidence_basis,
                    verdicts: input.verdicts,
                    attempt: input.attempt,
                    source_fingerprint: input.source_fingerprint,
                    model: input.model,
                    execution_identity: input.execution_identity,
                    parent_session: input.parent_session,
                },
                now,
            )
            .map_err(|error| VerbError::at(error, &work_ref))?;
        let after = self.refreshed(&view, now)?;
        let guidance = self.guidance(&after, "evaluate", now);
        let mut projection = result.projection;
        let outcome = match &projection.blocking {
            None => "all criteria pass".to_owned(),
            Some(blocking) => format!(
                "{} on \"{}\"",
                blocking.verdict.word(),
                short(&blocking.criterion)
            ),
        };
        let replay = if result.replayed { " (replayed)" } else { "" };
        let lines = vec![format!(
            "recorded {} evaluation on {work_ref} \"{}\": {}/{} pass, {outcome}{replay}{}",
            projection.mode.word(),
            short(&after.status.work.title),
            projection.passed,
            projection.verdicts_total,
            held_suffix(self.holder(&after, now), now)
        )];
        // The receipt is bounded by explicit omission of trailing verdict
        // rows; every count stays exact and the full read is named. The
        // finished receipt (with the process-default session metadata) is
        // what the shared strict budget rule measures. The service fitted
        // its own envelope with the word's reserve left free, so the row-free
        // form fits by construction; shedding guidance below is defensive.
        let mut guidance = guidance;
        loop {
            let mut evaluation = serde_json::to_value(&projection).map_err(StoreError::from)?;
            if let Some(object) = evaluation.as_object_mut() {
                object.insert("hash".into(), json!(result.evaluation.as_str()));
                object.insert("replayed".into(), json!(result.replayed));
            }
            let receipt = self.finish_mutation(super::super::mutation::receipt(
                &after,
                "evaluate",
                json!({ "evaluation": evaluation }),
                lines.clone(),
                guidance.clone(),
                self.holder(&after, now),
                false,
            )?);
            if super::super::receipts::agent_receipt_fits(&receipt, MAX_AGENT_WORK_RESPONSE_BYTES)?
            {
                return Ok(receipt);
            }
            if projection.verdicts.pop().is_some() {
                projection.verdicts_omitted += 1;
            } else if guidance.reminders.pop().is_some() {
            } else if guidance.next.len() > 1 {
                guidance.next.pop();
            } else {
                // Unreachable under the reserve derivation; if it were ever
                // reached, the answer is the minimal provenance receipt, which
                // is bounded by construction (a test pins its size) and
                // measured in debug builds. By contract no post-commit budget
                // error exists: the committed record is always answered.
                let minimal = self.finish_mutation(minimal_evaluate_receipt(
                    &work_ref,
                    after.status.work.revision,
                    &projection,
                    &result.evaluation,
                    result.replayed,
                ));
                debug_assert!(super::super::receipts::agent_receipt_fits(
                    &minimal,
                    MAX_AGENT_WORK_RESPONSE_BYTES
                )?);
                return Ok(minimal);
            }
        }
    }
}
