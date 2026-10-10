//! The field labels and refusal texts that storage and the checks made before
//! a word moves focus both give. Each is spelled once here, so the early
//! refusal and storage's later one cannot drift apart.

pub(crate) const NOTE_SUMMARY: &str = "note summary";
pub(crate) const CHECKPOINT_SUMMARY: &str = "checkpoint summary";
pub(crate) const EVIDENCE_SUMMARY: &str = "evidence summary";
pub(crate) const TITLE: &str = "title";
pub(crate) const OUTCOME: &str = "outcome";
pub(crate) const CHILD_TITLE: &str = "child title";
pub(crate) const CHILD_OUTCOME: &str = "child outcome";
pub(crate) const PRIORITY: &str = "priority";
pub(crate) const BLOCKER_DETAIL: &str = "blocker detail";
pub(crate) const RELEASE_REASON: &str = "release reason";
pub(crate) const REOPEN_REASON: &str = "reopen reason";
pub(crate) const WORK_DISPOSAL_REASON: &str = "work disposal reason";
pub(crate) const REQUIRED_CHILD_REJECTION_REASON: &str = "required-child rejection reason";
pub(crate) const REQUIRED_CHILD_WAIVER_REASON: &str = "required-child waiver reason";
pub(crate) const DETACH_REASON: &str = "detach reason";
pub(crate) const HANDOFF_CANCELLATION_REASON: &str = "handoff cancellation reason";
pub(crate) const ACCEPTANCE_CRITERION: &str = "acceptance criterion";
pub(crate) const RATIONALE: &str = "rationale";

pub(crate) const DUPLICATE_ACCEPTANCE_CRITERION: &str =
    "acceptance results contain a duplicate criterion";
pub(crate) const SELF_SUPERSEDE: &str = "work cannot supersede itself";
pub(crate) const WAIVER_NEEDS_DISPOSED_REQUIRED_CHILD: &str =
    "completion waiver requires a directly required cancelled or superseded child";
pub(crate) const HANDOFF_TO_ITSELF: &str = "handoff source and destination must differ";
pub(crate) const UNBOUND_LOCAL_ACTOR: &str =
    "local work requires a non-empty asserted actor and session binding";

/// A proposed prerequisite edge whose gated child is not among the proposal.
pub(crate) fn unknown_child_edge(work_key: &str) -> String {
    format!("prerequisite edge references unknown child {work_key:?}")
}
