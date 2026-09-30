use super::{
    DateTime, DevelopmentNoopRedactor, ForgetProjectMemoryRequest, LocalWorkService,
    ProjectMemoryFullResponse, ProjectMemoryList, ProjectMemoryListingCut,
    ProjectMemoryMutationReceipt, RememberProjectMemoryRequest, StoreError, Utc,
    ensure_project_memory_full_is_admissible, project_memory_full_response,
};

impl LocalWorkService {
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
        self.store_at(now)?.remember_project_memory_with_admission(
            &RememberProjectMemoryRequest {
                project_id: self.project_id.clone(),
                session_id: self.session_id.clone(),
                key,
                revise,
                expected_revision,
                body,
                retiring_target,
                actor: self.actor("remember", "record attributed project memory"),
                created_at: now,
            },
            &DevelopmentNoopRedactor,
            ensure_project_memory_full_is_admissible,
        )
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
        self.project_memories_at_cut(query, after, false, now)
            .map(|(list, _)| list)
    }

    /// The listing and, with `records`, the memory position its snapshot
    /// read, for [`Self::acknowledge_project_memory_listing`].
    pub(crate) fn project_memories_at_cut(
        &self,
        query: Option<&str>,
        after: Option<&str>,
        records: bool,
        now: DateTime<Utc>,
    ) -> Result<(ProjectMemoryList, Option<ProjectMemoryListingCut>), StoreError> {
        self.read_store_at(now)?.project_memories_at_cut(
            &self.project_id,
            &self.session_id,
            &self.actor("memories", "list attributed project memories"),
            query,
            after,
            records,
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
        now: DateTime<Utc>,
    ) -> Result<ProjectMemoryFullResponse, StoreError> {
        let full = self.read_store_at(now)?.project_memory_full(
            &self.project_id,
            &self.session_id,
            &self.actor("memories", "read attributed project memory"),
            key,
            revision,
        )?;
        project_memory_full_response(full).map_err(|error| match error {
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
        self.store_at(now)?.forget_project_memory(
            &ForgetProjectMemoryRequest {
                project_id: self.project_id.clone(),
                session_id: self.session_id.clone(),
                key,
                actor: self.actor("forget", "retire attributed project memory"),
                created_at: now,
            },
            &DevelopmentNoopRedactor,
        )
    }
}

#[cfg(test)]
mod tests;
