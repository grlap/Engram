//! Shared opt-in/read-only admission for the explicit HTTPS adapter.

use super::LocalWorkService;
use crate::task_sync::exchange::{
    self, Cancellation, ExchangeOperation, RunLimits, RunOutput, TargetSelection,
};

mod delta_resolution;
mod driver;

impl LocalWorkService {
    /// Read family capture and coverage without enabling sync or opening a
    /// writable store. A settled operation alone is not successful coverage.
    ///
    /// # Errors
    /// Refuses a foreign binding or an inconsistent canonical exchange chain.
    pub fn task_sync_family_frontiers(
        &self,
        binding: &crate::ObjectId,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<crate::task_sync::ledger::FamilyFrontiers, crate::storage::StoreError> {
        let store = self.read_store_at(now)?;
        let selected = store
            .task_sync_binding(binding)?
            .ok_or_else(|| crate::task_sync::invalid("exchange binding is missing"))?;
        if selected.project != self.project_id {
            return Err(crate::task_sync::invalid(
                "exchange binding belongs to another project",
            ));
        }
        store.task_sync_family_frontiers(binding)
    }

    /// Drive the original selected outgoing delta through one confined Git
    /// publication attempt. Replay observes only, and a known refusal returns
    /// the agent reconciliation step. This grants no Git authority.
    ///
    /// # Errors
    /// Refuses read-only use, missing opt-in or foreign request scope before
    /// opening a store or reading Git, then applies custody and transport limits.
    pub async fn publish_task_sync_delta_outgoing(
        &self,
        selected: &exchange::DeltaOutgoingSelection<'_>,
        limits: RunLimits,
        cancel: &Cancellation,
    ) -> anyhow::Result<exchange::PublicationAttempt> {
        limits.validate_request()?;
        let _warning = crate::operation_time::Warning::start("task exchange service");
        let started = std::time::Instant::now();
        self.require_task_sync_writer()?;
        self.require_task_sync_enabled()?;
        let now = chrono::Utc::now();
        self.validate_read_attribution(now)?;
        anyhow::ensure!(
            selected.binding.project == self.project_id
                && selected.intent.local_cut.position.feed
                    == crate::FeedId::Project(self.project_id.clone())
                && selected.intent.local_cut.binding == *selected.binding_id
                && selected.intent.actor.actor_id == self.actor_id
                && selected.intent.actor.session_id.as_ref() == Some(&self.session_id),
            "delta publication request belongs to another project, binding, actor or session"
        );
        let mut store = crate::storage::SqliteStore::open_unresolved(&self.database)?;
        self.register_on(&mut store, now)?;
        exchange::publish_delta_outgoing_remaining(
            &mut store,
            selected,
            exchange::remaining(limits, started)?,
            cancel,
        )
        .await
    }

    /// Admit one caller-owned delta publication step before inspecting files
    /// or Git. Recording a start grants no Git authority; replay never grants
    /// another dispatch. The awaited operation owns its SQLite connection.
    ///
    /// # Errors
    /// Refuses read-only use, missing opt-in and foreign request scope before
    /// opening a store, then enforces exact canonical and retained custody.
    pub async fn record_task_sync_delta_publication(
        &self,
        request: &exchange::DeltaPublicationRequest<'_>,
        limits: RunLimits,
        cancel: &Cancellation,
    ) -> anyhow::Result<(crate::ObjectId, bool)> {
        limits.validate_request()?;
        let _warning = crate::operation_time::Warning::start("task exchange service");
        let started = std::time::Instant::now();
        self.require_task_sync_writer()?;
        self.require_task_sync_enabled()?;
        let now = chrono::Utc::now();
        self.validate_read_attribution(now)?;
        anyhow::ensure!(
            request.binding.project == self.project_id
                && request.intent.local_cut.position.feed
                    == crate::FeedId::Project(self.project_id.clone())
                && request.intent.local_cut.binding == *request.binding_id
                && request.intent.actor.actor_id == self.actor_id
                && request.intent.actor.session_id.as_ref() == Some(&self.session_id),
            "delta publication request belongs to another project, binding, actor or session"
        );
        let mut store = crate::storage::SqliteStore::open_unresolved(&self.database)?;
        self.register_on(&mut store, now)?;
        exchange::record_delta_publication_remaining(
            &mut store,
            request,
            exchange::remaining(limits, started)?,
            cancel,
        )
        .await
    }

    /// Bind a retained data commit to its outgoing delta through the same
    /// opt-in and attributed service boundary as preparation. Git inspection
    /// owns its connection and never holds the ambient service mutex.
    ///
    /// # Errors
    /// Refuses read-only use, missing opt-in and foreign project, binding or
    /// actor scope before opening SQLite or reading the selected repository.
    /// The exchange owner then checks canonical preparation and retained input.
    pub async fn record_task_sync_delta_export_commit(
        &self,
        binding_id: &crate::ObjectId,
        binding: &crate::task_sync::ledger::Binding,
        intent: &crate::task_sync::delta::DeltaIntent,
        selection: &crate::task_sync::delta::DeltaCommitSelection,
        limits: RunLimits,
        cancel: &Cancellation,
    ) -> anyhow::Result<crate::ObjectId> {
        limits.validate_request()?;
        let _warning = crate::operation_time::Warning::start("task exchange service");
        let started = std::time::Instant::now();
        self.require_task_sync_writer()?;
        self.require_task_sync_enabled()?;
        let now = chrono::Utc::now();
        self.validate_read_attribution(now)?;
        anyhow::ensure!(
            binding.project == self.project_id
                && intent.local_cut.position.feed
                    == crate::FeedId::Project(self.project_id.clone())
                && intent.local_cut.binding == *binding_id
                && intent.actor.actor_id == self.actor_id
                && intent.actor.session_id.as_ref() == Some(&self.session_id),
            "delta commit request belongs to another project, binding, actor or session"
        );
        let mut store = crate::storage::SqliteStore::open_unresolved(&self.database)?;
        self.register_on(&mut store, now)?;
        exchange::record_delta_export_commit_remaining(
            &mut store,
            binding_id,
            binding,
            intent,
            selection,
            exchange::remaining(limits, started)?,
            cancel,
        )
        .await
    }

    /// Retain one selected native outgoing delta through shared opt-in and
    /// actor admission. Creates no Git commit, ref or transport effect.
    ///
    /// # Errors
    /// Refuses read-only use, missing opt-in or another project/actor/session
    /// before opening SQLite, then applies the storage owner's exact custody,
    /// graph, retained request and replay checks.
    pub fn prepare_task_sync_delta_export(
        &self,
        binding: &crate::ObjectId,
        intent: &crate::task_sync::delta::DeltaIntent,
        now: chrono::DateTime<chrono::Utc>,
    ) -> anyhow::Result<crate::ObjectId> {
        self.require_task_sync_writer()?;
        self.require_task_sync_enabled()?;
        self.validate_read_attribution(now)?;
        anyhow::ensure!(
            intent.local_cut.position.feed == crate::FeedId::Project(self.project_id.clone())
                && intent.local_cut.binding == *binding
                && intent.actor.actor_id == self.actor_id
                && intent.actor.session_id.as_ref() == Some(&self.session_id),
            "delta export request belongs to another project, binding, actor or session"
        );
        Ok(self
            .store_at(now)?
            .prepare_task_sync_delta_export(binding, intent, now)?)
    }

    /// Publish one retained data candidate through its existing canonical
    /// operation. A known refusal returns the agent reconciliation step; a
    /// replay observes the remote without repeating a recorded push.
    ///
    /// # Errors
    /// Refuses read-only use, missing opt-in or different project/actor/session
    /// before opening a store or launching Git, then applies ordinary custody
    /// and bounded transport admission. This grants no commit/push authority.
    pub async fn publish_task_sync_outgoing(
        &self,
        input: &exchange::InspectedExchange,
        request: &crate::task_sync::ledger::CompletedExchangeRequest,
        limits: RunLimits,
        cancel: &Cancellation,
    ) -> anyhow::Result<exchange::PublicationAttempt> {
        limits.validate_request()?;
        let _warning = crate::operation_time::Warning::start("task exchange service");
        let started = std::time::Instant::now();
        self.require_task_sync_writer()?;
        self.require_task_sync_enabled()?;
        let now = chrono::Utc::now();
        self.validate_read_attribution(now)?;
        anyhow::ensure!(
            input.binding.project == self.project_id
                && input.intent.actor.actor_id == self.actor_id
                && input.intent.actor.session_id.as_ref() == Some(&self.session_id)
                && request.intent.actor == input.intent.actor
                && request.binding == input.binding_id,
            "publication request belongs to another project, actor or session"
        );
        // This awaited operation owns its connection. It never holds the
        // ambient service mutex while waiting on a remote Git process.
        let mut store = crate::storage::SqliteStore::open_unresolved(&self.database)?;
        self.register_on(&mut store, now)?;
        exchange::publish_outgoing_remaining(
            &mut store,
            input,
            request,
            exchange::remaining(limits, started)?,
            cancel,
        )
        .await
    }

    /// Observe effective configuration then execute one explicit HTTPS Git operation.
    /// Invocation supplies no commit/push authority and records no import success.
    ///
    /// # Errors
    /// Refuses read-only use and missing opt-in before any subprocess, then
    /// refuses unsupported targets, drift and bounded execution failures.
    pub async fn run_task_sync_exchange(
        &self,
        selection: &TargetSelection,
        operation: &ExchangeOperation,
        limits: RunLimits,
        cancel: &Cancellation,
    ) -> anyhow::Result<RunOutput> {
        limits.validate_request()?;
        let _warning = crate::operation_time::Warning::start("task exchange service");
        let started = std::time::Instant::now();
        #[cfg(test)]
        let started = exchange::test_support::SERVICE_STARTED
            .with(std::cell::Cell::take)
            .unwrap_or(started);
        self.require_task_sync_writer()?;
        self.require_task_sync_enabled()?;
        self.validate_read_attribution(chrono::Utc::now())?;
        let target = exchange::ExchangeTarget::capture_remaining(
            selection,
            exchange::remaining(limits, started)?,
            cancel,
        )
        .await?;
        Ok(exchange::run_remaining(
            &target,
            operation,
            exchange::remaining(limits, started)?,
            cancel,
        )
        .await?)
    }
}
