//! File-based source intake: bounded input and JSON receipts, no network adapter.

use super::attribution::resolve_shell_work_attribution;
use crate::ImportCommand;
use anyhow::Result;
use engram::domain::{WorkImportInput, WorkSourceKey};
use engram::{LocalWorkService, ObjectHash, ProjectId, SessionId, StoreError, store_error_value};
use std::{
    fs::File,
    io::Read,
    path::{Path, PathBuf},
    process::ExitCode,
};

pub(crate) fn run(
    database: PathBuf,
    project: ProjectId,
    actor: Option<String>,
    session: Option<String>,
    actor_context: Option<String>,
    operation: ImportCommand,
) -> Result<ExitCode> {
    let attribution = resolve_shell_work_attribution(actor, session);
    attribution.print_notices();
    let service = LocalWorkService::new_with_attribution(
        database,
        project,
        attribution.actor_id,
        SessionId(attribution.session_id),
        None,
        actor_context,
        attribution.defaults,
    );
    let now = chrono::Utc::now();
    let result = (|| -> Result<serde_json::Value, StoreError> {
        match operation {
            ImportCommand::Preview { file } => service
                .preview_work_import(&read_input(&file)?, now)
                .and_then(|value| serde_json::to_value(value).map_err(StoreError::from)),
            ImportCommand::Apply { file, preview } => {
                let token: ObjectHash = preview.parse().map_err(|_| {
                    StoreError::InvalidWork(
                        "--preview must be the complete token returned by preview".into(),
                    )
                })?;
                service
                    .apply_work_import(&read_input(&file)?, &token, now)
                    .and_then(|value| serde_json::to_value(value).map_err(StoreError::from))
            }
            ImportCommand::Lookup { adapter, reference } => service
                .lookup_work_source(
                    &WorkSourceKey {
                        adapter_kind: adapter,
                        canonical_ref: reference,
                    },
                    now,
                )
                .and_then(|value| serde_json::to_value(value).map_err(StoreError::from)),
        }
    })();
    match result {
        Ok(value) => {
            println!("{}", serde_json::to_string_pretty(&value)?);
            Ok(ExitCode::SUCCESS)
        }
        Err(error) => {
            eprintln!("{}", serde_json::to_string(&store_error_value(&error))?);
            Ok(ExitCode::FAILURE)
        }
    }
}

fn read_input(file: &Path) -> Result<WorkImportInput, StoreError> {
    let mut bytes = Vec::new();
    File::open(file)
        .map_err(|error| {
            StoreError::InvalidWork(format!(
                "cannot open import input {}: {error}",
                file.display()
            ))
        })?
        .take(engram::work_service::MAX_WORK_IMPORT_INPUT_BYTES as u64 + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| StoreError::InvalidWork(format!("cannot read import input: {error}")))?;
    engram::work_service::parse_work_import_input(&bytes)
}
