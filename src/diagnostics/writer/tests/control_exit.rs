use super::*;
use crate::diagnostics::tests::{RECORD_DRAIN, records};

fn fatal_line(writer: &Writer) -> Vec<u8> {
    let mut bytes = serde_json::to_vec(
        &writer
            .control_exit_envelope(
                &serde_json::json!({"kind":"control_exit","code":"control_abnormal_exit"}),
                None,
            )
            .unwrap(),
    )
    .unwrap();
    bytes.push(b'\n');
    bytes
}

#[test]
fn control_exit_serializes_with_the_worker_after_a_drain_timeout() {
    let home = crate::test_support::temp_home().unwrap();
    let (initializing, initialized) = mpsc::sync_channel(1);
    let (resume_identity, identity) = mpsc::sync_channel(1);
    let writer = Writer::with_identity(
        "session-1",
        Surface::Control,
        Ring::new(home.path()),
        move || {
            initializing.send(()).unwrap();
            identity.recv_timeout(RECORD_DRAIN).unwrap();
            None
        },
    );
    initialized.recv_timeout(RECORD_DRAIN).unwrap();
    writer.emit(&event());
    assert!(!writer.flush(Duration::ZERO));
    assert_eq!(writer.shared.counters.timeout.load(Ordering::Relaxed), 1);

    let (held, observe_held) = mpsc::sync_channel(1);
    let (release, resume_append) = mpsc::sync_channel(1);
    let (contended, observe_contention) = mpsc::sync_channel(1);
    *writer.shared.append_hooks.lock().unwrap() = Some(Arc::new(AppendHooks {
        held,
        release: Mutex::new(resume_append),
        contended,
    }));
    let fatal_writer = Arc::clone(&writer);
    let fatal_home = home.path().to_owned();
    let line = fatal_line(&writer);
    let fatal = std::thread::spawn(move || {
        fatal_writer.append_control_exit_by(&fatal_home, &line, Instant::now() + RECORD_DRAIN)
    });
    // Both local and OS locks are held before the worker is released.
    observe_held.recv_timeout(RECORD_DRAIN).unwrap();
    resume_identity.send(()).unwrap();
    // The worker actually attempted the same local gate; absence of a
    // completion or a scheduling delay is not used as evidence.
    observe_contention.recv_timeout(RECORD_DRAIN).unwrap();
    release.send(()).unwrap();
    fatal.join().unwrap().unwrap();
    *writer.shared.append_hooks.lock().unwrap() = None;
    assert!(drain(&writer), "{writer:#?}");
    let rows = records(home.path());
    for code in ["control_abnormal_exit", "tool_error"] {
        assert_eq!(
            rows.iter()
                .filter(|row| row["event"]["code"] == code)
                .count(),
            1,
            "{rows:?}"
        );
    }
    assert!(rows.iter().any(|row| row["event"]["kind"] == "summary"));
    assert_eq!(rows.last().unwrap()["loss"]["flush_timeout"], 1);
    let shared = &writer.shared;
    for counter in [
        &shared.counters.lock,
        &shared.counters.io,
        &shared.counters.start,
        &shared.counters.queue,
        &shared.counters.tracking,
    ] {
        assert_eq!(counter.load(Ordering::Relaxed), 0, "{writer:#?}");
    }
    assert_eq!(shared.counters.timeout.load(Ordering::Relaxed), 1);
    assert_eq!(shared.accepted.load(Ordering::Acquire), 1);
    assert_eq!(shared.processed.load(Ordering::Acquire), 1);
    assert_eq!(
        shared.flushed.load(Ordering::Acquire),
        shared.flush_requested.load(Ordering::Acquire)
    );
    assert!(drain(&writer), "a later flush remains healthy");
    assert_eq!(shared.counters.timeout.load(Ordering::Relaxed), 1);
}

struct BlockedAppend {
    entered: mpsc::SyncSender<()>,
    resume: Mutex<mpsc::Receiver<()>>,
    calls: AtomicUsize,
}

impl Sink for BlockedAppend {
    fn append(&self, _: &[u8]) -> io::Result<()> {
        if self.calls.fetch_add(1, Ordering::Relaxed) == 0 {
            self.entered.send(()).unwrap();
            self.resume
                .lock()
                .unwrap()
                .recv_timeout(RECORD_DRAIN)
                .unwrap();
        }
        Ok(())
    }
}

#[test]
fn control_exit_append_deadline_does_not_wait_for_a_blocked_worker() {
    let home = crate::test_support::temp_home().unwrap();
    let (entered, observe_append) = mpsc::sync_channel(1);
    let (resume, release_append) = mpsc::sync_channel(1);
    let writer = Writer::with_identity(
        "session-1",
        Surface::Control,
        BlockedAppend {
            entered,
            resume: Mutex::new(release_append),
            calls: AtomicUsize::new(0),
        },
        || None,
    );
    writer.emit(&event());
    observe_append.recv_timeout(RECORD_DRAIN).unwrap();
    assert!(matches!(
        writer.shared.append.try_lock(),
        Err(TryLockError::WouldBlock)
    ));
    let line = fatal_line(&writer);
    let result = writer.append_control_exit_by(home.path(), &line, Instant::now());
    resume.send(()).unwrap();
    let error = result.unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::TimedOut);
    assert_eq!(error.to_string(), "diagnostic append lock deadline expired");
    assert!(!home.path().join("diagnostics").exists());
    assert!(drain(&writer));
    writer.append_control_exit(home.path(), &line).unwrap();
    assert_eq!(records(home.path()).len(), 1);
}

#[test]
fn control_exit_and_worker_recover_the_poisoned_unit_append_gate() {
    let home = crate::test_support::temp_home().unwrap();
    let writer = Writer::with_identity(
        "session-1",
        Surface::Control,
        Ring::new(home.path()),
        || None,
    );
    assert!(
        std::panic::catch_unwind(|| {
            let _gate = writer.shared.append.lock().unwrap();
            panic!("injected append-gate unwind");
        })
        .is_err()
    );
    writer
        .append_control_exit(home.path(), &fatal_line(&writer))
        .unwrap();
    writer.emit(&event());
    assert!(drain(&writer));
    let rows = records(home.path());
    assert!(rows.iter().any(|row| row["event"]["code"] == "tool_error"));
    assert!(
        rows.iter()
            .any(|row| row["event"]["code"] == "control_abnormal_exit")
    );
    assert_eq!(writer.shared.counters.lock.load(Ordering::Relaxed), 0);
}
