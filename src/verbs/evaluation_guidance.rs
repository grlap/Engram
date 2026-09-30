//! Safe, bounded timing guidance for an evaluated item's open obligations,
//! and the words of the stock source-change reminder for any policy, both
//! chosen from what completion would do with the obligation. Exact obligation
//! ids and definitions remain in the host-only focus view.

use serde::Serialize;

use crate::storage::WorkObligationCompletionAction;
use crate::work_service::WorkObligationSummary;

use super::{VerificationKind, WorkObligationPage, WorkObligationState, short};

const TIMING: &str = "A check or waiver recorded after an evaluation's evidence basis makes it stale, unless the check passed on the source revision the evaluation declared. Resolve obligations marked action required before evaluation; done handles those marked no action before evaluation.";

#[derive(Clone, Debug, Serialize)]
pub(super) struct EvaluationObligations {
    /// The read cut for a pre-evaluation show; omitted on a post-write receipt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) read_cut: Option<i64>,
    pub(super) open_total: usize,
    pub(super) omitted_open: usize,
    /// Exact count when this page was read from current evaluated-policy state.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(super) action_required_total: Option<usize>,
    pub(super) items: Vec<EvaluationObligation>,
    pub(super) timing: &'static str,
}

#[derive(Clone, Debug, Serialize)]
pub(super) struct EvaluationObligation {
    /// A display ordinal, not a stable identity or waiver target.
    pub(super) label: String,
    pub(super) check_kind: VerificationKind,
    pub(super) remedy: &'static str,
    pub(super) action_required_before_evaluation: bool,
}

impl EvaluationObligations {
    pub(super) fn from_page(
        page: &WorkObligationPage,
        read_cut: Option<i64>,
        visible_limit: usize,
    ) -> Option<Self> {
        let open_total = page.open_total?;
        if open_total == 0 {
            return None;
        }
        let items = page
            .items
            .iter()
            .filter(|item| item.state == WorkObligationState::Open)
            .take(visible_limit)
            .enumerate()
            .map(|(index, item)| EvaluationObligation::new(index + 1, item))
            .collect::<Vec<_>>();
        Some(Self {
            read_cut,
            open_total,
            omitted_open: open_total.saturating_sub(items.len()),
            action_required_total: page.action_required_total,
            items,
            timing: TIMING,
        })
    }

    pub(super) fn omit_one(&mut self) -> bool {
        if self.items.pop().is_none() {
            return false;
        }
        self.omitted_open += 1;
        true
    }

    pub(super) fn minimal(&self) -> Self {
        Self {
            read_cut: self.read_cut,
            open_total: self.open_total,
            omitted_open: self.open_total,
            action_required_total: self.action_required_total,
            items: Vec::new(),
            timing: self.timing,
        }
    }

    pub(super) fn reminder_lines(&self) -> Vec<String> {
        let mut lines = vec![format!(
            "open obligations: {} total, {} not shown; {}",
            self.open_total, self.omitted_open, self.timing
        )];
        lines.extend(
            self.items
                .iter()
                .map(|item| format!("{}: {}", item.label, item.remedy)),
        );
        lines
    }

    pub(super) fn requires_action(&self) -> bool {
        self.action_required_total.is_some_and(|total| total > 0)
            || self.action_required_total.is_none()
                && (self
                    .items
                    .iter()
                    .any(|item| item.action_required_before_evaluation)
                    || self.omitted_open > 0)
    }
}

impl EvaluationObligation {
    fn new(position: usize, item: &WorkObligationSummary) -> Self {
        let kind = match item.requirement.check_kind {
            VerificationKind::Test => "test",
            VerificationKind::Build => "build",
            VerificationKind::Lint => "lint",
            VerificationKind::Review => "review",
            VerificationKind::Acceptance => "acceptance",
        };
        let pinned = if item.requirement.check_fingerprint.is_some() {
            ", pinned check"
        } else {
            ""
        };
        let action = item
            .completion_action
            .unwrap_or(WorkObligationCompletionAction::CheckOrWaiver);
        let remedy = match action {
            WorkObligationCompletionAction::DoneWaives => {
                "no action before evaluation; done records the source change as untested"
            }
            WorkObligationCompletionAction::DoneDisplaces => {
                "no action before evaluation; done records the source change as displaced"
            }
            WorkObligationCompletionAction::CheckOrWaiver => {
                "run the credited check or obtain an authorized waiver before evaluation"
            }
            WorkObligationCompletionAction::NameRootCheckOrWaiver => {
                "name a source root and run its credited check, or obtain an authorized waiver before evaluation"
            }
            WorkObligationCompletionAction::WaiverOnly => {
                "obtain an authorized human waiver before evaluation; a check in this root cannot satisfy this foreign change"
            }
        };
        Self {
            label: format!(
                "open obligation {position} ({} v{}, {kind}{pinned})",
                short(&item.rule.rule_id),
                item.rule.rule_version,
            ),
            check_kind: item.requirement.check_kind,
            remedy,
            action_required_before_evaluation: !matches!(
                action,
                WorkObligationCompletionAction::DoneWaives
                    | WorkObligationCompletionAction::DoneDisplaces
            ),
        }
    }
}

/// The stock source-change reminder, in the words of what `done` would do
/// with the obligation at this read. Completion classifies again; without a
/// classification the words promise neither an untested record nor a
/// particular refusal.
pub(super) fn stock_source_change_reminder(
    action: Option<WorkObligationCompletionAction>,
) -> &'static str {
    use WorkObligationCompletionAction as Action;
    match action {
        Some(Action::DoneWaives) => {
            "tests have not run since your last source change — run them; the host records the result, and done records the change as untested without one"
        }
        Some(Action::DoneDisplaces) => {
            "a source change made in another workspace before the root was named is open — no action is needed; done records it as displaced"
        }
        Some(Action::CheckOrWaiver) => {
            "tests have not run since a source change done cannot record as untested — run the credited check or obtain an authorized waiver; done refuses until one of them resolves it"
        }
        Some(Action::NameRootCheckOrWaiver) => {
            "tests have not run since a source change whose workspace is unknown — name a source root and run its credited check, or obtain an authorized waiver; done refuses until one of them resolves it"
        }
        Some(Action::WaiverOnly) => {
            "a source change made outside the named root while it was bound is open — only an authorized human waiver resolves it, since no check in a named root can; done refuses until then"
        }
        None => {
            "tests have not run since your last source change — run them; the host records the result, and done says whether it records the change as untested or needs a credited check or waiver"
        }
    }
}
