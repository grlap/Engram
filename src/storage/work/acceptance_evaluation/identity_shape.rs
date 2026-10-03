//! The shape an evaluation must carry, read the same way when it is admitted
//! and when it is consumed: its identity facts and its verdicts' own fields.
//!
//! Every mode records the evaluator's session, and `sub_agent` also records
//! the attested parent session and a bounded execution identity, which no
//! other mode carries. Each verdict has a rationale and a bounded citation
//! list, and a pass cites at least one record and never rests on a
//! `human_required` basis. Admission refuses a request without that shape,
//! so an ordinary store never holds such a record; consumption still asks,
//! so a record that reached a store by import or edit cannot complete work.
//! The shape is all this owns: whether a session is registered, executes the
//! run or stands apart from it, and whether a citation is on the run and
//! before the cut, is decided elsewhere, at its own stage.

use super::{
    AcceptanceBasis, AcceptanceEvaluation, AcceptanceEvaluationMode, AcceptanceVerdict,
    MAX_ACCEPTANCE_VERDICT_CITATIONS, MAX_EXECUTION_IDENTITY_BYTES,
    RecordAcceptanceEvaluationRequest,
};
use crate::domain::SessionId;

/// Why a `sub_agent` evaluation's execution identity is refused.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ExecutionIdentityFault {
    /// Missing, or blank.
    Missing,
    /// Longer than the bound, or holding a control character.
    OutOfBounds,
}

/// The fault in a `sub_agent` evaluation's execution identity, if any.
pub(super) fn execution_identity_fault(identity: Option<&str>) -> Option<ExecutionIdentityFault> {
    match identity.filter(|identity| !identity.trim().is_empty()) {
        None => Some(ExecutionIdentityFault::Missing),
        Some(identity)
            if identity.len() > MAX_EXECUTION_IDENTITY_BYTES
                || identity.chars().any(char::is_control) =>
        {
            Some(ExecutionIdentityFault::OutOfBounds)
        }
        Some(_) => None,
    }
}

/// Why one verdict's own fields are refused, in the order admission checks
/// them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum VerdictFault {
    BlankRationale,
    TooManyCitations,
    PassWithoutCitation,
    PassOnHumanRequired,
}

/// The first fault in one verdict's own fields, if any.
pub(super) fn verdict_fault(
    verdict: AcceptanceVerdict,
    basis: AcceptanceBasis,
    rationale: &str,
    citations: usize,
) -> Option<VerdictFault> {
    let pass = verdict == AcceptanceVerdict::Pass;
    if rationale.trim().is_empty() {
        Some(VerdictFault::BlankRationale)
    } else if citations > MAX_ACCEPTANCE_VERDICT_CITATIONS {
        Some(VerdictFault::TooManyCitations)
    } else if pass && citations == 0 {
        Some(VerdictFault::PassWithoutCitation)
    } else if pass && basis == AcceptanceBasis::HumanRequired {
        Some(VerdictFault::PassOnHumanRequired)
    } else {
        None
    }
}

/// Whether a stored record's verdicts have a shape admission refuses: none
/// at all, not one per recorded criterion in its order, or one whose own
/// fields are refused. Each verdict is matched to the criterion at its own
/// position, by text.
pub(super) fn record_verdicts_malformed(record: &AcceptanceEvaluation) -> bool {
    record.verdicts.is_empty()
        || record.verdicts.len() != record.criteria.len()
        || record
            .verdicts
            .iter()
            .zip(&record.criteria)
            .any(|(verdict, criterion)| {
                verdict.criterion != *criterion
                    || verdict_fault(
                        verdict.verdict,
                        verdict.basis,
                        &verdict.rationale,
                        verdict.evidence.len(),
                    )
                    .is_some()
            })
}

/// The recorded identity of one evaluation, borrowed from a request or a
/// stored record.
#[derive(Clone, Copy)]
pub(super) struct IdentityShape<'a> {
    mode: AcceptanceEvaluationMode,
    evaluator: Option<&'a SessionId>,
    parent: Option<&'a SessionId>,
    execution: Option<&'a str>,
}

impl<'a> IdentityShape<'a> {
    pub(super) fn of_request(request: &'a RecordAcceptanceEvaluationRequest) -> Self {
        Self {
            mode: request.mode,
            evaluator: request.evaluator.session_id.as_ref(),
            parent: request.parent_session.as_ref(),
            execution: request.execution_identity.as_deref(),
        }
    }

    pub(super) fn of_record(record: &'a AcceptanceEvaluation) -> Self {
        Self {
            mode: record.mode,
            evaluator: record.evaluator.session_id.as_ref(),
            parent: record.parent_session.as_ref(),
            execution: record.execution_identity.as_deref(),
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

    /// Whether the record's identity is one admission refuses: a session its
    /// mode requires is missing, or a `sub_agent` record's execution identity
    /// is missing, blank or out of bounds, or its parent session is not a
    /// session id admission accepts.
    pub(super) fn identity_defect(self) -> bool {
        self.lacks_required_session()
            || (self.mode == AcceptanceEvaluationMode::SubAgent
                && (execution_identity_fault(self.execution).is_some()
                    || self
                        .child_parent()
                        .is_some_and(|parent| crate::storage::admit_session_id(parent).is_err())))
    }

    /// Whether a record of a mode other than `sub_agent` carries the parent
    /// session or execution identity only `sub_agent` records.
    pub(super) const fn stray_child_metadata(self) -> bool {
        !matches!(self.mode, AcceptanceEvaluationMode::SubAgent)
            && (self.execution.is_some() || self.parent.is_some())
    }
}

#[cfg(test)]
mod tests {
    use super::{
        ExecutionIdentityFault, IdentityShape, VerdictFault, execution_identity_fault,
        verdict_fault,
    };
    use crate::domain::{
        AcceptanceBasis as Basis, AcceptanceEvaluationMode as Mode, AcceptanceVerdict as Verdict,
        MAX_ACCEPTANCE_VERDICT_CITATIONS, MAX_EXECUTION_IDENTITY_BYTES, SessionId,
    };

    fn shape<'a>(
        mode: Mode,
        evaluator: Option<&'a SessionId>,
        parent: Option<&'a SessionId>,
    ) -> IdentityShape<'a> {
        IdentityShape {
            mode,
            evaluator,
            parent,
            execution: (mode == Mode::SubAgent).then_some("child-execution"),
        }
    }

    #[test]
    fn a_sub_agent_execution_identity_is_present_and_bounded() {
        let exact = "x".repeat(MAX_EXECUTION_IDENTITY_BYTES);
        // A multibyte identity at the byte bound counts its bytes.
        let multibyte = "\u{e9}".repeat(MAX_EXECUTION_IDENTITY_BYTES / 2);
        assert_eq!(multibyte.len(), MAX_EXECUTION_IDENTITY_BYTES);
        for admitted in ["child", exact.as_str(), multibyte.as_str()] {
            assert_eq!(execution_identity_fault(Some(admitted)), None, "{admitted}");
        }
        for missing in [None, Some(""), Some(" \t")] {
            assert_eq!(
                execution_identity_fault(missing),
                Some(ExecutionIdentityFault::Missing),
                "{missing:?}"
            );
        }
        let over = format!("{exact}x");
        let over_multibyte = format!("{multibyte}\u{e9}");
        for refused in [over.as_str(), over_multibyte.as_str(), "child\nexecution"] {
            assert_eq!(
                execution_identity_fault(Some(refused)),
                Some(ExecutionIdentityFault::OutOfBounds),
                "{refused}"
            );
        }
        let (evaluator, parent) = (SessionId("evaluator".into()), SessionId("parent".into()));
        let child = |execution| IdentityShape {
            mode: Mode::SubAgent,
            evaluator: Some(&evaluator),
            parent: Some(&parent),
            execution,
        };
        assert!(!child(Some("child")).identity_defect());
        assert!(child(None).identity_defect());
        assert!(child(Some(over.as_str())).identity_defect());
        assert!(!child(Some("child")).stray_child_metadata());
        // Only sub_agent records carry either field.
        for mode in [Mode::SameSession, Mode::IndependentSession] {
            let plain = IdentityShape {
                mode,
                evaluator: Some(&evaluator),
                parent: None,
                execution: None,
            };
            assert!(!plain.stray_child_metadata() && !plain.identity_defect());
            assert!(
                IdentityShape {
                    execution: Some("child"),
                    ..plain
                }
                .stray_child_metadata()
            );
            assert!(
                IdentityShape {
                    parent: Some(&parent),
                    ..plain
                }
                .stray_child_metadata()
            );
        }
    }

    #[test]
    fn a_verdicts_own_fields_are_checked_in_admissions_order() {
        let bound = MAX_ACCEPTANCE_VERDICT_CITATIONS;
        assert_eq!(
            verdict_fault(Verdict::Pass, Basis::Judgment, "why", 1),
            None
        );
        assert_eq!(
            verdict_fault(Verdict::Pass, Basis::Judgment, "why", bound),
            None
        );
        assert_eq!(
            verdict_fault(Verdict::Fail, Basis::Judgment, "why", 0),
            None
        );
        assert_eq!(
            verdict_fault(Verdict::Fail, Basis::HumanRequired, "why", 0),
            None
        );
        assert_eq!(
            verdict_fault(Verdict::Pass, Basis::Judgment, " ", 0),
            Some(VerdictFault::BlankRationale)
        );
        assert_eq!(
            verdict_fault(Verdict::Fail, Basis::Judgment, "why", bound + 1),
            Some(VerdictFault::TooManyCitations)
        );
        assert_eq!(
            verdict_fault(Verdict::Pass, Basis::Judgment, "why", 0),
            Some(VerdictFault::PassWithoutCitation)
        );
        assert_eq!(
            verdict_fault(Verdict::Pass, Basis::HumanRequired, "why", 1),
            Some(VerdictFault::PassOnHumanRequired)
        );
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
