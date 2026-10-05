//! The remedies of the shared error rendering that name a word's arguments,
//! each written in its CLI and its MCP spelling. The rendering emits the CLI
//! spelling; the agent projection gives an MCP caller the field names.

use crate::argument_names::{ArgumentNames, Twin};
use crate::storage::StoreError;

/// A refused listing continuation.
pub(crate) const CATALOG_CURSOR_REMEDY: Twin = Twin {
    cli: "repeat the listing without --after, then continue from the new token",
    mcp: "repeat the listing without after, then continue from the new token",
};
/// A refused show continuation.
pub(crate) const SHOW_CURSOR_REMEDY: Twin = Twin {
    cli: "repeat show without --after, then continue from the new token",
    mcp: "repeat show without after, then continue from the new token",
};
/// A refused criterion evidence link.
pub(crate) const CRITERION_LINK_REMEDY: Twin = Twin {
    cli: "read show for the current acceptance basis and show --notes --gates for existing current-run evidence; an explicit author link is not verification",
    mcp: "read show for the current acceptance basis and show with notes and gates for existing current-run evidence; an explicit author link is not verification",
};
/// A peer's refused decomposition of work it does not hold.
pub(crate) const PEER_DECOMPOSITION_REMEDY: Twin = Twin {
    cli: "ask the parent holder to add required children or prerequisites; a peer may use add --under REF --optional",
    mcp: "ask the parent holder to add required children or prerequisites; a peer may use add with under and optional",
};

/// The remedy of a project-memory refusal that names the memory's key, with
/// the `memories` and `remember` arguments spelled as the caller passes them.
pub(crate) fn project_memory_remedy(error: &StoreError, names: ArgumentNames) -> Option<String> {
    let mcp = names == ArgumentNames::Mcp;
    let read = |key: &str| {
        if mcp {
            format!("read memories with query {key} and full")
        } else {
            format!("read memories {key} --full")
        }
    };
    match error {
        StoreError::ProjectMemoryExists(key) => Some(if mcp {
            format!(
                "{}; use remember with key {key} and revise to retain history",
                read(key)
            )
        } else {
            format!(
                "{}; use remember with --key {key} --revise to retain history",
                read(key)
            )
        }),
        StoreError::ProjectMemoryRevisionConflict { key, .. } => {
            Some(format!("{} and reconcile before revising", read(key)))
        }
        StoreError::ProjectMemoryRevisionNotFound { key, .. } => {
            Some(format!("{} for history navigation", read(key)))
        }
        _ => None,
    }
}
