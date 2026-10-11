//! Bounded operational diagnostics, independent of canonical storage.
//!
//! Front doors submit fixed records without waiting for disk. The writer owns
//! the home-wide ring; diagnostics never change admission or operation results.

pub(crate) mod control;
pub mod mcp;
mod record;
mod ring;
mod writer;

pub use record::{Correlation, CorrelationLoss, Event, Fault, Fingerprint, Surface};
use std::{path::Path, sync::Arc, time::Duration};
use writer::Writer;

pub const SLOW_CALL: Duration = Duration::from_millis(2_000);
pub const SHUTDOWN_OVERRUN: Duration = Duration::from_secs(10);
pub const EXIT_FLUSH: Duration = Duration::from_secs(1);
pub const MAX_RECORD_BYTES: usize = 4_096;
pub const QUEUE_CAPACITY: usize = 256;

/// One producer's fixed metadata and bounded writer. Cloning shares its queue.
#[derive(Clone, Debug)]
pub struct Diagnostics {
    writer: Arc<Writer>,
}

impl Diagnostics {
    /// Uses the explicitly selected home; never discovers or opens a store.
    pub fn start(home: &Path, session: &str, surface: Surface) -> Self {
        Self {
            writer: Writer::start(home, session, surface),
        }
    }

    /// Bounded admission only. Serialization and filesystem work are off-path.
    pub fn emit(&self, event: Event) {
        self.writer.emit(&event);
    }

    /// Waits only up to the allowance, including when the data queue is full.
    /// `true` means drained without writer-start, I/O or lock losses. Queue and
    /// tracking losses remain visible in records; a timed-out writer is not joined.
    pub fn flush(&self, allowance: Duration) -> bool {
        self.writer.flush(allowance)
    }

    pub fn tracking_lost(&self) {
        self.writer.tracking_lost();
    }

    pub(super) fn quarantine_overflow(&self) {
        self.writer.quarantine_overflow();
    }
}

#[cfg(test)]
pub(crate) mod tests;
