//! Shared spellings of work facts, independent of receipt layout.

use super::{WorkEvidenceKind, WorkLifecycle};

impl WorkEvidenceKind {
    pub(crate) const fn word(self) -> &'static str {
        match self {
            Self::Generic => "generic",
            Self::Verification => "verification",
            Self::Environment => "environment",
        }
    }
}

impl WorkLifecycle {
    pub(crate) const fn word(self) -> &'static str {
        match self {
            Self::Proposed => "proposed",
            Self::Open => "open",
            Self::Completed => "completed",
            Self::Cancelled => "cancelled",
            Self::Superseded => "superseded",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fact_words_match_canonical_serialization() {
        for kind in [
            WorkEvidenceKind::Generic,
            WorkEvidenceKind::Verification,
            WorkEvidenceKind::Environment,
        ] {
            assert_eq!(serde_json::to_value(kind).unwrap(), kind.word());
        }
        for state in [
            WorkLifecycle::Proposed,
            WorkLifecycle::Open,
            WorkLifecycle::Completed,
            WorkLifecycle::Cancelled,
            WorkLifecycle::Superseded,
        ] {
            assert_eq!(serde_json::to_value(state).unwrap(), state.word());
        }
    }
}
