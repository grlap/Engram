//! The MCP spelling of refusals, remedies and reminders that tell a caller to
//! pass an argument.
//!
//! The core and the words raise each such sentence in its CLI spelling, the
//! one every CLI surface and the raw host envelope keep. An MCP caller passes
//! fields, not flags, so the agent projection respells a registered sentence
//! where it ends a refusal's text. Only the registered sentences are respelled,
//! never an arbitrary `--flag`, and runnable `engram work …` commands stay CLI
//! syntax on every surface.

use std::borrow::Cow;
use std::sync::LazyLock;

pub(crate) use crate::argument_names::{ArgumentNames, Twin};

/// The reminder `add` gives when the caller supplied no criterion.
pub(super) const DEFAULTED_ACCEPTANCE_REMINDER: Twin = Twin {
    cli: "acceptance defaulted to the title being done; set --accept",
    mcp: "acceptance defaulted to the title being done; set acceptance",
};

/// A supplied blank evaluation mode, on `add` or `update`.
pub(super) const BLANK_EVALUATION_MODE_REFUSAL: Twin = Twin {
    cli: "evaluation mode must not be blank; pass same_session, sub_agent, or independent_session, or leave it out (--clear-evaluation-mode clears an existing pin)",
    mcp: "evaluation mode must not be blank; pass same_session, sub_agent, or independent_session, or leave it out (update's evaluation_mode action without evaluation_mode clears an existing pin)",
};

/// `remember` given a retirement target and its clear together.
pub(super) const RETIRES_WITH_COMBINED_REFUSAL: Twin = Twin {
    cli: "--retires-with and --clear-retires-with cannot be combined",
    mcp: "retires_with and clear_retires_with cannot be combined",
};

/// `add` asked for an optional item without a parent.
pub(super) const OPTIONAL_NEEDS_PARENT_REFUSAL: Twin = Twin {
    cli: "optional work needs a parent; use --under REF with --optional",
    mcp: "optional work needs a parent; pass under with optional",
};

/// `memories` given a revision outside a full read of one key.
pub(super) const REVISION_NEEDS_FULL_REFUSAL: Twin = Twin {
    cli: "--revision requires --full and a memory key",
    mcp: "revision requires full and a memory key in query",
};

/// `memories` given a full read and a continuation together.
pub(super) const FULL_WITH_AFTER_REFUSAL: Twin = Twin {
    cli: "--full cannot be combined with --after",
    mcp: "full cannot be combined with after",
};

/// `memories` asked for a full read without a key.
pub(super) const FULL_NEEDS_KEY_REFUSAL: Twin = Twin {
    cli: "--full requires a memory key",
    mcp: "full requires a memory key in query",
};

/// `ls` asked for ready and blocked work together.
pub(super) const READY_WITH_BLOCKED_REFUSAL: Twin = Twin {
    cli: "choose --ready or --blocked, not both",
    mcp: "choose ready or blocked, not both",
};

/// `ls` given a child-requirement filter without a single parent.
pub(super) const CHILD_FILTER_REFUSAL: Twin = Twin {
    cli: "choose --optional or --required with --under PARENT",
    mcp: "choose optional or required with under",
};

/// The listing hint when a page stopped at its row limit.
pub(super) const PAGE_LIMIT_HINT: Twin = Twin {
    cli: "page reached --limit; continue with the same filters and ordering",
    mcp: "page reached limit; continue with the same filters and ordering",
};

/// `update` with nothing to do.
pub(super) const UPDATE_NEEDS_ACTION_REFUSAL: Twin = Twin {
    cli: "update needs one action: --release, --blocked, --unblock, --cancel, or a field to change",
    mcp: "update needs one action: release, blocked, unblock, cancel, or revise with a field to change",
};

/// `show` asked for observations beside another window.
pub(super) const OBSERVATIONS_ALONE_REFUSAL: Twin = Twin {
    cli: "choose --observations with optional --after, alone",
    mcp: "choose observations with optional after, alone",
};

/// `show` asked for the evaluation window beside another window.
pub(super) const EVALUATIONS_ALONE_REFUSAL: Twin = Twin {
    cli: "choose --evaluations with optional --after, or --evaluation RECORD_ID alone",
    mcp: "choose evaluations with optional after, or evaluation RECORD_ID alone",
};

/// `show` given windows that do not combine.
pub(super) const SHOW_WINDOWS_REFUSAL: Twin = Twin {
    cli: "choose --notes [--gates] or --history with optional --after, --note LOCATOR with optional --after, or --full",
    mcp: "choose notes (with optional gates) or history with optional after, note LOCATOR with optional after, or full",
};

/// `remember` given a partial edit of both kinds.
pub(super) const APPEND_WITH_SECTION_REFUSAL: Twin = Twin {
    cli: "--append and --section are alternatives; choose one",
    mcp: "append and section are alternatives; choose one",
};

/// The `next` reminder that a clipped status line is not the whole status.
pub(super) const CLIPPED_STATUS: Twin = Twin {
    cli: "read full status via its --note locator before acting on approval or STOP conditions; a clipped prefix grants no permission",
    mcp: "read full status via its note locator before acting on approval or STOP conditions; a clipped prefix grants no permission",
};

/// `handoff` given a peer's display label as its target.
pub(super) const HANDOFF_LABEL_TARGET: Twin = Twin {
    cli: "a peer display label is not a handoff target; ask the host or coordinator for the recipient's real session id, then use handoff --to SESSION",
    mcp: "a peer display label is not a handoff target; ask the host or coordinator for the recipient's real session id, then pass it as handoff's to",
};

/// `gate` with neither a named item nor a focus.
pub(super) const GATE_NEEDS_TARGET: Twin = Twin {
    cli: "no item is selected for this gate; use gate NAME --work-ref REF",
    mcp: "no item is selected for this gate; pass work_ref",
};

/// `evaluate` with neither a named item nor a focus.
pub(super) const EVALUATE_NEEDS_TARGET: Twin = Twin {
    cli: "no item is selected for this evaluation; use evaluate REF --mode MODE …",
    mcp: "no item is selected for this evaluation; pass work_ref",
};

/// Every registered sentence, CLI spelling first. A sentence a refusal ends
/// with is respelled whole for an MCP caller.
static REGISTERED: LazyLock<Vec<(String, String)>> = LazyLock::new(|| {
    let twins = [
        DEFAULTED_ACCEPTANCE_REMINDER,
        BLANK_EVALUATION_MODE_REFUSAL,
        RETIRES_WITH_COMBINED_REFUSAL,
        OPTIONAL_NEEDS_PARENT_REFUSAL,
        REVISION_NEEDS_FULL_REFUSAL,
        FULL_WITH_AFTER_REFUSAL,
        FULL_NEEDS_KEY_REFUSAL,
        READY_WITH_BLOCKED_REFUSAL,
        CHILD_FILTER_REFUSAL,
        PAGE_LIMIT_HINT,
        UPDATE_NEEDS_ACTION_REFUSAL,
        OBSERVATIONS_ALONE_REFUSAL,
        EVALUATIONS_ALONE_REFUSAL,
        SHOW_WINDOWS_REFUSAL,
        APPEND_WITH_SECTION_REFUSAL,
        CLIPPED_STATUS,
        HANDOFF_LABEL_TARGET,
        GATE_NEEDS_TARGET,
        EVALUATE_NEEDS_TARGET,
        crate::storage::REVISE_NEEDS_KEY_REFUSAL,
        crate::storage::PARTIAL_EDIT_NEEDS_REVISE_REFUSAL,
        crate::storage::CLEAR_TARGET_NEEDS_REVISE_REFUSAL,
        crate::storage::FILTERED_SEARCH_AFTER_REFUSAL,
        crate::storage::UNSAFE_KEY_REFUSAL,
        crate::storage::CHILD_REQUIREMENT_NEEDS_PARENT_REFUSAL,
        crate::storage::AMBIGUOUS_LOCATOR_REFUSAL,
        crate::storage::CHECKPOINT_LOCATOR_REFUSAL,
        crate::storage::HISTORY_LOCATOR_REFUSAL,
        crate::storage::FOREIGN_LOCATOR_REFUSAL,
        crate::storage::CarriedFailureRefusal::Unacknowledged.remedy_twin(),
        crate::storage::CarriedFailureRefusal::NothingToSupersede.remedy_twin(),
        crate::work_service::ASSESSMENT_CONTINUATION_REFUSAL,
        crate::work_service::UNKNOWN_EVALUATION_REFUSAL,
        crate::work_service::READ_RUN_EVIDENCE_REMEDY,
        crate::work_service::MEASURE_SOURCE_REMEDY,
        crate::verbs::error_rendering::remedies::CATALOG_CURSOR_REMEDY,
        crate::verbs::error_rendering::remedies::SHOW_CURSOR_REMEDY,
        crate::verbs::error_rendering::remedies::CRITERION_LINK_REMEDY,
        crate::verbs::error_rendering::remedies::PEER_DECOMPOSITION_REMEDY,
        crate::domain::GATE_INPUT_TOO_LARGE_REMEDY,
    ];
    twins
        .into_iter()
        .map(|twin| (twin.cli.to_owned(), twin.mcp.to_owned()))
        .chain(std::iter::once(crate::domain::gate_ref_refusal_twin()))
        .collect()
});

/// Every registered sentence as (CLI, MCP) spellings, for the tests that
/// check each one.
#[cfg(test)]
pub(super) fn registered() -> &'static [(String, String)] {
    &REGISTERED
}

/// The sentences that can end a successful receipt's text: a reminder, the
/// listing hint or a completion refusal's remedy. A receipt is fitted to its
/// byte budget before it is respelled, so none of these may grow when
/// respelled; every other registered sentence ends refusals only.
pub(super) const RECEIPT_SENTENCES: [Twin; 3] = [
    CLIPPED_STATUS,
    PAGE_LIMIT_HINT,
    crate::work_service::MEASURE_SOURCE_REMEDY,
];

/// The text as the caller reads it: unchanged for the CLI; for MCP, a
/// registered sentence that ends it is respelled with the field names.
pub(crate) fn respell(names: ArgumentNames, text: &str) -> Cow<'_, str> {
    respell_from(
        names,
        text,
        REGISTERED
            .iter()
            .map(|(cli, mcp)| (cli.as_str(), mcp.as_str())),
    )
}

/// A successful receipt's text as the caller reads it: only the sentences a
/// receipt can carry are respelled, and none of them grows the fitted text.
pub(super) fn respell_receipt_text(names: ArgumentNames, text: &str) -> Cow<'_, str> {
    respell_from(
        names,
        text,
        RECEIPT_SENTENCES.iter().map(|twin| (twin.cli, twin.mcp)),
    )
}

fn respell_from<'text, 'table>(
    names: ArgumentNames,
    text: &'text str,
    mut table: impl Iterator<Item = (&'table str, &'table str)>,
) -> Cow<'text, str> {
    if names == ArgumentNames::Cli {
        return Cow::Borrowed(text);
    }
    table
        .find_map(|(cli, mcp)| {
            text.strip_suffix(cli)
                .map(|head| Cow::Owned(format!("{head}{mcp}")))
        })
        .unwrap_or(Cow::Borrowed(text))
}
