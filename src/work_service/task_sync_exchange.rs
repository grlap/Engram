//! Shared opt-in/read-only admission for the explicit HTTPS adapter.

use super::LocalWorkService;
use crate::task_sync::exchange::{
    self, Cancellation, ExchangeOperation, RunLimits, RunOutput, TargetSelection,
};

impl LocalWorkService {
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
        let started = std::time::Instant::now();
        #[cfg(test)]
        let started = exchange::test_support::SERVICE_STARTED
            .with(std::cell::Cell::take)
            .unwrap_or(started);
        self.require_task_sync_writer()?;
        self.require_task_sync_enabled()?;
        self.validate_read_attribution(chrono::Utc::now())?;
        let target = exchange::ExchangeTarget::capture(
            selection,
            exchange::remaining(limits, started)?,
            cancel,
        )
        .await?;
        Ok(exchange::run(
            &target,
            operation,
            exchange::remaining(limits, started)?,
            cancel,
        )
        .await?)
    }
}
