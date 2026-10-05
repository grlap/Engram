//! CLI project-file refusal before store opening or session attribution.
//! Selection searches ancestors only when no explicit path was supplied.

use std::{
    env,
    fmt::Write as _,
    fs, io,
    path::{Path, PathBuf},
};

use anyhow::Context as _;
use engram::{ProjectId, project_database_path};
use serde_json::{Value, json};

const SELECTION: &str = "When --project-file is omitted, search from the current directory through its ancestors to the filesystem root for the nearest .engram-project. Explicit paths resolve from the current directory and never fall back. An invalid nearest marker refuses without searching farther.";
const REMEDY: &str = "Change to the intended project directory, or replace PROJECT_DIRECTORY in the next command with its absolute path. No project was selected or created.";
const NEXT: &str = "engram --project-file 'PROJECT_DIRECTORY/.engram-project' work next";

/// Selected marker pathname, stable project id and host-local database path.
pub(crate) struct ResolvedProject {
    pub(crate) project_file: PathBuf,
    pub(crate) project_id: ProjectId,
    pub(crate) database: PathBuf,
}

/// Resolves the stable project id and its host-local database. The project
/// root is the directory holding the selected marker pathname.
pub(crate) fn resolve_project(
    explicit: Option<&Path>,
    home: Option<PathBuf>,
) -> anyhow::Result<ResolvedProject> {
    let (project_file, project_id) = select_project_file(explicit)?;
    let home = home.or_else(|| env::var_os("ENGRAM_HOME").map(PathBuf::from));
    let home = home.context("pass --home or set ENGRAM_HOME")?;
    let project_id = ProjectId(project_id);
    let database = project_database_path(&home, &project_id);
    Ok(ResolvedProject {
        project_file,
        project_id,
        database,
    })
}

#[derive(Debug, thiserror::Error)]
#[error("{reason}")]
pub(crate) struct ProjectFileRefusal {
    reason: String,
    kind: &'static str,
    project_file: PathBuf,
    cwd: Option<PathBuf>,
    searched_directory: PathBuf,
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
            searched_directory: project_file
                .parent()
                .filter(|p| !p.as_os_str().is_empty())
                .unwrap_or(Path::new("."))
                .to_path_buf(),
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
                "searched_directory": self.searched_directory.to_string_lossy(),
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

/// Selects the marker once; its pathname, not a symlink target, owns the root.
pub(crate) fn select_project_file(explicit: Option<&Path>) -> anyhow::Result<(PathBuf, String)> {
    select_with_cwd(explicit, env::current_dir, read_entry)
}

fn select_with_cwd(
    explicit: Option<&Path>,
    current_dir: impl FnOnce() -> io::Result<PathBuf>,
    read: impl FnMut(&Path) -> io::Result<Option<Vec<u8>>>,
) -> anyhow::Result<(PathBuf, String)> {
    if explicit.is_some_and(Path::is_absolute) {
        return select_from(Path::new(""), explicit, read);
    }
    let cwd = current_dir().map_err(|error| {
        ProjectFileRefusal::new(
            explicit.unwrap_or(Path::new(".engram-project")),
            "unreadable",
            format!("cannot determine the working directory: {error}"),
        )
    })?;
    select_from(&cwd, explicit, read)
}

fn read_entry(path: &Path) -> io::Result<Option<Vec<u8>>> {
    match fs::symlink_metadata(path) {
        Ok(_) => fs::read(path).map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error),
    }
}

fn select_from(
    cwd: &Path,
    explicit: Option<&Path>,
    mut read: impl FnMut(&Path) -> io::Result<Option<Vec<u8>>>,
) -> anyhow::Result<(PathBuf, String)> {
    let candidates: Vec<PathBuf> = explicit.map_or_else(
        || {
            cwd.ancestors()
                .map(|directory| directory.join(".engram-project"))
                .collect()
        },
        |path| vec![cwd.join(path)],
    );
    for path in candidates {
        match read(&path) {
            Ok(Some(bytes)) => return Ok((path.clone(), decode_project_id(&path, bytes)?)),
            Ok(None) if explicit.is_none() => (),
            Ok(None) => {
                return Err(ProjectFileRefusal::new(
                    &path,
                    "missing",
                    format!("project file {} was not found", path.display()),
                )
                .into());
            }
            Err(error) => {
                return Err(ProjectFileRefusal::new(
                    &path,
                    "unreadable",
                    format!("failed to read {}: {error}", path.display()),
                )
                .into());
            }
        }
    }
    let mut refusal = ProjectFileRefusal::new(
        &cwd.join(".engram-project"),
        "missing",
        format!(
            "no .engram-project found searching from {} through its ancestors to the filesystem root",
            cwd.display()
        ),
    );
    refusal.cwd = Some(cwd.to_path_buf());
    refusal.searched_directory = cwd.to_path_buf();
    Err(refusal.into())
}

/// Decodes the marker bytes into a project id, refusing non-UTF-8 content
/// (`undecodable`) or blank content (`empty`).
fn decode_project_id(project_file: &Path, bytes: Vec<u8>) -> anyhow::Result<String> {
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
    fn absolute_explicit_marker_does_not_require_a_working_directory() {
        let marker = std::path::absolute("override/.engram-project").unwrap();
        let mut visited = Vec::new();
        let selected = select_with_cwd(
            Some(&marker),
            || panic!("absolute selection must not look up the working directory"),
            |path| {
                visited.push(path.to_path_buf());
                Ok(Some(b"explicit-project".to_vec()))
            },
        )
        .unwrap();
        assert_eq!(selected, (marker.clone(), "explicit-project".into()));
        assert_eq!(visited, vec![marker]);
    }

    #[test]
    fn unavailable_cwd_keeps_a_nonempty_searched_directory() {
        // Model current_dir failure without changing process-global cwd.
        let refusal = ProjectFileRefusal {
            reason: "project file is unavailable".into(),
            kind: "unreadable",
            project_file: PathBuf::from(".engram-project"),
            cwd: None,
            searched_directory: PathBuf::from("."),
        };
        let value = refusal.value();
        assert!(value["error"]["details"]["cwd"].is_null());
        assert_eq!(value["error"]["details"]["project_file"], ".engram-project");
        assert_eq!(value["error"]["details"]["searched_directory"], ".");
        assert_eq!(value["error"]["next"], json!([NEXT]));
    }

    #[test]
    fn automatic_search_visits_root_once_and_reports_its_origin() {
        let cwd = std::path::absolute("nested/project/child").unwrap();
        let mut visited = Vec::new();
        let error = select_from(&cwd, None, |path| {
            visited.push(path.to_path_buf());
            Ok(None)
        })
        .unwrap_err();
        let expected: Vec<_> = cwd
            .ancestors()
            .map(|directory| directory.join(".engram-project"))
            .collect();
        assert_eq!(visited, expected);
        let value = error.downcast_ref::<ProjectFileRefusal>().unwrap().value();
        assert_eq!(
            value["error"]["details"]["searched_directory"],
            cwd.to_string_lossy().as_ref()
        );
        assert!(
            value["error"]["details"]["reason"]
                .as_str()
                .unwrap()
                .contains("filesystem root")
        );
        assert_eq!(value["error"]["next"], json!([NEXT]));
    }

    #[test]
    fn encountered_read_errors_never_fall_through_to_an_ancestor() {
        let cwd = std::path::absolute("nested/child").unwrap();
        for kind in [io::ErrorKind::PermissionDenied, io::ErrorKind::NotFound] {
            let mut visited = Vec::new();
            let error = select_from(&cwd, None, |path| {
                visited.push(path.to_path_buf());
                Err(io::Error::from(kind))
            })
            .unwrap_err();
            assert_eq!(visited, vec![cwd.join(".engram-project")]);
            assert_eq!(
                error.downcast_ref::<ProjectFileRefusal>().unwrap().kind,
                "unreadable"
            );
        }
    }

    #[test]
    fn nearest_marker_and_explicit_paths_select_without_canonicalizing() {
        let cwd = std::path::absolute("outer/nested/child").unwrap();
        let nearest = cwd.parent().unwrap().join(".engram-project");
        let mut visited = Vec::new();
        let selected = select_from(&cwd, None, |path| {
            visited.push(path.to_path_buf());
            Ok((path == nearest).then(|| b"nearest-project".to_vec()))
        })
        .unwrap();
        assert_eq!(selected, (nearest.clone(), "nearest-project".into()));
        assert_eq!(visited, vec![cwd.join(".engram-project"), nearest]);
        for explicit in [
            PathBuf::from(".engram-project"),
            PathBuf::from("../override"),
            std::path::absolute("override").unwrap(),
        ] {
            let mut visited = Vec::new();
            let error = select_from(&cwd, Some(&explicit), |path| {
                visited.push(path.to_path_buf());
                Ok(None)
            })
            .unwrap_err();
            assert_eq!(visited, vec![cwd.join(&explicit)]);
            assert_eq!(
                error.downcast_ref::<ProjectFileRefusal>().unwrap().kind,
                "missing"
            );
        }
    }

    #[test]
    fn invalid_nearest_marker_blocks_a_valid_ancestor() {
        let cwd = std::path::absolute("outer/child").unwrap();
        for (bytes, kind) in [(b" \n".to_vec(), "empty"), (vec![255], "undecodable")] {
            let mut visited = Vec::new();
            let error = select_from(&cwd, None, |path| {
                visited.push(path.to_path_buf());
                Ok(Some(if path == cwd.join(".engram-project") {
                    bytes.clone()
                } else {
                    b"outer-project".to_vec()
                }))
            })
            .unwrap_err();
            assert_eq!(visited, vec![cwd.join(".engram-project")]);
            assert_eq!(
                error.downcast_ref::<ProjectFileRefusal>().unwrap().kind,
                kind
            );
        }
    }
}
