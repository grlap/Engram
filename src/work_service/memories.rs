use super::{
    DateTime, DevelopmentNoopRedactor, ForgetProjectMemoryRequest, LocalWorkService,
    ProjectMemoryFullResponse, ProjectMemoryList, ProjectMemoryMutationReceipt,
    RememberProjectMemoryRequest, StoreError, Utc, ensure_project_memory_full_is_admissible,
    project_memory_full_response,
};

impl LocalWorkService {
    /// Creates one attributed project memory without changing work focus or
    /// renewing a work claim.
    ///
    /// # Errors
    ///
    /// Returns a typed storage refusal when authorization, normalization,
    /// size, redaction, or create-only lifecycle admission fails.
    pub fn remember_project_memory(
        &self,
        body: String,
        key: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<ProjectMemoryMutationReceipt, StoreError> {
        self.store_at(now)?.remember_project_memory_with_admission(
            &RememberProjectMemoryRequest {
                project_id: self.project_id.clone(),
                session_id: self.session_id.clone(),
                key,
                body,
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
        self.store_at(now)?.project_memories(
            &self.project_id,
            &self.session_id,
            &self.actor("memories", "list attributed project memories"),
            query,
            after,
        )
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
        now: DateTime<Utc>,
    ) -> Result<ProjectMemoryFullResponse, StoreError> {
        let full = self.store_at(now)?.project_memory_full(
            &self.project_id,
            &self.session_id,
            &self.actor("memories", "read attributed project memory"),
            key,
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
