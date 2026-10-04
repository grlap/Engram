//! Display-only identity shaping. Core errors and canonical audit remain raw.

use super::{AgentVerbs, Guidance, SessionId, StoreError, Value, VerbError, json};

pub(super) const HANDOFF_DISPLAY_TARGET_REFUSAL: &str = "a peer display label is not a handoff target; ask the host or coordinator for the recipient's real session id, then use handoff --to SESSION";

/// The work item a refusal concerns, for the refusals whose agent rendering
/// names it by short reference instead of its raw work id. The host/core
/// envelope keeps the raw id; this is the agent projection only.
fn refused_work(error: &StoreError) -> Option<crate::domain::WorkId> {
    match error {
        StoreError::WorkNotFound(work)
        | StoreError::WorkNotOpen(work)
        | StoreError::WorkPrerequisiteAlreadySatisfied(work)
        | StoreError::WorkRevisionConflict { work, .. }
        | StoreError::WorkClaimMismatch { work }
        | StoreError::WorkClaimLapsed { work, .. }
        | StoreError::WorkReleaseWaiverRequired { work }
        | StoreError::WorkCompletionRefused { work, .. }
        | StoreError::WorkBoundVerificationRefused { work, .. }
        | StoreError::WorkCompletionRecoveryRequired { work, .. }
        | StoreError::AcceptanceCriteriaRequired { work }
        | StoreError::AcceptanceEvaluationRefused { work, .. }
        | StoreError::AcceptanceEvaluationAdmissionRefused { work, .. }
        | StoreError::AcceptanceEvaluationCarriedFailure { work, .. }
        | StoreError::AcceptanceEvaluationBasisMoved { work, .. }
        | StoreError::OpenWorkObligations { work, .. } => Some(*work),
        StoreError::WorkPeerDecompositionRefused { parent } => Some(*parent),
        StoreError::WorkDetachRefused { work_id, .. } => Some(*work_id),
        _ => None,
    }
}

/// The work item a refusal's own message renders first, before any reason,
/// criterion or other text a caller may have supplied: the listed refusals
/// except the two whose message names no item at all.
fn work_named_first_in_message(error: &StoreError) -> Option<crate::domain::WorkId> {
    match error {
        StoreError::WorkPeerDecompositionRefused { .. } | StoreError::WorkDetachRefused { .. } => {
            None
        }
        other => refused_work(other),
    }
}

impl AgentVerbs {
    /// Keep the claim refusal's navigation while attributing its holder with
    /// the same display label used by the item and compact lists.
    #[must_use]
    pub fn error_guidance(&self, error: &VerbError) -> Guidance {
        if let StoreError::WorkClaimHeld { holder, .. } = &error.error {
            let label = self
                .service
                .display_identity()
                .session(&SessionId(holder.clone()));
            return error.guidance_with_holder(&label);
        }
        let mut guidance = error.guidance();
        // A reminder that repeats the refusal's message word for word is
        // replaced by the agent message, which names the item by short
        // reference. Every other reminder is left as written: it may carry a
        // reason or criterion a caller supplied.
        if work_named_first_in_message(&error.error).is_some() {
            let raw_message = error.error.to_string();
            for reminder in &mut guidance.reminders {
                if *reminder == raw_message {
                    *reminder = self.error_message(error);
                }
            }
        }
        guidance
    }
    /// Render an agent refusal without a raw claim-holder identity, and with
    /// the work item it concerns named by short reference. Other diagnostic
    /// text and caller-provided bodies are not a secrecy boundary and are
    /// never rewritten.
    #[must_use]
    pub fn error_message(&self, error: &VerbError) -> String {
        match &error.error {
            StoreError::WorkClaimHeld {
                work,
                holder,
                expires_at,
            } => format!(
                "work {} is claimed by {} until {}",
                super::short_ref_for_work_id(*work),
                self.service
                    .display_identity()
                    .session(&SessionId(holder.clone())),
                super::receipts::claim_expiry_text(*expires_at),
            ),
            // The candidates are listed by full work id: a short reference
            // that collides cannot tell them apart.
            StoreError::WorkReferenceAmbiguous {
                reference,
                candidates,
                more,
            } => format!(
                "work reference {reference:?} is ambiguous; use a full work id for one of {}; {more} additional candidates omitted",
                candidates
                    .iter()
                    .map(|candidate| candidate.work_id.0.to_string())
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            other => {
                let message = other.to_string();
                // Each of these messages renders its own work item first,
                // before any caller-supplied reason or criterion, so only that
                // first rendering of the raw id becomes the short reference.
                match work_named_first_in_message(other) {
                    Some(work) => message.replacen(
                        &format!("{work:?}"),
                        &super::short_ref_for_work_id(work),
                        1,
                    ),
                    None => message,
                }
            }
        }
    }

    /// Apply the agent identity projection to a shared structured error. Host
    /// core consumers keep the original envelope; no input identity is resolved.
    #[must_use]
    pub fn project_error(&self, error: &VerbError, mut value: Value) -> Value {
        // A word that moved focus before it refused says so beside the error.
        if let Some(disclosure) = error.focus_change() {
            value["focus_change"] = disclosure.value.clone();
        }
        if let StoreError::WorkClaimHeld { work, holder, .. } = &error.error {
            if let Some(Value::Object(details)) = value.pointer_mut("/error/details") {
                details.remove("holder_session_id");
                details.remove("work_id");
                details.insert(
                    "work_ref".into(),
                    json!(super::short_ref_for_work_id(*work)),
                );
                details.insert(
                    "holder".into(),
                    json!(
                        self.service
                            .display_identity()
                            .session(&SessionId(holder.clone()))
                    ),
                );
            }
            if let Some(Value::Object(fields)) = value.get_mut("error") {
                fields.insert("message".into(), json!(self.error_message(error)));
            }
            return value;
        }
        let ambiguous = matches!(error.error, StoreError::WorkReferenceAmbiguous { .. });
        let work = refused_work(&error.error);
        if work.is_none() && !ambiguous {
            return value;
        }
        // The item the refusal concerns is named by short reference; scoped
        // evidence, seal and evaluation ids and the candidates' full ids stay.
        if let (Some(work), Some(Value::Object(details))) =
            (work, value.pointer_mut("/error/details"))
            && details.remove("work_id").is_some()
        {
            details.insert("work_ref".into(), json!(super::short_ref_for_work_id(work)));
        }
        if let Some(Value::Object(fields)) = value.get_mut("error") {
            fields.insert("message".into(), json!(self.error_message(error)));
        }
        value
    }
}
