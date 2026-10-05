//! Which item a word acts on: the one it names, or the session's ambient
//! focus, refused when that focus is not held while other work is.

use super::{
    AgentVerbs, DateTime, Holder, StoreError, Utc, VerbError, WorkFocusView, WorkLifecycle,
};
use crate::storage::{IMPLICIT_TARGET_HELD_SHOWN, ImplicitFocusState};

impl AgentVerbs {
    /// The item `word` acts on: the named one, focused, or else the ambient
    /// focus under [`Self::implicit_target`].
    pub(super) fn target(
        &self,
        word: &str,
        work_ref: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<WorkFocusView, VerbError> {
        match work_ref {
            Some(work_ref) => {
                self.service
                    .select_work(work_ref, now)
                    .map_err(|error| VerbError::at(error, work_ref))?;
                self.service
                    .inspect_work(work_ref, now)
                    .map_err(|error| VerbError::at(error, work_ref))
            }
            None => self.implicit_target(word, now),
        }
    }

    /// The ambient focus, for a word that named no item. `add` and `add
    /// --under` focus the item they create, so a bare word right after one
    /// would otherwise act on that new item rather than the one the session
    /// holds. When the focus is not an item this session holds while it
    /// holds others, the word is refused and nothing is recorded; the caller
    /// names the item. A session that holds nothing, or whose focus is an
    /// item it holds, acts on the focus as before, except that a bare `done`
    /// or `evaluate` is refused while another claim is live beside it.
    fn implicit_target(&self, word: &str, now: DateTime<Utc>) -> Result<WorkFocusView, VerbError> {
        let focus = self.ambient_focus(now)?;
        if matches!(self.holder(&focus, now), Holder::You(_)) {
            // Completion and evaluation record a verdict on one item, so a
            // bare one acts only when that item is certain: refused,
            // recording nothing, while the session holds another live claim
            // beside its held focus. Checked before any write and, for
            // evaluate, before its attempt replay. A focus the session does
            // not hold falls to the refusal below, which offers the focus.
            if matches!(word, "done" | "evaluate") {
                let held = self.service.held_work_refs(now)?;
                if held.len() > 1 {
                    return Err(StoreError::WorkBareTargetAmbiguous(Box::new(
                        crate::storage::BareTargetAmbiguity::new(
                            word,
                            &focus.status.work.short_ref,
                            &held
                                .into_iter()
                                .map(|(_, short_ref)| short_ref)
                                .collect::<Vec<_>>(),
                        ),
                    ))
                    .into());
                }
            }
            return Ok(focus);
        }
        let held: Vec<String> = self
            .service
            .held_work_refs(now)?
            .into_iter()
            .filter(|(work_id, _)| *work_id != focus.status.work.work_id)
            .map(|(_, short_ref)| short_ref)
            .collect();
        if held.is_empty() {
            return Ok(focus);
        }
        let more = held.len().saturating_sub(IMPLICIT_TARGET_HELD_SHOWN);
        let focus_state = match self.holder(&focus, now) {
            Holder::Other(..) => ImplicitFocusState::HeldElsewhere,
            _ if focus.status.work.lifecycle != WorkLifecycle::Open => ImplicitFocusState::NotOpen,
            _ => ImplicitFocusState::Unclaimed,
        };
        Err(StoreError::WorkImplicitTargetConflict(Box::new(
            crate::storage::ImplicitTargetConflict {
                operation: word.to_owned(),
                focus: focus.status.work.short_ref.clone(),
                focus_state,
                focus_lifecycle: focus.status.work.lifecycle,
                held: held.into_iter().take(IMPLICIT_TARGET_HELD_SHOWN).collect(),
                more,
            },
        ))
        .into())
    }

    /// Resolves a named item without focusing it, for a word a non-holder may
    /// use (note, gate, evaluate): the service moves focus only for the
    /// item's live holder, so a peer's word leaves focus, and the claim the
    /// next host turn binds, where it was.
    pub(super) fn target_unfocused(
        &self,
        word: &str,
        work_ref: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<WorkFocusView, VerbError> {
        match work_ref {
            Some(work_ref) => self
                .service
                .inspect_work(work_ref, now)
                .map_err(|error| VerbError::at(error, work_ref)),
            None => self.implicit_target(word, now),
        }
    }

    /// The ambient focus, or the refusal that names no item is selected.
    pub(super) fn ambient_focus(&self, now: DateTime<Utc>) -> Result<WorkFocusView, VerbError> {
        self.focused(now)?.ok_or_else(|| {
            VerbError::from(StoreError::InvalidWork(
                "this session has no focused work; name the item or claim one first".into(),
            ))
        })
    }
}
