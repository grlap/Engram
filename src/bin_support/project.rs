//! CLI project-file refusal before store opening or session attribution.
//! This boundary never searches for, selects, or creates a replacement project.

use std::{
    env,
    fmt::Write as _,
    fs,
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
                    // JSON framing escapes ASCII controls; Unicode escaping
                    // also neutralises bidi, separator and invisible controls.
                    eprintln!("  {name}: {}", terminal_detail(value));
                }
            }
            eprintln!("next:\n  {NEXT}");
        }
        Ok(())
    }
}

fn terminal_detail(value: &Value) -> String {
    let json = value.to_string();
    let mut text = String::with_capacity(json.len());
    for character in json.chars() {
        if character.is_ascii() {
            text.push(character);
        } else {
            // Conservatively escape every non-ASCII scalar at this CLI-only
            // boundary without duplicating the library's private text policy.
            // Keep the framed value valid JSON, including supplementary
            // scalars represented by a UTF-16 surrogate pair.
            for unit in character.encode_utf16(&mut [0; 2]) {
                let _ = write!(text, "\\u{unit:04x}");
            }
        }
    }
    text
}

pub(crate) fn read_project_id(project_file: &Path) -> anyhow::Result<String> {
    let text = fs::read_to_string(project_file).map_err(|error| {
        ProjectFileRefusal::new(
            project_file,
            "unreadable",
            format!("failed to read {}: {error}", project_file.display()),
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
