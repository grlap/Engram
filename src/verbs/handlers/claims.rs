//! `claim` and `claim --under` word handlers: holding one item, or a
//! parent's next ready child.

use super::{
    AgentVerbs, ClaimInput, ClaimUnderInput, DateTime, Receipt, Utc, VerbError, WorkUpdateInput,
    held_suffix, json, short,
};

impl AgentVerbs {
    /// `claim`: hold the item; later words default to it.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] when the item is unknown, held elsewhere, or the
    /// core does not admit claiming.
    pub fn claim(&self, input: ClaimInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        self.disclosing_focus(|| self.claim_word(input, now))
    }

    fn claim_word(&self, input: ClaimInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        let view = self.target("claim", Some(&input.work_ref), now)?;
        let work_ref = view.status.work.short_ref.clone();
        let target = view.status.work.work_id.0.to_string();
        let _result = self
            .service
            .work_update_on(
                Some(&target),
                WorkUpdateInput::Claim {
                    ttl_seconds: input.ttl_seconds,
                    recovery_reason: input
                        .recover
                        .map(|value| value.trim().to_owned())
                        .filter(|value| !value.is_empty()),
                    idempotency_key: String::new(),
                },
                now,
            )
            .map_err(|error| VerbError::at(error, &work_ref))?;
        let after = self.refreshed(&view, now)?;
        let lines = vec![format!(
            "claimed {work_ref} \"{}\"{}",
            short(&after.status.work.title),
            held_suffix(self.holder(&after, now), now)
        )];
        let guidance = self.guidance(&after, "claim", now);
        Ok(self.finish_mutation(super::super::mutation::receipt(
            &after,
            "claim",
            json!({}),
            lines,
            guidance,
            self.holder(&after, now),
            false,
        )?))
    }

    /// `claim --under PARENT`: select the parent's next ready child in the
    /// `ls --ready` order and hold it, in one core transaction; later words
    /// default to the child. A repeat renews the child this session already
    /// holds under the parent.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] when the parent is unknown, no child is ready, or
    /// the core does not admit the claim.
    pub fn claim_under(
        &self,
        input: ClaimUnderInput,
        now: DateTime<Utc>,
    ) -> Result<Receipt, VerbError> {
        self.disclosing_focus(|| self.claim_under_word(input, now))
    }

    fn claim_under_word(
        &self,
        input: ClaimUnderInput,
        now: DateTime<Utc>,
    ) -> Result<Receipt, VerbError> {
        let parent = self.target("claim", Some(&input.under), now)?;
        let parent_ref = parent.status.work.short_ref.clone();
        let target = parent.status.work.work_id.0.to_string();
        let result = self
            .service
            .work_update_on(
                Some(&target),
                WorkUpdateInput::ClaimNextReady {
                    ttl_seconds: input.ttl_seconds,
                    recovery_reason: input
                        .recover
                        .map(|value| value.trim().to_owned())
                        .filter(|value| !value.is_empty()),
                    idempotency_key: String::new(),
                },
                now,
            )
            .map_err(|error| VerbError::at(error, &parent_ref))?;
        let selection = &result.receipt.result;
        let renewed = selection["renewed"].as_bool().unwrap_or(false);
        let ready = selection["ready_count"].as_u64().unwrap_or(0);
        let position = selection["position"].as_u64();
        let after = self.target("claim", Some(result.receipt.work_ref.as_str()), now)?;
        let work_ref = after.status.work.short_ref.clone();
        let title = short(&after.status.work.title);
        let held = held_suffix(self.holder(&after, now), now);
        let lines = vec![if renewed {
            format!("renewed {work_ref} \"{title}\", already held under {parent_ref}{held}")
        } else {
            format!(
                "claimed {work_ref} \"{title}\", ready child {} of {ready} under {parent_ref}{held}",
                position.unwrap_or(1)
            )
        }];
        let guidance = self.guidance(&after, "claim", now);
        Ok(self.finish_mutation(super::super::mutation::receipt(
            &after,
            "claim",
            json!({
                "under": {
                    "parent_ref": parent_ref,
                    "position": position,
                    "ready_count": ready,
                    "renewed": renewed,
                }
            }),
            lines,
            guidance,
            self.holder(&after, now),
            false,
        )?))
    }
}
