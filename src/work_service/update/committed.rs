//! Commit proof retained while the note/gate response is being completed.

use super::{ObjectId, StoreError, WorkMutationReceipt, WorkNoteCapture};

pub(crate) struct WorkGateIntent<'a> {
    pub(crate) name: &'a str,
    pub(crate) failed: &'a [String],
    pub(crate) evidence_ref: Option<&'a str>,
}

/// Created only from a successful core capture or its durable replay. It is
/// request-local: registration and protocol attempts never establish this proof.
pub(crate) struct CommittedMutation {
    pub(crate) operation: &'static str,
    pub(crate) work_ref: String,
    pub(crate) evidence: ObjectId,
    pub(crate) checkpoint: Option<ObjectId>,
    pub(crate) non_holder: bool,
    pub(crate) phase: &'static str,
}

impl CommittedMutation {
    pub(super) fn capture(
        slot: &mut Option<Self>,
        operation: &'static str,
        work_ref: &str,
        capture: &WorkNoteCapture,
    ) {
        *slot = Some(Self {
            operation,
            work_ref: work_ref.to_owned(),
            evidence: capture.evidence.clone(),
            checkpoint: capture.checkpoint.clone(),
            non_holder: capture.non_holder,
            phase: "response_projection",
        });
    }

    pub(crate) fn phase(slot: &mut Option<Self>, phase: &'static str) {
        if let Some(committed) = slot {
            committed.phase = phase;
        }
    }

    pub(super) fn replay(
        slot: &mut Option<Self>,
        operation: &'static str,
        receipt: &WorkMutationReceipt,
        evidence: &WorkMutationReceipt,
        non_holder: bool,
    ) -> Result<(), StoreError> {
        let capture = WorkNoteCapture {
            non_holder,
            evidence: serde_json::from_value(evidence.result.clone())?,
            checkpoint: (receipt.result != evidence.result)
                .then(|| serde_json::from_value(receipt.result.clone()))
                .transpose()?,
        };
        Self::capture(slot, operation, &receipt.work_ref, &capture);
        Ok(())
    }
}
