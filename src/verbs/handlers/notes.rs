//! Note capture and its committed response.

use super::{
    AgentVerbs, DateTime, NoteInput, Receipt, StoreError, Utc, VerbError, WorkLifecycle,
    held_suffix, observation_guidance, short, trimmed,
};

impl AgentVerbs {
    /// `note`: record an attributed observation; only a live holder also
    /// checkpoints its execution. Non-holders need no claim on open work.
    ///
    /// # Errors
    ///
    /// Returns [`VerbError`] for empty text, invalid project/lifecycle binding,
    /// or a stale holder authority basis.
    pub fn note(&self, input: &NoteInput, now: DateTime<Utc>) -> Result<Receipt, VerbError> {
        self.service.note_writer_admission_word(|| {
            self.disclosing_focus(|| {
                self.committed_mutation(|committed| self.note_word(input, now, committed))
            })
        })
    }

    fn note_word(
        &self,
        input: &NoteInput,
        now: DateTime<Utc>,
        committed: &mut Option<crate::work_service::CommittedMutation>,
    ) -> Result<Receipt, VerbError> {
        let text = input.text.trim().to_owned();
        if text.is_empty() {
            return Err(StoreError::InvalidWork("note text must not be empty".into()).into());
        }
        let view = self.target_unfocused("note", input.work_ref.as_deref(), now)?;
        let work_ref = view.status.work.short_ref.clone();
        let target = view.status.work.work_id.0.to_string();
        let refs = trimmed(&input.refs);
        let result = self
            .service
            .work_note_with_commit_on(
                Some(&target),
                &crate::work_service::WorkNoteIntent {
                    status: input.status,
                    summary: &text,
                    refs: &refs,
                },
                now,
                committed,
            )
            .map_err(|error| VerbError::at(error, &work_ref))?;
        crate::work_service::CommittedMutation::phase(committed, "receipt_enrichment");
        let after = self.refreshed(&view, now)?;
        let mut guidance = self.guidance(&after, "note", now);
        // Chosen from the recorded result, not from who holds the item now:
        // a claim or a replay after the note cannot turn it into execution.
        if result.non_holder && after.status.work.lifecycle == WorkLifecycle::Open {
            observation_guidance(&mut guidance, &work_ref);
        }
        let value = super::super::mutation::NoteResult::from(&result);
        let observation = if result.non_holder {
            " (observation, no run credit)"
        } else {
            ""
        };
        let lines = vec![format!(
            "noted on {work_ref} \"{}\"{observation}: {}{}",
            short(&after.status.work.title),
            short(&text),
            held_suffix(self.holder(&after, now), now)
        )];
        Ok(self.finish_mutation(super::super::mutation::receipt(
            &after,
            "note",
            value,
            lines,
            guidance,
            self.holder(&after, now),
            false,
        )?))
    }
}
