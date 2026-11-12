//! Shared admission for the optional Git/filesystem adapter; no store is opened.

use super::LocalWorkService;
use crate::{
    storage::StoreError,
    task_sync::{self, setup},
};
use std::path::Path;

#[cfg(test)]
mod tests;

impl LocalWorkService {
    /// Retain the caller's exact staged data commit under this project's
    /// marker. Does not advance a branch, open SQLite or contact a remote.
    /// Invocation supplies no commit or publication authority.
    ///
    /// # Errors
    /// Refuses read-only use, missing opt-in, invalid attribution, another
    /// project or data checkout, staged drift and changed retained selection.
    pub fn create_task_sync_data_commit(
        &self,
        marker: &Path,
        branch: &str,
        request: &task_sync::data_worktree::StagedCommitRequest,
    ) -> anyhow::Result<task_sync::ledger::RetainedCommit> {
        self.require_task_sync_writer()?;
        self.require_task_sync_enabled()?;
        self.validate_read_attribution(chrono::Utc::now())?;
        let data = task_sync::data_worktree::DataWorktree::observe_project(
            marker,
            &self.project_id,
            branch,
        )?;
        data.create_retained(request)
    }

    /// Explicitly provision the ignored persistent data checkout, without SQLite.
    ///
    /// # Errors
    /// Refuses read-only use, missing opt-in, invalid attribution or project paths.
    pub fn initialize_task_sync_data_worktree(
        &self,
        marker: &Path,
        branch: &str,
    ) -> anyhow::Result<task_sync::data_worktree::Initialization> {
        self.require_task_sync_writer()?;
        self.require_task_sync_enabled()?;
        self.validate_read_attribution(chrono::Utc::now())?;
        setup::Attribution {
            actor_id: self.actor_id.clone(),
            session_id: self.session_id.0.clone(),
            actor_context: self.setup_actor_context.clone(),
            source_skill: self.source_skill.clone(),
        }
        .validate()?;
        task_sync::data_worktree::initialize(marker, &self.project_id, branch)
    }

    /// Observe the private opt-in before starting new task-exchange work.
    ///
    /// # Errors
    /// Refuses disabled, absent, malformed or unreadable settings without writes.
    pub fn require_task_sync_enabled(&self) -> Result<(), StoreError> {
        task_sync::settings::require_enabled(&self.database, &self.project_id)
    }

    pub(super) fn require_task_sync_writer(&self) -> Result<(), StoreError> {
        if self.read_only {
            return Err(task_sync::invalid(
                "task sync mutation refused: this service is read-only",
            ));
        }
        Ok(())
    }

    /// Prepare the private data-worktree intent through shared service admission.
    /// This creates no SQLite store and publishes no shared planning files.
    ///
    /// # Errors
    /// Refuses read-only use, missing opt-in, invalid attribution or repository basis.
    pub fn prepare_task_sync_setup(
        &self,
        marker: &Path,
        repository: &Path,
        branch: &str,
        assertions: setup::Assertions,
    ) -> anyhow::Result<setup::Receipt> {
        self.require_task_sync_writer()?;
        self.require_task_sync_enabled()?;
        self.validate_read_attribution(chrono::Utc::now())?;
        setup::prepare(
            marker,
            &self.project_id,
            repository,
            branch,
            assertions,
            setup::Attribution {
                actor_id: self.actor_id.clone(),
                session_id: self.session_id.0.clone(),
                actor_context: self.setup_actor_context.clone(),
                source_skill: self.source_skill.clone(),
            },
        )
    }

    /// Publish one explicitly selected setup intent and retained task candidate.
    /// A valid admitted operation may finish after opt-out; new work requires opt-in.
    /// No SQLite store is opened or captured by this operation.
    ///
    /// # Errors
    /// Refuses read-only use, invalid attribution, disclosure, unsafe paths,
    /// repository drift, contention and public entries without ownership proof.
    pub fn materialize_task_sync_setup(
        &self,
        marker: &Path,
        selection: &setup::MaterializeSelection,
    ) -> anyhow::Result<setup::MaterializeReceipt> {
        self.require_task_sync_writer()?;
        self.validate_read_attribution(chrono::Utc::now())?;
        setup::materialize(
            marker,
            &self.project_id,
            selection,
            self.require_task_sync_enabled().is_ok(),
            setup::Attribution {
                actor_id: self.actor_id.clone(),
                session_id: self.session_id.0.clone(),
                actor_context: self.setup_actor_context.clone(),
                source_skill: self.source_skill.clone(),
            },
        )
    }
}
