//! Constant-label timing for the owned HTTPS fixture, without payloads.

use std::{io::Write, time::Instant};

pub(super) struct Phase {
    name: &'static str,
    started: Instant,
}

pub(super) fn phase(name: &'static str) -> Phase {
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "\ndata-workflow phase={name} begin").unwrap();
    stdout.flush().unwrap();
    Phase {
        name,
        started: Instant::now(),
    }
}

impl Drop for Phase {
    fn drop(&mut self) {
        let mut stdout = std::io::stdout().lock();
        writeln!(
            stdout,
            "\ndata-workflow phase={} end elapsed_us={}",
            self.name,
            self.started.elapsed().as_micros()
        )
        .unwrap();
        stdout.flush().unwrap();
    }
}
