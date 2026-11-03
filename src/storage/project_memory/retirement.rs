//! Retirement-target validation, state and candidate reads.

use super::read::validate_stored_project_memory_key;
use super::{
    ActorContext, Connection, OptionalExtension, SessionId, SqliteStore, StoreError,
    admit_live_project_memory_sessions, params, validate_project_memory_authorization,
};

pub(super) fn resolve_retiring_target_on(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
    input: &crate::domain::ProjectMemoryRetiringTargetInput,
) -> Result<crate::domain::ProjectMemoryRetiringTarget, StoreError> {
    use crate::domain::{
        ProjectMemoryRetiringTarget as Target, ProjectMemoryRetiringTargetInput as Input,
    };
    validate_retiring_target_input(input)?;
    let target = match input {
        Input::Local { work_ref } => {
            let work = crate::storage::work::resolve_work_ref_on(connection, project_id, work_ref)?;
            Target::Local {
                work_id: work.work_id,
                work_ref: work.short_ref,
            }
        }
        Input::External { project, reference } => Target::External {
            project: project.clone(),
            reference: reference.clone(),
        },
    };
    validate_retiring_target_shape(&target)?;
    Ok(target)
}

/// The checks of a retirement target as given that read nothing from the
/// store; resolving a local reference is the store's.
pub(super) fn validate_retiring_target_input(
    input: &crate::domain::ProjectMemoryRetiringTargetInput,
) -> Result<(), StoreError> {
    use crate::domain::ProjectMemoryRetiringTargetInput as Input;
    match input {
        Input::Local { work_ref } => {
            if work_ref.is_empty() || work_ref.len() > 128 || work_ref.trim() != work_ref {
                return Err(StoreError::InvalidProjectMemory(
                    "local retirement reference is invalid".into(),
                ));
            }
        }
        Input::External { project, reference } => {
            for (value, label) in [(project, "project"), (reference, "reference")] {
                if value.is_empty() || value.len() > 256 || !is_shell_safe_target_text(value) {
                    return Err(StoreError::InvalidProjectMemory(format!(
                        "external retirement {label} must be 1-256 bytes of {SHELL_SAFE_TARGET_TEXT}"
                    )));
                }
            }
        }
    }
    Ok(())
}

/// What target text may hold, in words for refusals and documentation.
const SHELL_SAFE_TARGET_TEXT: &str = "ASCII letters, digits and . _ - / : @ +";

/// Target text is echoed back inside suggested commands, such as the restore
/// argument after a dropped target. Holding it to characters that no common
/// shell splits or interprets keeps every printed form safe to copy as one
/// argument.
fn is_shell_safe_target_text(value: &str) -> bool {
    value
        .bytes()
        .all(|byte| byte.is_ascii_alphanumeric() || b"._-/:@+".contains(&byte))
}

pub(in crate::storage) fn validate_retiring_target_shape(
    target: &crate::domain::ProjectMemoryRetiringTarget,
) -> Result<(), StoreError> {
    use crate::domain::ProjectMemoryRetiringTarget as Target;
    let fields: Vec<(&str, &str, usize)> = match target {
        Target::Local { work_ref, .. } => vec![("local work reference", work_ref, 128)],
        Target::External { project, reference } => vec![
            ("external project", project, 256),
            ("external reference", reference, 256),
        ],
    };
    for (label, value, max) in fields {
        if value.is_empty() || value.len() > max || !is_shell_safe_target_text(value) {
            return Err(StoreError::InvalidProjectMemory(format!(
                "{label} must be 1-{max} bytes of {SHELL_SAFE_TARGET_TEXT}"
            )));
        }
    }
    Ok(())
}

pub(super) fn retiring_state_on(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
    target: Option<&crate::domain::ProjectMemoryRetiringTarget>,
) -> Result<Option<crate::domain::ProjectMemoryRetiringState>, StoreError> {
    let Some(crate::domain::ProjectMemoryRetiringTarget::Local { work_id, work_ref }) = target
    else {
        return Ok(None);
    };
    let item = local_target_item_on(connection, project_id, *work_id)?.ok_or_else(|| {
        StoreError::InvalidMemoryProjection("local retirement target is missing".into())
    })?;
    if item.work_id != *work_id || item.short_ref != *work_ref {
        return Err(StoreError::InvalidMemoryProjection(
            "local retirement target changed identity".into(),
        ));
    }
    Ok(Some(crate::domain::ProjectMemoryRetiringState {
        lifecycle: item.lifecycle,
        updated_at: item.updated_at,
    }))
}

/// The item a local target names, or `None` when the project holds no item
/// with that work id. Other failures, such as a busy or failing read, are
/// returned as they are rather than read as a missing target.
fn local_target_item_on(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
    work_id: crate::domain::WorkId,
) -> Result<Option<crate::domain::WorkItem>, StoreError> {
    let exists = connection
        .query_row(
            "SELECT 1 FROM work_items WHERE project_id = ?1 AND work_id = ?2",
            params![project_id.0, work_id.0.to_string()],
            |_| Ok(()),
        )
        .optional()?;
    if exists.is_none() {
        return Ok(None);
    }
    crate::storage::work::resolve_work_ref_on(connection, project_id, &work_id.0.to_string())
        .map(Some)
}

impl SqliteStore {
    /// Doctor check: every stored local retiring target, current or
    /// historical, names an item of its memory's own project by that item's
    /// work id and short ref. Reads rely on this, so a row that fails it (from
    /// an import file, for example) is reported here instead of failing later
    /// reads of that memory.
    pub(in crate::storage) fn verify_project_memory_retiring_targets_on(
        connection: &Connection,
        checked: &mut usize,
        invalid: &mut Vec<String>,
    ) -> Result<(), StoreError> {
        let mut statement = connection.prepare(
            "SELECT object_id,
                    json_extract(canonical_json, '$.scope.project'),
                    json_extract(canonical_json, '$.retiring_target.work_id'),
                    json_extract(canonical_json, '$.retiring_target.work_ref')
             FROM objects
             WHERE object_kind = 'memory_version'
               AND json_type(canonical_json, '$.project_key') = 'text'
               AND json_extract(canonical_json, '$.retiring_target.kind') = 'local'
             ORDER BY object_id",
        )?;
        let rows = statement
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, Option<String>>(3)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        for (version_id, project, work_id, work_ref) in rows {
            *checked += 1;
            let bound = match (project, work_id, work_ref) {
                (Some(project), Some(work_id), Some(work_ref)) => connection
                    .query_row(
                        "SELECT short_ref FROM work_items WHERE project_id = ?1 AND work_id = ?2",
                        params![project, work_id],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?
                    .is_some_and(|short_ref| short_ref == work_ref),
                _ => false,
            };
            if !bound {
                invalid.push(format!("project_memory:{version_id}:retiring_target"));
            }
        }
        Ok(())
    }
}

/// Candidate keys a lifecycle advisory lists before the exact omitted count.
const RETIREMENT_CANDIDATE_LIMIT: i64 = 16;

impl SqliteStore {
    /// Active project memories whose current version names `work_id` as its
    /// local retiring target, in key order: at most 16 keys, the exact total
    /// and the omitted count. One statement reads one consistent snapshot of
    /// the current heads; historical versions never match.
    ///
    /// # Errors
    ///
    /// Returns a typed refusal when authorization fails, or an invalid
    /// projection error when a stored key or the counted total is invalid.
    pub fn project_memory_retirement_candidates(
        &self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        actor: &ActorContext,
        work_id: crate::domain::WorkId,
    ) -> Result<crate::domain::ProjectMemoryRetirementCandidates, StoreError> {
        admit_live_project_memory_sessions(session_id, actor)?;
        validate_project_memory_authorization(session_id, actor)?;
        let mut statement = self.connection.prepare(
            "SELECT json_extract(object.canonical_json, '$.project_key'), COUNT(*) OVER()
             FROM memory_heads AS head
             JOIN objects AS object ON object.object_id = head.version_id
             WHERE head.status = 'active'
               AND object.object_kind = 'memory_version'
               AND json_extract(object.canonical_json, '$.scope.kind') = 'project'
               AND json_extract(object.canonical_json, '$.scope.project') = ?1
               AND json_type(object.canonical_json, '$.project_key') = 'text'
               AND json_extract(object.canonical_json, '$.retiring_target.kind') = 'local'
               AND json_extract(object.canonical_json, '$.retiring_target.work_id') = ?2
             ORDER BY json_extract(object.canonical_json, '$.project_key')
             LIMIT ?3",
        )?;
        let rows = statement
            .query_map(
                params![
                    project_id.0,
                    work_id.0.to_string(),
                    RETIREMENT_CANDIDATE_LIMIT
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let total = rows
            .first()
            .map_or(Ok(0), |(_, count)| usize::try_from(*count))
            .map_err(|_| {
                StoreError::InvalidMemoryProjection("retirement candidate count is invalid".into())
            })?;
        let keys = rows
            .into_iter()
            .map(|(key, _)| validate_stored_project_memory_key(&key))
            .collect::<Result<Vec<_>, _>>()?;
        Ok(crate::domain::ProjectMemoryRetirementCandidates {
            total,
            omitted: total.saturating_sub(keys.len()),
            keys,
        })
    }
}
