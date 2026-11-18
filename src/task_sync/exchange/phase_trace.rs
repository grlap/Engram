//! Opt-in, constant-label timing for an owned test child only.

use std::{io::Write, time::Instant};

pub(super) struct Phase(&'static str, Instant);

pub(super) fn phase(name: &'static str) -> Option<Phase> {
    std::env::var_os("ENGRAM_EXCHANGE_PHASE_TRACE")?;
    // The wall clock lets the supervisor age a phase still open at its deadline.
    let unix_us = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |since| since.as_micros());
    let mut stdout = std::io::stdout().lock();
    writeln!(
        stdout,
        "\nexchange-retry phase={name} begin unix_us={unix_us}"
    )
    .unwrap();
    stdout.flush().unwrap();
    Some(Phase(name, Instant::now()))
}

impl Drop for Phase {
    fn drop(&mut self) {
        let mut stdout = std::io::stdout().lock();
        writeln!(
            stdout,
            "\nexchange-retry phase={} end elapsed_us={}",
            self.0,
            self.1.elapsed().as_micros()
        )
        .unwrap();
        stdout.flush().unwrap();
    }
}
