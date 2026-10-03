//! CLI project-file refusal before store opening or session attribution.
//! This boundary never searches for, selects, or creates a replacement project.

use std::{
    env,
    fmt::Write as _,
    fs, io,
    path::{Path, PathBuf},
};

use serde_json::{Value, json};

const SELECTION: &str = "Project selection is cwd-based: relative --project-file paths (default .engram-project) resolve from the current directory, without searching ancestors. An absolute --project-file path selects that file explicitly.";
const REMEDY: &str = "Change to the intended project directory, or replace PROJECT_DIRECTORY in the next command with its absolute path. No project was selected or created.";
const NEXT: &str = "engram --project-file 'PROJECT_DIRECTORY/.engram-project' work next";

#[derive(Debug, thiserror::Error)]
#[error("{reason}")]
pub(crate) struct ProjectFileRefusal {
    reason: String,
    kind: &'static str,
    project_file: PathBuf,
    cwd: Option<PathBuf>,
}

impl ProjectFileRefusal {
    fn new(project_file: &Path, kind: &'static str, reason: String) -> Self {
        let cwd = env::current_dir().ok();
        let project_file = cwd
            .as_ref()
            .map_or_else(|| project_file.to_path_buf(), |cwd| cwd.join(project_file));
        Self {
            reason,
            kind,
            project_file,
            cwd,
        }
    }

    fn value(&self) -> Value {
        json!({ "error": {
            "code": "project_resolution_failed",
            "message": "project selection refused",
            "details": {
                "reason": self.reason,
                "kind": self.kind,
                "project_file": self.project_file.to_string_lossy(),
                "searched_directory": self.project_file.parent()
                    .filter(|parent| !parent.as_os_str().is_empty())
                    .unwrap_or(Path::new(".")).to_string_lossy(),
                "cwd": self.cwd.as_ref().map(|path| path.to_string_lossy()),
                "selection": SELECTION,
                "remedy": REMEDY,
            },
            "reminders": [],
            "next": [NEXT],
        } })
    }

    pub(crate) fn emit(&self, json: bool) -> anyhow::Result<()> {
        let value = self.value();
        if json {
            eprintln!("{}", serde_json::to_string_pretty(&value)?);
        } else {
            eprintln!("error: project_resolution_failed: project selection refused");
            if let Some(details) = value["error"]["details"].as_object() {
                for (name, value) in details {
                    eprintln!("  {name}: {}", terminal_detail(value));
                }
            }
            eprintln!("next:\n  {NEXT}");
        }
        Ok(())
    }
}

/// A detail as one terminal-safe line. A string, such as an attempted path,
/// prints as written, so a Windows path keeps its single backslashes. An
/// ASCII control and every non-ASCII scalar, which covers bidi, separator and
/// invisible controls, is escaped as its UTF-16 `\uXXXX` units at this
/// CLI-only boundary. Any other value prints as JSON.
fn terminal_detail(value: &Value) -> String {
    let Value::String(value) = value else {
        return value.to_string();
    };
    let mut text = String::with_capacity(value.len());
    for character in value.chars() {
        if character.is_ascii() && !character.is_ascii_control() {
            text.push(character);
        } else {
            for unit in character.encode_utf16(&mut [0; 2]) {
                let _ = write!(text, "\\u{unit:04x}");
            }
        }
    }
    text
}

/// Reads the project id, refusing with a kind a caller can act on: no file
/// (`missing`), any other read failure (`unreadable`), content that is not
/// UTF-8 (`undecodable`) or blank content (`empty`).
pub(crate) fn read_project_id(project_file: &Path) -> anyhow::Result<String> {
    let bytes = fs::read(project_file).map_err(|error| {
        let kind = if error.kind() == io::ErrorKind::NotFound {
            "missing"
        } else {
            "unreadable"
        };
        ProjectFileRefusal::new(
            project_file,
            kind,
            format!("failed to read {}: {error}", project_file.display()),
        )
    })?;
    let text = String::from_utf8(bytes).map_err(|_| {
        ProjectFileRefusal::new(
            project_file,
            "undecodable",
            format!("project file {} is not valid UTF-8", project_file.display()),
        )
    })?;
    let project_id = text.trim();
    if project_id.is_empty() {
        return Err(ProjectFileRefusal::new(
            project_file,
            "empty",
            format!("project id in {} is empty", project_file.display()),
        )
        .into());
    }
    Ok(project_id.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unavailable_cwd_keeps_a_nonempty_searched_directory() {
        // Model current_dir failure without changing process-global cwd.
        let refusal = ProjectFileRefusal {
            reason: "project file is unavailable".into(),
            kind: "unreadable",
            project_file: PathBuf::from(".engram-project"),
            cwd: None,
        };
        let value = refusal.value();
        assert!(value["error"]["details"]["cwd"].is_null());
        assert_eq!(value["error"]["details"]["project_file"], ".engram-project");
        assert_eq!(value["error"]["details"]["searched_directory"], ".");
        assert_eq!(value["error"]["next"], json!([NEXT]));
    }
}
