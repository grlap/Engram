//! Typed observations dependency fields.

use super::super::RecordKind as K;
use super::{EvidenceError, Links, RecordEnvelope, decode, domain};

pub(super) fn collect(links: &mut Links, record: &RecordEnvelope) -> Result<(), EvidenceError> {
    match record.kind() {
        K::VerificationEvidence => {
            let value: domain::VerificationEvidence = decode(record)?;
            links.choices(
                &value.producer_observation,
                &[
                    K::ExecutionObservation,
                    K::RegisteredHostRun,
                    K::RemoteVerificationAssociation,
                ],
            );
            links.optional(value.environment.as_ref(), Some(K::EnvironmentEvidence));
            if let Some(bound) = value.bound_from {
                links.add(&bound.verification, Some(K::VerificationEvidence));
                links.choices(
                    &bound.sighting,
                    &[K::ExecutionObservation, K::UnadmittedExecutionObservation],
                );
            }
        }
        K::ExecutionObservation => {
            let value: domain::ExecutionObservation = decode(record)?;
            links.add(&value.obligation_rule_set, Some(K::ObligationRuleSet));
        }
        K::UnadmittedExecutionObservation => {
            let value: domain::UnadmittedExecutionObservation = decode(record)?;
            links.add(&value.claim_epoch_event, Some(K::WorkEvent));
            links.optional(
                value.root_basis.latest_event.as_ref(),
                Some(K::NamedRootBinding),
            );
            if let domain::ObservationPolicyBasis::AccountIfEligible {
                policy,
                obligation_rule_set,
                ..
            } = value.policy_basis
            {
                links.add(&policy, Some(K::ControlPolicy));
                links.add(&obligation_rule_set, Some(K::ObligationRuleSet));
            }
            match value.accounting {
                domain::ObservationAccounting::SourceChange { source_change } => links.optional(
                    source_change.as_ref(),
                    Some(K::UnadmittedExecutionObservation),
                ),
                domain::ObservationAccounting::Repeat { source_change } => {
                    links.add(&source_change, Some(K::UnadmittedExecutionObservation));
                }
                domain::ObservationAccounting::Sighting {}
                | domain::ObservationAccounting::NoSourceChange {}
                | domain::ObservationAccounting::AuditOnly { .. } => {}
            }
        }
        K::RemoteVerificationAssociation => {
            let value: domain::RemoteVerificationAssociationRecord = decode(record)?;
            links.optional(value.origin.as_ref(), Some(K::RemoteTerminalOrigin));
        }
        _ => {
            return Err(EvidenceError::InvalidPack(
                "dependency inventory dispatch differs",
            ));
        }
    }
    Ok(())
}
