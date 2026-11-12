//! Admit the resolved retry through the ordinary attributed service boundary.

use super::{Cancellation, LocalWorkService, RunLimits, exchange};
use crate::{
    ObjectId,
    storage::SqliteStore,
    task_sync::ledger::{OutgoingProgress, OutgoingReconciliation},
};
use std::time::Instant;

impl LocalWorkService {
    /// Record an inspected merged recipe under its original pending export.
    /// The exchange owner repeats Git and custody inspection before writing.
    ///
    /// # Errors
    /// Refuses read-only use, missing opt-in or foreign original/resolved scope
    /// before opening SQLite or Git, then enforces exact canonical admission.
    pub async fn record_task_sync_delta_resolution(
        &self,
        input: &exchange::InspectedDeltaResolution,
        limits: RunLimits,
        cancel: &Cancellation,
    ) -> anyhow::Result<ObjectId> {
        limits.validate_request()?;
        let _warning = crate::operation_time::Warning::start("task exchange service");
        let started = Instant::now();
        let mut store = self.delta_resolution_store(input)?;
        exchange::record_delta_resolution_remaining(
            &mut store,
            input,
            exchange::remaining(limits, started)?,
            cancel,
        )
        .await
    }

    /// Record one resolved retry step without dispatching transport. Only a
    /// newly committed start returns permission; replay never returns it again.
    /// Invocation supplies no Git authority.
    ///
    /// # Errors
    /// Refuses read-only use, missing opt-in or foreign original/resolved scope
    /// before opening SQLite or Git, then checks recipe, progress and custody.
    pub async fn record_task_sync_delta_retry_progress(
        &self,
        input: &exchange::InspectedDeltaResolution,
        progress: &OutgoingProgress,
        limits: RunLimits,
        cancel: &Cancellation,
    ) -> anyhow::Result<(ObjectId, bool)> {
        limits.validate_request()?;
        let _warning = crate::operation_time::Warning::start("task exchange service");
        let started = Instant::now();
        let mut store = self.delta_resolution_store(input)?;
        exchange::record_delta_retry_progress_remaining(
            &mut store,
            input,
            progress,
            exchange::remaining(limits, started)?,
            cancel,
        )
        .await
    }

    /// Drive the single admitted retry. Existing starts and Unknown only
    /// observe; terminal canonical results replay without Git or retained files.
    /// A second known refusal stops. Invocation supplies no Git authority.
    ///
    /// # Errors
    /// Refuses read-only use, missing opt-in or foreign original/resolved scope
    /// before opening SQLite or Git, then applies bounded transport admission.
    pub async fn publish_task_sync_delta_retry(
        &self,
        input: &exchange::InspectedDeltaResolution,
        limits: RunLimits,
        cancel: &Cancellation,
    ) -> anyhow::Result<exchange::PublicationAttempt> {
        limits.validate_request()?;
        let _warning = crate::operation_time::Warning::start("task exchange service");
        let started = Instant::now();
        let mut store = self.delta_resolution_store(input)?;
        Box::pin(exchange::publish_delta_retry_remaining(
            &mut store,
            input,
            exchange::remaining(limits, started)?,
            cancel,
        ))
        .await
    }

    fn delta_resolution_store(
        &self,
        input: &exchange::InspectedDeltaResolution,
    ) -> anyhow::Result<SqliteStore> {
        self.require_task_sync_writer()?;
        self.require_task_sync_enabled()?;
        let now = chrono::Utc::now();
        self.validate_read_attribution(now)?;
        let original = &input.original;
        anyhow::ensure!(
            original.binding.project == self.project_id
                && original.intent.local_cut.position.feed
                    == crate::FeedId::Project(self.project_id.clone())
                && original.intent.local_cut.binding == original.binding_id
                && original.intent.actor.actor_id == self.actor_id
                && original.intent.actor.session_id.as_ref() == Some(&self.session_id),
            "delta resolution request belongs to another project, binding, actor or session"
        );
        let OutgoingReconciliation::DeltaResolved {
            resolved, payload, ..
        } = &input.recipe
        else {
            anyhow::bail!("resolved delta recipe missing");
        };
        anyhow::ensure!(
            resolved.actor == original.intent.actor
                && resolved.local_cut == original.intent.local_cut
                && payload.project_id == self.project_id,
            "resolved delta request belongs to another project, binding, actor or session"
        );
        let mut store = SqliteStore::open_unresolved(&self.database)?;
        self.register_on(&mut store, now)?;
        Ok(store)
    }
}
