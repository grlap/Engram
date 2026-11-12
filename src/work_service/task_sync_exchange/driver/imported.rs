//! Qualification and explicit local use keep their existing independent owners.

use super::{LocalWorkService, Result, StepReceipt, StepRequest};

impl LocalWorkService {
    pub(super) fn step_imported(
        &self,
        request: &StepRequest,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<StepReceipt> {
        // No selection creates a store merely to discover that its original or
        // destination does not exist. The owners check actual project/run scope.
        drop(self.read_store_at(now)?);
        match request {
            StepRequest::QualifyVerification { request } => Ok(StepReceipt::Qualified {
                decision: self.associate_imported_verification(request, now)?,
            }),
            StepRequest::VerificationSummary { decision } => Ok(StepReceipt::Verification {
                summary: Box::new(self.imported_verification_summary(decision, now)?),
            }),
            StepRequest::QualifyAcceptance { request } => Ok(StepReceipt::Qualified {
                decision: self.associate_imported_acceptance(request, now)?,
            }),
            StepRequest::AcceptanceSummary { decision } => Ok(StepReceipt::Acceptance {
                summary: Box::new(self.imported_acceptance_summary(decision, now)?),
            }),
            StepRequest::ConsumeVerification { request } => Ok(StepReceipt::Consumed {
                receipt: self.consume_imported_verification(request, now)?,
            }),
            StepRequest::ConsumeAcceptance { request } => Ok(StepReceipt::Consumed {
                receipt: self.consume_imported_acceptance(request, now)?,
            }),
            _ => anyhow::bail!("invalid internal imported evidence step selection"),
        }
    }
}
