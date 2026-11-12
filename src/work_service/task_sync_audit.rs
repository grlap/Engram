//! Explicit cold custody admission; ordinary work never triggers an audit.

use super::LocalWorkService;
use crate::{
    FeedId,
    storage::StoreError,
    task_sync::{
        self,
        delta::{CustodyAuditReceipt, CustodyAuditRequest},
    },
};

impl LocalWorkService {
    /// Observe an exact data-branch commit before its explicit cold source audit.
    ///
    /// # Errors
    /// Refuses missing write admission, opt-in, foreign scope or unsafe Git input.
    pub async fn audit_task_sync_incoming_data_commit(
        &self,
        request: &CustodyAuditRequest,
        limits: task_sync::exchange::RunLimits,
        cancel: &task_sync::exchange::Cancellation,
    ) -> anyhow::Result<CustodyAuditReceipt> {
        limits.validate_request()?;
        let _warning = crate::operation_time::Warning::start("task exchange service");
        let started = std::time::Instant::now();
        self.require_task_sync_writer()?;
        self.require_task_sync_enabled()?;
        anyhow::ensure!(
            request.expected_local_cut.position.feed == FeedId::Project(self.project_id.clone()),
            "custody audit request belongs to another project",
        );
        let now = chrono::Utc::now();
        self.validate_read_attribution(now)?;
        let actor = self.actor(
            "task_sync_custody_audit",
            "explicit incoming data bootstrap",
        );
        let mut store = crate::storage::SqliteStore::open_unresolved(&self.database)?;
        self.register_on(&mut store, now)?;
        task_sync::exchange::audit_incoming_data_commit_remaining(
            &mut store,
            request,
            &actor,
            now,
            task_sync::exchange::remaining(limits, started)?,
            cancel,
        )
        .await
    }

    /// Audit retained custody and optionally seed source or export membership.
    /// This does not enable ordinary managed writes or publish task text.
    ///
    /// # Errors
    /// Refuses read-only access and missing opt-in before opening the store,
    /// then refuses mismatched scope, unresolved effects and uncertain custody.
    pub fn audit_task_sync_custody(
        &self,
        request: &CustodyAuditRequest,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<CustodyAuditReceipt, StoreError> {
        self.require_task_sync_writer()?;
        self.require_task_sync_enabled()?;
        if request.expected_local_cut.position.feed != FeedId::Project(self.project_id.clone()) {
            return Err(task_sync::invalid(
                "custody audit request belongs to another project",
            ));
        }
        let actor = self.actor("task_sync_custody_audit", "explicit cold custody audit");
        self.store_at(now)?
            .audit_task_sync_custody(request, &actor, now)
    }
}
