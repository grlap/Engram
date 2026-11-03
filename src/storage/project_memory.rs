use super::{
    ActorContext, Authority, CanonicalObject, Connection, Delivery, ForgetProjectMemoryRequest,
    MAX_CONTEXT_GENERATION_BYTES, MAX_PROJECT_MEMORY_ADVERTISEMENTS_PER_PROJECT,
    MAX_PROJECT_MEMORY_ATTRIBUTION_BYTES, MAX_PROJECT_MEMORY_ATTRIBUTION_TEXT_BYTES,
    MAX_PROJECT_MEMORY_BODY_BYTES, MAX_PROJECT_MEMORY_KEY_BYTES,
    MAX_PROJECT_MEMORY_PROVENANCE_LINKS, MemoryAssertionEvent, MemoryHeadProjectionRow, MemoryId,
    MemoryKind, MemoryProjectionMode, MemoryStatus, MemoryVersion, ObjectId, OptionalExtension,
    PROJECT_MEMORY_FIRST_LINE_BYTES, PROJECT_MEMORY_LIST_LIMIT, PreparedProjectMemory,
    ProjectMemoryAdvertisement, ProjectMemoryFull, ProjectMemoryList, ProjectMemoryListRow,
    ProjectMemoryListingCut, ProjectMemoryMutationReceipt, Redactor, RememberProjectMemoryRequest,
    SCHEMA_VERSION, Scope, Sensitivity, SessionId, SqliteStore, StoreError, StoredProjectMemory,
    TransactionBehavior, fts_query, normalize_project_memory_query, params,
};

mod delivery;
mod edit;
mod history;
mod read;
mod retirement;
mod state;
#[cfg(test)]
mod tests;
use history::memory_full;
pub(in crate::storage) use history::project_memory_history_on;
use read::{history_revision, lookup_project_memory_history_on};
pub(in crate::storage) use read::{lookup_project_memory_on, validate_stored_project_memory_key};
#[cfg(test)]
use read::{project_memory_first_line, project_memory_rows_on};
pub(in crate::storage) use retirement::validate_retiring_target_shape;
use retirement::{resolve_retiring_target_on, validate_retiring_target_input};
use state::advance_project_memory_state_on;
pub(in crate::storage) use state::{
    derived_project_memory_state_on, derived_project_memory_state_rows_on, project_memory_state_on,
};

use crate::argument_names::Twin;

/// The refusals of `remember` and `memories` that name their arguments; the
/// core raises the CLI spelling and the agent projection shows MCP callers
/// the field names.
pub(crate) const REVISE_NEEDS_KEY_REFUSAL: Twin = Twin {
    cli: "--revise requires --key; --expected-revision requires --revise and a positive revision",
    mcp: "revise requires key; expected_revision requires revise and a positive revision",
};
pub(crate) const PARTIAL_EDIT_NEEDS_REVISE_REFUSAL: Twin = Twin {
    cli: "--append and --section revise a memory: they require --revise, --key and --expected-revision",
    mcp: "append and section revise a memory: they require revise, key and expected_revision",
};
pub(crate) const CLEAR_TARGET_NEEDS_REVISE_REFUSAL: Twin = Twin {
    cli: "clearing a retirement target requires --revise",
    mcp: "clearing a retirement target requires revise",
};
pub(crate) const FILTERED_SEARCH_AFTER_REFUSAL: Twin = Twin {
    cli: "filtered memory search does not accept --after; refine the query instead",
    mcp: "filtered memory search does not accept after; refine the query instead",
};
pub(crate) const UNSAFE_KEY_REFUSAL: Twin = Twin {
    cli: "memory body cannot produce a safe key; pass --key KEY",
    mcp: "memory body cannot produce a safe key; pass key",
};

/// A context generation is a plain token, so that the `memories` command a
/// peek prints with it reaches either supported shell, and the terminal, as
/// exactly the value that was supplied.
pub(crate) fn validate_context_generation(
    context_generation: Option<&str>,
) -> Result<(), StoreError> {
    if context_generation.is_some_and(|value| {
        value.is_empty()
            || value.len() > MAX_CONTEXT_GENERATION_BYTES
            || value.starts_with('-')
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-'))
    }) {
        return Err(StoreError::InvalidProjectMemory(format!(
            "context_generation must be 1 to {MAX_CONTEXT_GENERATION_BYTES} ASCII letters, digits, dots, underscores or dashes, and must not start with a dash"
        )));
    }
    Ok(())
}

/// Every check of a remember request that reads nothing from the store, in
/// the order the write applies them, so a caller can refuse a request before
/// it opens or registers anything. Returns the redacted actor and the key.
///
/// # Errors
///
/// Refuses the session or actor, an empty or oversized body, text the
/// redactor refuses, an unsafe key, a revise or partial edit without its key
/// and basis, partial-edit text no basis admits, a cleared retirement target
/// without a revise, and retirement target text out of shape. Whether a
/// local target names an item is the store's to answer.
pub(crate) fn validate_remember_request<R: Redactor>(
    request: &RememberProjectMemoryRequest,
    edit: &crate::domain::ProjectMemoryEdit,
    redactor: &R,
) -> Result<(ActorContext, String), StoreError> {
    let partial = *edit != crate::domain::ProjectMemoryEdit::Whole;
    admit_live_project_memory_sessions(&request.session_id, &request.actor)?;
    validate_project_memory_authorization(&request.session_id, &request.actor)?;
    let actor = validated_project_memory_actor(&request.actor, redactor)?;
    validate_project_memory_authorization(&request.session_id, &actor)?;
    // A section edit may clear its section; the body it builds keeps the
    // markers, so it is never empty.
    if request.body.trim().is_empty()
        && !matches!(edit, crate::domain::ProjectMemoryEdit::Section { .. })
    {
        return Err(StoreError::InvalidProjectMemory(
            "memory body must not be empty".into(),
        ));
    }
    if request.body.len() > MAX_PROJECT_MEMORY_BODY_BYTES {
        return Err(StoreError::InvalidProjectMemory(format!(
            "memory body exceeds {MAX_PROJECT_MEMORY_BODY_BYTES} UTF-8 bytes"
        )));
    }
    redactor
        .inspect(&request.body)
        .map_err(StoreError::RedactionRefused)?;
    // Target text is stored and shown like the body, so it is inspected
    // like the body before anything is resolved or written.
    if let crate::domain::ProjectMemoryRetiringTargetChange::Set { target } =
        &request.retiring_target
    {
        match target {
            crate::domain::ProjectMemoryRetiringTargetInput::Local { work_ref } => {
                redactor.inspect(work_ref)
            }
            crate::domain::ProjectMemoryRetiringTargetInput::External { project, reference } => {
                redactor
                    .inspect(project)
                    .and_then(|()| redactor.inspect(reference))
            }
        }
        .map_err(StoreError::RedactionRefused)?;
    }
    let key = match request.key.as_deref() {
        Some(key) => validate_project_memory_key(key)?,
        None => slug_project_memory_key(&request.body)?,
    };
    if (request.revise && request.key.is_none())
        || (!request.revise && request.expected_revision.is_some())
        || request.expected_revision == Some(0)
    {
        return Err(StoreError::InvalidProjectMemory(
            REVISE_NEEDS_KEY_REFUSAL.cli.into(),
        ));
    }
    if partial && (!request.revise || request.expected_revision.is_none()) {
        return Err(StoreError::InvalidProjectMemory(
            PARTIAL_EDIT_NEEDS_REVISE_REFUSAL.cli.into(),
        ));
    }
    match &request.retiring_target {
        crate::domain::ProjectMemoryRetiringTargetChange::Clear if !request.revise => {
            return Err(StoreError::InvalidProjectMemory(
                CLEAR_TARGET_NEEDS_REVISE_REFUSAL.cli.into(),
            ));
        }
        crate::domain::ProjectMemoryRetiringTargetChange::Set { target } => {
            validate_retiring_target_input(target)?;
        }
        _ => {}
    }
    Ok((actor, key))
}

/// Every check of a forget request that reads nothing from the store, so a
/// caller can refuse it before it opens or registers anything. Returns the
/// redacted actor and the key. A key the store does not hold is the store's
/// refusal, not this one.
///
/// # Errors
///
/// Refuses the session or actor and an unsafe key.
pub(crate) fn validate_forget_request<R: Redactor>(
    request: &ForgetProjectMemoryRequest,
    redactor: &R,
) -> Result<(ActorContext, String), StoreError> {
    admit_live_project_memory_sessions(&request.session_id, &request.actor)?;
    validate_project_memory_authorization(&request.session_id, &request.actor)?;
    let actor = validated_project_memory_actor(&request.actor, redactor)?;
    validate_project_memory_authorization(&request.session_id, &actor)?;
    let key = validate_project_memory_key(&request.key)?;
    Ok((actor, key))
}

impl SqliteStore {
    /// Builds an attributed legacy revision that dropped its retirement
    /// target without recording a clear, for full-read surface regressions.
    #[cfg(test)]
    pub(crate) fn test_append_memory_revision_without_target(
        &mut self,
        project: &str,
        key: &str,
        body: &str,
        at_ms: i64,
    ) {
        tests::retiring::insert_raw_version(self, project, key, body, None, at_ms);
    }

    /// Creates one attributed project episode or replays the identical create.
    ///
    /// # Errors
    ///
    /// Returns a typed project-memory refusal when authorization, key, size,
    /// redaction, revision-basis, or terminal lifecycle admission fails.
    #[cfg(test)]
    pub fn remember_project_memory<R: Redactor>(
        &mut self,
        request: &RememberProjectMemoryRequest,
        redactor: &R,
    ) -> Result<ProjectMemoryMutationReceipt, StoreError> {
        self.remember_project_memory_with_admission(request, redactor, |_, _| Ok(()))
    }

    #[cfg(test)]
    pub(crate) fn remember_project_memory_with_admission<R, A>(
        &mut self,
        request: &RememberProjectMemoryRequest,
        redactor: &R,
        admit_full_response: A,
    ) -> Result<ProjectMemoryMutationReceipt, StoreError>
    where
        R: Redactor,
        A: Fn(&ProjectMemoryFull, ProjectMemoryAdmission) -> Result<(), StoreError>,
    {
        self.remember_project_memory_edit_with_admission(
            request,
            &crate::domain::ProjectMemoryEdit::Whole,
            redactor,
            admit_full_response,
        )
    }

    /// [`Self::remember_project_memory_registering`] for a caller that
    /// registers no session.
    #[cfg(test)]
    pub(crate) fn remember_project_memory_edit_with_admission<R, A>(
        &mut self,
        request: &RememberProjectMemoryRequest,
        edit: &crate::domain::ProjectMemoryEdit,
        redactor: &R,
        admit_full_response: A,
    ) -> Result<ProjectMemoryMutationReceipt, StoreError>
    where
        R: Redactor,
        A: Fn(&ProjectMemoryFull, ProjectMemoryAdmission) -> Result<(), StoreError>,
    {
        self.remember_project_memory_registering(request, edit, redactor, admit_full_response, None)
    }

    /// Creates or revises one project memory as the whole-body path does,
    /// building a revise's body from the revision it names: the request's
    /// text replaces the whole body, is appended, or replaces one marked
    /// section. A partial edit names its basis revision, which must be the
    /// current one, and is assembled in the writer transaction, so an exact
    /// retry replays rather than appending twice.
    ///
    /// With `register_at`, it first registers the request's fresh
    /// process-default session inside that same transaction: the write keeps
    /// the registration only when it succeeds, a replay included, so any
    /// refusal leaves no session row and reclaims nothing.
    ///
    /// # Errors
    ///
    /// Refuses as the whole-body path does; a partial edit without `--revise`,
    /// a key and a positive basis; a basis revision that does not exist; a
    /// section the basis lacks or a body whose markers do not pair; or a
    /// registration that fails.
    pub(crate) fn remember_project_memory_registering<R, A>(
        &mut self,
        request: &RememberProjectMemoryRequest,
        edit: &crate::domain::ProjectMemoryEdit,
        redactor: &R,
        admit_full_response: A,
        register_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<ProjectMemoryMutationReceipt, StoreError>
    where
        R: Redactor,
        A: Fn(&ProjectMemoryFull, ProjectMemoryAdmission) -> Result<(), StoreError>,
    {
        let partial = *edit != crate::domain::ProjectMemoryEdit::Whole;
        let (actor, key) = validate_remember_request(request, edit, redactor)?;
        let mut request = request.clone();
        request.actor = actor;

        let work_schema_version = self.opened_work_schema_version();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(now) = register_at {
            super::work::register_process_default_in_write(
                &transaction,
                work_schema_version,
                &request.project_id,
                &request.session_id,
                now,
            )?;
        }
        let _ = project_memory_state_on(&transaction, &request.project_id)?;
        let history = lookup_project_memory_history_on(&transaction, &request.project_id, &key)?;
        let existing = history.last();
        let current = history_revision(&history)?;
        // A partial edit stores the full body it builds from its basis, so
        // the replay and conflict checks below see what will be stored.
        if partial {
            let basis = request.expected_revision.unwrap_or_default();
            match existing {
                None => return Err(StoreError::ProjectMemoryNotFound(key)),
                Some(entry) if entry.assertion.status == MemoryStatus::Tombstoned => {
                    return Err(StoreError::ProjectMemoryRetired(key));
                }
                Some(_) => {}
            }
            let version = usize::try_from(basis - 1)
                .ok()
                .and_then(|index| history.get(index))
                .ok_or_else(|| StoreError::ProjectMemoryRevisionNotFound {
                    key: key.clone(),
                    revision: basis,
                    current,
                })?;
            let built = edit::assemble(&key, basis, &version.version.body, edit, &request.body)
                .and_then(|body| {
                    if body.len() > MAX_PROJECT_MEMORY_BODY_BYTES {
                        return Err(StoreError::InvalidProjectMemory(format!(
                            "the revised memory body exceeds {MAX_PROJECT_MEMORY_BODY_BYTES} UTF-8 bytes"
                        )));
                    }
                    Ok(body)
                });
            // An edit that cannot be built on a basis that is no longer
            // current is a stale edit: the writer must read the head again,
            // not mend an old revision. Building is deterministic, so a
            // replay never lands here.
            let body = match built {
                Err(_) if basis != current => {
                    return Err(StoreError::ProjectMemoryRevisionConflict {
                        key,
                        expected: basis,
                        current,
                    });
                }
                built => built?,
            };
            redactor
                .inspect(&body)
                .map_err(StoreError::RedactionRefused)?;
            request.body = body;
        }
        // The version a revise builds on: the supplied basis, or the head.
        let basis_version = if request.revise {
            request
                .expected_revision
                .and_then(|basis| {
                    usize::try_from(basis.saturating_sub(1))
                        .ok()
                        .and_then(|index| history.get(index))
                })
                .or(existing)
        } else {
            None
        };
        // A clear without a revise was refused with the request's own checks.
        let clears_target = matches!(
            request.retiring_target,
            crate::domain::ProjectMemoryRetiringTargetChange::Clear
        );
        let retiring_target = match &request.retiring_target {
            crate::domain::ProjectMemoryRetiringTargetChange::Keep => {
                basis_version.and_then(|entry| entry.version.retiring_target.clone())
            }
            crate::domain::ProjectMemoryRetiringTargetChange::Clear => None,
            crate::domain::ProjectMemoryRetiringTargetChange::Set { target } => Some(
                resolve_retiring_target_on(&transaction, &request.project_id, target)?,
            ),
        };
        if let Some(existing) = &existing {
            if existing.assertion.status == MemoryStatus::Tombstoned {
                return Err(StoreError::ProjectMemoryRetired(key));
            }
            // A supplied basis identifies an exact historical revision intent.
            // Without a basis, only an identical current capture replays; a
            // later differing head is a new append, never an old-head overwrite.
            let replay_revision = if request.revise {
                request
                    .expected_revision
                    .map_or(Some(current), |basis| basis.checked_add(1))
            } else {
                Some(1)
            };
            if let Some(replay_revision) = replay_revision
                && let Some(replay) = usize::try_from(replay_revision.saturating_sub(1))
                    .ok()
                    .and_then(|index| history.get(index))
                && (request.revise == (replay_revision > 1))
                && replay.version.body == request.body
                && replay.version.retiring_target == retiring_target
                && replay.version.retiring_target_cleared == clears_target
                && replay.version.actor.actor_id == request.actor.actor_id
                && replay.version.actor.session_id == request.actor.session_id
            {
                let replay_index = usize::try_from(replay_revision - 1).map_err(|_| {
                    StoreError::InvalidProjectMemory("memory revision exceeds its range".into())
                })?;
                // A replay answers with a stored version: it is judged by the
                // bound it was admitted under.
                admit_full_response(
                    &with_read_time_reserve(memory_full(&key, &history, replay_index, current)),
                    ProjectMemoryAdmission::Retained,
                )?;
                // The change is read from the two revisions the replay names,
                // never from the current head.
                let change = replay_index
                    .checked_sub(1)
                    .and_then(|index| history.get(index))
                    .map(|before| edit::change(edit, &before.version.body, &replay.version.body));
                let receipt = ProjectMemoryMutationReceipt {
                    key,
                    revision: replay_revision,
                    replaced_revision: request.revise.then_some(replay_revision - 1),
                    remembered_at: replay.version.created_at,
                    forgotten_at: None,
                    duplicate: true,
                    change,
                };
                // A replay writes nothing of its own; it commits so that a
                // registration made in this transaction is kept.
                transaction.commit()?;
                return Ok(receipt);
            }
            if !request.revise {
                return Err(StoreError::ProjectMemoryExists(key));
            }
            if let Some(expected) = request.expected_revision
                && expected != current
            {
                return Err(StoreError::ProjectMemoryRevisionConflict {
                    key,
                    expected,
                    current,
                });
            }
            if request.created_at < existing.version.created_at {
                return Err(StoreError::InvalidProjectMemory(
                    "revision timestamp precedes the current memory".into(),
                ));
            }
            // A clear marker must always mean that a target was removed on
            // purpose: the current one, or one a revision dropped, which the
            // clear then acknowledges. With neither, the clear is refused.
            if clears_target && !history::has_clearable_retiring_target(&history) {
                return Err(StoreError::InvalidProjectMemory(format!(
                    "project memory {key} has no retirement target to clear"
                )));
            }
        } else if request.revise {
            return Err(StoreError::ProjectMemoryNotFound(key));
        }
        let revision = current.checked_add(1).ok_or_else(|| {
            StoreError::InvalidProjectMemory("memory revision exceeds its range".into())
        })?;

        let full = ProjectMemoryFull {
            key: key.clone(),
            revision,
            current_revision: revision,
            body: request.body.clone(),
            remembered_at: request.created_at,
            actor_id: request.actor.actor_id.clone(),
            actor_context: request.actor.attribution_context().map(str::to_owned),
            session_id: request.actor.session_id.clone(),
            retiring_target: retiring_target.clone(),
            retiring_state: None,
            retiring_target_dropped: if retiring_target.is_none() && !clears_target {
                history::retiring_target_dropped_before(&history)
            } else {
                None
            },
            workaround: retiring_target.as_ref().map(|_| true),
        };
        // Stored versions must stay readable at the new revision count; only
        // the version being admitted takes the newer, tighter bound.
        for index in 0..history.len() {
            admit_full_response(
                &with_read_time_reserve(memory_full(&key, &history, index, revision)),
                ProjectMemoryAdmission::Retained,
            )?;
        }
        admit_full_response(
            &with_read_time_reserve(full),
            ProjectMemoryAdmission::NewVersion,
        )?;

        let prepared =
            prepare_project_memory(&request, &key, existing, retiring_target, clears_target)?;
        Self::insert_project_memory_version_object(
            &transaction,
            &prepared.version_object,
            &request.project_id,
            &key,
        )?;
        Self::insert_object(
            &transaction,
            "memory_assertion_event",
            &prepared.assertion_object,
        )?;
        Self::apply_memory_projection(
            &transaction,
            prepared.version_object.key(),
            prepared.assertion_object.key(),
            &prepared.version,
            &prepared.assertion,
            MemoryProjectionMode::Live,
        )?;
        advance_project_memory_state_on(
            &transaction,
            &request.project_id,
            i64::from(existing.is_none()),
        )?;
        let change = existing
            .filter(|_| request.revise)
            .map(|before| edit::change(edit, &before.version.body, &request.body));
        transaction.commit()?;
        Ok(ProjectMemoryMutationReceipt {
            key,
            revision,
            replaced_revision: request.revise.then_some(current),
            remembered_at: request.created_at,
            forgotten_at: None,
            duplicate: false,
            change,
        })
    }

    /// Appends an attributed terminal tombstone for one project-memory key.
    ///
    /// # Errors
    ///
    /// Returns a typed project-memory refusal when authorization, key
    /// resolution, or terminal lifecycle validation fails.
    pub fn forget_project_memory<R: Redactor>(
        &mut self,
        request: &ForgetProjectMemoryRequest,
        redactor: &R,
    ) -> Result<ProjectMemoryMutationReceipt, StoreError> {
        self.forget_project_memory_registering(request, redactor, None)
    }

    /// [`Self::forget_project_memory`], first registering the request's
    /// fresh process-default session at `register_at`, when given, inside the
    /// same transaction: any refusal leaves no session row and reclaims
    /// nothing.
    ///
    /// # Errors
    ///
    /// Refuses as that function does, or when the registration fails.
    pub(crate) fn forget_project_memory_registering<R: Redactor>(
        &mut self,
        request: &ForgetProjectMemoryRequest,
        redactor: &R,
        register_at: Option<chrono::DateTime<chrono::Utc>>,
    ) -> Result<ProjectMemoryMutationReceipt, StoreError> {
        let (actor, key) = validate_forget_request(request, redactor)?;
        let work_schema_version = self.opened_work_schema_version();
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(now) = register_at {
            super::work::register_process_default_in_write(
                &transaction,
                work_schema_version,
                &request.project_id,
                &request.session_id,
                now,
            )?;
        }
        let _ = project_memory_state_on(&transaction, &request.project_id)?;
        let history = lookup_project_memory_history_on(&transaction, &request.project_id, &key)?;
        let existing = history
            .last()
            .ok_or_else(|| StoreError::ProjectMemoryNotFound(key.clone()))?;
        let revision = history_revision(&history)?;
        if existing.assertion.status == MemoryStatus::Tombstoned {
            let receipt = ProjectMemoryMutationReceipt {
                key,
                revision,
                replaced_revision: None,
                remembered_at: existing.version.created_at,
                forgotten_at: Some(existing.assertion.created_at),
                duplicate: true,
                change: None,
            };
            // A replay writes nothing of its own; it commits so that a
            // registration made in this transaction is kept.
            transaction.commit()?;
            return Ok(receipt);
        }
        if existing.assertion.status != MemoryStatus::Active {
            return Err(StoreError::InvalidMemoryProjection(format!(
                "project memory key has unsupported status {:?}",
                existing.assertion.status
            )));
        }
        if request.created_at < existing.version.created_at {
            return Err(StoreError::InvalidProjectMemory(format!(
                "forget timestamp {} precedes the remembered timestamp {} for project memory {key}",
                request.created_at, existing.version.created_at
            )));
        }
        let assertion = MemoryAssertionEvent {
            schema_version: SCHEMA_VERSION,
            memory_id: existing.version.memory_id,
            version: existing.version_id.clone(),
            status: MemoryStatus::Tombstoned,
            policy_reason: "explicit project-memory forget".into(),
            actor,
            created_at: request.created_at,
        };
        let assertion_object = CanonicalObject::mint(&assertion)?;
        Self::insert_object(&transaction, "memory_assertion_event", &assertion_object)?;
        Self::apply_memory_projection(
            &transaction,
            &existing.version_id,
            assertion_object.key(),
            &existing.version,
            &assertion,
            MemoryProjectionMode::Live,
        )?;
        advance_project_memory_state_on(&transaction, &request.project_id, -1)?;
        transaction.commit()?;
        Ok(ProjectMemoryMutationReceipt {
            key,
            revision,
            replaced_revision: None,
            remembered_at: existing.version.created_at,
            forgotten_at: Some(request.created_at),
            duplicate: false,
            change: None,
        })
    }
}

fn bounded_project_memory_attribution_text(value: &str, label: &str) -> Result<String, StoreError> {
    if value.trim().is_empty() || value.len() > MAX_PROJECT_MEMORY_ATTRIBUTION_TEXT_BYTES {
        return Err(StoreError::InvalidProjectMemory(format!(
            "{label} must contain from 1 through {MAX_PROJECT_MEMORY_ATTRIBUTION_TEXT_BYTES} UTF-8 bytes"
        )));
    }
    Ok(value.to_owned())
}

fn bounded_optional_project_memory_attribution_text(
    value: Option<&str>,
    label: &str,
) -> Result<Option<String>, StoreError> {
    value
        .map(|value| bounded_project_memory_attribution_text(value, label))
        .transpose()
}

fn validated_project_memory_actor<R: Redactor>(
    actor: &ActorContext,
    redactor: &R,
) -> Result<ActorContext, StoreError> {
    validate_project_memory_actor_shape(actor)?;
    let validated = actor.clone();
    for prose in [
        Some(validated.actor_id.as_str()),
        Some(validated.actor_kind.as_str()),
        Some(validated.reason.as_str()),
        validated.run_id.as_deref(),
        validated
            .session_id
            .as_ref()
            .map(|session| session.0.as_str()),
        validated.source_tool.as_deref(),
        validated.source_skill.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        redactor
            .inspect(prose)
            .map_err(StoreError::RedactionRefused)?;
    }
    for link in &validated.provenance_chain {
        redactor
            .inspect(&link.source)
            .map_err(StoreError::RedactionRefused)?;
        if let Some(reference) = link.reference.as_deref() {
            redactor
                .inspect(reference)
                .map_err(StoreError::RedactionRefused)?;
        }
    }
    Ok(validated)
}

/// The attribution every stored project-memory version, assertion and
/// tombstone carries: what remember and forget admit, and what a snapshot of
/// those records must therefore hold.
pub(in crate::storage) fn validate_project_memory_actor_shape(
    actor: &ActorContext,
) -> Result<(), StoreError> {
    actor.validate_attribution_context().map_err(|detail| {
        StoreError::InvalidProjectMemory(format!(
            "project-memory attribution has invalid actor context: {detail}"
        ))
    })?;
    if actor.provenance_chain.len() > MAX_PROJECT_MEMORY_PROVENANCE_LINKS {
        return Err(StoreError::InvalidProjectMemory(format!(
            "project-memory attribution must contain at most {MAX_PROJECT_MEMORY_PROVENANCE_LINKS} provenance links"
        )));
    }
    bounded_project_memory_attribution_text(&actor.actor_id, "project-memory actor")?;
    bounded_project_memory_attribution_text(&actor.actor_kind, "project-memory actor kind")?;
    bounded_project_memory_attribution_text(&actor.reason, "project-memory attribution reason")?;
    bounded_optional_project_memory_attribution_text(
        actor.run_id.as_deref(),
        "project-memory run",
    )?;
    let session = actor.session_id.as_ref().ok_or_else(|| {
        StoreError::InvalidProjectMemory(
            "project-memory attribution requires a nonblank session".into(),
        )
    })?;
    bounded_project_memory_attribution_text(&session.0, "project-memory session")?;
    bounded_optional_project_memory_attribution_text(
        actor.source_tool.as_deref(),
        "project-memory source tool",
    )?;
    bounded_optional_project_memory_attribution_text(
        actor.source_skill.as_deref(),
        "project-memory source skill",
    )?;
    for (index, link) in actor.provenance_chain.iter().enumerate() {
        bounded_project_memory_attribution_text(
            &link.source,
            &format!("project-memory provenance source {index}"),
        )?;
        bounded_optional_project_memory_attribution_text(
            link.reference.as_deref(),
            &format!("project-memory provenance reference {index}"),
        )?;
    }
    let attribution_bytes = crate::canonical::canonical_bytes(actor)?;
    if attribution_bytes.len() > MAX_PROJECT_MEMORY_ATTRIBUTION_BYTES {
        return Err(StoreError::InvalidProjectMemory(format!(
            "project-memory attribution exceeds the {MAX_PROJECT_MEMORY_ATTRIBUTION_BYTES}-byte canonical limit"
        )));
    }
    Ok(())
}

fn admit_live_project_memory_sessions(
    session_id: &SessionId,
    actor: &ActorContext,
) -> Result<(), StoreError> {
    crate::storage::admit_session_id(session_id)?;
    if let Some(session) = actor.session_id.as_ref() {
        crate::storage::admit_session_id(session)?;
    }
    Ok(())
}

fn validate_project_memory_authorization(
    session_id: &SessionId,
    actor: &ActorContext,
) -> Result<(), StoreError> {
    if actor.actor_id.trim().is_empty()
        || session_id.0.trim().is_empty()
        || actor
            .session_id
            .as_ref()
            .is_none_or(|value| value.0.trim().is_empty())
        || actor.session_id.as_ref() != Some(session_id)
    {
        return Err(StoreError::ProjectMemoryBindingInvalid);
    }
    Ok(())
}

/// Which version a full-read admission check judges.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum ProjectMemoryAdmission {
    /// The version this write admits now.
    NewVersion,
    /// A version already stored: history re-read at the new revision count,
    /// or the version an exact retry replays.
    Retained,
}

/// Admission sees a full read as it will be read later: a local target's item
/// state is computed at read time, so admission fills it with its largest
/// form. That is a completed item, whose read adds the forget-candidate
/// reminder and command, with a timestamp carrying nanoseconds.
fn with_read_time_reserve(mut full: ProjectMemoryFull) -> ProjectMemoryFull {
    if matches!(
        full.retiring_target,
        Some(crate::domain::ProjectMemoryRetiringTarget::Local { .. })
    ) {
        full.retiring_state = Some(crate::domain::ProjectMemoryRetiringState {
            lifecycle: crate::domain::WorkLifecycle::Completed,
            // 9999-12-31T23:59:59.999999999Z, the longest four-digit-year form.
            updated_at: chrono::DateTime::from_timestamp(253_402_300_799, 999_999_999)
                .unwrap_or(chrono::DateTime::<chrono::Utc>::MAX_UTC),
        });
    }
    full
}

fn prepare_project_memory(
    request: &RememberProjectMemoryRequest,
    key: &str,
    previous: Option<&StoredProjectMemory>,
    retiring_target: Option<crate::domain::ProjectMemoryRetiringTarget>,
    retiring_target_cleared: bool,
) -> Result<PreparedProjectMemory, StoreError> {
    let memory_id = previous.map_or_else(MemoryId::new, |entry| entry.version.memory_id);
    let version = MemoryVersion {
        schema_version: SCHEMA_VERSION,
        memory_id,
        project_key: Some(key.to_owned()),
        retiring_target,
        retiring_target_cleared,
        parents: previous
            .map(|entry| entry.version_id.clone())
            .into_iter()
            .collect(),
        kind: MemoryKind::Episode,
        authority: Authority::Soft,
        delivery: Delivery::OnDemand,
        scope: Scope::Project {
            project: request.project_id.clone(),
        },
        title: format!("Project memory {key}"),
        body: request.body.clone(),
        structured_value: None,
        tags: vec!["project-memory".into()],
        evidence: Vec::new(),
        refs: Vec::new(),
        source_snapshot: None,
        confidence: None,
        sensitivity: Sensitivity::Internal,
        classification_reason: "explicit project episode".into(),
        delivery_override_reason: None,
        valid_from: None,
        valid_until: None,
        review_by: None,
        last_verified: None,
        actor: request.actor.clone(),
        created_at: request.created_at,
    };
    let version_object = CanonicalObject::mint(&version)?;
    let assertion = MemoryAssertionEvent {
        schema_version: SCHEMA_VERSION,
        memory_id,
        version: version_object.key().clone(),
        status: MemoryStatus::Active,
        policy_reason: "project episodes are active immediately".into(),
        actor: request.actor.clone(),
        created_at: request.created_at,
    };
    let assertion_object = CanonicalObject::mint(&assertion)?;
    Ok(PreparedProjectMemory {
        version,
        assertion,
        version_object,
        assertion_object,
    })
}

fn validate_project_memory_key(value: &str) -> Result<String, StoreError> {
    let bytes = value.as_bytes();
    let valid = !bytes.is_empty()
        && bytes.len() <= MAX_PROJECT_MEMORY_KEY_BYTES
        && (bytes[0].is_ascii_lowercase() || bytes[0].is_ascii_digit());
    let tail_valid = bytes
        .iter()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || b"._-".contains(byte));
    if !valid || !tail_valid {
        return Err(StoreError::InvalidProjectMemory(format!(
            "memory key must be 1-{MAX_PROJECT_MEMORY_KEY_BYTES} ASCII bytes matching [a-z0-9][a-z0-9._-]*"
        )));
    }
    Ok(value.to_owned())
}

fn slug_project_memory_key(body: &str) -> Result<String, StoreError> {
    let mut slug = String::new();
    let mut between_words = false;
    for byte in body.bytes() {
        if byte.is_ascii_alphanumeric() {
            if between_words && !slug.is_empty() && slug.len() < MAX_PROJECT_MEMORY_KEY_BYTES {
                slug.push('-');
            }
            if slug.len() >= MAX_PROJECT_MEMORY_KEY_BYTES {
                break;
            }
            slug.push(char::from(byte.to_ascii_lowercase()));
            between_words = false;
        } else if !slug.is_empty() {
            between_words = true;
        }
    }
    while slug.ends_with('-') {
        slug.pop();
    }
    if slug.is_empty() {
        return Err(StoreError::InvalidProjectMemory(
            UNSAFE_KEY_REFUSAL.cli.into(),
        ));
    }
    validate_project_memory_key(&slug)
}

pub(super) fn validate_keyed_project_memory_shape(
    version: &MemoryVersion,
    assertion: &MemoryAssertionEvent,
) -> Result<(), StoreError> {
    let Some(key) = version.project_key.as_deref() else {
        // Retiring targets and clears belong to keyed project memories only.
        if version.retiring_target.is_some() || version.retiring_target_cleared {
            return Err(StoreError::InvalidMemoryProjection(
                "a memory without a project key carries a retirement target or clear".into(),
            ));
        }
        return Ok(());
    };
    let invalid = |detail: &str| {
        StoreError::InvalidMemoryProjection(format!(
            "keyed project memory has invalid canonical shape: {detail}"
        ))
    };
    validate_project_memory_key(key).map_err(|error| invalid(&error.to_string()))?;
    let Scope::Project { .. } = &version.scope else {
        return Err(invalid("project_key requires project scope"));
    };
    let restored = version.source_snapshot.as_ref().is_some_and(|source| {
        (source.source_ref == super::graph_snapshot::RESTORED_MEMORY_SOURCE
            || source.source_ref == super::graph_snapshot::RESTORED_REDACTED_MEMORY_SOURCE)
            && ObjectId::from_stored(source.fingerprint.clone()).is_some()
    });
    if let Some(target) = version.retiring_target.as_ref() {
        validate_retiring_target_shape(target).map_err(|error| invalid(&error.to_string()))?;
    }
    // A clear removes the previous version's target, so it never carries one
    // and never starts a chain.
    if version.retiring_target_cleared
        && (version.retiring_target.is_some() || version.parents.is_empty())
    {
        return Err(invalid(
            "a retirement-target clear must follow a version and carry no target",
        ));
    }
    if version.parents.len() <= 1
        && version.kind == MemoryKind::Episode
        && version.authority == Authority::Soft
        && version.delivery == Delivery::OnDemand
        && version.title == format!("Project memory {key}")
        && !version.body.trim().is_empty()
        && version.body.len() <= MAX_PROJECT_MEMORY_BODY_BYTES
        && version.structured_value.is_none()
        && version.tags.len() == 1
        && version.tags[0] == "project-memory"
        && version.evidence.is_empty()
        && version.refs.is_empty()
        && (version.source_snapshot.is_none() || restored)
        && version.confidence.is_none()
        && (version.sensitivity == Sensitivity::Internal || restored)
        && version.classification_reason
            == if restored {
                "restored project episode"
            } else {
                "explicit project episode"
            }
        && version.delivery_override_reason.is_none()
        && version.valid_from.is_none()
        && version.valid_until.is_none()
        && version.review_by.is_none()
        && version.last_verified.is_none()
    {
        validate_project_memory_actor_shape(&version.actor)
            .map_err(|error| invalid(&error.to_string()))?;
        validate_project_memory_actor_shape(&assertion.actor)
            .map_err(|error| invalid(&error.to_string()))?;
    } else {
        return Err(invalid(
            "version fields do not match the fixed project-episode contract",
        ));
    }
    let lifecycle_matches = match assertion.status {
        MemoryStatus::Active => {
            assertion.policy_reason
                == if restored {
                    "restored project episode is active immediately"
                } else {
                    "project episodes are active immediately"
                }
                && assertion.actor == version.actor
                && assertion.created_at == version.created_at
        }
        MemoryStatus::Tombstoned => {
            (assertion.policy_reason == "explicit project-memory forget"
                || (restored && assertion.policy_reason == "restored project-memory tombstone"))
                && assertion.created_at >= version.created_at
        }
        _ => false,
    };
    if !lifecycle_matches {
        return Err(invalid(
            "assertion does not match the active-or-terminal project-memory lifecycle",
        ));
    }
    Ok(())
}
