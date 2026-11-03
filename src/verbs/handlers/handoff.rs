//! Work handoff word handler.

use super::{
    AgentVerbs, DateTime, HandoffAction, HandoffInput, Receipt, SessionId, StoreError, Utc,
    VerbError, WorkHandoffInput, held_suffix, nonempty, short,
};

impl AgentVerbs {
    /// `handoff`: offer the held item, accept an offer, or cancel one.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] when no matching offer or claim exists.
    pub fn handoff(&self, input: HandoffInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        self.disclosing_focus(|| self.handoff_word(input, now))
    }

    fn handoff_word(&self, input: HandoffInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        // Refuse display labels before target binding can write focus or an
        // offer. This is a usability guard, not identity resolution or trust.
        if let HandoffAction::Offer { to, .. } = &input.action {
            let forwarded = to.trim();
            crate::storage::admit_session_id_text(forwarded)?;
            if crate::work_service::identity::is_display_label(forwarded) {
                return Err(StoreError::InvalidWork(
                    super::super::attribution::HANDOFF_DISPLAY_TARGET_REFUSAL.into(),
                )
                .into());
            }
        }
        // A bare handoff keeps the ambient focus without the implicit-target
        // guard: its recipient accepts an item it does not hold yet, and an
        // offer or a cancel already needs the claim, so it cannot act on an
        // item the caller did not mean.
        let view = match input.work_ref.as_deref() {
            Some(work_ref) => self.target_unfocused("handoff", Some(work_ref), now)?,
            None => self.ambient_focus(now)?,
        };
        let work_ref = view.status.work.short_ref.clone();
        let title = short(&view.status.work.title);
        let (core, verb) = match input.action {
            HandoffAction::Offer {
                to,
                summary,
                ttl_seconds,
            } => {
                let to = to.trim().to_owned();
                if to.is_empty() {
                    return Err(
                        StoreError::InvalidWork("say who receives the handoff".into()).into(),
                    );
                }
                (
                    WorkHandoffInput::Offer {
                        checkpoint_summary: nonempty(summary)
                            .unwrap_or_else(|| "handoff offered".into()),
                        to: to.clone(),
                        ttl_seconds,
                        idempotency_key: String::new(),
                    },
                    // Raw targets no longer reach this compact surface. The
                    // hostile-target framing test covers rich verbose output.
                    format!(
                        "offered {work_ref} \"{title}\" to {}",
                        self.service
                            .display_identity()
                            .session(&SessionId(to.clone()))
                    ),
                )
            }
            HandoffAction::Accept => (
                WorkHandoffInput::Accept {
                    idempotency_key: String::new(),
                },
                format!("accepted {work_ref} \"{title}\""),
            ),
            HandoffAction::Cancel { reason } => {
                let reason = reason.trim().to_owned();
                if reason.is_empty() {
                    return Err(
                        StoreError::InvalidWork("say why the handoff is cancelled".into()).into(),
                    );
                }
                (
                    WorkHandoffInput::Cancel {
                        reason,
                        idempotency_key: String::new(),
                    },
                    format!("cancelled handoff of {work_ref} \"{title}\""),
                )
            }
        };
        let target = view.status.work.work_id.0.to_string();
        let result = self
            .service
            .work_handoff_on(Some(&target), core, now)
            .map_err(|error| VerbError::at(error, &work_ref))?;
        let after = self.refreshed(&view, now)?;
        let holder = self.holder(&after, now);
        let line = format!("{verb}{}", held_suffix(holder, now));
        let guidance = self.guidance(&after, "handoff", now);
        // Like note's, this receipt is not fitted to the budget less the
        // focus reserve: it holds one line, the compact mutation receipt
        // (ids, refs, revisions and states only), obligation counts and the
        // allowed next words, all bounded and short, so it stays far below
        // that budget whatever the item holds.
        Ok(self.finish_mutation(Receipt::assemble(
            vec![line],
            guidance,
            serde_json::to_value(&result)?,
            false,
        )))
    }
}
