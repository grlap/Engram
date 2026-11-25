//! Operation timing policy and non-interrupting, noncanonical warnings.
//!
//! A warning owns no cancellation, database connection, or retry. One shared
//! observer serves blocking and async callers, including a blocked native call.

use std::io::Write;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

/// Minimum automatic operation timeout. Longer transfer budgets are retained.
pub const MINIMUM: Duration = Duration::from_secs(60);
/// Warn once while the operation is still running; never cancel it here.
pub const WARN_AFTER: Duration = Duration::from_secs(10);

static STDERR_WARNINGS: AtomicBool = AtomicBool::new(true);

/// Configure warning output before this process starts its operations.
/// Libraries retain warnings by default; a CLI may reserve stderr for JSON.
/// This affects new guards only and never changes an operation's deadline.
pub fn set_stderr_warnings(enabled: bool) {
    STDERR_WARNINGS.store(enabled, Ordering::Release);
}

/// Run the complete asynchronous operation, including its result processing.
pub async fn observe<T>(name: &'static str, operation: impl std::future::Future<Output = T>) -> T {
    let _warning = Warning::start(name);
    operation.await
}

#[cfg(test)]
pub(crate) mod test_clock;

pub(crate) fn now() -> Instant {
    #[cfg(test)]
    if let Some(now) = test_clock::now() {
        return now;
    }
    Instant::now()
}

const ACTIVE: u8 = 0;
const WARNED: u8 = 1;
const FINISHED: u8 = 2;

#[derive(Debug)]
struct Pending {
    operation: &'static str,
    attempt: u64,
    started: Instant,
    due: Instant,
    state: AtomicU8,
}

type Report = (&'static str, u64, Duration);

#[derive(Default)]
struct Queue {
    pending: Vec<Weak<Pending>>,
    /// Operations that ended past their warning time before a scan named
    /// them; the monitor writes these so the operation never waits on stderr.
    late: Vec<Report>,
    /// Set while the monitor writes a batch it has already taken, so a flush
    /// at exit knows to wait for it.
    batch_in_flight: bool,
}

impl Queue {
    fn collect(&mut self, now: Instant) -> (Vec<Report>, Option<Duration>) {
        let mut warnings = std::mem::take(&mut self.late);
        let mut next: Option<Duration> = None;
        self.pending.retain(|weak| {
            let Some(pending) = weak.upgrade() else {
                return false;
            };
            if pending.state.load(Ordering::Acquire) != ACTIVE {
                return false;
            }
            if now >= pending.due {
                if pending
                    .state
                    .compare_exchange(ACTIVE, WARNED, Ordering::AcqRel, Ordering::Acquire)
                    .is_ok()
                {
                    warnings.push((
                        pending.operation,
                        pending.attempt,
                        now.saturating_duration_since(pending.started),
                    ));
                }
                return false;
            }
            let left = pending.due.saturating_duration_since(now);
            next = Some(next.map_or(left, |previous| previous.min(left)));
            true
        });
        (warnings, next)
    }
}

#[derive(Default)]
struct Monitor {
    queue: Mutex<Queue>,
    changed: Condvar,
    /// Held while a batch is written, always taken after `queue`, so a flush
    /// at process exit waits for a batch the monitor is writing.
    writing: Mutex<()>,
}

static MONITOR: OnceLock<Arc<Monitor>> = OnceLock::new();

/// How long ending the process waits for its due warnings to reach stderr.
/// It ends no operation, which has already settled: it only keeps a stderr
/// pipe nobody drains from holding the finished process alive.
const EXIT_FLUSH_WAIT: Duration = Duration::from_secs(1);

/// Writes, before the process ends, every warning that is due and not yet
/// written: one for an operation that ended late and was only queued, and one
/// for an operation still running past its time when the process exits. A
/// short-lived CLI calls this after its command; it never starts the monitor.
/// A stderr that does not take the warnings within a second is left behind.
pub fn flush() {
    if let Some(monitor) = MONITOR.get() {
        monitor.flush_within(EXIT_FLUSH_WAIT, || -> Box<dyn Write> {
            match warning_output() {
                Some(output) => Box::new(output),
                None => Box::new(std::io::sink()),
            }
        });
    }
}

/// Warnings reach stderr through a handle of their own, duplicated once, and
/// never through the standard library's shared stderr lock, so a slow warning
/// write never holds the lock the command's own error output needs. Where no
/// handle can be duplicated, warnings are not written.
fn warning_output() -> Option<&'static std::fs::File> {
    static OUTPUT: OnceLock<Option<std::fs::File>> = OnceLock::new();
    OUTPUT.get_or_init(duplicate_stderr).as_ref()
}

#[cfg(unix)]
fn duplicate_stderr() -> Option<std::fs::File> {
    use std::os::fd::AsFd;
    std::io::stderr()
        .as_fd()
        .try_clone_to_owned()
        .ok()
        .map(std::fs::File::from)
}

#[cfg(windows)]
fn duplicate_stderr() -> Option<std::fs::File> {
    use std::os::windows::io::AsHandle;
    std::io::stderr()
        .as_handle()
        .try_clone_to_owned()
        .ok()
        .map(std::fs::File::from)
}

#[cfg(not(any(unix, windows)))]
fn duplicate_stderr() -> Option<std::fs::File> {
    None
}

/// The whole line, newline included, goes out in one write, so another stderr
/// writer, such as a phase trace record, never lands inside it.
fn write_warning(out: &mut impl Write, (operation, attempt, elapsed): Report) {
    let line = format!(
        "warning: {operation} is still running after 10 seconds; attempt={}/{attempt}; elapsed_ms={}\n",
        std::process::id(),
        elapsed.as_millis()
    );
    let _ = out.write_all(line.as_bytes());
}

impl Monitor {
    /// Flushes on a helper thread and waits at most `wait` for it, so neither
    /// a monitor batch blocked on the output nor the flush's own blocked write
    /// keeps the exiting thread waiting.
    fn flush_within<W: Write>(
        self: &Arc<Self>,
        wait: Duration,
        open: impl FnOnce() -> W + Send + 'static,
    ) {
        let (done, finished) = std::sync::mpsc::channel();
        let monitor = Arc::clone(self);
        let spawned = std::thread::Builder::new()
            .name("engram-warning-flush".into())
            .spawn(move || {
                monitor.flush_into(open);
                let _ = done.send(());
            });
        if spawned.is_ok() {
            let _ = finished.recv_timeout(wait);
        }
    }

    /// Opens the output only after taking `writing`, the monitor's own order,
    /// so a flush never holds stderr while the monitor holds `writing`.
    fn flush_into<W: Write>(&self, open: impl FnOnce() -> W) {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        let (warnings, _) = queue.collect(now());
        // With nothing to write and no batch being written, leave stderr
        // alone: another thread may hold its lock, blocked on a pipe nobody
        // drains, and the process must still exit.
        if warnings.is_empty() && !queue.batch_in_flight {
            return;
        }
        let _writing = self
            .writing
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        drop(queue);
        let mut out = open();
        for warning in warnings {
            write_warning(&mut out, warning);
        }
        let _ = out.flush();
    }

    fn shared() -> &'static Arc<Self> {
        MONITOR.get_or_init(|| {
            let monitor = Arc::new(Self::default());
            let worker = Arc::clone(&monitor);
            // Observation failure never refuses the operation or alters its result.
            let _ = std::thread::Builder::new()
                .name("engram-operation-warnings".into())
                .spawn(move || worker.run());
            monitor
        })
    }

    fn run(&self) {
        let mut queue = self
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        loop {
            let (warnings, next) = queue.collect(Instant::now());
            if !warnings.is_empty() {
                // Do not hold the queue lock while stderr is being written;
                // hold `writing` instead, so a flush at exit waits for this.
                let writing = self
                    .writing
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                queue.batch_in_flight = true;
                drop(queue);
                if let Some(mut output) = warning_output() {
                    for warning in warnings {
                        write_warning(&mut output, warning);
                    }
                }
                drop(writing);
                queue = self
                    .queue
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                queue.batch_in_flight = false;
                continue;
            }
            queue = match next {
                Some(left) => {
                    self.changed
                        .wait_timeout(queue, left)
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .0
                }
                None => self
                    .changed
                    .wait(queue)
                    .unwrap_or_else(std::sync::PoisonError::into_inner),
            };
        }
    }
}

/// Keep this guard alive until the named operation settles, including unwinding.
/// Labels must be fixed operation names, never caller text, paths, or credentials.
#[derive(Debug)]
#[must_use]
pub struct Warning {
    pending: Option<Arc<Pending>>,
}

impl Warning {
    pub fn start(operation: &'static str) -> Self {
        Self::start_with_output(operation, STDERR_WARNINGS.load(Ordering::Acquire))
    }

    fn start_with_output(operation: &'static str, enabled: bool) -> Self {
        static ATTEMPTS: AtomicU64 = AtomicU64::new(0);
        if !enabled {
            return Self { pending: None };
        }
        let started = now();
        let pending = Arc::new(Pending {
            operation,
            attempt: ATTEMPTS.fetch_add(1, Ordering::Relaxed),
            started,
            due: started + WARN_AFTER,
            state: AtomicU8::new(ACTIVE),
        });
        #[cfg(test)]
        if test_clock::register(&pending) {
            return Self {
                pending: Some(pending),
            };
        }
        let monitor = Monitor::shared();
        monitor
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .pending
            .push(Arc::downgrade(&pending));
        monitor.changed.notify_one();
        Self {
            pending: Some(pending),
        }
    }
}

impl Drop for Warning {
    fn drop(&mut self) {
        let Some(pending) = &self.pending else {
            return;
        };
        // An operation that ends past its warning time before the monitor's
        // scan named it claims the warning here, so it is still named once.
        let ended = now();
        let late = ended >= pending.due
            && pending
                .state
                .compare_exchange(ACTIVE, WARNED, Ordering::AcqRel, Ordering::Acquire)
                .is_ok();
        pending.state.store(FINISHED, Ordering::Release);
        let report = late.then(|| {
            (
                pending.operation,
                pending.attempt,
                ended.saturating_duration_since(pending.started),
            )
        });
        #[cfg(test)]
        if test_clock::owns(pending) {
            if let Some(report) = report {
                test_clock::report_late(report);
            }
            return;
        }
        let monitor = Monitor::shared();
        if let Some(report) = report {
            monitor
                .queue
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .late
                .push(report);
        }
        monitor.changed.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A short-lived process flushes before it ends: a late report the monitor
    // has not written yet, and an operation still running past its time (as
    // when a process exits without dropping its guard), are each written once.
    #[test]
    fn a_flush_at_exit_writes_every_due_warning_once() {
        let monitor = Monitor::default();
        let started = now()
            .checked_sub(Duration::from_secs(12))
            .expect("a monotonic clock twelve seconds back");
        let running = Arc::new(Pending {
            operation: "still running",
            attempt: 3,
            started,
            due: started + WARN_AFTER,
            state: AtomicU8::new(ACTIVE),
        });
        {
            let mut queue = monitor.queue.lock().unwrap();
            queue.late.push(("ended late", 7, Duration::from_secs(11)));
            queue.pending.push(Arc::downgrade(&running));
        }
        let mut out = Vec::new();
        let sink = &mut out;
        monitor.flush_into(move || sink);
        let text = String::from_utf8(out).unwrap();
        let lines: Vec<_> = text.lines().collect();
        assert_eq!(lines.len(), 2, "{text}");
        assert!(
            lines.iter().any(|line| line
                .starts_with("warning: ended late is still running after 10 seconds;")
                && line.ends_with("elapsed_ms=11000")),
            "{text}"
        );
        assert!(
            lines.iter().any(|line| line
                .starts_with("warning: still running is still running after 10 seconds;")),
            "{text}"
        );
        let mut again = Vec::new();
        let sink = &mut again;
        monitor.flush_into(move || sink);
        assert_eq!(again, Vec::<u8>::new());
    }

    struct Shared<'a>(std::sync::MutexGuard<'a, Vec<u8>>);

    impl Write for Shared<'_> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.write(bytes)
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    // A flush at exit can meet the monitor in the middle of a batch: the
    // monitor holds `writing` and then needs the output. The flush waits for
    // that batch without holding the output, so both finish and the late
    // warning is still written.
    #[test]
    fn a_flush_during_a_monitor_batch_waits_without_holding_the_output() {
        let monitor = Monitor::default();
        monitor
            .queue
            .lock()
            .unwrap()
            .late
            .push(("ended late", 1, Duration::from_secs(11)));
        let output = Mutex::new(Vec::new());
        std::thread::scope(|scope| {
            // The monitor's side: a batch in progress.
            let batch = monitor.writing.lock().unwrap();
            let flushing = scope.spawn(|| monitor.flush_into(|| Shared(output.lock().unwrap())));
            // The flush holds the queue while it waits for the batch to end.
            while monitor.queue.try_lock().is_ok() {
                std::thread::yield_now();
            }
            // The monitor can still take the output to finish its batch.
            let taken = output.try_lock();
            assert!(taken.is_ok(), "the flush holds the output during a batch");
            drop(taken);
            drop(batch);
            flushing.join().unwrap();
        });
        let text = String::from_utf8(output.into_inner().unwrap()).unwrap();
        assert!(
            text.starts_with("warning: ended late is still running after 10 seconds;"),
            "{text}"
        );
    }

    /// An output that takes bytes only while its buffer is not held, as a
    /// stderr pipe takes them only while someone drains it.
    struct Gated(Arc<Mutex<Vec<u8>>>);

    impl Write for Gated {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(bytes);
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    fn written_once_drained(output: &Mutex<Vec<u8>>) -> String {
        for _ in 0..1000 {
            let text = String::from_utf8(output.lock().unwrap().clone()).unwrap();
            // A warning is one line; it may arrive in several writes.
            if text.ends_with('\n') {
                return text;
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        String::new()
    }

    // A finished process whose stderr nobody drains still ends: the flush at
    // exit stops waiting for its own blocked write, and the warning is still
    // written once the output drains.
    #[test]
    fn a_flush_at_exit_does_not_wait_for_a_blocked_output() {
        let monitor = Arc::new(Monitor::default());
        monitor
            .queue
            .lock()
            .unwrap()
            .late
            .push(("ended late", 1, Duration::from_secs(11)));
        let output = Arc::new(Mutex::new(Vec::new()));
        let blocked = output.lock().unwrap();
        let sink = Arc::clone(&output);
        monitor.flush_within(Duration::from_millis(50), move || Gated(sink));
        drop(blocked);
        let text = written_once_drained(&output);
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(
            text.starts_with("warning: ended late is still running after 10 seconds;"),
            "{text}"
        );
    }

    // A monitor batch blocked on that same output does not hold the exit
    // either; the queued warning follows the batch once the output drains.
    #[test]
    fn a_flush_at_exit_does_not_wait_for_a_blocked_monitor_batch() {
        let monitor = Arc::new(Monitor::default());
        let batch = monitor.writing.lock().unwrap();
        {
            let mut queue = monitor.queue.lock().unwrap();
            queue.batch_in_flight = true;
            queue.late.push(("ended late", 1, Duration::from_secs(11)));
        }
        let output = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&output);
        monitor.flush_within(Duration::from_millis(50), move || Gated(sink));
        drop(batch);
        let text = written_once_drained(&output);
        assert_eq!(text.lines().count(), 1, "{text}");
        assert!(
            text.starts_with("warning: ended late is still running after 10 seconds;"),
            "{text}"
        );
    }

    // A process whose stderr another thread holds, as a trace writer blocked
    // on a pipe nobody drains, still exits: with nothing due and no batch
    // being written, the flush never takes the output.
    #[test]
    fn a_flush_with_nothing_due_leaves_a_held_output_alone() {
        let monitor = Monitor::default();
        let output = Mutex::new(Vec::new());
        let held = output.lock().unwrap();
        monitor.flush_into(|| Shared(output.try_lock().expect("the flush took a held output")));
        drop(held);
        assert_eq!(output.into_inner().unwrap(), Vec::<u8>::new());
    }

    // An operation can end after its warning time but before the monitor
    // wakes to scan it: it is still named once, with its real elapsed time,
    // and a later scan never names it again. One that ends in time stays quiet.
    #[test]
    fn an_operation_ending_past_its_warning_time_before_a_scan_is_named_once() {
        let clock = test_clock::Probe::install();
        let late = Warning::start_with_output("late operation", true);
        let quick = Warning::start_with_output("quick operation", true);
        clock.pass_unscanned(Duration::from_secs(9));
        drop(quick);
        clock.pass_unscanned(Duration::from_secs(2));
        drop(late);
        clock.advance(Duration::from_secs(60));
        let named: Vec<_> = clock
            .events()
            .into_iter()
            .map(|(operation, _, elapsed)| (operation, elapsed))
            .collect();
        assert_eq!(named, [("late operation", Duration::from_secs(11))]);
        assert!(!clock.active("late operation"));
    }

    #[test]
    fn quiet_operations_do_not_register_or_deliver_stderr_warnings() {
        let clock = test_clock::Probe::install();
        let warning = Warning::start_with_output("quiet operation", false);
        clock.advance(Duration::from_secs(11));
        assert_eq!(clock.events(), [] as [(&str, u64, Duration); 0]);
        assert!(!clock.active("quiet operation"));
        drop(warning);
        clock.advance(Duration::from_secs(60));
        assert_eq!(clock.events(), [] as [(&str, u64, Duration); 0]);
    }

    #[test]
    fn task_data_query_warns_once_and_keeps_its_sixty_second_deadline() {
        use std::cell::Cell;

        let started = Instant::now();
        let pending = Arc::new(Pending {
            operation: "local Git query",
            attempt: 42,
            started,
            due: started + WARN_AFTER,
            state: AtomicU8::new(ACTIVE),
        });
        let mut queue = Queue {
            pending: vec![Arc::downgrade(&pending)],
            ..Queue::default()
        };
        let elapsed = Cell::new(Duration::ZERO);
        let mut ticks = [9, 10, 30, 59, 60].into_iter();
        let mut observed = Vec::new();
        let mut warnings = Vec::new();
        let error = crate::task_sync::git::wait_until_exit(
            || {
                observed.push(elapsed.get());
                Ok(None::<()>)
            },
            || crate::task_sync::data_worktree::QUERY_BOUND.saturating_sub(elapsed.get()),
            || {
                elapsed.set(Duration::from_secs(ticks.next().expect("bounded wait")));
                warnings.extend(queue.collect(started + elapsed.get()).0);
            },
        )
        .unwrap_err();
        assert!(matches!(
            error.downcast_ref::<crate::task_sync::git::QueryFailure>(),
            Some(crate::task_sync::git::QueryFailure::Deadline)
        ));
        assert_eq!(elapsed.get(), Duration::from_secs(60));
        assert!(observed.contains(&Duration::from_secs(30)));
        assert!(observed.contains(&Duration::from_secs(59)));
        assert_eq!(warnings, [("local Git query", 42, WARN_AFTER)]);
        // The diagnostic did not finish or cancel the operation at ten seconds.
        assert_eq!(pending.state.load(Ordering::Acquire), WARNED);
    }

    #[test]
    fn a_live_operation_warns_once_at_ten_seconds_without_finishing_it() {
        let start = Instant::now();
        let pending = Arc::new(Pending {
            operation: "control request",
            attempt: 1,
            started: start,
            due: start + WARN_AFTER,
            state: AtomicU8::new(ACTIVE),
        });
        let mut queue = Queue {
            pending: vec![Arc::downgrade(&pending)],
            ..Queue::default()
        };
        assert_eq!(queue.collect(start + Duration::from_secs(9)).0, []);
        assert_eq!(
            queue.collect(start + WARN_AFTER).0,
            [("control request", 1, WARN_AFTER)]
        );
        assert_eq!(pending.state.load(Ordering::Acquire), WARNED);
        assert_eq!(queue.collect(start + MINIMUM).0, []);
        // Warning delivery neither finishes nor cancels the owned operation.
        assert_ne!(pending.state.load(Ordering::Acquire), FINISHED);
    }

    #[test]
    fn completion_and_unwind_suppress_a_later_warning() {
        let start = Instant::now();
        let pending = Arc::new(Pending {
            operation: "SQLite writer",
            attempt: 2,
            started: start,
            due: start + WARN_AFTER,
            state: AtomicU8::new(ACTIVE),
        });
        let guard = Warning {
            pending: Some(Arc::clone(&pending)),
        };
        let mut queue = Queue {
            pending: vec![Arc::downgrade(&pending)],
            ..Queue::default()
        };
        let _ = std::panic::catch_unwind(|| {
            let _guard = guard;
            panic!("operation failed");
        });
        assert_eq!(queue.collect(start + MINIMUM).0, []);
        assert_eq!(pending.state.load(Ordering::Acquire), FINISHED);
    }
}
