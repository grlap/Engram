//! Explicit native proof selection, independent of retaining a foreign pack.

use super::LocalWorkService;
use crate::ObjectId;
use crate::storage::StoreError;
use crate::task_sync::evidence::acceptance::{AcceptanceUseRequest, AcceptanceUseSummary};
use crate::task_sync::evidence::consumption::{ConsumptionReceipt, ConsumptionRequest};
use crate::task_sync::evidence::verification::{ApplicabilityRequest, ApplicabilitySummary};

impl LocalWorkService {
    /// Assess a retained original through the existing verification qualifier.
    ///
    /// # Errors
    /// Refuses read-only access, stale destination authority and changed retries.
    pub fn associate_imported_verification(
        &self,
        request: &ApplicabilityRequest,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<ObjectId, StoreError> {
        self.require_task_sync_writer()?;
        self.validate_read_attribution(now)?;
        let mut actor = self.actor(
            "associate_imported_verification",
            "explicit imported verification assessment",
        );
        actor.run_id = Some(request.binding.run_id.0.to_string());
        self.store_at(now)?
            .associate_imported_verification(
                &self.project_id,
                &self.session_id,
                &actor,
                request,
                now,
            )
            .map_err(StoreError::from)
    }

    /// Read this project's bounded verification assessment and terminal status.
    ///
    /// # Errors
    /// Refuses missing, malformed or other-project decisions.
    pub fn imported_verification_summary(
        &self,
        decision: &ObjectId,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<ApplicabilitySummary, StoreError> {
        self.validate_read_attribution(now)?;
        self.read_store_at(now)?
            .imported_verification_summary_for_project(&self.project_id, decision)
            .map_err(StoreError::from)
    }

    /// Assess an original evaluation or native seal through the existing C owner.
    ///
    /// # Errors
    /// Refuses read-only access, stale destination authority and changed retries.
    pub fn associate_imported_acceptance(
        &self,
        request: &AcceptanceUseRequest,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<ObjectId, StoreError> {
        self.require_task_sync_writer()?;
        self.validate_read_attribution(now)?;
        let mut actor = self.actor(
            "associate_imported_acceptance",
            "explicit imported acceptance assessment",
        );
        actor.run_id = Some(request.binding.run_id.0.to_string());
        self.store_at(now)?
            .associate_imported_acceptance(&self.project_id, &self.session_id, &actor, request, now)
            .map_err(StoreError::from)
    }

    /// Read this project's bounded acceptance assessment under current policy.
    ///
    /// # Errors
    /// Refuses missing, malformed or other-project decisions.
    pub fn imported_acceptance_summary(
        &self,
        decision: &ObjectId,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<AcceptanceUseSummary, StoreError> {
        self.validate_read_attribution(now)?;
        self.read_store_at(now)?
            .imported_acceptance_summary_for_project(&self.project_id, decision, now)
            .map_err(StoreError::from)
    }

    /// Select a retained B decision for ordinary local requirement/evaluation use.
    ///
    /// # Errors
    /// Refuses read-only access or stale authority, source or original context.
    pub fn consume_imported_verification(
        &self,
        request: &ConsumptionRequest,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<ConsumptionReceipt, StoreError> {
        self.require_task_sync_writer()?;
        self.validate_read_attribution(now)?;
        let mut actor = self.actor(
            "consume_imported_verification",
            "explicit imported verification use",
        );
        actor.run_id = Some(request.binding.run_id.0.to_string());
        self.store_at(now)?.consume_imported_verification(
            &self.project_id,
            &self.session_id,
            &actor,
            request,
            now,
        )
    }

    /// Select a retained C decision; completion still rechecks current policy.
    ///
    /// # Errors
    /// Refuses read-only access or stale authority, policy, source or closure.
    pub fn consume_imported_acceptance(
        &self,
        request: &ConsumptionRequest,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<ConsumptionReceipt, StoreError> {
        self.require_task_sync_writer()?;
        self.validate_read_attribution(now)?;
        let mut actor = self.actor(
            "consume_imported_acceptance",
            "explicit imported acceptance use",
        );
        actor.run_id = Some(request.binding.run_id.0.to_string());
        self.store_at(now)?.consume_imported_acceptance(
            &self.project_id,
            &self.session_id,
            &actor,
            request,
            now,
        )
    }
}
