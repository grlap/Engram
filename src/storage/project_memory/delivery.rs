//! Project-memory advertisements and listing acknowledgements.

use super::state::{project_memory_key_changes_on, project_memory_state_on};
use super::{
    MAX_PROJECT_MEMORY_ADVERTISEMENTS_PER_PROJECT, OptionalExtension, ProjectMemoryAdvertisement,
    ProjectMemoryListingCut, SessionId, SqliteStore, StoreError, TransactionBehavior, params,
    validate_context_generation,
};

impl SqliteStore {
    /// Returns the advisory memory signal and acknowledges it as
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
            let key_changes = match &prior {
                Some((prior_position, _)) if changed => Some(project_memory_key_changes_on(
                    connection,
                    project_id,
                    *prior_position,
                    change_position,
                )?),
                _ => None,
            };
            Ok(ProjectMemoryAdvertisement {
                count,
                changed,
                generation_unlisted,
                changes: key_changes,
                change_position,
            })
        })
    }

    /// The memory position recorded for the session, if it has one.
    pub(super) fn recorded_project_memory_position_on(
        connection: &rusqlite::Connection,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
    ) -> Result<Option<i64>, StoreError> {
        Ok(connection
            .query_row(
                "SELECT memory_position FROM project_memory_advertisements
                 WHERE project_id = ?1 AND session_id = ?2",
                params![project_id.0, session_id.0],
                |row| row.get::<_, i64>(0),
            )
            .optional()?)
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

fn project_memory_context_generation_digest(value: &str) -> String {
    const DOMAIN: &[u8] = b"engram-project-memory-context-generation-v1\0";
    let mut input = Vec::with_capacity(DOMAIN.len() + value.len());
    input.extend_from_slice(DOMAIN);
    input.extend_from_slice(value.as_bytes());
    let digest = <sha2::Sha256 as sha2::Digest>::digest(input);
    format!("{digest:x}")
}
