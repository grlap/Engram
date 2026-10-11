use super::{Correlation, Diagnostics, Event, Fault, Fingerprint};
use crate::host::HostControlRequest;
use std::time::Instant;

/// Fixed frame metadata; neither input bodies nor response values are retained.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Frame {
    pub(crate) operation: &'static str,
    pub(crate) correlation: Correlation,
    pub(crate) started: Instant,
}
impl Frame {
    pub(crate) fn new(sequence: u64) -> Self {
        Self {
            operation: "unknown",
            correlation: Correlation {
                frame_seq: Some(sequence),
                ..Correlation::default()
            },
            started: Instant::now(),
        }
    }
    pub(crate) fn request(&mut self, request: &HostControlRequest) {
        self.operation = request.operation();
        let (key, grant) = match request {
            HostControlRequest::SessionBind {
                idempotency_key, ..
            }
            | HostControlRequest::NamedRootBind {
                idempotency_key, ..
            }
            | HostControlRequest::TurnEvaluate {
                idempotency_key, ..
            }
            | HostControlRequest::ExecutionObserve {
                idempotency_key, ..
            }
            | HostControlRequest::VerificationBind {
                idempotency_key, ..
            } => (Some(idempotency_key), None),
            HostControlRequest::TurnBegin {
                idempotency_key,
                grant_id,
                ..
            }
            | HostControlRequest::TurnCheckpoint {
                idempotency_key,
                grant_id,
                ..
            } => (Some(idempotency_key), Some(grant_id)),
            HostControlRequest::TurnGrantRead { grant_id, .. } => (None, Some(grant_id)),
            HostControlRequest::SessionStatus { .. }
            | HostControlRequest::NamedRootRead { .. }
            | HostControlRequest::NamedRootSightingRead { .. }
            | HostControlRequest::AcceptanceBindingRead { .. }
            | HostControlRequest::AcceptanceVerificationRead { .. } => (None, None),
        };
        self.correlation.attempt_fingerprint = key.map(|key| Fingerprint::attempt(key));
        self.correlation.grant_id = grant.and_then(|id| uuid::Uuid::parse_str(id).ok());
    }
    pub(crate) fn emit(self, sink: &Diagnostics, fault: Fault) {
        sink.emit(Event::failure(
            self.operation,
            self.correlation,
            self.started.elapsed(),
            fault,
        ));
    }
}
