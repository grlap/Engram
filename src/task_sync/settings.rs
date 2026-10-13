//! Engram-owned host-local opt-in. No SQLite access or task-file publication.

mod files;

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::path::Path;

use crate::{ProjectId, project_home_of};

pub const STATUS_FORMAT: &str = "engram-task-sync-status-v1";
const SETTING_FORMAT: &str = "engram-task-sync-setting-v1";
const EFFECT: &str = "Opt-in for explicit Git task sync for this project on this machine; changing it does not create or remove shared task files.";
const ABSENT: &str = "not configured (off by default)";
const MAX_TEXT: usize = 4096;

#[derive(Clone, Copy, Debug, Deserialize, Serialize, JsonSchema, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    Status,
    Enable,
    Disable,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub action: Action,
    pub user_instruction: Option<String>,
}

#[derive(Clone, Copy, Debug, Deserialize, Serialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Enabled,
    Disabled,
    Unknown,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Status {
    pub format: String,
    pub project_id: String,
    pub state: State,
    pub reason: String,
    pub effect: String,
    pub user_instruction: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct Error {
    pub format: &'static str,
    pub project_id: Option<String>,
    pub operation: Action,
    pub error: Problem,
    pub status: Option<Box<Status>>,
}

#[derive(Debug, Serialize)]
pub struct Problem {
    pub code: &'static str,
    pub message: &'static str,
}

impl std::fmt::Display for Error {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.error.message)
    }
}

impl std::error::Error for Error {}

impl Error {
    pub fn new(
        project: Option<&ProjectId>,
        operation: Action,
        code: &'static str,
        message: &'static str,
        status: Option<Status>,
    ) -> Self {
        Self {
            format: "engram-task-sync-error-v1",
            project_id: project.map(|value| value.0.clone()),
            operation,
            error: Problem { code, message },
            status: status.map(Box::new),
        }
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Setting {
    format: String,
    project_id: String,
    enabled: bool,
    actor_id: String,
    session_id: String,
    changed_at: DateTime<Utc>,
    user_instruction: Option<String>,
}

fn text_valid(text: &str) -> bool {
    !text.trim().is_empty() && text.len() <= MAX_TEXT
}

fn status(
    project: &ProjectId,
    state: State,
    reason: &str,
    user_instruction: Option<String>,
) -> Status {
    Status {
        format: STATUS_FORMAT.into(),
        project_id: project.0.clone(),
        state,
        reason: reason.into(),
        effect: EFFECT.into(),
        user_instruction,
    }
}

fn from_bytes(project: &ProjectId, bytes: &[u8]) -> Status {
    let valid = serde_json::from_slice::<Setting>(bytes)
        .ok()
        .filter(|setting| {
            setting.format == SETTING_FORMAT
                && setting.project_id == project.0
                && text_valid(&setting.actor_id)
                && text_valid(&setting.session_id)
                && if setting.enabled {
                    setting.user_instruction.as_deref().is_some_and(text_valid)
                } else {
                    setting.user_instruction.is_none()
                }
        });
    match valid {
        Some(setting) if setting.enabled => status(
            project,
            State::Enabled,
            "explicitly enabled",
            setting.user_instruction,
        ),
        Some(_) => status(project, State::Disabled, "explicitly disabled", None),
        None => status(project, State::Unknown, "setting document is invalid", None),
    }
}

/// One bounded observation of the final file. Creates nothing and never opens SQLite.
pub fn read(database: &Path, project: &ProjectId) -> Status {
    if project_home_of(database, project).is_none() {
        return status(
            project,
            State::Unknown,
            "setting requires the standard project store location",
            None,
        );
    }
    let Some(parent) = database.parent() else {
        return status(
            project,
            State::Unknown,
            "setting parent is unavailable",
            None,
        );
    };
    match files::Directory::open(parent, false)
        .and_then(|directory| directory.map_or(Ok(None), |directory| directory.read()))
    {
        Ok(None) => status(project, State::Disabled, ABSENT, None),
        Ok(Some(bytes)) => from_bytes(project, &bytes),
        Err(files::Failure::Oversized) => status(
            project,
            State::Unknown,
            "setting document exceeds the byte bound",
            None,
        ),
        Err(files::Failure::Io) => status(
            project,
            State::Unknown,
            "setting path is unreadable or not an ordinary unlinked file",
            None,
        ),
    }
}

pub(crate) fn set(
    database: &Path,
    project: &ProjectId,
    action: Action,
    instruction: Option<&str>,
    actor: &str,
    session: &str,
    now: DateTime<Utc>,
) -> Result<Status, Error> {
    let refuse = |code, message| {
        Error::new(
            Some(project),
            action,
            code,
            message,
            Some(read(database, project)),
        )
    };
    if action == Action::Enable && !instruction.is_some_and(text_valid) {
        return Err(refuse(
            "invalid_user_instruction",
            "enable requires a non-empty user instruction of at most 4096 bytes",
        ));
    }
    if !text_valid(actor) || !text_valid(session) {
        return Err(refuse(
            "invalid_attribution",
            "actor and session must be non-empty and at most 4096 bytes",
        ));
    }
    if project_home_of(database, project).is_none() {
        return Err(refuse(
            "setting_unreadable",
            "setting requires the standard project store location",
        ));
    }
    let setting = Setting {
        format: SETTING_FORMAT.into(),
        project_id: project.0.clone(),
        enabled: action == Action::Enable,
        actor_id: actor.into(),
        session_id: session.into(),
        changed_at: now,
        user_instruction: if action == Action::Enable {
            instruction.map(str::to_owned)
        } else {
            None
        },
    };
    let bytes = serde_json::to_vec(&setting)
        .map_err(|_| refuse("setting_write_failed", "setting serialization failed"))?;
    if bytes.len() > files::MAX_BYTES {
        return Err(refuse(
            "setting_write_failed",
            "setting document exceeds the byte bound",
        ));
    }
    let parent = database
        .parent()
        .ok_or_else(|| refuse("setting_unreadable", "setting parent is unavailable"))?;
    let directory = files::Directory::open(parent, true)
        .map_err(|_| {
            refuse(
                "setting_unreadable",
                "setting directory is unreadable or linked",
            )
        })?
        .ok_or_else(|| refuse("setting_unreadable", "setting directory is unavailable"))?;
    directory.replace(&bytes).map_err(|failure| match failure {
        files::Failure::Io => refuse(
            "setting_unreadable",
            "setting path could not be read or atomically replaced",
        ),
        files::Failure::Oversized => refuse(
            "setting_write_failed",
            "setting document exceeds the byte bound",
        ),
    })?;
    // Last writer wins. Readback may observe a different concurrent setter.
    Ok(read(database, project))
}
