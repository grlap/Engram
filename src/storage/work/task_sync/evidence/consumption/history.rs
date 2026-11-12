//! Structural validation of the immutable qualification captured by local use.
//! Reassessing current eligibility is separate and may refuse a later use.

use super::{ConsumedProof, ImportedEvidenceConsumption, Result, invalid, object_on, store_error};
use crate::task_sync::evidence::acceptance::AcceptanceUseDecision;
use crate::task_sync::evidence::verification::{TerminalFact, VerificationApplicabilityDecision};

pub(super) fn validate_on(
    connection: &rusqlite::Connection,
    record: &ImportedEvidenceConsumption,
) -> Result<()> {
    let (kind, bytes) = object_on(connection, &record.request.decision)?
        .ok_or_else(|| invalid("consumption decision original is missing"))?;
    match &record.proof {
        ConsumedProof::Verification {
            origin,
            original,
            origin_cut,
            requirement,
            criterion,
            execution,
            contributors,
        } => {
            if kind != "task_sync_verification_applicability" {
                return Err(invalid("consumed verification decision kind differs"));
            }
            let decision: VerificationApplicabilityDecision = serde_json::from_slice(&bytes)?;
            if decision.project_id != record.project_id
                || decision.binding != record.request.binding
                || decision.source != record.source
                || decision.named_root != record.named_root
                || decision.governing_fingerprint != record.governing_fingerprint
                || decision.refusal.is_some()
                || &decision.origin != origin
                || &decision.original != original
                || &decision.origin_cut != origin_cut
                || &decision.requirement != requirement
                || &decision.criterion != criterion
                || &decision.execution != execution
                || contributors.windows(2).any(|pair| pair[0] >= pair[1])
            {
                return Err(invalid("consumed verification decision binding differs"));
            }
            let (evidence, producer, _) =
                super::super::verification::applicability::candidate_in_context(
                    connection,
                    &record.project_id,
                    origin,
                    original,
                    origin_cut,
                )
                .map_err(store_error)?;
            crate::control::check_verification_result(&evidence, &producer, requirement)
                .map_err(|_| invalid("consumed original does not qualify"))?;
            if evidence.source_basis.source_id() != Some(&record.source) {
                return Err(invalid("consumed original source differs"));
            }
            let mut representative: Option<TerminalFact> = None;
            for contributor in contributors {
                let (kind, body) = object_on(connection, contributor)?
                    .ok_or_else(|| invalid("consumed terminal contributor is missing"))?;
                let kind = crate::task_sync::evidence::RecordKind::parse(&kind)
                    .map_err(|error| super::invalid_error(&error))?;
                let fact = TerminalFact::from_bytes(contributor.clone(), kind, &body)
                    .map_err(|error| super::invalid_error(&error))?
                    .ok_or_else(|| invalid("consumed terminal contributor is unsupported"))?;
                if Some(&fact.key) != execution.as_ref()
                    || representative
                        .as_ref()
                        .is_some_and(|prior| !prior.same_terminal(&fact))
                {
                    return Err(invalid("consumption captured conflicting terminal facts"));
                }
                representative = Some(fact);
            }
            if execution.is_some() != representative.is_some() {
                return Err(invalid("consumed terminal union is incomplete"));
            }
        }
        ConsumedProof::Acceptance { origin, proof } => {
            if kind != "task_sync_acceptance_use" {
                return Err(invalid("consumed acceptance decision kind differs"));
            }
            let decision: AcceptanceUseDecision = serde_json::from_slice(&bytes)?;
            if decision.project != record.project_id
                || decision.request.binding != record.request.binding
                || decision.local_generation != record.generation
                || decision.source != record.source
                || decision.named_root != record.named_root
                || decision.governing_fingerprint != record.governing_fingerprint
                || decision.refusal.is_some()
                || &decision.request.origin != origin
                || &decision.request.proof != proof
            {
                return Err(invalid("consumed acceptance decision binding differs"));
            }
            // The captured use is a historical fact. Full policy/standing and
            // late conflict checks run again before every NEW completion, not
            // while doctor validates a previously frozen consumption or seal.
            super::super::acceptance::frozen_results_on(connection, record).map_err(store_error)?;
        }
    }
    Ok(())
}
