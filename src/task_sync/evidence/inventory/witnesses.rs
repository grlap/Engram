//! Typed witnesses dependency fields.

use super::super::{RecordKind as K, witnesses};
use super::{EvidenceError, Links, RecordEnvelope, decode};

pub(super) fn collect(links: &mut Links, record: &RecordEnvelope) -> Result<(), EvidenceError> {
    match record.kind() {
        K::OriginFeedSegment => {
            let value: witnesses::OriginFeedSegment = decode(record)?;
            links.add(&value.source.origin, Some(K::ProducingOrigin));
            links.optional(value.predecessor.as_ref(), Some(K::OriginFeedSegment));
            for member in value.anchor.iter().chain(&value.entries) {
                links.add(&member.object_id, Some(K::parse(&member.object_kind)?));
            }
        }
        K::OriginFeedCut => {
            let value: witnesses::OriginFeedCut = decode(record)?;
            links.add(&value.source.origin, Some(K::ProducingOrigin));
            links.many(&value.segments, Some(K::OriginFeedSegment));
            if let Some(member) = value.endpoint {
                links.add(&member.object_id, Some(K::parse(&member.object_kind)?));
            }
        }
        K::GoverningSnapshotWitness => {
            let value: witnesses::GoverningSnapshotWitness = decode(record)?;
            links.add(&value.basis.evaluation, Some(K::AcceptanceEvaluation));
            if let witnesses::SnapshotAvailability::Exact {
                snapshot,
                source_snapshot,
            } = value.snapshot
            {
                links.work(&snapshot);
                links.optional(source_snapshot.as_ref(), Some(K::WorkSourceSnapshot));
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
