//! Explicit use links are originals; local claims and custody remain scalars.

use super::super::acceptance::{AcceptanceUseDecision, HistoricalProof};
use super::super::consumption::{ConsumedProof, ImportedEvidenceConsumption};
use super::super::verification::VerificationApplicabilityDecision;
use super::{EvidenceError, Links, RecordEnvelope, RecordKind as K, decode};

pub(super) fn collect(links: &mut Links, record: &RecordEnvelope) -> Result<(), EvidenceError> {
    match record.kind() {
        K::VerificationApplicability => {
            let decision: VerificationApplicabilityDecision = decode(record)?;
            links.add(&decision.original, Some(K::VerificationEvidence));
            links.add(&decision.origin, Some(K::ProducingOrigin));
            links.add(&decision.origin_cut, Some(K::OriginFeedCut));
            links.add(&decision.named_root, Some(K::NamedRootBinding));
            links.many(&decision.contributors, None);
            // known_import is original local custody provenance, not an
            // execution dependency or an imported admission capability.
        }
        K::AcceptanceUse => {
            let decision: AcceptanceUseDecision = decode(record)?;
            links.add(&decision.request.origin, Some(K::ProducingOrigin));
            proof(links, &decision.request.proof);
            links.many(&decision.request.origin_cuts, Some(K::OriginFeedCut));
            links.many(
                &decision.request.verification_decisions,
                Some(K::VerificationApplicability),
            );
            links.add(&decision.named_root, Some(K::NamedRootBinding));
        }
        K::ImportedConsumption => {
            let consumption: ImportedEvidenceConsumption = decode(record)?;
            let decision_kind = match &consumption.proof {
                ConsumedProof::Verification {
                    origin,
                    original,
                    origin_cut,
                    contributors,
                    ..
                } => {
                    links.add(origin, Some(K::ProducingOrigin));
                    links.add(original, Some(K::VerificationEvidence));
                    links.add(origin_cut, Some(K::OriginFeedCut));
                    links.many(contributors, None);
                    K::VerificationApplicability
                }
                ConsumedProof::Acceptance {
                    origin,
                    proof: original,
                } => {
                    links.add(origin, Some(K::ProducingOrigin));
                    proof(links, original);
                    K::AcceptanceUse
                }
            };
            links.add(&consumption.request.decision, Some(decision_kind));
            links.add(&consumption.named_root, Some(K::NamedRootBinding));
            links.work(&consumption.governing);
        }
        _ => {
            return Err(EvidenceError::InvalidPack(
                "consumption inventory dispatch differs",
            ));
        }
    }
    Ok(())
}

fn proof(links: &mut Links, proof: &HistoricalProof) {
    match proof {
        HistoricalProof::Evaluation {
            evaluation,
            governing_witness,
        } => {
            links.add(evaluation, Some(K::AcceptanceEvaluation));
            links.add(governing_witness, Some(K::GoverningSnapshotWitness));
        }
        HistoricalProof::NativeCompletion {
            seal,
            governing_witness,
        } => {
            links.add(seal, Some(K::CompletionSeal));
            links.optional(
                governing_witness.as_ref(),
                Some(K::GoverningSnapshotWitness),
            );
        }
    }
}
