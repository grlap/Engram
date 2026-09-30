//! Words for typed bound-check refusals; never classify the human reason.

use crate::{BoundVerificationRemedy, VerificationKind, WorkBoundVerificationCause};

pub(crate) fn bound_verification_remedy(cause: &WorkBoundVerificationCause) -> String {
    let kind = match cause.requirement.check_kind {
        VerificationKind::Test => "test",
        VerificationKind::Build => "build",
        VerificationKind::Lint => "lint",
        VerificationKind::Review => "review",
        VerificationKind::Acceptance => "acceptance",
    };
    let check = cause.requirement.check_fingerprint.as_ref().map_or_else(
        || format!("a passing {kind} check"),
        |pin| format!("a passing {kind} check matching command fingerprint {pin}"),
    );
    let action = match cause.remedy {
        BoundVerificationRemedy::RunCurrentCheck => {
            format!("run {check} on the run's current source")
        }
        BoundVerificationRemedy::RunPassingCheckAfter => {
            format!("run {check} after verification {}", cause.verification)
        }
    };
    format!(
        "criterion {}: {action}, have the host record it, then retry done",
        cause.criterion
    )
}
