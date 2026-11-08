//! Work completion word handler and receipt shaping.

use super::{
    AgentVerbs, DateTime, DoneInput, EvaluationRemedy, Receipt, Utc, VerbError, WorkCompleteInput,
    WorkCompleteResult, WorkCompletionCaptureInput, WorkLifecycle, completion_recovery_reminder,
    json, nonempty, obligation_reminders, short,
};

impl AgentVerbs {
    /// `done`: complete the held item and disclose absent criterion evidence
    /// links from its frozen seal; a typed refusal says what is owed.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] when this session does not hold the item, nothing
    /// has been noted, or a lifecycle fence moved.
    pub fn done(&self, input: DoneInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        self.disclosing_focus(|| self.done_word(input, now))
    }

    fn done_word(&self, input: DoneInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        let completion = WorkCompleteInput {
            source_fingerprint: nonempty(input.source_fingerprint),

            source_id: input.source_id,
            landing: input.landing,
            links: input.links,
            link_basis: input.link_basis,
            capture: nonempty(input.summary).map(|summary| WorkCompletionCaptureInput {
                summary,
                refs: Vec::new(),
            }),
            evidence: Vec::new(),
            acceptance: None,
            note: nonempty(input.note),
            idempotency_key: String::new(),
        };
        // A malformed request is refused before its target is resolved, so
        // no refusal about the target can stand in front of it.
        crate::work_service::validate_completion_request(&completion)?;
        let view = self.target_unfocused("done", input.work_ref.as_deref(), now)?;
        let work_ref = view.status.work.short_ref.clone();
        let title = short(&view.status.work.title);
        let target = view.status.work.work_id.0.to_string();
        let result = self
            .service
            .work_complete_on(Some(&target), completion, now)
            .map_err(|error| VerbError::at(error, &work_ref))?;
        let retirement_candidates = matches!(result, WorkCompleteResult::Completed(_)).then(|| {
            self.service
                .project_memory_retirement_candidates(view.status.work.work_id, now)
        });
        let after = self.refreshed(&view, now)?;
        let child_resolution = match &result {
            WorkCompleteResult::Refused(refusal) => {
                super::super::child_obligations::ShowChildSuccessor::for_refusal(refusal)
            }
            WorkCompleteResult::Completed(_) => None,
        };
        let (lines, guidance, owed) = match &result {
            WorkCompleteResult::Completed(completed) => {
                let mut guidance = self.guidance(&after, "done", now);
                guidance.reminders.clear();
                // Finishing a child points back at the parent that is still
                // open, with the parent's own next commands.
                if let Some(parent_id) = after.status.work.parent_id
                    && let Ok(parent) = self.service.inspect_work(&parent_id.0.to_string(), now)
                    && parent.status.work.lifecycle == WorkLifecycle::Open
                {
                    let parent_ref = parent.status.work.short_ref.clone();
                    guidance = self.guidance(&parent, "done", now);
                    // Every reminder about the parent names the parent.
                    for reminder in &mut guidance.reminders {
                        *reminder = format!("{parent_ref}: {reminder}");
                    }
                    guidance.reminders.insert(
                        0,
                        format!(
                            "{parent_ref} \"{}\" is still open",
                            short(&parent.status.work.title)
                        ),
                    );
                }
                let mut lines = vec![
                    format!("done {work_ref} \"{title}\""),
                    format!(
                        "asserted {} acceptance {} satisfied; completion changed no criterion",
                        completed.acceptance_criteria_asserted,
                        if completed.acceptance_criteria_asserted == 1 {
                            "criterion"
                        } else {
                            "criteria"
                        }
                    ),
                    super::super::show::acceptance_provenance_line(
                        completed.acceptance_provenance.as_ref(),
                        self.service.display_identity(),
                    ),
                ];
                if completed.landing.is_some() || completed.landing_unavailable.is_some() {
                    lines.push(super::super::show::landing_line(
                        completed.landing.as_ref(),
                        completed.landing_unavailable,
                    ));
                }
                lines.extend(super::super::show::untested_change_lines(
                    &completed.obligation_page,
                ));
                lines.extend(super::super::show::displaced_change_lines(
                    &completed.obligation_page,
                ));
                (lines, guidance, false)
            }
            WorkCompleteResult::Refused(refusal) => {
                let mut guidance = self.guidance(&after, "done", now);
                // Which remedy a missing evaluation, or one the policy, the
                // mark or the evaluator's identity no longer admits, has
                // depends on the task's mark and what the project admits;
                // only those causes read the policy.
                let evaluation = if matches!(
                    refusal.recovery.cause,
                    crate::WorkCompletionRecoveryCause::MissingAcceptanceEvaluation { .. }
                        | crate::WorkCompletionRecoveryCause::AcceptanceEvaluationStale {
                            reason: crate::AcceptanceStaleReason::Policy
                                | crate::AcceptanceStaleReason::Identity
                        }
                ) {
                    EvaluationRemedy {
                        mark: (refusal.recovery.item.work_id == after.status.work.work_id)
                            .then_some(after.status.work.evaluation_mode)
                            .flatten(),
                        admitted: self
                            .service
                            .acceptance_evaluation_policy(now)
                            .map_err(|error| VerbError::at(error, &work_ref))?
                            .allowed_modes,
                    }
                } else {
                    EvaluationRemedy::default()
                };
                let mut reminder = completion_recovery_reminder(
                    &refusal.recovery,
                    refusal.recovery.item.work_id != after.status.work.work_id,
                    &evaluation,
                    self.argument_names,
                );
                if let Some(resolution) = &child_resolution {
                    reminder.push_str("; ");
                    reminder.push_str(&resolution.line());
                }
                guidance.reminders.push(reminder);
                guidance.next = vec![refusal.recovery.command.clone()];
                // The check an open obligation waits for follows the cause's
                // unchanged words as its own reminder, with its detail to read.
                if let (
                    Some(check),
                    crate::WorkCompletionRecoveryCause::OpenObligation { required_check, .. },
                ) = (
                    refusal.recovery.open_obligation_check.as_deref(),
                    &refusal.recovery.cause,
                ) {
                    guidance.reminders.push(
                        super::super::verification_assessment::open_obligation_check_line(
                            check,
                            *required_check,
                        ),
                    );
                    if let crate::domain::OpenObligationCheck::Newest { verification, .. } = check {
                        guidance.next.push(format!(
                            "engram work show {} --note {}",
                            refusal.recovery.item.short_ref,
                            verification.as_str()
                        ));
                    }
                }
                for reminder in obligation_reminders(&refusal.obligation_page) {
                    if !guidance.reminders.contains(&reminder) {
                        guidance.reminders.push(reminder);
                    }
                }
                (
                    vec![format!(
                        "not done {work_ref} \"{title}\": something is still owed"
                    )],
                    guidance,
                    true,
                )
            }
        };
        let value = match &result {
            WorkCompleteResult::Refused(refusal) => {
                super::super::child_obligations::done_refusal_value(
                    refusal,
                    child_resolution,
                    &self.service.display_identity(),
                )?
            }
            WorkCompleteResult::Completed(receipt) => {
                let mut value = json!({
                    "seal": receipt.seal,
                    "completed_at": receipt.completed_at,
                    "acceptance_criteria_asserted": receipt.acceptance_criteria_asserted,
                    "acceptance_criteria_changed": false,
                    "acceptance": super::super::show::acceptance_provenance_value(
                        receipt.acceptance_provenance.as_ref(),
                        self.service.display_identity(),
                    ),
                });
                if receipt.landing.is_some() || receipt.landing_unavailable.is_some() {
                    value["landing"] = super::super::show::landing_value(
                        receipt.landing.as_ref(),
                        receipt.landing_unavailable,
                    );
                }
                let untested = super::super::show::untested_changes(&receipt.obligation_page);
                if !untested.is_empty() {
                    value["untested_changes"] = serde_json::to_value(untested)?;
                }
                let omitted =
                    super::super::show::untested_changes_omitted(&receipt.obligation_page);
                if omitted > 0 {
                    value["untested_changes_omitted"] = json!(omitted);
                }
                let displaced = super::super::show::displaced_changes(&receipt.obligation_page);
                if !displaced.is_empty() {
                    value["foreign_workspace_changes"] = serde_json::to_value(displaced)?;
                }
                let omitted =
                    super::super::show::displaced_changes_omitted(&receipt.obligation_page);
                if omitted > 0 {
                    value["foreign_workspace_changes_omitted"] = json!(omitted);
                }
                value
            }
        };
        let receipt = self.finish_mutation(super::super::mutation::receipt(
            &after,
            "done",
            value,
            lines,
            guidance,
            self.holder(&after, now),
            owed,
        )?);
        if !owed {
            let children = self.service.remaining_optional_children(
                view.status.work.work_id,
                super::super::child_obligations::MAX_CHILD_OBLIGATION_REFS,
                now,
            );
            let facts = match &result {
                WorkCompleteResult::Completed(completed) => completed.acceptance_evidence.as_ref(),
                WorkCompleteResult::Refused(_) => None,
            };
            let error_class = match &result {
                WorkCompleteResult::Completed(completed) => {
                    completed.acceptance_evidence_error_class
                }
                WorkCompleteResult::Refused(_) => None,
            };
            let action = super::super::memory_retirement::RetirementAction::Completed;
            let reserve = retirement_candidates.as_ref().map_or(Ok(0), |candidates| {
                super::super::memory_retirement::reserve(
                    &receipt,
                    candidates,
                    &action,
                    self.argument_names,
                )
            })?;
            let composed = super::super::child_obligations::done_with_acceptance(
                &receipt,
                facts,
                error_class,
                &children,
                &work_ref,
                super::super::FOCUS_DISCLOSED_BUDGET.saturating_sub(reserve),
            )?;
            return match retirement_candidates.as_ref() {
                Some(candidates) => super::super::memory_retirement::append(
                    &composed,
                    candidates,
                    &action,
                    super::super::FOCUS_DISCLOSED_BUDGET,
                    self.argument_names,
                ),
                None => Ok(composed),
            };
        }
        Ok(receipt)
    }
}
