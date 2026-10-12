use super::*;
use crate::diagnostics::{Correlation, Fault};
use std::sync::atomic::AtomicUsize;

fn drain(writer: &Writer) -> bool {
    let result = writer.flush(crate::diagnostics::tests::RECORD_DRAIN);
    assert_eq!(
        writer.shared.flushed.load(Ordering::Acquire),
        writer.shared.flush_requested.load(Ordering::Acquire),
        "{writer:#?}"
    );
    assert!(
        writer.shared.processed.load(Ordering::Acquire)
            >= writer.shared.flush_target.load(Ordering::Acquire),
        "{writer:#?}"
    );
    result
}

#[test]
fn completed_flush_is_observed_before_wait_without_notification() {
    let (sender, _receiver) = mpsc::sync_channel(1);
    let shared = Arc::new(Shared::default());
    let writer = Writer::from_spawn(sender, Arc::clone(&shared), true);
    shared.accepted.store(1, Ordering::Release);
    shared.processed.store(1, Ordering::Release);
    shared.flush_target.store(1, Ordering::Release);
    shared.flush_requested.store(1, Ordering::Release);
    shared.flushed.store(1, Ordering::Release);
    let guard = shared.notification.0.lock().unwrap();
    // Model acknowledgement arriving before the caller starts to wait. No
    // notification is sent, and no scheduling or remaining budget is needed.
    assert!(writer.wait_for_flush(1, guard, Instant::now(), Duration::ZERO));
    assert_eq!(shared.counters.timeout.load(Ordering::Relaxed), 0);
}

#[test]
fn pending_flush_with_no_remaining_allowance_counts_timeout() {
    let (sender, _receiver) = mpsc::sync_channel(1);
    let shared = Arc::new(Shared::default());
    let writer = Writer::from_spawn(sender, Arc::clone(&shared), true);
    shared.accepted.store(1, Ordering::Release);
    shared.flush_target.store(1, Ordering::Release);
    shared.flush_requested.store(1, Ordering::Release);
    let guard = shared.notification.0.lock().unwrap();
    assert!(!writer.wait_for_flush(1, guard, Instant::now(), Duration::ZERO));
    assert_eq!(shared.flushed.load(Ordering::Acquire), 0);
    assert_eq!(shared.processed.load(Ordering::Acquire), 0);
    assert_eq!(shared.counters.timeout.load(Ordering::Relaxed), 1);
}

fn event() -> Event {
    Event::failure(
        "note",
        Correlation::default(),
        Duration::ZERO,
        Fault::tool_error(),
    )
}

#[test]
fn post_fence_drops_cannot_acknowledge_older_queued_records() {
    struct Barriers {
        entered: mpsc::SyncSender<usize>,
        resume: Mutex<mpsc::Receiver<()>>,
        appends: AtomicUsize,
        lines: Arc<Mutex<Vec<serde_json::Value>>>,
    }
    impl Sink for Barriers {
        fn append(&self, line: &[u8]) -> io::Result<()> {
            let index = self.appends.fetch_add(1, Ordering::Relaxed);
            if index < 2 {
                self.entered.send(index).unwrap();
                self.resume.lock().unwrap().recv().unwrap();
            }
            self.lines
                .lock()
                .unwrap()
                .push(serde_json::from_slice(line).unwrap());
            Ok(())
        }
    }
    let (entered, observed) = mpsc::sync_channel(1);
    let (resume, waiting) = mpsc::sync_channel(1);
    let lines = Arc::new(Mutex::new(Vec::new()));
    let writer = Writer::with_sink(
        "session-1",
        Surface::Mcp,
        Barriers {
            entered,
            resume: Mutex::new(waiting),
            appends: AtomicUsize::new(0),
            lines: Arc::clone(&lines),
        },
    );
    writer.emit(&event());
    assert_eq!(observed.recv().unwrap(), 0);
    for _ in 0..super::super::QUEUE_CAPACITY {
        writer.emit(&event());
    }
    assert!(!writer.flush(Duration::ZERO));
    assert_eq!(writer.shared.flush_target.load(Ordering::Acquire), 257);
    let concurrent = Arc::clone(&writer);
    std::thread::spawn(move || {
        for _ in 0..=super::super::QUEUE_CAPACITY {
            concurrent.emit(&event());
        }
    })
    .join()
    .unwrap();
    resume.send(()).unwrap();
    assert_eq!(observed.recv().unwrap(), 1);
    assert_eq!(writer.shared.flushed.load(Ordering::Acquire), 0);
    assert_eq!(writer.shared.processed.load(Ordering::Acquire), 1);
    assert_eq!(lines.lock().unwrap().len(), 1);
    resume.send(()).unwrap();
    assert!(drain(&writer), "{writer:#?}");
    let rows = lines.lock().unwrap();
    assert_eq!(
        rows.iter()
            .filter(|row| row["event"]["code"] == "tool_error")
            .count(),
        257
    );
    assert_eq!(rows.last().unwrap()["loss"]["queue"], 257);
}

#[test]
fn flush_does_not_overtake_a_publication_in_progress() {
    let (sender, _receiver) = mpsc::sync_channel(1);
    let shared = Arc::new(Shared::default());
    let writer = Writer::from_spawn(sender, Arc::clone(&shared), true);
    let _publication = shared.publication.lock().unwrap();
    assert!(!writer.flush(super::super::EXIT_FLUSH));
    assert_eq!(shared.flush_requested.load(Ordering::Acquire), 0);
    assert_eq!(shared.counters.timeout.load(Ordering::Relaxed), 1);
    writer.emit(&event());
    assert_eq!(shared.accepted.load(Ordering::Acquire), 0);
    assert_eq!(shared.processed.load(Ordering::Acquire), 0);
    assert_eq!(shared.counters.queue.load(Ordering::Relaxed), 1);
}

#[test]
fn writer_start_failure_is_counted_and_exit_stays_bounded() {
    let (sender, _receiver) = mpsc::sync_channel(1);
    let shared = Arc::new(Shared::default());
    let writer = Writer::from_spawn(sender, Arc::clone(&shared), false);
    assert_eq!(shared.counters.start.load(Ordering::Relaxed), 1);
    writer.emit(&event());
    assert_eq!(shared.counters.queue.load(Ordering::Relaxed), 1);
    assert!(!writer.flush(super::super::EXIT_FLUSH));
    assert_eq!(shared.counters.timeout.load(Ordering::Relaxed), 0);
    assert_eq!(shared.flush_requested.load(Ordering::Relaxed), 0);
}

struct Fake {
    lines: Arc<Mutex<Vec<serde_json::Value>>>,
    failures: Arc<AtomicUsize>,
    failure: io::ErrorKind,
}
impl Sink for Fake {
    fn append(&self, line: &[u8]) -> io::Result<()> {
        if self
            .failures
            .try_update(Ordering::AcqRel, Ordering::Acquire, |remaining| {
                remaining.checked_sub(1)
            })
            .is_ok()
        {
            return Err(self.failure.into());
        }
        self.lines
            .lock()
            .unwrap()
            .push(serde_json::from_slice(line).unwrap());
        Ok(())
    }
}

#[test]
fn control_startup_and_admission_do_not_wait_for_build_identity() {
    let expected = crate::build_identity::current()
        .build_fingerprint
        .as_ref()
        .map(ToString::to_string);
    let identity = expected.clone();
    let lines = Arc::new(Mutex::new(Vec::new()));
    let retained = Arc::clone(&lines);
    let (entered, observed) = mpsc::sync_channel(1);
    let (resume, waiting) = mpsc::sync_channel(1);
    let (returned, ready) = mpsc::sync_channel(1);
    let startup = std::thread::spawn(move || {
        let caller = std::thread::current().id();
        let writer = Writer::with_identity(
            "session-1",
            Surface::Control,
            Fake {
                lines: retained,
                failures: Arc::new(AtomicUsize::new(0)),
                failure: io::ErrorKind::Other,
            },
            move || {
                entered.send(std::thread::current().id()).unwrap();
                waiting.recv().unwrap();
                identity
            },
        );
        returned.send((caller, writer)).unwrap();
    });
    let reader = observed.recv_timeout(Duration::from_secs(10)).unwrap();
    // This timeout only bounds a broken test: the initializer remains blocked
    // until the test releases it, independent of scheduling or disk speed.
    let started = ready.recv_timeout(Duration::from_secs(10));
    if let Ok((caller, writer)) = &started {
        assert_ne!(*caller, reader);
        writer.emit(&event());
        assert_eq!(writer.shared.accepted.load(Ordering::Acquire), 1);
        assert_eq!(writer.shared.processed.load(Ordering::Acquire), 0);
        assert!(!writer.flush(Duration::ZERO));
        assert_eq!(writer.shared.flush_target.load(Ordering::Acquire), 1);
        assert_eq!(writer.shared.flushed.load(Ordering::Acquire), 0);
        assert_eq!(writer.shared.counters.timeout.load(Ordering::Relaxed), 1);
    }
    resume.send(()).unwrap();
    startup.join().unwrap();
    let (_, writer) = started.expect("control startup waited for the identity reader");
    assert!(drain(&writer), "{writer:#?}");
    let rows = lines.lock().unwrap();
    assert_eq!(rows[0]["event"]["code"], "tool_error");
    assert_eq!(rows[0]["producer"]["surface"], "control");
    assert_eq!(
        rows[0]["producer"]["build_fingerprint"],
        serde_json::json!(expected)
    );
    assert_eq!(rows.last().unwrap()["event"]["code"], "loss_summary");
    assert_eq!(rows.last().unwrap()["loss"]["flush_timeout"], 1);
}

#[test]
fn idle_flush_wakes_writer_and_clean_exit_is_a_summary() {
    let lines = Arc::new(Mutex::new(Vec::new()));
    let writer = Writer::with_sink(
        "session-1",
        Surface::Control,
        Fake {
            lines: Arc::clone(&lines),
            failures: Arc::new(AtomicUsize::new(0)),
            failure: io::ErrorKind::Other,
        },
    );
    assert!(drain(&writer), "{writer:#?}");
    let lines = lines.lock().unwrap();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["event"]["kind"], "summary");
    assert_eq!(lines[0]["event"]["code"], "loss_summary");
    assert_eq!(lines[0]["loss"]["queue"], 0);
    assert_eq!(lines[0]["loss"]["flush_timeout"], 0);
}

#[test]
fn losses_are_saturating_and_disclosed_after_each_injected_io_failure() {
    for failure in [
        io::ErrorKind::PermissionDenied,
        io::ErrorKind::StorageFull,
        io::ErrorKind::UnexpectedEof,
        io::ErrorKind::InvalidInput,
        io::ErrorKind::WouldBlock,
    ] {
        let lines = Arc::new(Mutex::new(Vec::new()));
        let writer = Writer::with_sink(
            "session-11296",
            Surface::Control,
            Fake {
                lines: Arc::clone(&lines),
                failures: Arc::new(AtomicUsize::new(1)),
                failure,
            },
        );
        writer.emit(&event());
        writer.emit(&event());
        assert!(!drain(&writer), "{writer:#?}");
        let lines = lines.lock().unwrap();
        assert_eq!(lines.len(), 2);
        let field = if failure == io::ErrorKind::WouldBlock {
            "lock"
        } else {
            "io"
        };
        assert_eq!(lines.last().unwrap()["loss"][field], 1);
        assert_eq!(lines[0]["producer"]["session_id"], "session-11296");
    }
    let counter = AtomicU64::new(u64::MAX);
    increment(&counter);
    assert_eq!(counter.load(Ordering::Relaxed), u64::MAX);
}

#[test]
fn queue_full_exit_signal_cannot_be_dropped_or_join_a_blocked_writer() {
    struct Blocked {
        entered: mpsc::SyncSender<()>,
        resume: Mutex<mpsc::Receiver<()>>,
        first: AtomicUsize,
        lines: Arc<Mutex<Vec<serde_json::Value>>>,
    }
    impl Sink for Blocked {
        fn append(&self, line: &[u8]) -> io::Result<()> {
            if self.first.fetch_add(1, Ordering::Relaxed) == 0 {
                self.entered.send(()).unwrap();
                self.resume.lock().unwrap().recv().unwrap();
            }
            self.lines
                .lock()
                .unwrap()
                .push(serde_json::from_slice(line).unwrap());
            Ok(())
        }
    }
    let (entered, observed) = mpsc::sync_channel(1);
    let (resume, waiting) = mpsc::sync_channel(1);
    let lines = Arc::new(Mutex::new(Vec::new()));
    let writer = Writer::with_sink(
        "session-1",
        Surface::Mcp,
        Blocked {
            entered,
            resume: Mutex::new(waiting),
            first: AtomicUsize::new(0),
            lines: Arc::clone(&lines),
        },
    );
    writer.emit(&event());
    observed.recv().unwrap();
    for _ in 0..=super::super::QUEUE_CAPACITY {
        writer.emit(&event());
    }
    assert!(!writer.flush(Duration::from_millis(20)));
    assert_eq!(writer.shared.processed.load(Ordering::Acquire), 0);
    assert_eq!(writer.shared.flushed.load(Ordering::Acquire), 0);
    assert_eq!(writer.shared.counters.timeout.load(Ordering::Relaxed), 1);
    resume.send(()).unwrap();
    assert!(drain(&writer), "{writer:#?}");
    let lines = lines.lock().unwrap();
    let summary = lines.last().unwrap();
    assert_eq!(summary["event"]["code"], "loss_summary");
    assert_eq!(summary["event"]["kind"], "summary");
    assert_eq!(summary["loss"]["queue"], 1);
    assert_eq!(summary["loss"]["flush_timeout"], 1);
    assert_eq!(
        lines
            .iter()
            .filter(|row| row["event"]["code"] == "tool_error")
            .count(),
        super::super::QUEUE_CAPACITY + 1
    );
}
