//! Terminal framing for agent receipts and data.

use std::fmt::Write as _;

use super::{MAX_TEXT_NEXT_COMMANDS, is_unsafe_rendered_text_char};

pub(crate) fn render_agent_receipt_text(
    lines: &[String],
    reminders: &[String],
    next: &[String],
) -> String {
    let mut out = lines.join("\n");
    out.push_str("\nreminders:");
    if reminders.is_empty() {
        out.push_str(" none");
    }
    for reminder in reminders {
        out.push_str("\n  - ");
        out.push_str(reminder);
    }
    out.push_str("\nnext:");
    if next.is_empty() {
        out.push_str(" none");
    }
    for command in next.iter().take(MAX_TEXT_NEXT_COMMANDS) {
        out.push_str("\n  ");
        out.push_str(command);
    }
    if next.len() > MAX_TEXT_NEXT_COMMANDS {
        let _ = write!(
            &mut out,
            "\n  (+{} more)",
            next.len() - MAX_TEXT_NEXT_COMMANDS
        );
    }
    out
}

pub(crate) fn terminal_safe_multiline(text: &str) -> String {
    let mut safe = String::with_capacity(text.len());
    for character in text.chars() {
        if character == '\n' || character == '\t' || !is_unsafe_rendered_text_char(character) {
            safe.push(character);
        } else {
            safe.extend(character.escape_default());
        }
    }
    safe
}

/// Prefix-free single-line framing for success receipts, MCP text, and CLI
/// error, refusal, reminder, or warning lines.
///
/// Escapes unsafe controls and collapses whitespace so LF/TAB in data cannot
/// inject a renderer-owned line. This is not a store or JSON sanitizer.
#[must_use]
pub fn terminal_error_line(text: &str) -> String {
    terminal_safe_multiline(text)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

/// Prefix-free command framing for success-receipt / MCP next commands and
/// CLI error-receipt commands.
///
/// Escapes unsafe controls, including LF/TAB, and keeps every safe literal
/// byte (repeated spaces inside quoted arguments). Not a store or JSON sanitizer.
#[must_use]
pub fn terminal_error_command(text: &str) -> String {
    terminal_safe_multiline(text)
        .replace('\n', "\\n")
        .replace('\t', "\\t")
}

pub(super) fn terminal_safe_data_block(text: &str) -> String {
    terminal_safe_multiline(text)
        .split('\n')
        .map(|line| format!("  | {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}
