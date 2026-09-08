//! Display-only identity shaping. Core errors and canonical audit remain raw.

use super::{AgentVerbs, Guidance, SessionId, StoreError, Value, VerbError, json};

pub(super) const HANDOFF_DISPLAY_TARGET_REFUSAL: &str = "a peer display label is not a handoff target; ask the host or coordinator for the recipient's real session id, then use handoff --to SESSION";

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
        error.guidance()
    }
    /// Render an agent refusal without a raw claim-holder identity. Other
    /// diagnostic text and caller-provided bodies are not a secrecy boundary.
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
            _ => error.to_string(),
        }
    }

    /// Apply the agent identity projection to a shared structured error. Host
    /// core consumers keep the original envelope; no input identity is resolved.
    #[must_use]
    pub fn project_error(&self, error: &VerbError, mut value: Value) -> Value {
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
        }
        value
    }
}
