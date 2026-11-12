//! Explicit origin enrollment; ordinary work/open/opt-in never calls it.

use super::LocalWorkService;
use crate::{
    storage::StoreError,
    task_sync::origin::{EnrollRequest, ProducingOrigin},
};

impl LocalWorkService {
    /// Cancel only an exact retained enrollment that has no canonical record.
    /// This never cancels exchange custody or removes a producing origin.
    ///
    /// # Errors
    /// Refuses read-only access or uncertain enrollment state. Opt-out does not
    /// prevent releasing an uncommitted private reservation.
    pub fn cancel_task_sync_origin_enrollment(
        &self,
        request: &EnrollRequest,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool, StoreError> {
        self.require_task_sync_writer()?;
        let actor = self.actor("task_sync_origin", "cancel uncommitted origin enrollment");
        self.store_at(now)?
            .cancel_task_sync_origin_enrollment(&self.project_id, &actor, request)
    }

    /// Read the logical enrollment without changing its private witness.
    ///
    /// # Errors
    /// Refuses a malformed or competing canonical lineage.
    pub fn task_sync_origin(
        &self,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<Option<ProducingOrigin>, StoreError> {
        self.read_store_at(now)?
            .task_sync_producing_origin(&self.project_id)
    }

    /// Explicitly enroll or fork this installation, independently of aliases.
    /// This grants no Git action, host enrollment or execution authority.
    ///
    /// # Errors
    /// Refuses read-only access, missing opt-in and uncertain continuity.
    pub fn enroll_task_sync_origin(
        &self,
        request: &EnrollRequest,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<ProducingOrigin, StoreError> {
        self.require_task_sync_writer()?;
        self.require_task_sync_enabled()?;
        let actor = self.actor("task_sync_origin", "explicit origin enrollment");
        self.store_at(now)?
            .enroll_task_sync_origin(&self.project_id, &actor, request, now)
    }

    /// Explicit cold recovery creates a successor, never adopts foreign custody.
    /// The returned canonical receipt grants no current exchange authority.
    ///
    /// # Errors
    /// Refuses read-only access, missing opt-in or inconsistent retained recovery.
    pub fn recover_task_sync_origin(
        &self,
        request: &crate::task_sync::origin::recovery::RecoveryRequest,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<crate::task_sync::origin::recovery::RecoveryReceipt, StoreError> {
        self.require_task_sync_writer()?;
        self.require_task_sync_enabled()?;
        let actor = self.actor("task_sync_origin", "explicit origin recovery");
        self.store_at(now)?
            .recover_task_sync_origin(&self.project_id, &actor, request, now)
    }

    /// Release an exact uncommitted recovery recipe without deleting its archive.
    ///
    /// # Errors
    /// Refuses read-only access, a committed recovery or changed private custody.
    pub fn cancel_task_sync_origin_recovery(
        &self,
        request: &crate::task_sync::origin::recovery::RecoveryRequest,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<bool, StoreError> {
        self.require_task_sync_writer()?;
        let actor = self.actor("task_sync_origin", "cancel uncommitted origin recovery");
        self.store_at(now)?
            .cancel_task_sync_origin_recovery(&self.project_id, &actor, request)
    }
}
