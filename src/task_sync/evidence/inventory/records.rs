//! Typed records dependency fields.

use super::super::RecordKind as K;
use super::{EvidenceError, Links, RecordEnvelope, decode, domain};

pub(super) fn collect(links: &mut Links, record: &RecordEnvelope) -> Result<(), EvidenceError> {
    match record.kind() {
        K::WorkEvidence => {
            let value: domain::WorkEvidence = decode(record)?;
            if let Some(gate) = value.gate {
                links.optional(gate.previous.as_ref(), Some(K::WorkEvidence));
            }
        }
        K::WorkCheckpoint => {
            let value: domain::WorkCheckpoint = decode(record)?;
            links.many(&value.evidence, None);
        }
        K::WorkObligation => {
            let value: domain::WorkObligation = decode(record)?;
            links.add(&value.rule_set, Some(K::ObligationRuleSet));
            if crate::control::acceptance_binding_criterion(&value.rule).is_some() {
                links.add(&value.triggering_observation, Some(K::WorkEvent));
            } else {
                links.choices(
                    &value.triggering_observation,
                    &[K::ExecutionObservation, K::UnadmittedExecutionObservation],
                );
            }
        }
        K::WorkObligationResolution => {
            let value: domain::WorkObligationResolutionEvent = decode(record)?;
            links.add(&value.definition, Some(K::WorkObligation));
            links.decision(value.registered_input_decision.as_ref());
            match value.resolution {
                domain::WorkObligationResolution::Satisfied { evidence, .. } => {
                    if value.registered_input_decision.is_some() {
                        links.add(&evidence, Some(K::VerificationEvidence));
                    } else {
                        links.choices(
                            &evidence,
                            &[K::VerificationEvidence, K::ImportedConsumption],
                        );
                    }
                }
                domain::WorkObligationResolution::Displaced { binding, .. } => {
                    links.add(&binding, Some(K::NamedRootBinding));
                }
                domain::WorkObligationResolution::Waived { .. } => {}
            }
        }
        K::WorkRestoredEvidence => {
            let value: crate::graph_snapshot::RestoredWorkEvidence = decode(record)?;
            links.add(&value.restored_record, Some(K::WorkRestoredRecord));
            if let Some(gate) = value.gate {
                links.optional(gate.previous.as_ref(), Some(K::WorkRestoredEvidence));
            }
        }
        K::WorkRestoredRecord => {
            let value: crate::graph_snapshot::RestoredRecord = decode(record)?;
            links.optional(
                value.item.source_snapshot_id.as_ref(),
                Some(K::WorkSourceSnapshot),
            );
            for notice in value.history.source_notices {
                links.add(&notice.cited_snapshot, Some(K::WorkSourceSnapshot));
                links.add(&notice.proposed_snapshot, Some(K::WorkSourceSnapshot));
            }
        }
        _ => {
            return Err(EvidenceError::InvalidPack(
                "dependency inventory dispatch differs",
            ));
        }
    }
    Ok(())
}
