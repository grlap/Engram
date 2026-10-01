//! The identity facts an evaluation must carry, read the same way when it is
//! admitted and when it is consumed.
//!
//! Every mode records the evaluator's session, and `sub_agent` also records
//! the attested parent session. Admission refuses a request without them, so
//! an ordinary store never holds such a record; consumption still asks, so a
//! record that reached a store by import or edit cannot complete work.
//! Presence is all this owns: whether a session is registered, executes the
//! run or stands apart from it is decided elsewhere, at its own stage.

use super::{AcceptanceEvaluation, AcceptanceEvaluationMode, RecordAcceptanceEvaluationRequest};
use crate::domain::SessionId;

/// The recorded identity of one evaluation, borrowed from a request or a
/// stored record.
#[derive(Clone, Copy)]
pub(super) struct IdentityShape<'a> {
    mode: AcceptanceEvaluationMode,
    evaluator: Option<&'a SessionId>,
    parent: Option<&'a SessionId>,
}

impl<'a> IdentityShape<'a> {
    pub(super) fn of_request(request: &'a RecordAcceptanceEvaluationRequest) -> Self {
        Self {
            mode: request.mode,
            evaluator: request.evaluator.session_id.as_ref(),
            parent: request.parent_session.as_ref(),
        }
    }

    pub(super) fn of_record(record: &'a AcceptanceEvaluation) -> Self {
        Self {
            mode: record.mode,
            evaluator: record.evaluator.session_id.as_ref(),
            parent: record.parent_session.as_ref(),
        }
    }

    /// The evaluator's session, which every mode requires.
    pub(super) const fn evaluator(self) -> Option<&'a SessionId> {
        self.evaluator
    }

    /// The parent session a `sub_agent` evaluation requires; `None` when it
    /// is missing, and for every other mode, which carries none.
    pub(super) fn child_parent(self) -> Option<&'a SessionId> {
        if self.mode == AcceptanceEvaluationMode::SubAgent {
            self.parent
        } else {
            None
        }
    }

    /// Whether the record lacks a session its mode requires: the evaluator's
    /// in any mode, or a `sub_agent` record's parent.
    pub(super) fn lacks_required_session(self) -> bool {
        self.evaluator().is_none()
            || (self.mode == AcceptanceEvaluationMode::SubAgent && self.child_parent().is_none())
    }
}

#[cfg(test)]
mod tests {
    use super::IdentityShape;
    use crate::domain::{AcceptanceEvaluationMode as Mode, SessionId};

    fn shape<'a>(
        mode: Mode,
        evaluator: Option<&'a SessionId>,
        parent: Option<&'a SessionId>,
    ) -> IdentityShape<'a> {
        IdentityShape {
            mode,
            evaluator,
            parent,
        }
    }

    #[test]
    fn every_mode_requires_the_evaluator_and_sub_agent_its_parent() {
        let evaluator = SessionId("evaluator".into());
        let parent = SessionId("parent".into());
        for mode in [Mode::SameSession, Mode::IndependentSession] {
            assert!(!shape(mode, Some(&evaluator), None).lacks_required_session());
            assert!(shape(mode, None, None).lacks_required_session(), "{mode:?}");
            assert_eq!(
                shape(mode, Some(&evaluator), Some(&parent)).child_parent(),
                None
            );
        }
        assert!(!shape(Mode::SubAgent, Some(&evaluator), Some(&parent)).lacks_required_session());
        assert_eq!(
            shape(Mode::SubAgent, Some(&evaluator), Some(&parent)).child_parent(),
            Some(&parent)
        );
        for (evaluator, parent) in [
            (Some(&evaluator), None),
            (None, Some(&parent)),
            (None, None),
        ] {
            assert!(
                shape(Mode::SubAgent, evaluator, parent).lacks_required_session(),
                "{evaluator:?} {parent:?}"
            );
        }
    }
}
