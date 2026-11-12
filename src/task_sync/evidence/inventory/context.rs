//! Typed context dependency fields.

use super::super::RecordKind as K;
use super::{EvidenceError, Links, RecordEnvelope, decode, domain};

pub(super) fn collect(links: &mut Links, record: &RecordEnvelope) -> Result<(), EvidenceError> {
    match record.kind() {
        K::ProducingOrigin => {
            let value: crate::task_sync::origin::Enrollment = decode(record)?;
            links.optional(
                value.predecessor_origin_id.as_ref(),
                Some(K::ProducingOrigin),
            );
        }
        K::AcceptanceEvaluation => {
            let value: domain::AcceptanceEvaluation = decode(record)?;
            // work_revision_hash is a CONTENT fingerprint, not a record link.
            links.many(&value.evidence_basis, None);
            links.optional(value.named_root_binding.as_ref(), Some(K::NamedRootBinding));
            links.optional(
                value.registered_input_witness.as_ref(),
                Some(K::VerificationEvidence),
            );
            links.optional(value.supersedes.as_ref(), Some(K::AcceptanceEvaluation));
            links.decision(value.registered_input_decision.as_ref());
            for verdict in &value.verdicts {
                links.many(&verdict.evidence, None);
            }
        }
        K::MemoryVersion => {
            let value: domain::MemoryVersion = decode(record)?;
            if !matches!(value.scope, domain::Scope::Work { .. }) {
                return Err(EvidenceError::InvalidPack(
                    "private or project memory is outside selected work evidence",
                ));
            }
            links.many(&value.parents, Some(K::MemoryVersion));
            links.many(&value.evidence, None);
        }
        K::MemoryAssertionEvent => {
            let value: domain::MemoryAssertionEvent = decode(record)?;
            links.add(&value.version, Some(K::MemoryVersion));
        }
        K::ControlPolicy => {
            let value: domain::ControlPolicy = decode(record)?;
            links.optional(value.previous_policy.as_ref(), Some(K::ControlPolicy));
            links.add(&value.obligation_rule_set, Some(K::ObligationRuleSet));
            // The policy authority is retained asserted context, never a live grant.
        }
        _ => {
            return Err(EvidenceError::InvalidPack(
                "dependency inventory dispatch differs",
            ));
        }
    }
    Ok(())
}
