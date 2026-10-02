//! How readers word execution a host observed without admission. Every
//! wording says the record is unadmitted, names its cause as unknown or as
//! the host's unverified assertion, and calls every check in it uncredited:
//! no reader may let it read as a turn, a pass or a cause established.

use crate::domain::{
    ObservationAccounting, ObservationAuditReason, ObservationCausality, ObservedSourceChange,
    RecordedOccurrence, UnadmittedExecutionObservation, VerificationKind, VerificationResult,
};

/// Bytes a displayed host value may take once escaped, in JSON or on a
/// terminal, whichever is longer. Stored values stay complete.
pub(crate) const DISPLAY_BUDGET_BYTES: usize = 64;

/// A host-supplied value bounded for display by its escaped size: each
/// character costs what its longest escape costs (a quote or backslash 2
/// bytes in JSON, a control character up to 6 in JSON and 10 on a terminal),
/// so a value of any content stays within the budget wherever it is shown. A
/// shortened value ends with its stored length.
pub(crate) fn displayed(value: &str) -> String {
    fn cost(ch: char) -> usize {
        let json = match ch {
            '"' | '\\' | '\u{8}' | '\u{c}' | '\n' | '\r' | '\t' => 2,
            ch if u32::from(ch) < 0x20 => 6,
            ch => ch.len_utf8(),
        };
        let terminal = if crate::domain::is_unsafe_rendered_text_char(ch) {
            10
        } else {
            ch.len_utf8()
        };
        json.max(terminal)
    }
    if value.chars().map(cost).sum::<usize>() <= DISPLAY_BUDGET_BYTES {
        return value.to_owned();
    }
    let mut spent = 0;
    let mut end = 0;
    for (index, ch) in value.char_indices() {
        spent += cost(ch);
        if spent > DISPLAY_BUDGET_BYTES {
            break;
        }
        end = index + ch.len_utf8();
    }
    format!("{}… ({} bytes)", &value[..end], value.len())
}

/// The change kind `next` deltas carry for an unadmitted observation.
pub(crate) const UNADMITTED_CHANGE_KIND: &str = "unadmitted_observation";

/// What was observed, in words.
pub(crate) fn occurrence_words(occurrence: &RecordedOccurrence) -> String {
    let change = |change: &ObservedSourceChange| {
        format!(
            "source change ({}) in workspace {}",
            change.detection(),
            displayed(change.workspace_id())
        )
    };
    match occurrence {
        RecordedOccurrence::UnadmittedTurn { source_change, .. } => format!(
            "a turn seen without admission, not a begun turn; {}",
            source_change
                .as_ref()
                .map_or_else(|| "no source change reported".to_owned(), change)
        ),
        RecordedOccurrence::InterTurnChange { source_change } => {
            format!("a change between turns; {}", change(source_change))
        }
        RecordedOccurrence::ObservedCheck { .. } => {
            "a check seen inside a turn without admission".to_owned()
        }
    }
}

/// Who caused it, as far as anyone knows.
pub(crate) fn cause_words(causality: &ObservationCausality) -> String {
    match causality {
        ObservationCausality::Unknown {} => "cause unknown".to_owned(),
        ObservationCausality::HostAssertion { claimed_actor, .. } => format!(
            "host-asserted cause, not verified: {}",
            displayed(&claimed_actor.actor_id)
        ),
    }
}

/// How the record entered accounting.
pub(crate) const fn accounting_words(accounting: &ObservationAccounting) -> &'static str {
    match accounting {
        ObservationAccounting::SourceChange { .. } => "accounted as a source change",
        ObservationAccounting::Repeat { .. } => "accounted as a repeat of an earlier change",
        ObservationAccounting::NoSourceChange {} => "no source change accounted",
        ObservationAccounting::AuditOnly { reason } => match reason {
            ObservationAuditReason::ExplicitAudit => "audit only",
            ObservationAuditReason::FinishedRun => "audit only: the run had finished",
            ObservationAuditReason::HistoricalBinding => "audit only: a historical claim",
            ObservationAuditReason::RootBasisMoved => "audit only: the source root had moved",
        },
    }
}

/// One line for a `next` delta. Its change kind already says unadmitted; a
/// bounded line keeps the cause, the credit and the accounting first and
/// loses only the occurrence's detail.
pub(crate) fn delta_summary(observation: &UnadmittedExecutionObservation) -> String {
    let cause = match &observation.causality {
        ObservationCausality::Unknown {} => "cause unknown",
        ObservationCausality::HostAssertion { .. } => "unverified cause",
    };
    let checks = match observation.occurrence.checks().len() {
        0 => String::new(),
        1 => "; 1 check uncredited".to_owned(),
        count => format!("; {count} checks uncredited"),
    };
    format!(
        "{cause}{checks}; {}; {}",
        accounting_words(&observation.accounting),
        occurrence_words(&observation.occurrence),
    )
}

pub(crate) const fn check_kind_word(kind: VerificationKind) -> &'static str {
    match kind {
        VerificationKind::Test => "test",
        VerificationKind::Build => "build",
        VerificationKind::Lint => "lint",
        VerificationKind::Review => "review",
        VerificationKind::Acceptance => "acceptance",
    }
}

pub(crate) const fn check_result_word(result: VerificationResult) -> &'static str {
    match result {
        VerificationResult::Passed => "reported passed",
        VerificationResult::Failed => "reported failed",
        VerificationResult::Indeterminate => "reported indeterminate",
    }
}
