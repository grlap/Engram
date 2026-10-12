//! Local setup preparation, not enablement or publication. SQLite is never opened.
//! Proposed edits stay in the private intent until a separate materializer accepts it.

mod attributes;
mod files;
mod intent;
mod policy;
mod repository;

use std::path::{Path, PathBuf};

use anyhow::{Result, ensure};
use engram::{ProjectId, task_sync};
use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Assertions {
    pub visibility: String,
    pub readership: String,
    pub profile: String,
    pub include_notes: bool,
    pub origin_machine: Option<String>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Attribution {
    pub actor_id: String,
    pub session_id: String,
    pub actor_context: Option<String>,
    pub source_skill: Option<String>,
}

impl Assertions {
    fn validate(&self) -> Result<()> {
        ensure!(
            matches!(self.visibility.as_str(), "public" | "private"),
            "assert public or private repository visibility"
        );
        ensure!(
            self.profile == task_sync::FORMAT,
            "unsupported task export profile"
        );
        ensure!(
            !self.readership.trim().is_empty()
                && self.readership.len() <= 4096
                && !self.readership.chars().any(char::is_control),
            "readership must be nonblank bounded text without control characters"
        );
        ensure!(
            !self.include_notes || self.origin_machine.is_some(),
            "notes require an asserted origin-machine alias"
        );
        if let Some(alias) = &self.origin_machine {
            task_sync::notes::validate_machine_alias(alias)?;
        }
        Ok(())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
struct Basis {
    repository: PathBuf,
    git_directory: PathBuf,
    project_file: PathBuf,
    project_id: ProjectId,
    marker: Vec<u8>,
    branch: String,
    head: String,
    assertions: Assertions,
    edits: Vec<policy::Edit>,
}

#[derive(Debug, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    format: String,
    operation: uuid::Uuid,
    prepared_at: chrono::DateTime<chrono::Utc>,
    basis: Basis,
    attribution: Attribution,
}

#[derive(Serialize)]
pub(crate) struct Receipt {
    pub intent: PathBuf,
    pub operation: uuid::Uuid,
    pub repeated: bool,
    pub prepared: bool,
    pub enabled: bool,
    pub assurance: &'static str,
    pub redactor: &'static str,
}

pub(crate) fn prepare(
    selected_marker: &Path,
    project: &ProjectId,
    repository: &Path,
    branch: &str,
    assertions: Assertions,
    attribution: Attribution,
) -> Result<Receipt> {
    assertions.validate()?;
    for (name, text, limit) in [
        ("actor", Some(attribution.actor_id.as_str()), 1024),
        ("session", Some(attribution.session_id.as_str()), 1024),
        ("actor context", attribution.actor_context.as_deref(), 4096),
        ("source skill", attribution.source_skill.as_deref(), 1024),
    ] {
        ensure!(
            text.is_none_or(|text| !text.trim().is_empty()
                && text.len() <= limit
                && !text.chars().any(char::is_control)),
            "{name} must be nonblank bounded text without control characters"
        );
    }
    let basis = repository::inspect(selected_marker, project, repository, branch, assertions)?;
    intent::record(basis, attribution)
}
