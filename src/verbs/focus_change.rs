//! A word's focus move, as its receipt or refusal discloses it.
//!
//! The change comes from the word's own journal (see the storage focus
//! journal), so it says only what this invocation did. It names the claim
//! fence and named root only when the word's own transactions captured that
//! binding, and it never says the host has rebound: the claiming turn keeps
//! the binding it was admitted with, and the new target is a candidate for
//! the host's next admission.

use serde_json::{Value, json};

use super::{AgentVerbs, Receipt, VerbError, terminal_safe_line};
use crate::storage::{FocusChange, MAX_DISCLOSED_WORKSPACE_JSON_BYTES};

/// One focus change rendered for both surfaces.
#[derive(Clone, Debug)]
pub(super) struct FocusDisclosure {
    pub(super) value: Value,
    pub(super) line: String,
}

impl FocusDisclosure {
    pub(super) fn of(change: &FocusChange) -> Self {
        let from = change.from.as_deref().unwrap_or("no focus");
        let to = change.to.as_deref().unwrap_or("no focus");
        let mut value = json!({ "from": change.from, "to": change.to });
        let line = match &change.binding {
            Some(binding) => {
                value["claim_id"] = json!(binding.claim_id);
                value["claim_fence"] = json!(binding.claim_fence);
                let root = match (&binding.workspace_id, binding.generation) {
                    (Some(workspace), Some(generation)) => {
                        value["generation"] = json!(generation);
                        // Shown only when both surfaces stay within the bound:
                        // JSON escapes some characters and terminal text others.
                        let shown = terminal_safe_line(workspace);
                        if encoded_len(workspace) <= MAX_DISCLOSED_WORKSPACE_JSON_BYTES
                            && shown.len() <= MAX_DISCLOSED_WORKSPACE_JSON_BYTES
                        {
                            value["workspace_id"] = json!(workspace);
                            format!(" and named root {shown}/{generation}")
                        } else {
                            value["workspace_id_omitted_bytes"] = json!(workspace.len());
                            format!(
                                " and named root generation {generation} (its {}-byte workspace name is not shown)",
                                workspace.len()
                            )
                        }
                    }
                    _ => String::new(),
                };
                // Shell text never carries a fence; the JSON names it.
                format!(
                    "focus moved from {from} to {to}; the host binds {to}'s claim{root} from its next turn, not this one"
                )
            }
            None => format!("focus moved from {from} to {to}; {to} has no live claim to bind"),
        };
        Self { value, line }
    }
}

fn encoded_len(text: &str) -> usize {
    serde_json::to_string(text).map_or(usize::MAX, |encoded| encoded.len())
}

impl Receipt {
    /// The receipt with the word's focus change: a top-level `focus_change`
    /// and one line after the headline.
    pub(super) fn with_focus_change(mut self, disclosure: &FocusDisclosure) -> Self {
        self.value["focus_change"] = disclosure.value.clone();
        let at = self.lines.len().min(1);
        self.lines.insert(at, disclosure.line.clone());
        self
    }
}

impl AgentVerbs {
    /// Runs one word that can move focus with a journal installed, and
    /// discloses the net move on its receipt, or on its refusal when the word
    /// moved focus and then refused. The word body runs synchronously on this
    /// thread; it must not await while the journal is installed.
    pub(super) fn disclosing_focus(
        &self,
        word: impl FnOnce() -> Result<Receipt, VerbError>,
    ) -> Result<Receipt, VerbError> {
        let journal = self.service.focus_journal();
        let outcome = word();
        let Some(change) = journal.finish() else {
            return outcome;
        };
        let disclosure = FocusDisclosure::of(&change);
        match outcome {
            Ok(receipt) => Ok(receipt.with_focus_change(&disclosure)),
            Err(error) => Err(error.with_focus_change(disclosure)),
        }
    }
}
