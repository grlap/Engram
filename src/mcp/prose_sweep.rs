//! Test-only guard: over MCP, a sentence that tells the caller to pass an
//! argument names the field, so no CLI flag appears in an answer's prose
//! outside an embedded runnable command. Every MCP answer a test produces is
//! swept, so a sentence added later without its MCP spelling fails the
//! tests that reach it.
//!
//! The swept prose is the system text an answer carries: a refusal's message,
//! reason, remedy and reminders, or a receipt's reminders, hint, remedy and
//! memory-retirement instruction. Other fields carry caller data (titles,
//! bodies, notes) that keeps whatever the caller wrote, so they are not swept.

use serde_json::Value;

/// The system prose of one MCP answer.
fn answer_prose(value: &Value) -> Vec<&str> {
    let (source, mut texts) = match value.get("error") {
        Some(error) => (
            error,
            vec![
                &error["message"],
                &error["details"]["reason"],
                &error["details"]["remedy"],
            ],
        ),
        None => (
            value,
            vec![
                &value["hint"],
                &value["remedy"],
                &value["memory_retirement"]["instruction"],
            ],
        ),
    };
    if let Some(Value::Array(reminders)) = source.get("reminders") {
        texts.extend(reminders);
    }
    texts.into_iter().filter_map(Value::as_str).collect()
}

/// The text with every embedded runnable command removed: an `engram …`
/// command, or anything in backticks, runs through the CLI, so it keeps its
/// flags. A bare command ends at a backtick, a semicolon, a comma before a
/// space, or a line end.
fn without_commands(text: &str) -> String {
    let mut kept = String::new();
    for (index, part) in text.split('`').enumerate() {
        // Odd parts lie between backticks.
        if index % 2 == 1 {
            continue;
        }
        let mut rest = part;
        while let Some(start) = rest.find("engram ") {
            kept.push_str(&rest[..start]);
            let command = &rest[start..];
            let end = [";", ", ", "\n"]
                .iter()
                .filter_map(|stop| command.find(stop))
                .min()
                .unwrap_or(command.len());
            rest = &command[end..];
        }
        kept.push_str(rest);
    }
    kept
}

/// Asserts that no CLI flag appears in an MCP answer's system prose outside
/// an embedded runnable command.
pub(crate) fn assert_prose_names_fields(value: &Value) {
    for text in answer_prose(value) {
        let prose = without_commands(text);
        let flag = prose.match_indices("--").find(|(at, _)| {
            prose[at + 2..].starts_with(|c: char| c.is_ascii_lowercase())
                && prose[..*at]
                    .chars()
                    .next_back()
                    .is_none_or(|c| c.is_whitespace() || "('\"".contains(c))
        });
        assert!(flag.is_none(), "a CLI flag in MCP prose: {text}");
    }
}

#[cfg(test)]
mod tests {
    use super::assert_prose_names_fields;
    use serde_json::json;

    #[test]
    fn the_prose_sweep_ignores_runnable_commands_and_catches_flags() {
        assert_prose_names_fields(&json!({
            "reminders": [
                "inspect it with engram work show w-1 --notes --gates; then decide",
                "add at least one criterion with `engram work update REF --accept \"criterion\"`, then retry",
                "read it with engram work show w-1 --full, then retry",
                "run `engram doctor --repair-projections` explicitly",
                "a caller wrote a--b and x --",
            ],
            "hint": "page reached limit",
            "memory_retirement": {"instruction": "revise it with retires_with local:w-1"},
            "title": "a caller title may say --anything",
        }));
        for value in [
            json!({"reminders": ["pass --key KEY"]}),
            json!({"reminders": ["read it with engram work show w-1, then pass --full"]}),
            json!({"hint": "page reached --limit"}),
            json!({"memory_retirement": {"instruction": "revise it with --retires-with local:w-1"}}),
            json!({"error": {"message": "x", "details": {"remedy": "use (--after)"}}}),
            json!({"error": {"message": "--full requires a memory key", "details": null}}),
        ] {
            let caught = std::panic::catch_unwind(|| assert_prose_names_fields(&value));
            assert!(caught.is_err(), "{value}");
        }
    }
}
