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

mod edit;
mod history;
#[cfg(test)]
mod tests;
use history::memory_full;
pub(in crate::storage) use history::project_memory_history_on;

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

    /// Creates or revises one project memory as
    /// [`Self::remember_project_memory_with_admission`] does, building a
    /// revise's body from the revision it names: the request's text replaces
    /// the whole body, is appended, or replaces one marked section. A partial
    /// edit names its basis revision, which must be the current one, and is
    /// assembled in the writer transaction, so an exact retry replays rather
    /// than appending twice.
    ///
    /// # Errors
    ///
    /// Refuses as the whole-body path does; a partial edit without `--revise`,
    /// a key and a positive basis; a basis revision that does not exist; and
    /// a section the basis lacks or a body whose markers do not pair.
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
                crate::domain::ProjectMemoryRetiringTargetInput::External {
                    project,
                    reference,
                } => redactor
                    .inspect(project)
                    .and_then(|()| redactor.inspect(reference)),
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
        let mut request = request.clone();
        request.actor = actor;

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
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
        let clears_target = matches!(
            request.retiring_target,
            crate::domain::ProjectMemoryRetiringTargetChange::Clear
        );
        if clears_target && !request.revise {
            return Err(StoreError::InvalidProjectMemory(
                CLEAR_TARGET_NEEDS_REVISE_REFUSAL.cli.into(),
            ));
        }
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
                return Ok(ProjectMemoryMutationReceipt {
                    key,
                    revision: replay_revision,
                    replaced_revision: request.revise.then_some(replay_revision - 1),
                    remembered_at: replay.version.created_at,
                    forgotten_at: None,
                    duplicate: true,
                    change,
                });
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
        admit_live_project_memory_sessions(&request.session_id, &request.actor)?;
        validate_project_memory_authorization(&request.session_id, &request.actor)?;
        let actor = validated_project_memory_actor(&request.actor, redactor)?;
        validate_project_memory_authorization(&request.session_id, &actor)?;
        let key = validate_project_memory_key(&request.key)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let _ = project_memory_state_on(&transaction, &request.project_id)?;
        let history = lookup_project_memory_history_on(&transaction, &request.project_id, &key)?;
        let existing = history
            .last()
            .ok_or_else(|| StoreError::ProjectMemoryNotFound(key.clone()))?;
        let revision = history_revision(&history)?;
        if existing.assertion.status == MemoryStatus::Tombstoned {
            return Ok(ProjectMemoryMutationReceipt {
                key,
                revision,
                replaced_revision: None,
                remembered_at: existing.version.created_at,
                forgotten_at: Some(existing.assertion.created_at),
                duplicate: true,
                change: None,
            });
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

    /// Returns a dedicated bounded full-read envelope for one live key.
    ///
    /// # Errors
    ///
    /// Returns a typed project-memory refusal when authorization, key
    /// resolution, lifecycle, or stored-envelope validation fails.
    pub fn project_memory_full(
        &self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        actor: &ActorContext,
        key: &str,
        revision: Option<u64>,
    ) -> Result<ProjectMemoryFull, StoreError> {
        admit_live_project_memory_sessions(session_id, actor)?;
        validate_project_memory_authorization(session_id, actor)?;
        if self.connection.is_autocommit() {
            let snapshot = self.connection.unchecked_transaction()?;
            let full = self.project_memory_full(project_id, session_id, actor, key, revision)?;
            snapshot.commit()?;
            return Ok(full);
        }
        let key = validate_project_memory_key(key)?;
        let history = lookup_project_memory_history_on(&self.connection, project_id, &key)?;
        let existing = history
            .last()
            .ok_or_else(|| StoreError::ProjectMemoryNotFound(key.clone()))?;
        if existing.assertion.status == MemoryStatus::Tombstoned {
            return Err(StoreError::ProjectMemoryRetired(key));
        }
        if existing.assertion.status != MemoryStatus::Active {
            return Err(StoreError::InvalidMemoryProjection(format!(
                "project memory key has unsupported status {:?}",
                existing.assertion.status
            )));
        }
        let current = history_revision(&history)?;
        let revision = revision.unwrap_or(current);
        let index = revision
            .checked_sub(1)
            .and_then(|n| usize::try_from(n).ok())
            .filter(|index| *index < history.len())
            .ok_or_else(|| StoreError::ProjectMemoryRevisionNotFound {
                key: key.clone(),
                revision,
                current,
            })?;
        let mut full = memory_full(&key, &history, index, current);
        full.retiring_state =
            retiring_state_on(&self.connection, project_id, full.retiring_target.as_ref())?;
        Ok(full)
    }

    /// Lists live project memories without returning their bodies.
    ///
    /// # Errors
    ///
    /// Returns a typed project-memory refusal when authorization, cursor, or
    /// query validation fails, or when the stored projection is invalid.
    pub fn project_memories(
        &self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        actor: &ActorContext,
        query: Option<&str>,
        after: Option<&str>,
    ) -> Result<ProjectMemoryList, StoreError> {
        self.project_memories_at_cut(project_id, session_id, actor, query, after, false)
            .map(|(list, _)| list)
    }

    /// The listing, and with `records` the memory position its own snapshot
    /// read. A listing that carries a context generation records that
    /// position, so a memory a peer writes after the snapshot is announced
    /// again. Only that listing reads the position: every other form lists
    /// from the memory rows alone, as it always did, even while the position
    /// is missing. The record is advisory, so a position that cannot be read
    /// leaves the listing delivered and unrecorded.
    pub(crate) fn project_memories_at_cut(
        &self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        actor: &ActorContext,
        query: Option<&str>,
        after: Option<&str>,
        records: bool,
    ) -> Result<(ProjectMemoryList, Option<ProjectMemoryListingCut>), StoreError> {
        admit_live_project_memory_sessions(session_id, actor)?;
        validate_project_memory_authorization(session_id, actor)?;
        let normalized_query = normalize_project_memory_query(query)?;
        if normalized_query.is_some() && after.is_some() {
            return Err(StoreError::InvalidProjectMemory(
                FILTERED_SEARCH_AFTER_REFUSAL.cli.into(),
            ));
        }
        let normalized_after = after.map(validate_project_memory_key).transpose()?;
        // One snapshot: its own, or an enclosing read transaction it joins.
        let read = |connection: &Connection| -> Result<_, StoreError> {
            let rows = project_memory_rows_on(
                connection,
                project_id,
                normalized_query,
                normalized_after.as_deref(),
                PROJECT_MEMORY_LIST_LIMIT + 1,
            )?;
            let state = records
                .then(|| project_memory_state_on(connection, project_id).ok())
                .flatten();
            Ok((rows, state))
        };
        let ((rows, total_matches), state) = if self.connection.is_autocommit() {
            let transaction = self.connection.unchecked_transaction()?;
            let read = read(&transaction)?;
            transaction.commit()?;
            read
        } else {
            read(&self.connection)?
        };
        let has_more = rows.len() > PROJECT_MEMORY_LIST_LIMIT;
        let memories = rows
            .into_iter()
            .take(PROJECT_MEMORY_LIST_LIMIT)
            .collect::<Vec<_>>();
        let omitted_count = total_matches
            .unwrap_or(memories.len())
            .saturating_sub(memories.len());
        let next_after = if normalized_query.is_none() && has_more {
            memories.last().map(|row| row.key.clone())
        } else {
            None
        };
        let exhausted = if normalized_query.is_some() {
            omitted_count == 0
        } else {
            next_after.is_none()
        };
        let listing = state.map(|(_, change_position)| ProjectMemoryListingCut { change_position });
        Ok((
            ProjectMemoryList {
                memories,
                next_after,
                omitted_count,
                exhausted,
            },
            listing,
        ))
    }

    /// Returns the advisory content-free memory signal and acknowledges it as
    /// an advancing next does.
    ///
    /// # Errors
    ///
    /// Returns a typed project-memory refusal when the context generation is
    /// invalid or the advisory projection cannot be read or updated.
    #[cfg(test)]
    pub(crate) fn project_memory_advertisement(
        &mut self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        context_generation: Option<&str>,
    ) -> Result<(usize, bool), StoreError> {
        let advertisement = self.project_memory_advertisement_candidate(
            project_id,
            session_id,
            context_generation,
        )?;
        let result = (advertisement.count, advertisement.changed);
        if advertisement.changed {
            self.acknowledge_project_memory_advertisement(project_id, session_id, &advertisement)?;
        }
        Ok(result)
    }

    pub(crate) fn project_memory_advertisement_candidate(
        &self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        context_generation: Option<&str>,
    ) -> Result<ProjectMemoryAdvertisement, StoreError> {
        validate_context_generation(context_generation)?;
        self.work_read_snapshot(|store| {
            let connection = &store.connection;
            let (count, change_position) = project_memory_state_on(connection, project_id)?;
            let context_generation_digest =
                context_generation.map(project_memory_context_generation_digest);
            let prior = connection
                .query_row(
                    "SELECT memory_position, context_generation_digest
                 FROM project_memory_advertisements
                 WHERE project_id = ?1 AND session_id = ?2",
                    params![project_id.0, session_id.0],
                    |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
                )
                .optional()?;
            // The supplied generation is one that no recorded listing of the
            // session carries; a session with no row carries none.
            let generation_unlisted = context_generation_digest.as_deref().is_some_and(|digest| {
                prior
                    .as_ref()
                    .is_none_or(|(_, prior_generation)| prior_generation.as_deref() != Some(digest))
            });
            let changed = generation_unlisted
                || prior
                    .as_ref()
                    .is_none_or(|(prior_position, _)| *prior_position != change_position);
            Ok(ProjectMemoryAdvertisement {
                count,
                changed,
                generation_unlisted,
                change_position,
            })
        })
    }

    /// Records that an advancing `next` delivered the memory signal: the
    /// session's recorded memory position becomes the advertised one. The
    /// recorded context generation is kept and the supplied one is never
    /// recorded, because only a memories listing that carries a generation
    /// records it.
    pub(crate) fn acknowledge_project_memory_advertisement(
        &mut self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        advertisement: &ProjectMemoryAdvertisement,
    ) -> Result<(), StoreError> {
        crate::storage::admit_session_id(session_id)?;
        if !advertisement.changed {
            return Ok(());
        }
        self.record_project_memory_position(
            project_id,
            session_id,
            advertisement.change_position,
            None,
        )
    }

    /// Records that the session listed project memories from their start
    /// with this context generation: the generation's digest and the memory
    /// position the listing read. The row records a listing, not that the
    /// notes were read or applied. A listing that repeats the recorded
    /// position and generation writes nothing.
    ///
    /// # Errors
    ///
    /// Returns a typed project-memory refusal when the context generation is
    /// invalid or the advisory projection cannot be updated.
    pub(crate) fn acknowledge_project_memory_listing(
        &mut self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        listing: ProjectMemoryListingCut,
        context_generation: &str,
    ) -> Result<(), StoreError> {
        validate_context_generation(Some(context_generation))?;
        crate::storage::admit_session_id(session_id)?;
        let digest = project_memory_context_generation_digest(context_generation);
        let recorded = self
            .connection
            .query_row(
                "SELECT memory_position, context_generation_digest
                 FROM project_memory_advertisements
                 WHERE project_id = ?1 AND session_id = ?2",
                params![project_id.0, session_id.0],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Option<String>>(1)?)),
            )
            .optional()?;
        if recorded.is_some_and(|(position, recorded_digest)| {
            position == listing.change_position && recorded_digest.as_ref() == Some(&digest)
        }) {
            return Ok(());
        }
        self.record_project_memory_position(
            project_id,
            session_id,
            listing.change_position,
            Some(digest),
        )
    }

    /// Writes the session's row, keeping the recorded generation digest when
    /// none is given, and bounds the rows kept for the project.
    fn record_project_memory_position(
        &mut self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        memory_position: i64,
        context_generation_digest: Option<String>,
    ) -> Result<(), StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let context_generation_digest = context_generation_digest.or(transaction
            .query_row(
                "SELECT context_generation_digest FROM project_memory_advertisements
                 WHERE project_id = ?1 AND session_id = ?2",
                params![project_id.0, session_id.0],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten());
        transaction.execute(
            "DELETE FROM project_memory_advertisements
             WHERE project_id = ?1
               AND session_id != ?2
               AND rowid NOT IN (
                   SELECT rowid FROM project_memory_advertisements
                   WHERE project_id = ?1 AND session_id != ?2
                   ORDER BY rowid DESC
                   LIMIT ?3
               )",
            params![
                project_id.0,
                session_id.0,
                MAX_PROJECT_MEMORY_ADVERTISEMENTS_PER_PROJECT - 1
            ],
        )?;
        transaction.execute(
            "INSERT OR REPLACE INTO project_memory_advertisements (
                 project_id, session_id, context_generation_digest, memory_position
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                project_id.0,
                session_id.0,
                context_generation_digest,
                memory_position
            ],
        )?;
        transaction.commit()?;
        Ok(())
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

fn project_memory_context_generation_digest(value: &str) -> String {
    const DOMAIN: &[u8] = b"engram-project-memory-context-generation-v1\0";
    let mut input = Vec::with_capacity(DOMAIN.len() + value.len());
    input.extend_from_slice(DOMAIN);
    input.extend_from_slice(value.as_bytes());
    let digest = <sha2::Sha256 as sha2::Digest>::digest(input);
    format!("{digest:x}")
}

fn resolve_retiring_target_on(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
    input: &crate::domain::ProjectMemoryRetiringTargetInput,
) -> Result<crate::domain::ProjectMemoryRetiringTarget, StoreError> {
    use crate::domain::{
        ProjectMemoryRetiringTarget as Target, ProjectMemoryRetiringTargetInput as Input,
    };
    let target = match input {
        Input::Local { work_ref } => {
            if work_ref.is_empty() || work_ref.len() > 128 || work_ref.trim() != work_ref {
                return Err(StoreError::InvalidProjectMemory(
                    "local retirement reference is invalid".into(),
                ));
            }
            let work = super::work::resolve_work_ref_on(connection, project_id, work_ref)?;
            Target::Local {
                work_id: work.work_id,
                work_ref: work.short_ref,
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
            Target::External {
                project: project.clone(),
                reference: reference.clone(),
            }
        }
    };
    validate_retiring_target_shape(&target)?;
    Ok(target)
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

fn retiring_state_on(
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
    super::work::resolve_work_ref_on(connection, project_id, &work_id.0.to_string()).map(Some)
}

impl SqliteStore {
    /// Doctor check: every stored local retiring target, current or
    /// historical, names an item of its memory's own project by that item's
    /// work id and short ref. Reads rely on this, so a row that fails it (from
    /// an import file, for example) is reported here instead of failing later
    /// reads of that memory.
    pub(super) fn verify_project_memory_retiring_targets_on(
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

pub(super) fn lookup_project_memory_on(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
    key: &str,
) -> Result<Option<StoredProjectMemory>, StoreError> {
    Ok(lookup_project_memory_history_on(connection, project_id, key)?.pop())
}

fn history_revision(history: &[StoredProjectMemory]) -> Result<u64, StoreError> {
    u64::try_from(history.len()).map_err(|_| {
        StoreError::InvalidMemoryProjection("memory revision exceeds its range".into())
    })
}

// Return the validated chain to callers needing its head and revision count;
// neither counting nor selecting a historical body bypasses validation.
fn lookup_project_memory_history_on(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
    key: &str,
) -> Result<Vec<StoredProjectMemory>, StoreError> {
    // The hard index requirement makes a missing or incompatible rebuildable
    // projection fail closed; open/doctor names the explicit repair command.
    let stored = connection
        .query_row(
            "SELECT head.memory_id, head.version_id, head.assertion_id,
                    head.schema_version, head.status, head.scope_kind,
                    head.project_id, head.task_id, head.work_id, head.agent_id,
                    head.memory_kind, head.authority, head.delivery,
                    head.sensitivity, head.title, head.body, head.created_at_ms
             FROM objects AS object INDEXED BY objects_project_memory_key
             JOIN memory_heads AS head ON head.version_id = object.object_id
             WHERE object.object_kind = 'memory_version'
               AND json_extract(object.canonical_json, '$.scope.kind') = 'project'
               AND json_type(object.canonical_json, '$.project_key') = 'text'
               AND json_extract(object.canonical_json, '$.scope.project') = ?1
               AND json_extract(object.canonical_json, '$.project_key') = ?2",
            params![project_id.0, key],
            |row| {
                Ok(MemoryHeadProjectionRow {
                    memory_id: row.get(0)?,
                    version_id: row.get(1)?,
                    assertion_id: row.get(2)?,
                    schema_version: row.get(3)?,
                    status: row.get(4)?,
                    scope_kind: row.get(5)?,
                    project_id: row.get(6)?,
                    task_id: row.get(7)?,
                    work_id: row.get(8)?,
                    agent_id: row.get(9)?,
                    memory_kind: row.get(10)?,
                    authority: row.get(11)?,
                    delivery: row.get(12)?,
                    sensitivity: row.get(13)?,
                    title: row.get(14)?,
                    body: row.get(15)?,
                    created_at_ms: row.get(16)?,
                })
            },
        )
        .optional()?;
    let Some(stored) = stored else {
        let reserved = connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM objects INDEXED BY objects_project_memory_key
                 WHERE object_kind = 'memory_version'
                   AND json_extract(canonical_json, '$.scope.kind') = 'project'
                   AND json_type(canonical_json, '$.project_key') = 'text'
                   AND json_extract(canonical_json, '$.scope.project') = ?1
                   AND json_extract(canonical_json, '$.project_key') = ?2
             )",
            params![project_id.0, key],
            |row| row.get::<_, bool>(0),
        )?;
        if reserved {
            return Err(StoreError::InvalidMemoryProjection(
                "project memory key is reserved but its durable head is missing".into(),
            ));
        }
        return Ok(Vec::new());
    };
    let version_id = ObjectId::from_stored(stored.version_id.clone())
        .ok_or_else(|| StoreError::InvalidStoredKey(stored.version_id.clone()))?;
    let assertion_id = ObjectId::from_stored(stored.assertion_id.clone())
        .ok_or_else(|| StoreError::InvalidStoredKey(stored.assertion_id.clone()))?;
    let version: MemoryVersion =
        SqliteStore::get_typed_object_on(connection, &version_id, "memory_version")?.ok_or_else(
            || StoreError::InvalidMemoryProjection("project memory version is missing".into()),
        )?;
    let assertion: MemoryAssertionEvent =
        SqliteStore::get_typed_object_on(connection, &assertion_id, "memory_assertion_event")?
            .ok_or_else(|| {
                StoreError::InvalidMemoryProjection("project memory assertion is missing".into())
            })?;
    validate_keyed_project_memory_shape(&version, &assertion)?;
    let expected = SqliteStore::expected_memory_head_projection(
        &version_id,
        &assertion_id,
        &version,
        &assertion,
        assertion.status,
    )?;
    let shape_matches = stored == expected
        && version.project_key.as_deref() == Some(key)
        && matches!(&version.scope, Scope::Project { project } if project == project_id)
        && version.kind == MemoryKind::Episode
        && version.authority == Authority::Soft
        && version.delivery == Delivery::OnDemand
        && assertion.memory_id == version.memory_id
        && assertion.version == version_id;
    if !shape_matches {
        return Err(StoreError::InvalidMemoryProjection(
            "project memory key projection does not match its canonical objects".into(),
        ));
    }
    let history = project_memory_history_on(connection, project_id, key)?;
    if history
        .last()
        .is_none_or(|entry| entry.version_id != version_id || entry.assertion != assertion)
    {
        return Err(StoreError::InvalidMemoryProjection(
            "project memory head is not the current canonical revision".into(),
        ));
    }
    Ok(history)
}

pub(in crate::storage) fn validate_stored_project_memory_key(
    key: &str,
) -> Result<String, StoreError> {
    validate_project_memory_key(key).map_err(|_| {
        StoreError::InvalidMemoryProjection(
            "project memory list candidate has an unsafe canonical key".into(),
        )
    })
}

fn project_memory_rows_on(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
    query: Option<&str>,
    after: Option<&str>,
    limit: usize,
) -> Result<(Vec<ProjectMemoryListRow>, Option<usize>), StoreError> {
    let limit = i64::try_from(limit)
        .map_err(|_| StoreError::InvalidProjectMemory("memory list limit is invalid".into()))?;
    let (keys, total_matches) = if let Some(query) = query {
        let lowered_key_query = query.to_ascii_lowercase();
        let escaped_key_query = lowered_key_query
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        // A query with no searchable fragment finds nothing.
        let Some(fts_query) = fts_query(query)? else {
            return Ok((Vec::new(), Some(0)));
        };
        let mut statement = connection.prepare(
            "SELECT json_extract(object.canonical_json, '$.project_key'),
                    COUNT(*) OVER()
             FROM object_fts AS f
             JOIN memory_heads AS head ON head.version_id = f.object_id
             JOIN objects AS object ON object.object_id = head.version_id
             WHERE object.object_kind = 'memory_version'
               AND json_extract(object.canonical_json, '$.scope.kind') = 'project'
               AND json_extract(object.canonical_json, '$.scope.project') = ?1
               AND json_type(object.canonical_json, '$.project_key') = 'text'
               AND head.status = 'active'
               AND object_fts MATCH ?2
             ORDER BY
                 CASE
                     WHEN json_extract(object.canonical_json, '$.project_key') = ?3 THEN 0
                     WHEN lower(json_extract(object.canonical_json, '$.project_key'))
                         LIKE ?4 || '%' ESCAPE '\\' THEN 1
                     ELSE 2
                 END,
                 f.rank,
                 json_extract(object.canonical_json, '$.project_key')
             LIMIT ?5",
        )?;
        let matches = statement
            .query_map(
                params![
                    project_id.0,
                    fts_query,
                    lowered_key_query,
                    escaped_key_query,
                    limit
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let total = matches
            .first()
            .map_or(Ok(0), |(_, total)| usize::try_from(*total))
            .map_err(|_| {
                StoreError::InvalidMemoryProjection("memory match count is invalid".into())
            })?;
        (
            matches.into_iter().map(|(key, _)| key).collect(),
            Some(total),
        )
    } else {
        let mut statement = connection.prepare(
            "SELECT json_extract(object.canonical_json, '$.project_key')
             FROM memory_heads AS head
             JOIN objects AS object ON object.object_id = head.version_id
             WHERE object.object_kind = 'memory_version'
               AND json_extract(object.canonical_json, '$.scope.kind') = 'project'
               AND json_extract(object.canonical_json, '$.scope.project') = ?1
               AND json_type(object.canonical_json, '$.project_key') = 'text'
               AND head.status = 'active'
               AND (?2 IS NULL OR json_extract(object.canonical_json, '$.project_key') > ?2)
             ORDER BY json_extract(object.canonical_json, '$.project_key')
             LIMIT ?3",
        )?;
        (
            statement
                .query_map(params![project_id.0, after, limit], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?,
            None,
        )
    };
    let rows = keys
        .into_iter()
        .map(|key| {
            let key = validate_stored_project_memory_key(&key)?;
            let history = lookup_project_memory_history_on(connection, project_id, &key)?;
            let stored = history.last().ok_or_else(|| {
                StoreError::InvalidMemoryProjection(
                    "project memory list candidate has no canonical binding".into(),
                )
            })?;
            if stored.assertion.status != MemoryStatus::Active {
                return Err(StoreError::InvalidMemoryProjection(
                    "project memory list candidate is not active".into(),
                ));
            }
            let revision = history_revision(&history)?;
            let mut row = project_memory_list_row(key, stored, revision);
            row.retiring_state =
                retiring_state_on(connection, project_id, row.retiring_target.as_ref())?;
            row.retiring_target_dropped =
                history::retiring_target_dropped(&history, history.len() - 1);
            Ok(row)
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    Ok((rows, total_matches))
}

fn project_memory_list_row(
    key: String,
    stored: &StoredProjectMemory,
    revision: u64,
) -> ProjectMemoryListRow {
    ProjectMemoryListRow {
        key,
        revision,
        first_line: project_memory_first_line(&stored.version.body),
        remembered_at: stored.version.created_at,
        actor_id: stored.version.actor.actor_id.clone(),
        actor_context: stored
            .version
            .actor
            .attribution_context()
            .map(str::to_owned),
        retiring_target: stored.version.retiring_target.clone(),
        retiring_state: None,
        retiring_target_dropped: None,
        workaround: stored.version.retiring_target.as_ref().map(|_| true),
    }
}

pub(super) fn project_memory_state_on(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
) -> Result<(usize, i64), StoreError> {
    let state = connection
        .query_row(
            "SELECT active_count, change_position
             FROM project_memory_state WHERE project_id = ?1",
            [project_id.0.as_str()],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;
    let state = if let Some(state) = state {
        state
    } else {
        let has_project_memory = connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM objects INDEXED BY objects_project_memory_key
                 WHERE object_kind = 'memory_version'
                   AND json_extract(canonical_json, '$.scope.kind') = 'project'
                   AND json_type(canonical_json, '$.project_key') = 'text'
                   AND json_extract(canonical_json, '$.scope.project') = ?1
                 LIMIT 1
             )",
            [project_id.0.as_str()],
            |row| row.get::<_, bool>(0),
        )?;
        if has_project_memory {
            return Err(StoreError::InvalidMemoryProjection(
                "project memory state is missing for a retained project key".into(),
            ));
        }
        (0, 0)
    };
    let count = usize::try_from(state.0).map_err(|_| {
        StoreError::InvalidMemoryProjection("project memory count is invalid".into())
    })?;
    Ok((count, state.1))
}

fn advance_project_memory_state_on(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
    active_delta: i64,
) -> Result<(), StoreError> {
    let changed = if active_delta == 1 {
        connection.execute(
            "INSERT INTO project_memory_state (
                 project_id, active_count, change_position
             ) VALUES (?1, 1, 1)
             ON CONFLICT(project_id) DO UPDATE SET
                 active_count = project_memory_state.active_count + 1,
                 change_position = project_memory_state.change_position + 1",
            [project_id.0.as_str()],
        )?
    } else if active_delta == -1 {
        connection.execute(
            "UPDATE project_memory_state
             SET active_count = active_count - 1,
                 change_position = change_position + 1
             WHERE project_id = ?1 AND active_count > 0",
            [project_id.0.as_str()],
        )?
    } else if active_delta == 0 {
        connection.execute(
            "UPDATE project_memory_state SET change_position = change_position + 1
             WHERE project_id = ?1 AND active_count > 0",
            [project_id.0.as_str()],
        )?
    } else {
        return Err(StoreError::InvalidMemoryProjection(
            "project memory state delta must be minus one, zero or one".into(),
        ));
    };
    if changed != 1 {
        return Err(StoreError::InvalidMemoryProjection(
            "project memory state is missing or inconsistent".into(),
        ));
    }
    Ok(())
}

pub(super) fn derived_project_memory_state_rows_on(
    connection: &Connection,
) -> Result<Vec<(String, i64, i64)>, StoreError> {
    Ok(connection
        .prepare(
            "WITH assertion_counts AS (
                 SELECT json_extract(canonical_json, '$.version') AS version_id,
                        COUNT(*) AS assertion_count
                 FROM objects
                 WHERE object_kind = 'memory_assertion_event'
                 GROUP BY version_id
             )
             SELECT json_extract(version.canonical_json, '$.scope.project') AS project_id,
                    SUM(CASE WHEN head.status = 'active' THEN 1 ELSE 0 END),
                    SUM(COALESCE(assertion_counts.assertion_count, 0))
             FROM objects AS version
             LEFT JOIN memory_heads AS head ON head.version_id = version.object_id
             LEFT JOIN assertion_counts ON assertion_counts.version_id = version.object_id
             WHERE version.object_kind = 'memory_version'
               AND json_extract(version.canonical_json, '$.scope.kind') = 'project'
               AND json_type(version.canonical_json, '$.project_key') = 'text'
             GROUP BY json_extract(version.canonical_json, '$.scope.project')
             ORDER BY json_extract(version.canonical_json, '$.scope.project')",
        )?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?)
}

pub(super) fn derived_project_memory_state_on(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
) -> Result<(i64, i64), StoreError> {
    let heads = connection
        .prepare(
            "SELECT json_extract(version.canonical_json, '$.memory_id'),
                    COALESCE(head.status, 'superseded'), version.object_id
             FROM objects AS version INDEXED BY objects_project_memory_key
             LEFT JOIN memory_heads AS head ON head.version_id = version.object_id
             WHERE version.object_kind = 'memory_version'
               AND json_extract(version.canonical_json, '$.scope.kind') = 'project'
               AND json_type(version.canonical_json, '$.project_key') = 'text'
               AND json_extract(version.canonical_json, '$.scope.project') = ?1
             ORDER BY json_extract(version.canonical_json, '$.project_key')",
        )?
        .query_map([project_id.0.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut active_count = 0_i64;
    let mut change_position = 0_i64;
    let mut assertion_statement = connection.prepare(
        "SELECT object_id
         FROM objects INDEXED BY objects_memory_assertion_version
         WHERE object_kind = 'memory_assertion_event'
           AND json_extract(canonical_json, '$.version') = ?1
         ORDER BY object_id",
    )?;
    for (memory_id, status, version_id) in heads {
        active_count = active_count
            .checked_add(i64::from(status == "active"))
            .ok_or_else(|| {
                StoreError::InvalidMemoryProjection(
                    "project memory active count exceeds SQLite range".into(),
                )
            })?;
        let assertion_hashes = assertion_statement
            .query_map([version_id.as_str()], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        for stored_hash in assertion_hashes {
            let assertion_id = ObjectId::from_stored(stored_hash.clone())
                .ok_or(StoreError::InvalidStoredKey(stored_hash))?;
            let assertion: MemoryAssertionEvent = SqliteStore::get_typed_object_on(
                connection,
                &assertion_id,
                "memory_assertion_event",
            )?
            .ok_or_else(|| {
                StoreError::InvalidMemoryProjection(format!(
                    "project memory assertion {assertion_id} is missing"
                ))
            })?;
            if assertion.schema_version != SCHEMA_VERSION
                || assertion.memory_id.0.to_string() != memory_id
                || assertion.version.as_str() != version_id
            {
                return Err(StoreError::InvalidMemoryProjection(format!(
                    "project memory assertion {assertion_id} disagrees with its head"
                )));
            }
            change_position = change_position.checked_add(1).ok_or_else(|| {
                StoreError::InvalidMemoryProjection(
                    "project memory change position exceeds SQLite range".into(),
                )
            })?;
        }
    }
    Ok((active_count, change_position))
}

fn project_memory_first_line(body: &str) -> String {
    let line = body
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    if line.len() <= PROJECT_MEMORY_FIRST_LINE_BYTES {
        return line;
    }
    let mut end = PROJECT_MEMORY_FIRST_LINE_BYTES;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", line[..end].trim_end())
}
