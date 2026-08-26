//! External tracker publication boundary.
//!
//! V1 ships only the deterministic dummy adapter. Proprietary integrations
//! implement this same contract later without leaking backend-specific types
//! into the core.

use std::{collections::HashMap, sync::Mutex};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::ObjectHash;

/// Adapter feature declaration used to gate external side effects.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct TrackerCapabilities {
    pub publish_report: bool,
}

/// One immutable publication attempt. A key is permanently bound to the
/// report hash and canonical bytes supplied on its first successful handling.
#[derive(Clone, Debug)]
pub struct PublicationRequest<'a> {
    pub external_ref: Option<&'a str>,
    pub report_hash: &'a ObjectHash,
    pub report_bytes: &'a [u8],
    pub idempotency_key: &'a str,
}

/// Durable evidence that an adapter accepted a publication intent.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct PublicationReceipt {
    pub adapter: String,
    pub receipt_id: String,
    pub report_hash: ObjectHash,
    pub external_ref: Option<String>,
}

/// Failures at the side-effect boundary.
#[derive(Debug, Error)]
pub enum TrackerError {
    #[error("adapter does not support report publication")]
    Unsupported,
    #[error("idempotency key {0:?} was reused with a different payload")]
    IdempotencyConflict(String),
    #[error("report hash mismatch: expected {expected}, got {actual}")]
    ReportHashMismatch {
        expected: ObjectHash,
        actual: ObjectHash,
    },
    #[error("dummy adapter lock was poisoned")]
    LockPoisoned,
}

/// Backend-neutral tracker port.
pub trait TrackerAdapter: Send + Sync {
    fn capabilities(&self) -> TrackerCapabilities;
    /// Publishes one frozen report under its permanently bound idempotency key.
    ///
    /// # Errors
    ///
    /// Returns [`TrackerError`] when publication is unsupported, the key is
    /// reused with a different payload, or the adapter cannot complete the
    /// operation.
    fn publish_report(
        &self,
        request: PublicationRequest<'_>,
    ) -> Result<PublicationReceipt, TrackerError>;
}

#[derive(Clone)]
struct DummyPublication {
    payload_fingerprint: [u8; 32],
    receipt: PublicationReceipt,
}

/// In-memory V1 adapter that exercises publication and retry semantics without
/// performing external side effects.
#[derive(Default)]
pub struct DummyTrackerAdapter {
    publications: Mutex<HashMap<String, DummyPublication>>,
}

impl TrackerAdapter for DummyTrackerAdapter {
    fn capabilities(&self) -> TrackerCapabilities {
        TrackerCapabilities {
            publish_report: true,
        }
    }

    fn publish_report(
        &self,
        request: PublicationRequest<'_>,
    ) -> Result<PublicationReceipt, TrackerError> {
        let actual_hash = ObjectHash::from_canonical_bytes(request.report_bytes);
        if &actual_hash != request.report_hash {
            return Err(TrackerError::ReportHashMismatch {
                expected: request.report_hash.clone(),
                actual: actual_hash,
            });
        }

        let mut fingerprint_hasher = Sha256::new();
        fingerprint_hasher.update(request.report_hash.as_str().as_bytes());
        fingerprint_hasher.update(request.report_bytes);
        fingerprint_hasher.update(request.external_ref.unwrap_or_default().as_bytes());
        let payload_fingerprint: [u8; 32] = fingerprint_hasher.finalize().into();

        let mut publications = self
            .publications
            .lock()
            .map_err(|_| TrackerError::LockPoisoned)?;
        if let Some(existing) = publications.get(request.idempotency_key) {
            if existing.payload_fingerprint != payload_fingerprint {
                return Err(TrackerError::IdempotencyConflict(
                    request.idempotency_key.to_owned(),
                ));
            }
            return Ok(existing.receipt.clone());
        }

        let mut receipt_hasher = Sha256::new();
        receipt_hasher.update(b"engram-dummy-receipt-v1\0");
        receipt_hasher.update(request.idempotency_key.as_bytes());
        receipt_hasher.update(payload_fingerprint);
        let receipt_id = format!("dummy-{:x}", receipt_hasher.finalize());
        let receipt = PublicationReceipt {
            adapter: "dummy".into(),
            receipt_id,
            report_hash: request.report_hash.clone(),
            external_ref: request.external_ref.map(str::to_owned),
        };
        publications.insert(
            request.idempotency_key.to_owned(),
            DummyPublication {
                payload_fingerprint,
                receipt: receipt.clone(),
            },
        );
        Ok(receipt)
    }
}

#[cfg(test)]
mod tests {
    use crate::canonical::CanonicalObject;

    use super::*;

    #[test]
    fn identical_retry_returns_the_original_receipt() {
        let adapter = DummyTrackerAdapter::default();
        let report = CanonicalObject::freeze(&serde_json::json!({"outcome": "done"})).unwrap();
        let request = || PublicationRequest {
            external_ref: Some("dummy:TASK-1"),
            report_hash: report.hash(),
            report_bytes: report.bytes(),
            idempotency_key: "publish-task-1-report-1",
        };

        let first = adapter.publish_report(request()).unwrap();
        let retry = adapter.publish_report(request()).unwrap();

        assert_eq!(first, retry);
    }

    #[test]
    fn same_key_with_different_report_is_a_conflict() {
        let adapter = DummyTrackerAdapter::default();
        let first = CanonicalObject::freeze(&serde_json::json!({"outcome": "done"})).unwrap();
        let revised = CanonicalObject::freeze(&serde_json::json!({"outcome": "revised"})).unwrap();
        adapter
            .publish_report(PublicationRequest {
                external_ref: Some("dummy:TASK-1"),
                report_hash: first.hash(),
                report_bytes: first.bytes(),
                idempotency_key: "publish-task-1-report-1",
            })
            .unwrap();
        let conflict = adapter.publish_report(PublicationRequest {
            external_ref: Some("dummy:TASK-1"),
            report_hash: revised.hash(),
            report_bytes: revised.bytes(),
            idempotency_key: "publish-task-1-report-1",
        });

        assert!(matches!(
            conflict,
            Err(TrackerError::IdempotencyConflict(_))
        ));
    }

    #[test]
    fn mismatched_report_hash_is_rejected() {
        let adapter = DummyTrackerAdapter::default();
        let report = CanonicalObject::freeze(&serde_json::json!({"outcome": "done"})).unwrap();
        let different =
            CanonicalObject::freeze(&serde_json::json!({"outcome": "different"})).unwrap();

        let result = adapter.publish_report(PublicationRequest {
            external_ref: Some("dummy:TASK-1"),
            report_hash: report.hash(),
            report_bytes: different.bytes(),
            idempotency_key: "publish-task-1-report-1",
        });

        assert!(matches!(
            result,
            Err(TrackerError::ReportHashMismatch { .. })
        ));
    }
}
