//! Stable project, session, cursor, and record identities shared by every
//! domain family.

use std::fmt;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// Inclusive maximum UTF-8 byte length of an admitted live session identity.
pub const MAX_SESSION_ID_BYTES: usize = 64;

/// Why a live caller or recipient session identity was refused.
///
/// Length-only. Empty, whitespace, and charset rules stay at each existing
/// admission surface. This type does not depend on storage errors.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum SessionIdAdmissionError {
    /// The identity is longer than [`MAX_SESSION_ID_BYTES`].
    TooLong,
}

impl SessionIdAdmissionError {
    /// Bounded static refusal; never echoes the rejected identity.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::TooLong => "session id exceeds 64 UTF-8 bytes",
        }
    }
}

impl fmt::Display for SessionIdAdmissionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.as_str())
    }
}

impl std::error::Error for SessionIdAdmissionError {}

/// Length-only admission for a live session identity.
///
/// Does not trim, normalize, or reject charset. Serde still accepts any
/// string so historical rows can deserialize; call this at live admissions.
///
/// # Errors
///
/// Returns [`SessionIdAdmissionError::TooLong`] when `value` exceeds
/// [`MAX_SESSION_ID_BYTES`] UTF-8 bytes.
pub fn validate_session_id_length(value: &str) -> Result<(), SessionIdAdmissionError> {
    if value.len() > MAX_SESSION_ID_BYTES {
        Err(SessionIdAdmissionError::TooLong)
    } else {
        Ok(())
    }
}

/// Stable host-local project identity shared by every session and worktree.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ProjectId(pub String);

/// Runtime session identity asserted by the host integration.
#[derive(Clone, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SessionId(pub String);

impl SessionId {
    /// Length-only live admission. Construction and serde stay unrestricted.
    ///
    /// # Errors
    ///
    /// Returns [`SessionIdAdmissionError::TooLong`] when the identity exceeds
    /// [`MAX_SESSION_ID_BYTES`] UTF-8 bytes.
    pub fn validate_admitted(&self) -> Result<(), SessionIdAdmissionError> {
        validate_session_id_length(&self.0)
    }
}

/// Monotonic position in a task's durable change feed.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ChangeCursor(pub i64);

/// Stable identifier for a memory across immutable versions.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct MemoryId(pub Uuid);

impl MemoryId {
    /// Creates a time-sortable identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for MemoryId {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable identifier for a local operational task.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct TaskId(pub Uuid);

impl TaskId {
    /// Creates a time-sortable identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for TaskId {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable planning identity for first-class local work.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, JsonSchema, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WorkId(pub Uuid);

impl WorkId {
    /// Creates a time-sortable identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for WorkId {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable identity of one root execution generation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct RootExecutionId(pub Uuid);

impl RootExecutionId {
    /// Creates a time-sortable identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for RootExecutionId {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable identity of one execution generation for one work item.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WorkRunId(pub Uuid);

impl WorkRunId {
    /// Creates a time-sortable identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for WorkRunId {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable identity of one immutable execution obligation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WorkObligationId(pub Uuid);

impl WorkObligationId {
    /// Creates a time-sortable identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for WorkObligationId {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable identity of a fenced work claim across renewal and handoff.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WorkClaimId(pub Uuid);

impl WorkClaimId {
    /// Creates a time-sortable identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for WorkClaimId {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable identity of one pending handoff offer.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WorkHandoffOfferId(pub Uuid);

impl WorkHandoffOfferId {
    /// Creates a time-sortable identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for WorkHandoffOfferId {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_id_length_admits_64_ascii_and_refuses_65() {
        let admitted = "a".repeat(MAX_SESSION_ID_BYTES);
        assert_eq!(admitted.len(), 64);
        assert_eq!(validate_session_id_length(&admitted), Ok(()));
        SessionId(admitted).validate_admitted().unwrap();

        let refused = "a".repeat(MAX_SESSION_ID_BYTES + 1);
        assert_eq!(
            validate_session_id_length(&refused),
            Err(SessionIdAdmissionError::TooLong)
        );
        assert_eq!(
            SessionIdAdmissionError::TooLong.as_str(),
            "session id exceeds 64 UTF-8 bytes"
        );
        assert!(!SessionIdAdmissionError::TooLong.as_str().contains(&refused));
    }

    #[test]
    fn session_id_length_counts_utf8_bytes_not_chars() {
        let admitted = "é".repeat(32);
        assert_eq!(admitted.chars().count(), 32);
        assert_eq!(admitted.len(), 64);
        assert_eq!(validate_session_id_length(&admitted), Ok(()));

        let refused = format!("{admitted}x");
        assert_eq!(refused.len(), 65);
        assert_eq!(
            validate_session_id_length(&refused),
            Err(SessionIdAdmissionError::TooLong)
        );
    }

    #[test]
    fn session_id_length_accepts_uuid_termal_and_max_process_default_pattern() {
        assert_eq!(
            validate_session_id_length("01234567-89ab-cdef-0123-456789abcdef"),
            Ok(())
        );
        assert_eq!(validate_session_id_length("session-6670"), Ok(()));
        let max_generated = format!(
            "local-process-v1-4294967295-{}",
            "01234567-89ab-cdef-0123-456789abcdef"
        );
        assert_eq!(max_generated.len(), 64);
        assert_eq!(validate_session_id_length(&max_generated), Ok(()));
    }

    #[test]
    fn session_id_length_does_not_trim_or_normalize() {
        let spaced = format!(" {}", "s".repeat(63));
        assert_eq!(spaced.len(), 64);
        assert_eq!(validate_session_id_length(&spaced), Ok(()));
    }

    #[test]
    fn session_id_serde_still_round_trips_oversized_historical_values() {
        let stored = SessionId("h".repeat(128));
        let json = serde_json::to_string(&stored).expect("json");
        let decoded: SessionId = serde_json::from_str(&json).expect("decode");
        assert_eq!(decoded.0.len(), 128);
        assert_eq!(
            decoded.validate_admitted(),
            Err(SessionIdAdmissionError::TooLong)
        );
    }
}
