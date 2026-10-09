//! Display-only identity shaping. Core errors and canonical audit remain raw.

use std::borrow::Cow;

use super::argument_wording::{ArgumentNames, blocker_reason, respell, respell_receipt_text};
use super::{AgentVerbs, Guidance, SessionId, StoreError, Value, VerbError, json};

pub(super) const HANDOFF_DISPLAY_TARGET_REFUSAL: &str =
    super::argument_wording::HANDOFF_LABEL_TARGET.cli;

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
        | StoreError::WorkAncestorNotOpen { work, .. }
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
/// criterion or other text a caller may have supplied. Excludes refusals naming
/// no item or an item other than the refused work, such as the blocking ancestor.
fn work_named_first_in_message(error: &StoreError) -> Option<crate::domain::WorkId> {
    match error {
        StoreError::WorkPeerDecompositionRefused { .. }
        | StoreError::WorkDetachRefused { .. }
        | StoreError::WorkAncestorNotOpen { .. } => None,
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
            return error.guidance_with_holder(&label, self.argument_names);
        }
        let mut guidance = error.guidance_with_holder("another session", self.argument_names);
        // A reminder that repeats the refusal's message word for word is
        // replaced by the agent message, which names the item by short
        // reference and its arguments as this caller passes them. Any other
        // reminder keeps its words, a reason or criterion a caller supplied
        // among them; only a registered sentence that names arguments and
        // ends it is respelled for an MCP caller.
        let raw_message = error.error.to_string();
        for reminder in &mut guidance.reminders {
            if let Some(projected) = blocker_reason(reminder) {
                *reminder = respell(self.argument_names, projected).into_owned();
                continue;
            }
            if *reminder == raw_message {
                *reminder = self.error_message(error);
            } else if let Cow::Owned(spelled) = respell(self.argument_names, reminder) {
                *reminder = spelled;
            }
        }
        guidance
    }
    /// Render an agent refusal without a raw claim-holder identity, with the
    /// work item it concerns named by short reference, and with a registered
    /// sentence that names arguments, when it ends the refusal, spelled as
    /// this caller passes them. Other diagnostic text and caller-provided
    /// bodies are not a secrecy boundary and are never rewritten.
    #[must_use]
    pub fn error_message(&self, error: &VerbError) -> String {
        let message = self.error_message_with_short_refs(error);
        match respell(self.argument_names, &message) {
            Cow::Owned(spelled) => spelled,
            Cow::Borrowed(_) => message,
        }
    }

    fn error_message_with_short_refs(&self, error: &VerbError) -> String {
        match &error.error {
            StoreError::InvalidWork(reason) if blocker_reason(reason).is_some() => format!(
                "local work input is invalid: {}",
                blocker_reason(reason).expect("known blocker reason"),
            ),
            StoreError::WorkAncestorNotOpen { work, ancestor } => format!(
                "execution for work {} blocked by ancestor {} ({:?})",
                super::short_ref_for_work_id(*work),
                ancestor.short_ref,
                ancestor.lifecycle,
            ),
            StoreError::WorkCompletionRecoveryRequired { work, cause, .. } => format!(
                "completion for work {} requires recovery: {}",
                super::short_ref_for_work_id(*work),
                super::receipts::completion_recovery_cause_text(cause),
            ),
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
        // The item the refusal concerns is named by short reference; scoped
        // evidence, seal and evaluation ids and the candidates' full ids stay.
        if let (Some(work), Some(Value::Object(details))) = (
            refused_work(&error.error),
            value.pointer_mut("/error/details"),
        ) && details.remove("work_id").is_some()
        {
            details.insert("work_ref".into(), json!(super::short_ref_for_work_id(work)));
        }
        if let StoreError::WorkCompletionRecoveryRequired {
            cause: crate::WorkCompletionRecoveryCause::RequiredChildUnsealed { child },
            ..
        } = &error.error
            && let Some(Value::Object(cause)) = value.pointer_mut("/error/details/cause")
        {
            cause.insert("child".into(), json!(super::short_ref_for_work_id(*child)));
        }
        if let StoreError::InvalidWork(reason) = &error.error
            && let Some(projected) = blocker_reason(reason)
            && let Some(Value::Object(details)) = value.pointer_mut("/error/details")
        {
            details.insert(
                "reason".into(),
                json!(respell(self.argument_names, projected)),
            );
        }
        // An MCP caller reads the arguments a reason or remedy names as the
        // fields it passes.
        if self.argument_names == ArgumentNames::Mcp
            && let Some(Value::Object(details)) = value.pointer_mut("/error/details")
        {
            for key in ["reason", "remedy"] {
                if let Some(Value::String(text)) = details.get_mut(key)
                    && let Cow::Owned(spelled) = respell(self.argument_names, text)
                {
                    *text = spelled;
                }
            }
            if let Some(remedy) = crate::verbs::error_rendering::remedies::project_memory_remedy(
                &error.error,
                ArgumentNames::Mcp,
            ) {
                details.insert("remedy".into(), json!(remedy));
            }
        }
        if let Some(Value::Object(fields)) = value.get_mut("error") {
            fields.insert("message".into(), json!(self.error_message(error)));
        }
        value
    }

    /// A successful receipt as this caller reads it: for MCP, a sentence a
    /// receipt can carry that names arguments and ends a reminder, the listing
    /// hint or a completion refusal's remedy is spelled with the field names;
    /// none of them grows the already fitted receipt. Lines only the CLI
    /// prints and runnable commands are left as they are.
    #[must_use]
    pub(crate) fn spell_receipt(&self, mut receipt: super::Receipt) -> super::Receipt {
        if self.argument_names == ArgumentNames::Cli {
            return receipt;
        }
        let names = self.argument_names;
        let spell = |text: &mut String| {
            if let Cow::Owned(spelled) = respell_receipt_text(names, text) {
                *text = spelled;
            }
        };
        receipt.reminders.iter_mut().for_each(spell);
        if let Some(Value::Array(reminders)) = receipt.value.get_mut("reminders") {
            for reminder in reminders {
                if let Value::String(text) = reminder {
                    spell(text);
                }
            }
        }
        for key in ["hint", "remedy"] {
            if let Some(Value::String(text)) = receipt.value.get_mut(key) {
                spell(text);
            }
        }
        receipt
    }
}
