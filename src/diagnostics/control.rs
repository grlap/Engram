use super::{Correlation, Diagnostics, Event, Fault, Fingerprint};
use crate::host::HostControlRequest;
use std::{
    cell::Cell,
    time::{Duration, Instant},
};

/// Fixed frame metadata; neither input bodies nor response values are retained.
#[derive(Debug)]
pub(crate) struct Frame {
    pub(crate) operation: &'static str,
    pub(crate) correlation: Correlation,
    pub(crate) started: Instant,
    fault_observed: Cell<bool>,
    handler: Option<(Duration, crate::storage::ControlWriteTiming)>,
}
impl Frame {
    pub(crate) fn new(sequence: u64) -> Self {
        Self {
            operation: "unknown",
            correlation: Correlation {
                frame_seq: Some(sequence),
                ..Correlation::default()
            },
            started: crate::storage::control_diagnostic_now(),
            fault_observed: Cell::new(false),
            handler: None,
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
            HostControlRequest::VerificationRecord { operation_key, .. } => {
                (Some(operation_key), None)
            }
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
            | HostControlRequest::ControlConnectionRead {}
            | HostControlRequest::NamedRootRead { .. }
            | HostControlRequest::NamedRootSightingRead { .. }
            | HostControlRequest::AcceptanceBindingRead { .. }
            | HostControlRequest::AcceptanceVerificationRead { .. } => (None, None),
        };
        self.correlation.attempt_fingerprint = key.map(|key| Fingerprint::attempt(key));
        self.correlation.grant_id = grant.and_then(|id| uuid::Uuid::parse_str(id).ok());
    }
    pub(crate) fn emit(&self, sink: &Diagnostics, fault: Fault) {
        // A dropped diagnostic must still suppress the apparent success of a
        // typed refusal carried inside an Ok response envelope.
        self.fault_observed.set(true);
        sink.emit(Event::failure(
            self.operation,
            self.correlation,
            crate::storage::control_diagnostic_now().saturating_duration_since(self.started),
            fault,
        ));
    }

    pub(crate) fn handler_finished(&mut self, timing: crate::storage::ControlWriteTiming) {
        self.handler = Some((
            crate::storage::control_diagnostic_now().saturating_duration_since(self.started),
            timing,
        ));
    }

    pub(crate) fn complete(self, sink: &Diagnostics) {
        if self.fault_observed.get() {
            return;
        }
        let elapsed =
            crate::storage::control_diagnostic_now().saturating_duration_since(self.started);
        if elapsed > super::SLOW_CALL {
            let mut event =
                Event::failure(self.operation, self.correlation, elapsed, Fault::slow());
            if let Some((handler, timing)) = self.handler {
                event.handler_ms = Some(super::record::millis(handler));
                event.store_write = Some(timing.into());
            }
            sink.emit(event);
        }
    }
}
