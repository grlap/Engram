use super::{
    Event, Fingerprint, Surface,
    ring::{Ring, Sink},
};
use serde::Serialize;
use std::{
    io,
    path::Path,
    sync::{
        Arc, Condvar, Mutex,
        atomic::{AtomicU64, Ordering},
        mpsc,
    },
    time::{Duration, Instant},
};

#[derive(Debug, Default)]
struct Counters {
    queue: AtomicU64,
    io: AtomicU64,
    lock: AtomicU64,
    start: AtomicU64,
    oversized: AtomicU64,
    tracking: AtomicU64,
    quarantine_overflow: AtomicU64,
    timeout: AtomicU64,
}
fn increment(counter: &AtomicU64) {
    let _ = counter.try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
        Some(value.saturating_add(1))
    });
}

#[derive(Debug, Default)]
struct Shared {
    counters: Counters,
    publication: Mutex<()>,
    accepted: AtomicU64,
    processed: AtomicU64,
    flush_requested: AtomicU64,
    flush_target: AtomicU64,
    flushed: AtomicU64,
    notification: (Mutex<()>, Condvar),
}

#[derive(Debug, Serialize)]
struct Producer {
    build_fingerprint: Option<String>,
    source_revision: &'static str,
    pid: u32,
    instance_id: uuid::Uuid,
    surface: Surface,
    session_id: Option<String>,
    session_fingerprint: Option<Fingerprint>,
}

#[derive(Debug)]
pub(super) struct Writer {
    sender: Option<mpsc::SyncSender<Option<Event>>>,
    shared: Arc<Shared>,
}

impl Writer {
    pub(super) fn start(home: &Path, session: &str, surface: Surface) -> Arc<Self> {
        Self::with_sink(session, surface, Ring::new(home))
    }

    fn with_sink(session: &str, surface: Surface, ring: impl Sink + 'static) -> Arc<Self> {
        Self::with_identity(session, surface, ring, || {
            crate::build_identity::current()
                .build_fingerprint
                .as_ref()
                .map(ToString::to_string)
        })
    }

    fn with_identity(
        session: &str,
        surface: Surface,
        ring: impl Sink + 'static,
        identity: impl FnOnce() -> Option<String> + Send + 'static,
    ) -> Arc<Self> {
        let opaque = session.strip_prefix("session-").is_some_and(|tail| {
            !tail.is_empty() && tail.len() <= 20 && tail.bytes().all(|byte| byte.is_ascii_digit())
        });
        let mut producer = Producer {
            build_fingerprint: None,
            source_revision: crate::build_identity::source_revision(),
            pid: std::process::id(),
            instance_id: uuid::Uuid::new_v4(),
            surface,
            session_id: opaque.then(|| session.to_owned()),
            session_fingerprint: (!opaque).then(|| Fingerprint::session(session)),
        };
        let shared = Arc::new(Shared::default());
        let worker_shared = Arc::clone(&shared);
        let (sender, receiver) = mpsc::sync_channel(super::QUEUE_CAPACITY);
        let started = std::thread::Builder::new()
            .name("engram-diagnostics".into())
            .spawn(move || {
                // The first identity lookup may read the executable. Keep it
                // off the startup caller just like all diagnostic disk work.
                producer.build_fingerprint = identity();
                run(&receiver, &worker_shared, &ring, &producer);
            });
        Self::from_spawn(sender, shared, started.is_ok())
    }

    fn from_spawn(
        sender: mpsc::SyncSender<Option<Event>>,
        shared: Arc<Shared>,
        started: bool,
    ) -> Arc<Self> {
        if !started {
            increment(&shared.counters.start);
        }
        Arc::new(Self {
            sender: started.then_some(sender),
            shared,
        })
    }

    pub(super) fn emit(&self, event: &Event) {
        // Publish and count under one short, nonblocking admission lock.
        // Flush cannot capture an in-progress publication; dropped attempts
        // never advance the writer's ordered completion fence.
        let Ok(_publication) = self.shared.publication.try_lock() else {
            increment(&self.shared.counters.queue);
            return;
        };
        if self
            .sender
            .as_ref()
            .is_none_or(|sender| sender.try_send(Some(*event)).is_err())
        {
            increment(&self.shared.counters.queue);
        } else {
            increment(&self.shared.accepted);
        }
    }

    pub(super) fn tracking_lost(&self) {
        increment(&self.shared.counters.tracking);
    }

    pub(super) fn quarantine_overflow(&self) {
        increment(&self.shared.counters.quarantine_overflow);
    }

    pub(super) fn flush(&self, allowance: Duration) -> bool {
        let Some(sender) = &self.sender else {
            return false;
        };
        let started = Instant::now();
        let Ok(guard) = self.shared.notification.0.lock() else {
            return false;
        };
        {
            let Ok(_publication) = self.shared.publication.try_lock() else {
                increment(&self.shared.counters.timeout);
                return false;
            };
            self.shared.flush_target.store(
                self.shared.accepted.load(Ordering::Acquire),
                Ordering::Release,
            );
        }
        let generation = self
            .shared
            .flush_requested
            .fetch_add(1, Ordering::AcqRel)
            .saturating_add(1);
        // Empty queues need a wake. A full queue already keeps the writer
        // active; the independent atomic fence cannot be dropped with it.
        if let Err(mpsc::TrySendError::Disconnected(_)) = sender.try_send(None) {
            return false;
        }
        self.wait_for_flush(generation, guard, started, allowance)
    }

    fn wait_for_flush(
        &self,
        generation: u64,
        mut guard: std::sync::MutexGuard<'_, ()>,
        started: Instant,
        allowance: Duration,
    ) -> bool {
        while self.shared.flushed.load(Ordering::Acquire) < generation {
            let remaining = allowance.saturating_sub(started.elapsed());
            if remaining.is_zero() {
                increment(&self.shared.counters.timeout);
                return false;
            }
            let Ok((next, _)) = self.shared.notification.1.wait_timeout(guard, remaining) else {
                return false;
            };
            guard = next;
        }
        self.shared.counters.start.load(Ordering::Relaxed) == 0
            && self.shared.counters.io.load(Ordering::Relaxed) == 0
            && self.shared.counters.lock.load(Ordering::Relaxed) == 0
    }
}

fn write(event: &Event, shared: &Shared, ring: &impl Sink, producer: &Producer) {
    let value = serde_json::json!({"engram_diagnostic":1,"producer":producer,"event":event,"loss":{"queue":shared.counters.queue.load(Ordering::Relaxed),"io":shared.counters.io.load(Ordering::Relaxed),"lock":shared.counters.lock.load(Ordering::Relaxed),"writer_start":shared.counters.start.load(Ordering::Relaxed),"oversized":shared.counters.oversized.load(Ordering::Relaxed),"tracking":shared.counters.tracking.load(Ordering::Relaxed),"quarantine_overflow":shared.counters.quarantine_overflow.load(Ordering::Relaxed),"flush_timeout":shared.counters.timeout.load(Ordering::Relaxed)}});
    let mut line = value.to_string().into_bytes();
    line.push(b'\n');
    if line.len() > super::MAX_RECORD_BYTES {
        increment(&shared.counters.oversized);
        return;
    }
    if let Err(error) = ring.append(&line) {
        increment(if error.kind() == io::ErrorKind::WouldBlock {
            &shared.counters.lock
        } else {
            &shared.counters.io
        });
    }
}

fn run(
    receiver: &mpsc::Receiver<Option<Event>>,
    shared: &Shared,
    ring: &impl Sink,
    producer: &Producer,
) {
    loop {
        match receiver.recv() {
            Ok(Some(event)) => {
                write(&event, shared, ring, producer);
                increment(&shared.processed);
            }
            Err(_) => return,
            Ok(None) => {}
        }
        let generation = shared.flush_requested.load(Ordering::Acquire);
        if generation > shared.flushed.load(Ordering::Acquire)
            && shared.processed.load(Ordering::Acquire)
                >= shared.flush_target.load(Ordering::Acquire)
        {
            write(&Event::summary(), shared, ring, producer);
            shared.flushed.store(generation, Ordering::Release);
            if let Ok(_guard) = shared.notification.0.lock() {
                shared.notification.1.notify_all();
            }
        }
    }
}

#[cfg(test)]
mod tests;
