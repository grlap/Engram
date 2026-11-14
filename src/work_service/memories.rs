use super::{
    MAX_PROJECT_MEMORY_FULL_BYTES, ProjectMemoryFull, READ_REMINDER_RESERVE, WorkLifecycle,
    render_agent_receipt_text, terminal_safe_multiline, work_lifecycle_word,
};

use super::{
    DateTime, DevelopmentNoopRedactor, ForgetProjectMemoryRequest, LocalWorkService,
    ProjectMemoryFullResponse, ProjectMemoryList, ProjectMemoryListingCut,
    ProjectMemoryMutationReceipt, RememberProjectMemoryRequest, StoreError, Utc,
};

impl LocalWorkService {
    /// Exports opt-in project-memory text against the selected shared histories.
    ///
    /// # Errors
    /// Refuses missing task-sync opt-in or unreadable canonical storage.
    pub fn export_project_memory_sync(
        &self,
        selected: &std::collections::BTreeMap<String, crate::task_sync::memories::KeyHistory>,
        now: DateTime<Utc>,
    ) -> Result<crate::task_sync::memories::ExportOutcome, StoreError> {
        self.require_task_sync_enabled()?;
        self.read_store_at(now)?
            .export_project_memory_sync(&self.project_id, selected)
    }

    /// Applies one selected memory after its task dependencies have been imported.
    /// Source labels remain asserted; the local service actor records the write.
    ///
    /// # Errors
    /// Refuses disabled/read-only use, foreign project scope and storage import
    /// fences, disclosure, target, history or explicit rebaseline failures.
    pub fn import_project_memory_sync(
        &self,
        selected: &crate::task_sync::memories::KeyHistory,
        options: &crate::task_sync::memories::ImportOptions,
        dry_run: bool,
        now: DateTime<Utc>,
    ) -> Result<crate::task_sync::memories::ImportOutcome, StoreError> {
        self.require_task_sync_writer()?;
        self.require_task_sync_enabled()?;
        if selected.project_id != self.project_id {
            return Err(StoreError::InvalidProjectMemory(
                "memory import belongs to another project".into(),
            ));
        }
        let mut store = self.lock_store_at(now)?;
        let expected = store.project_memory_sync_head(&self.project_id, &selected.key)?;
        let actor = self.actor("task-sync:import", "import reported project memory");
        store.import_project_memory_sync(&crate::task_sync::memories::ImportRequest {
            selected,
            expected_local_head: expected.as_ref(),
            actor: &actor,
            session_id: &self.session_id,
            now,
            dry_run,
            merge: options.merge.as_ref(),
            seed: options.seed.as_ref(),
            mark_untagged_seed: options.mark_untagged_seed,
        })
    }
    /// Active project memories whose current version names `work_id` as its
    /// local retiring target: bounded keys, the exact total and omitted count.
    ///
    /// # Errors
    ///
    /// Returns a typed storage refusal when authorization fails or a stored
    /// memory key is invalid.
    pub fn project_memory_retirement_candidates(
        &self,
        work_id: crate::domain::WorkId,
        now: DateTime<Utc>,
    ) -> Result<crate::domain::ProjectMemoryRetirementCandidates, StoreError> {
        self.read_store_at(now)?
            .project_memory_retirement_candidates(
                &self.project_id,
                &self.session_id,
                &self.actor("memories", "read retirement candidates"),
                work_id,
            )
    }
    /// Creates one attributed project memory without changing work focus or
    /// renewing a work claim.
    ///
    /// # Errors
    ///
    /// Returns a typed storage refusal when authorization, normalization,
    /// size, redaction, revision-basis, or terminal lifecycle admission fails.
    pub fn remember_project_memory(
        &self,
        body: String,
        key: Option<String>,
        revise: bool,
        expected_revision: Option<u64>,
        now: DateTime<Utc>,
    ) -> Result<ProjectMemoryMutationReceipt, StoreError> {
        self.remember_project_memory_with_target(
            body,
            key,
            revise,
            expected_revision,
            crate::domain::ProjectMemoryRetiringTargetChange::Keep,
            now,
        )
    }

    /// Creates or revises one attributed project memory, keeping, setting or
    /// clearing its retiring target.
    ///
    /// # Errors
    ///
    /// Returns a typed storage refusal as [`Self::remember_project_memory`]
    /// does, or when the target does not resolve or a clear is not admitted.
    pub fn remember_project_memory_with_target(
        &self,
        body: String,
        key: Option<String>,
        revise: bool,
        expected_revision: Option<u64>,
        retiring_target: crate::domain::ProjectMemoryRetiringTargetChange,
        now: DateTime<Utc>,
    ) -> Result<ProjectMemoryMutationReceipt, StoreError> {
        self.revise_project_memory(
            body,
            key,
            revise,
            expected_revision,
            retiring_target,
            &crate::domain::ProjectMemoryEdit::Whole,
            now,
        )
    }

    /// Creates or revises one project memory as
    /// [`Self::remember_project_memory_with_target`] does, with `edit`
    /// choosing how a revise builds its body: the text replaces it, is
    /// appended, or replaces one marked section.
    ///
    /// # Errors
    ///
    /// Returns a typed storage refusal as that function does, or when a
    /// partial edit lacks its basis, names a missing basis revision or
    /// section, or would leave unpaired section markers.
    #[allow(
        clippy::too_many_arguments,
        reason = "one call carries the memory's text, key, revise basis, target and edit"
    )]
    pub fn revise_project_memory(
        &self,
        body: String,
        key: Option<String>,
        revise: bool,
        expected_revision: Option<u64>,
        retiring_target: crate::domain::ProjectMemoryRetiringTargetChange,
        edit: &crate::domain::ProjectMemoryEdit,
        now: DateTime<Utc>,
    ) -> Result<ProjectMemoryMutationReceipt, StoreError> {
        self.revise_project_memory_with_import_provenance(
            body,
            key,
            revise,
            expected_revision,
            retiring_target,
            edit,
            None,
            now,
        )
    }

    /// Records asserted shared attribution while retaining the actual local
    /// actor/session as recorder. The explicit tag is also used for first seeds.
    ///
    /// # Errors
    /// Refuses malformed, unbound or oversized provenance and ordinary memory
    /// write failures. Source metadata never grants authority.
    #[allow(
        clippy::too_many_arguments,
        reason = "ordinary memory capture plus optional reported source metadata"
    )]
    pub fn revise_project_memory_with_import_provenance(
        &self,
        body: String,
        key: Option<String>,
        revise: bool,
        expected_revision: Option<u64>,
        retiring_target: crate::domain::ProjectMemoryRetiringTargetChange,
        edit: &crate::domain::ProjectMemoryEdit,
        import_provenance: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<ProjectMemoryMutationReceipt, StoreError> {
        let mut actor = self.actor("remember", "record attributed project memory");
        if let Some(marked) = import_provenance {
            if *edit != crate::domain::ProjectMemoryEdit::Whole {
                return Err(StoreError::InvalidProjectMemory(
                    "import provenance requires a whole-body capture".into(),
                ));
            }
            let tag = crate::task_sync::memories::ImportProvenance::parse(marked)?;
            tag.validate_capture(&self.project_id, key.as_deref(), &body)?;
            tag.attach(&mut actor)?;
        }
        let request = RememberProjectMemoryRequest {
            project_id: self.project_id.clone(),
            session_id: self.session_id.clone(),
            key,
            revise,
            expected_revision,
            body,
            retiring_target,
            actor,
            created_at: now,
        };
        // Attribution and a read-only connection refuse first, as before; the
        // lock registers nothing. The request's own shape checks then refuse
        // early, and the write repeats them.
        let mut store = self.lock_store_at(now)?;
        crate::storage::validate_remember_request(&request, edit, &DevelopmentNoopRedactor)?;
        // The session registers inside the write's own transaction, which
        // keeps it only when the write succeeds: no refusal, of the request
        // or of the store, leaves a session behind.
        let register_at = self.process_default_registration_due(now);
        let receipt = store.remember_project_memory_registering(
            &request,
            edit,
            &DevelopmentNoopRedactor,
            ensure_project_memory_full_is_admissible,
            register_at,
        )?;
        if register_at.is_some() {
            self.process_default_registered();
        }
        Ok(receipt)
    }

    /// Lists live project memories without exposing body text.
    ///
    /// # Errors
    ///
    /// Returns a typed storage refusal when authorization, query, cursor, or
    /// stored-projection validation fails.
    pub fn project_memories(
        &self,
        query: Option<&str>,
        after: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<ProjectMemoryList, StoreError> {
        self.project_memories_at_cut(query, after, false, false, now)
            .map(|(list, _)| list)
    }

    /// The listing and, with `records`, the memory position its snapshot
    /// read, for [`Self::acknowledge_project_memory_listing`], with the keys
    /// changed since the session's recorded position. A `recovery` listing
    /// reads larger pages, for compact rows.
    pub(crate) fn project_memories_at_cut(
        &self,
        query: Option<&str>,
        after: Option<&str>,
        records: bool,
        recovery: bool,
        now: DateTime<Utc>,
    ) -> Result<(ProjectMemoryList, Option<ProjectMemoryListingCut>), StoreError> {
        self.read_store_at(now)?.project_memories_at_cut(
            &self.project_id,
            &self.session_id,
            &self.actor("memories", "list attributed project memories"),
            query,
            after,
            records,
            if recovery {
                crate::storage::PROJECT_MEMORY_RECOVERY_LIST_LIMIT
            } else {
                crate::storage::PROJECT_MEMORY_LIST_LIMIT
            },
        )
    }

    /// Records, after the listing was rendered, that this session listed
    /// project memories from their start with this context generation. The
    /// record is advisory: any failure (a store that cannot be written, a
    /// writer that stays busy) leaves the listing delivered and the session
    /// still told to list, never an error.
    pub(crate) fn acknowledge_project_memory_listing(
        &self,
        listing: ProjectMemoryListingCut,
        context_generation: &str,
        now: DateTime<Utc>,
    ) {
        let Ok(mut store) = self.record_store_at(now) else {
            return;
        };
        let _ = store.acknowledge_project_memory_listing(
            &self.project_id,
            &self.session_id,
            listing,
            context_generation,
        );
    }

    /// Reads one live project memory through its dedicated bounded envelope.
    ///
    /// # Errors
    ///
    /// Returns a typed storage refusal when authorization, key resolution,
    /// lifecycle, or stored-envelope validation fails.
    pub(crate) fn project_memory_full(
        &self,
        key: &str,
        revision: Option<u64>,
        names: crate::argument_names::ArgumentNames,
        now: DateTime<Utc>,
    ) -> Result<ProjectMemoryFullResponse, StoreError> {
        let full = self.read_store_at(now)?.project_memory_full(
            &self.project_id,
            &self.session_id,
            &self.actor("memories", "read attributed project memory"),
            key,
            revision,
        )?;
        project_memory_full_response(full, names).map_err(|error| match error {
            StoreError::InvalidProjectMemory(detail) => StoreError::InvalidMemoryProjection(detail),
            other => other,
        })
    }

    /// Appends an attributed terminal project-memory tombstone.
    ///
    /// # Errors
    ///
    /// Returns a typed storage refusal when authorization, key resolution, or
    /// terminal lifecycle validation fails.
    pub fn forget_project_memory(
        &self,
        key: String,
        now: DateTime<Utc>,
    ) -> Result<ProjectMemoryMutationReceipt, StoreError> {
        let request = ForgetProjectMemoryRequest {
            project_id: self.project_id.clone(),
            session_id: self.session_id.clone(),
            key,
            actor: self.actor("forget", "retire attributed project memory"),
            created_at: now,
        };
        // Attribution and a read-only connection refuse first, as before; the
        // lock registers nothing. The request's own shape checks then refuse
        // early, and the write repeats them.
        let mut store = self.lock_store_at(now)?;
        crate::storage::validate_forget_request(&request, &DevelopmentNoopRedactor)?;
        // The session registers inside the write's own transaction, which
        // keeps it only when the write succeeds: a key the store lacks leaves
        // no session behind either.
        let register_at = self.process_default_registration_due(now);
        let receipt = store.forget_project_memory_registering(
            &request,
            &DevelopmentNoopRedactor,
            register_at,
        )?;
        if register_at.is_some() {
            self.process_default_registered();
        }
        Ok(receipt)
    }
}

#[cfg(test)]
mod tests;

/// The reminder a full read of the current version gives when a revision
/// dropped the retirement target without a clear, naming the arguments that
/// keep or clear it as the caller passes them.
pub(crate) fn retiring_target_dropped_reminder(
    dropped: &crate::domain::ProjectMemoryRetiringTargetDropped,
    names: crate::argument_names::ArgumentNames,
) -> String {
    let (retires_with, clear) = match names {
        crate::argument_names::ArgumentNames::Cli => ("--retires-with", "--clear-retires-with"),
        crate::argument_names::ArgumentNames::Mcp => ("retires_with", "clear_retires_with"),
    };
    format!(
        "revision {} dropped the retirement target without a clear; to keep it, revise with {retires_with} {}, or to let it go, revise with {clear}",
        dropped.revision,
        retiring_target_command_form(&dropped.target)
    )
}

/// The `--retires-with` form of a stored target as shown to a reader,
/// terminal-safe, with a local item's short ref.
pub(crate) fn retiring_target_form(target: &crate::domain::ProjectMemoryRetiringTarget) -> String {
    match target {
        crate::domain::ProjectMemoryRetiringTarget::Local { work_ref, .. } => {
            format!("local:{}", terminal_safe_multiline(work_ref))
        }
        crate::domain::ProjectMemoryRetiringTarget::External { project, reference } => format!(
            "external:{}#{}",
            terminal_safe_multiline(project),
            terminal_safe_multiline(reference)
        ),
    }
}

/// The `--retires-with` argument that restores a stored target. A local item
/// is named by its work id, which never becomes ambiguous the way a short ref
/// can once more items share its prefix.
pub(crate) fn retiring_target_command_form(
    target: &crate::domain::ProjectMemoryRetiringTarget,
) -> String {
    match target {
        crate::domain::ProjectMemoryRetiringTarget::Local { work_id, .. } => {
            format!("local:{}", work_id.0)
        }
        external @ crate::domain::ProjectMemoryRetiringTarget::External { .. } => {
            retiring_target_form(external)
        }
    }
}

/// The text lines that list rows and full reads show for a memory's retiring
/// target: the target with its item state read now, or a target a revision
/// dropped without a clear. Only a read of the current version calls a
/// completed target a forget candidate: an older version's target may since
/// have been replaced or cleared.
pub(crate) fn retiring_target_lines(
    target: Option<&crate::domain::ProjectMemoryRetiringTarget>,
    state: Option<&crate::domain::ProjectMemoryRetiringState>,
    dropped: Option<&crate::domain::ProjectMemoryRetiringTargetDropped>,
    current: bool,
) -> Vec<String> {
    let mut lines = Vec::new();
    if let Some(target) = target {
        let status = state.map_or_else(
            || "status in the other tracker is not observed".to_owned(),
            |state| {
                format!(
                    "item {} since {}{}",
                    work_lifecycle_word(state.lifecycle),
                    state.updated_at.format("%Y-%m-%d %H:%M UTC"),
                    if current && state.lifecycle == WorkLifecycle::Completed {
                        " — forget candidate after review"
                    } else {
                        ""
                    }
                )
            },
        );
        lines.push(format!(
            "workaround retires with {}: {status}",
            retiring_target_form(target)
        ));
    }
    if let Some(dropped) = dropped {
        lines.push(format!(
            "retirement target {} dropped by revision {} without a clear",
            retiring_target_form(&dropped.target),
            dropped.revision
        ));
    }
    lines
}

pub(crate) fn project_memory_full_response(
    memory: ProjectMemoryFull,
    names: crate::argument_names::ArgumentNames,
) -> Result<ProjectMemoryFullResponse, StoreError> {
    project_memory_full_response_within(memory, names, MAX_PROJECT_MEMORY_FULL_BYTES)
}

fn project_memory_full_response_within(
    memory: ProjectMemoryFull,
    names: crate::argument_names::ArgumentNames,
    limit: usize,
) -> Result<ProjectMemoryFullResponse, StoreError> {
    let response = ProjectMemoryFullResponse::new(memory, names);
    let bytes = serde_json::to_vec(&response)?.len();
    if bytes > limit {
        return Err(StoreError::InvalidProjectMemory(format!(
            "serialized full memory response requires {bytes} bytes, exceeding the {limit}-byte limit"
        )));
    }
    let terminal_bytes = render_agent_receipt_text(
        &response.terminal_lines(),
        &response.reminders,
        &response.next,
    )
    .len();
    if terminal_bytes > limit {
        return Err(StoreError::InvalidProjectMemory(format!(
            "terminal-safe full memory response requires {terminal_bytes} bytes, exceeding the {limit}-byte limit"
        )));
    }
    Ok(response)
}

pub(crate) fn ensure_project_memory_full_is_admissible(
    memory: &ProjectMemoryFull,
    admission: crate::storage::ProjectMemoryAdmission,
) -> Result<(), StoreError> {
    // Reserve the largest numeric/history-navigation envelope up front: a
    // later revision must not make an accepted earlier body unreadable. A
    // new version also leaves room for a read reminder; a stored version
    // keeps the bound it was admitted under, so history admitted before the
    // reserve never blocks a revision or an exact retry.
    let mut historical = memory.clone();
    historical.revision = u64::MAX - 1;
    historical.current_revision = u64::MAX;
    let limit = match admission {
        crate::storage::ProjectMemoryAdmission::NewVersion => {
            MAX_PROJECT_MEMORY_FULL_BYTES - READ_REMINDER_RESERVE
        }
        crate::storage::ProjectMemoryAdmission::Retained => MAX_PROJECT_MEMORY_FULL_BYTES,
    };
    for names in [
        crate::argument_names::ArgumentNames::Cli,
        crate::argument_names::ArgumentNames::Mcp,
    ] {
        project_memory_full_response_within(memory.clone(), names, limit)?;
        project_memory_full_response_within(historical.clone(), names, limit)?;
    }
    Ok(())
}
