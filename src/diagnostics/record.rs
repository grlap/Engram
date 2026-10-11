use serde::{Serialize, Serializer};
use sha2::{Digest, Sha256};
use std::time::Duration;

/// A comparison of exact input bytes, never an identity or authentication.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct Fingerprint([u8; 32]);

impl Fingerprint {
    fn of(namespace: &[u8], value: &str) -> Self {
        let mut digest = Sha256::new();
        digest.update(namespace);
        digest.update([0]);
        digest.update(value.as_bytes());
        Self(digest.finalize().into())
    }

    pub fn session(value: &str) -> Self {
        Self::of(b"engram-diagnostic-session-v1", value)
    }

    pub fn attempt(value: &str) -> Self {
        Self::of(b"engram-diagnostic-attempt-v1", value)
    }

    pub(super) fn request(value: &str) -> Self {
        Self::of(b"engram-diagnostic-request-v1", value)
    }
}

impl Serialize for Fingerprint {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        let mut encoded = [0; 64];
        for (index, byte) in self.0.iter().copied().enumerate() {
            encoded[2 * index] = HEX[usize::from(byte >> 4)];
            encoded[2 * index + 1] = HEX[usize::from(byte & 15)];
        }
        serializer.serialize_str(std::str::from_utf8(&encoded).map_err(serde::ser::Error::custom)?)
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Surface {
    Mcp,
    Control,
}

#[derive(Clone, Copy, Debug, Default, Serialize)]
pub struct Correlation {
    pub frame_seq: Option<u64>,
    pub request_id: Option<i64>,
    pub request_seq: Option<u64>,
    pub grant_id: Option<uuid::Uuid>,
    pub attempt_fingerprint: Option<Fingerprint>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub uncorrelated_reason: Option<CorrelationLoss>,
}

/// Why the observer refuses to assign a receive generation to this event.
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CorrelationLoss {
    AmbiguousId,
    QuarantineOverflow,
    TrackingUnavailable,
}

/// Fixed outcome classification. No error's display text is admitted.
#[derive(Clone, Copy, Debug, Serialize)]
pub struct Fault {
    code: &'static str,
    reason: &'static str,
    sqlite_code: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    writer_admission: Option<WriterAdmission>,
}

#[derive(Clone, Copy, Debug, Serialize)]
struct WriterAdmission {
    reason: crate::storage::WorkWriterAdmissionReason,
    elapsed_ms: u64,
    budget_ms: u64,
    sqlite_primary_code: Option<i32>,
    sqlite_extended_code: Option<i32>,
}

impl Fault {
    pub fn store(error: &crate::StoreError) -> Self {
        if let crate::StoreError::WorkWriterAdmissionRefused {
            reason,
            elapsed_ms,
            budget_ms,
            sqlite_primary_code,
            sqlite_extended_code,
            ..
        } = error
        {
            return Self {
                code: "writer_admission_refused",
                reason: "Inspect native SQLite writer admission; this transaction was not started.",
                sqlite_code: *sqlite_extended_code,
                writer_admission: Some(WriterAdmission {
                    reason: *reason,
                    elapsed_ms: *elapsed_ms,
                    budget_ms: *budget_ms,
                    sqlite_primary_code: *sqlite_primary_code,
                    sqlite_extended_code: *sqlite_extended_code,
                }),
            };
        }
        if let crate::StoreError::Sqlite(rusqlite::Error::SqliteFailure(error, _)) = error {
            let code = match error.code {
                rusqlite::ErrorCode::DatabaseBusy => "sqlite_busy",
                rusqlite::ErrorCode::DatabaseLocked => "sqlite_locked",
                _ => "sqlite_error",
            };
            return Self {
                code,
                reason: "Inspect concurrent writer admission and store availability.",
                sqlite_code: Some(error.extended_code),
                writer_admission: None,
            };
        }
        Self::fixed(crate::host::store_error_code(error))
    }

    pub fn control(code: crate::ControlRefusalCode) -> Self {
        Self::fixed(code.as_str())
    }

    pub fn tool_error() -> Self {
        Self::fixed("tool_error")
    }
    pub fn rpc_error() -> Self {
        Self::fixed("rpc_error")
    }
    pub fn send_failed() -> Self {
        Self::fixed("send_failed")
    }
    pub fn invalid_request() -> Self {
        Self::fixed("invalid_request")
    }
    pub fn transport_failed() -> Self {
        Self::fixed("transport_failed")
    }
    pub fn slow() -> Self {
        Self::fixed("slow_call")
    }
    pub fn shutdown() -> Self {
        Self::fixed("shutdown_overrun")
    }
    pub fn incomplete() -> Self {
        Self::fixed("incomplete")
    }
    pub fn cancelled() -> Self {
        Self::fixed("cancel_requested")
    }
    pub fn summary() -> Self {
        Self::fixed("loss_summary")
    }

    fn fixed(code: &'static str) -> Self {
        let reason = match code {
            "invalid_argument" | "invalid_request" => "Check the documented input schema.",
            "stale_fence" => "Recover the current claim before retrying.",
            "grant_expired" | "grant_not_begun" => "Evaluate and begin a fresh turn.",
            "grant_scope_mismatch" => "Recover the current grant and its declared scope.",
            "send_failed" | "transport_failed" => "Inspect the host transport and connection.",
            "shutdown_overrun" => "Inspect the last observed shutdown milestone.",
            "slow_call" => "Compare server timing with host-side observations.",
            "loss_summary" => "Inspect diagnostic loss counters and log availability.",
            _ => "Recover current session state and inspect the fixed refusal code.",
        };
        Self {
            code,
            reason,
            sqlite_code: None,
            writer_admission: None,
        }
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum EventKind {
    Failure,
    Shutdown,
    Summary,
}

#[derive(Clone, Copy, Debug, Serialize)]
pub struct Event {
    pub(super) observed_at: chrono::DateTime<chrono::Utc>,
    pub(super) kind: EventKind,
    pub(super) operation: &'static str,
    #[serde(flatten)]
    pub(super) correlation: Correlation,
    pub(super) elapsed_ms: u64,
    pub(super) handler_ms: Option<u64>,
    pub(super) dispatch_wait_ms: Option<u64>,
    pub(super) send_ms: Option<u64>,
    pub(super) stage: Option<&'static str>,
    #[serde(flatten)]
    pub(super) fault: Fault,
}

pub(super) fn millis(value: Duration) -> u64 {
    u64::try_from(value.as_millis()).unwrap_or(u64::MAX)
}

impl Event {
    pub(super) fn summary() -> Self {
        Self {
            kind: EventKind::Summary,
            ..Self::failure(
                "shutdown",
                Correlation::default(),
                Duration::ZERO,
                Fault::summary(),
            )
        }
    }
    pub fn failure(
        operation: &str,
        correlation: Correlation,
        elapsed: Duration,
        fault: Fault,
    ) -> Self {
        Self {
            observed_at: chrono::Utc::now(),
            kind: EventKind::Failure,
            operation: label(operation),
            correlation,
            elapsed_ms: millis(elapsed),
            handler_ms: None,
            dispatch_wait_ms: None,
            send_ms: None,
            stage: None,
            fault,
        }
    }

    pub(super) fn shutdown(stage: &'static str, elapsed: Duration) -> Self {
        Self {
            kind: EventKind::Shutdown,
            stage: Some(stage),
            ..Self::failure(
                "shutdown",
                Correlation::default(),
                elapsed,
                Fault::shutdown(),
            )
        }
    }
}

pub(super) fn label(value: &str) -> &'static str {
    const LABELS: &[&str] = &[
        "next",
        "ls",
        "show",
        "add",
        "claim",
        "update",
        "gate",
        "evaluate",
        "note",
        "done",
        "handoff",
        "remember",
        "memories",
        "forget",
        "search",
        "shutdown",
        "session_bind",
        "session_status",
        "turn_grant_read",
        "named_root_bind",
        "named_root_read",
        "named_root_sighting_read",
        "acceptance_binding_read",
        "acceptance_verification_read",
        "turn_evaluate",
        "turn_begin",
        "execution_observe",
        "turn_checkpoint",
        "verification_bind",
        "startup",
    ];
    LABELS
        .iter()
        .copied()
        .find(|label| *label == value)
        .unwrap_or("unknown")
}
