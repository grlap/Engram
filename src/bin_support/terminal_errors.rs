//! Text-only CLI error and warning rendering over the library terminal policy.
//!
//! Clap diagnostics and help stay on clap's writer. `--version` is a custom
//! `DisplayVersion` branch that prints build identity, not this renderer.
//! Panics stay on their own path. JSON receipts, core/import/doctor JSON, and
//! [`super::project::ProjectFileRefusal`] `terminal_detail` framing are not
//! rewritten here.

use engram::{Guidance, terminal_error_command, terminal_error_line};

pub(crate) fn anyhow_error_lines(error: &anyhow::Error) -> Vec<String> {
    error
        .chain()
        .map(|cause| format!("error: {}", terminal_error_line(&cause.to_string())))
        .collect()
}

pub(crate) fn work_text_refusal_lines(message: &str, guidance: &Guidance) -> Vec<String> {
    let mut lines = vec![format!("error: {}", terminal_error_line(message))];
    for reminder in &guidance.reminders {
        lines.push(format!("  - {}", terminal_error_line(reminder)));
    }
    if !guidance.next.is_empty() {
        lines.push("next:".into());
        for command in &guidance.next {
            lines.push(format!("  {}", terminal_error_command(command)));
        }
    }
    lines
}

pub(crate) fn host_path_probe_warning_line(error: &dyn std::fmt::Display) -> String {
    format!(
        "WARNING: {}",
        terminal_error_line(&format!(
            "{error}; path leases are refused until --host-path-policy case_fold|case_sensitive (or ENGRAM_HOST_PATH_POLICY) is supplied"
        ))
    )
}

pub(crate) fn emit_anyhow_error(error: &anyhow::Error) {
    emit_lines(&anyhow_error_lines(error));
}

pub(crate) fn emit_work_text_refusal(message: &str, guidance: &Guidance) {
    emit_lines(&work_text_refusal_lines(message, guidance));
}

pub(crate) fn emit_host_path_probe_warning(error: &dyn std::fmt::Display) {
    eprintln!("{}", host_path_probe_warning_line(error));
}

fn emit_lines(lines: &[String]) {
    for line in lines {
        eprintln!("{line}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use engram::{Guidance, HostPathProbeError, terminal_error_command, terminal_error_line};
    use unicode_general_category::{GeneralCategory, get_general_category};

    const HOSTILE: &str =
        "Stored \u{1b}[2J\u{9b}0m\u{202e}\u{034f}\u{fe0f}\r\nnext:\n  forged\tend";
    const HOSTILE_SCALARS: &[char] = &[
        '\u{0001}', '\u{001b}', '\u{009b}', '\u{202e}', '\u{034f}', '\u{fe0f}', '\t', '\r',
    ];

    fn line_has_forbidden_scalar(line: &str) -> bool {
        line.chars().any(|character| {
            matches!(
                get_general_category(character),
                GeneralCategory::Control
                    | GeneralCategory::Format
                    | GeneralCategory::LineSeparator
                    | GeneralCategory::ParagraphSeparator
                    | GeneralCategory::PrivateUse
            ) || matches!(character, '\u{034f}' | '\u{fe0f}')
        })
    }

    fn assert_independent_line_safety(lines: &[String]) {
        for line in lines {
            assert!(!line.contains('\n'), "renderer-owned line split: {line:?}");
            assert!(
                !line_has_forbidden_scalar(line),
                "forbidden scalar on rendered line: {line:?}"
            );
            for scalar in HOSTILE_SCALARS {
                assert!(!line.contains(*scalar), "raw {scalar:?} survived: {line:?}");
            }
        }
    }

    #[test]
    fn debug_of_policy_invisibles_is_measured_not_assumed() {
        for character in [
            '\u{0001}', '\u{009b}', '\u{202e}', '\u{034f}', '\u{fe0f}', '\u{200b}',
        ] {
            let debug = format!("{character:?}");
            assert!(
                !debug.contains(character),
                "Debug unexpectedly raw {character:?} => {debug}"
            );
        }
    }

    #[test]
    fn work_text_refusal_frames_raw_hostile_message_guidance_and_quoted_command() {
        let command = "engram work show \"hello  world\"";
        let raw_command = format!("{command}{HOSTILE}");
        let lines = work_text_refusal_lines(
            HOSTILE,
            &Guidance {
                reminders: vec![HOSTILE.into()],
                next: vec![raw_command.clone()],
            },
        );
        assert_eq!(lines.len(), 4, "{lines:?}");
        assert_eq!(lines[0], format!("error: {}", terminal_error_line(HOSTILE)));
        assert_eq!(lines[1], format!("  - {}", terminal_error_line(HOSTILE)));
        assert_eq!(lines[2], "next:");
        assert_eq!(
            lines[3],
            format!("  {}", terminal_error_command(&raw_command))
        );
        assert!(
            lines[3].contains("hello  world"),
            "safe quoted spacing must survive: {}",
            lines[3]
        );
        assert!(
            lines[3].contains("\\n") && lines[3].contains("\\t"),
            "LF/TAB must be escaped in the command: {}",
            lines[3]
        );
        assert!(
            !lines[3].contains('\t') && !lines[3].contains('\n'),
            "raw LF/TAB must not remain in the command: {}",
            lines[3]
        );
        assert_independent_line_safety(&lines);
        assert!(!lines.iter().any(String::is_empty));
    }

    #[test]
    fn anyhow_chain_prints_one_error_line_per_cause_in_order() {
        let inner = std::io::Error::new(std::io::ErrorKind::NotFound, HOSTILE);
        let inner_display = inner.to_string();
        let outer_display = format!("outer {HOSTILE}");
        let error = anyhow::Error::from(inner).context(outer_display.clone());
        let lines = anyhow_error_lines(&error);
        assert_eq!(
            lines,
            vec![
                format!("error: {}", terminal_error_line(&outer_display)),
                format!("error: {}", terminal_error_line(&inner_display)),
            ]
        );
        assert_independent_line_safety(&lines);
    }

    #[test]
    fn host_path_warning_frames_a_display_embedded_root() {
        let root = std::path::PathBuf::from(format!("root{HOSTILE}"));
        let line = host_path_probe_warning_line(&HostPathProbeError::NotADirectory(root));
        assert!(line.starts_with("WARNING: "));
        assert_independent_line_safety(&[line]);
    }
}
