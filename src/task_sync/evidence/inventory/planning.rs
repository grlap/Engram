//! Typed planning dependency fields.

use super::super::RecordKind as K;
use super::{EvidenceError, Links, RecordEnvelope, decode, domain};

pub(super) fn collect(links: &mut Links, record: &RecordEnvelope) -> Result<(), EvidenceError> {
    match record.kind() {
        K::WorkRootDelta => {
            let value: domain::RootExecutionDelta = decode(record)?;
            links.optional(value.predecessor.as_ref(), Some(K::WorkRootDelta));
            for member in value.removed.iter().chain(&value.added) {
                links.member(member);
            }
        }
        K::WorkItemRevision => links.work(&decode(record)?),
        K::WorkHandoffOffer => {
            let value: domain::WorkHandoffOffer = decode(record)?;
            links.add(&value.checkpoint, Some(K::WorkCheckpoint));
        }
        K::WorkObservation => {
            let value: domain::WorkObservation = decode(record)?;
            links.basis(&value.basis);
        }
        K::WorkSourceProposal => {
            let value: domain::WorkSourceProposal = decode(record)?;
            links.basis(&value.basis);
            links.add(&value.notice.cited_snapshot, Some(K::WorkSourceSnapshot));
            links.add(&value.notice.proposed_snapshot, Some(K::WorkSourceSnapshot));
        }
        K::WorkEvent => super::work_event(links, &decode(record)?),
        _ => {
            return Err(EvidenceError::InvalidPack(
                "dependency inventory dispatch differs",
            ));
        }
    }
    Ok(())
}
