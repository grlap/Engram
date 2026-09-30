//! One source remedy for service recovery, words and show.

use crate::domain::{AcceptanceSourceRecoveryCause, AcceptanceSourceRemedy};

pub(crate) fn source_recovery_remedy(cause: &AcceptanceSourceRecoveryCause) -> &'static str {
    match cause.remedy {
        AcceptanceSourceRemedy::EndTurnReadAndRetry => {
            "this snapshot does not confirm the declared revision; end the turn, read the same run again, then retry done; matching host confirmation can preserve this judgment, but no future report is promised"
        }
        AcceptanceSourceRemedy::ReadSourceAndEvaluate => {
            "read the named root's current source and request a new acceptance evaluation of it, then retry done"
        }
        AcceptanceSourceRemedy::MeasureSourceAndRetry => {
            "obtain a fresh source measurement from the host and retry done with it (--source-fingerprint F); copying the evaluated fingerprint is not a measurement"
        }
        AcceptanceSourceRemedy::EvaluateCurrentSource => {
            "request a new acceptance evaluation of the current source with its host-measured source basis, then retry done; copying an earlier fingerprint is insufficient"
        }
    }
}

/// Bound host-recorded strings in agent receipts; raw storage details stay whole.
pub(crate) fn shown_source_recovery(
    cause: &AcceptanceSourceRecoveryCause,
) -> AcceptanceSourceRecoveryCause {
    let mut shown = cause.clone();
    for field in [
        &mut shown.workspace_id,
        &mut shown.declared_revision,
        &mut shown.reported_revision,
        &mut shown.expected_fingerprint,
        &mut shown.presented_fingerprint,
    ]
    .into_iter()
    .flatten()
    {
        *field = super::deciding::bounded(field);
    }
    shown
}
