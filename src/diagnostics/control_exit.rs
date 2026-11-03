//! Terminal control observations bypass the lossy asynchronous queue and SQLite.
use super::{Correlation, Diagnostics};
use std::{io, time::Instant};

impl Diagnostics {
    /// Append and sync a fixed terminal record, outside the serving connection.
    /// A killed process cannot call this; its supervisor owns that observation.
    ///
    /// # Errors
    /// Returns an error if the bounded ring cannot be locked, written or synced.
    pub fn control_abnormal_exit(
        &self,
        reason: &'static str,
        operation: &'static str,
        correlation: Correlation,
        phase: &'static str,
        elapsed: std::time::Duration,
    ) -> io::Result<()> {
        // Drain earlier observations before taking the same ring lock. The
        // fatal append still runs if that bounded asynchronous drain fails.
        let _ = self.flush(super::EXIT_FLUSH);
        let identity = crate::build_identity::current_by(Some(Instant::now() + super::EXIT_FLUSH));
        let event = serde_json::json!({
                "kind": "control_exit", "code": "control_abnormal_exit", "reason": reason,
                "operation": operation, "phase": phase,
                "elapsed_ms": super::record::millis(elapsed),
                "frame_seq": correlation.frame_seq,
                "attempt_fingerprint": correlation.attempt_fingerprint,
                "observed_at": chrono::Utc::now(),
        });
        let record = self.writer.control_exit_envelope(
            &event,
            identity
                .and_then(|identity| identity.build_fingerprint.as_ref().map(ToString::to_string)),
        )?;
        let mut bytes = serde_json::to_vec(&record)?;
        bytes.push(b'\n');
        let result = self.writer.append_control_exit(&self.home, &bytes);
        if result
            .as_ref()
            .is_err_and(|error| error.kind() == io::ErrorKind::TimedOut)
        {
            eprintln!("Engram control abnormal-exit diagnostic append lock deadline expired");
        }
        result
    }
}
