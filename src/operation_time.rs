//! Operation timing policy and non-interrupting, noncanonical warnings.
//!
//! A warning owns no cancellation, database connection, or retry. One shared
//! observer serves blocking and async callers, including a blocked native call.

use std::io::Write;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

/// Minimum automatic operation timeout. Longer transfer budgets are retained.
pub const MINIMUM: Duration = Duration::from_secs(60);
/// Warn once while the operation is still running; never cancel it here.
pub const WARN_AFTER: Duration = Duration::from_secs(10);

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

#[derive(Default)]
struct Queue(Vec<Weak<Pending>>);

impl Queue {
    fn collect(&mut self, now: Instant) -> (Vec<(&'static str, u64, Duration)>, Option<Duration>) {
        let mut warnings = Vec::new();
        let mut next: Option<Duration> = None;
        self.0.retain(|weak| {
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
}

impl Monitor {
    fn shared() -> &'static Arc<Self> {
        static MONITOR: OnceLock<Arc<Monitor>> = OnceLock::new();
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
                // Do not hold the queue lock while stderr is being written.
                drop(queue);
                for (operation, attempt, elapsed) in warnings {
                    let _ = writeln!(
                        std::io::stderr().lock(),
                        "warning: {operation} is still running after 10 seconds; attempt={}/{attempt}; elapsed_ms={}",
                        std::process::id(),
                        elapsed.as_millis()
                    );
                }
                queue = self
                    .queue
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
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
    pending: Arc<Pending>,
}

impl Warning {
    pub fn start(operation: &'static str) -> Self {
        static ATTEMPTS: AtomicU64 = AtomicU64::new(0);
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
            return Self { pending };
        }
        let monitor = Monitor::shared();
        monitor
            .queue
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .0
            .push(Arc::downgrade(&pending));
        monitor.changed.notify_one();
        Self { pending }
    }
}

impl Drop for Warning {
    fn drop(&mut self) {
        self.pending.state.store(FINISHED, Ordering::Release);
        #[cfg(test)]
        if test_clock::owns(&self.pending) {
            return;
        }
        Monitor::shared().changed.notify_one();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let mut queue = Queue(vec![Arc::downgrade(&pending)]);
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
        let mut queue = Queue(vec![Arc::downgrade(&pending)]);
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
            pending: Arc::clone(&pending),
        };
        let mut queue = Queue(vec![Arc::downgrade(&pending)]);
        let _ = std::panic::catch_unwind(|| {
            let _guard = guard;
            panic!("operation failed");
        });
        assert_eq!(queue.collect(start + MINIMUM).0, []);
        assert_eq!(pending.state.load(Ordering::Acquire), FINISHED);
    }
}
