//! Reminders an obligation page gives: the fixed words for each kind of
//! open typed obligation, and whether more are open than the page shows.

use super::{VerificationKind, WorkObligationPage, WorkObligationState};

/// Fixed table from open typed obligations to words. Waiver authority and
/// identities stay host-private.
///
/// A historical page, whose run completed or is sealed, owes nothing: its
/// rows are read as stored, so it reminds of nothing.
pub(super) fn obligation_reminders(page: &WorkObligationPage) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if page.historical {
        return out;
    }
    for item in page
        .items
        .iter()
        .filter(|item| item.state == WorkObligationState::Open)
    {
        let words = match item.requirement.check_kind {
            VerificationKind::Test
                if crate::control::is_stock_source_change_obligation(
                    &item.rule,
                    &item.requirement,
                ) =>
            {
                super::evaluation_guidance::stock_source_change_reminder(item.completion_action)
            }
            VerificationKind::Test => {
                "tests have not run since your last source change — run them; the host records the result"
            }
            VerificationKind::Build => {
                "the build has not run since your last source change — run it; the host records the result"
            }
            VerificationKind::Lint => {
                "lint has not run since your last source change — run it; the host records the result"
            }
            VerificationKind::Review => {
                "a review is still owed for your last source change; the host records the result"
            }
            VerificationKind::Acceptance => {
                "acceptance verification is still owed; the host records the result"
            }
        };
        if !out.iter().any(|existing| existing == words) {
            out.push(words.into());
        }
    }
    // Only an open obligation the page leaves out is still owed; a completed
    // item's omitted obligations are all terminal. A page stored before the
    // open count existed says nothing about what it left out, so any omission
    // may hide an open one.
    let open_shown = page
        .items
        .iter()
        .filter(|item| item.state == WorkObligationState::Open)
        .count();
    let more_open = page
        .open_total
        .map_or(page.omitted_count > 0, |total| total > open_shown);
    if more_open {
        out.push("more obligations are open than shown here".into());
    }
    out
}
