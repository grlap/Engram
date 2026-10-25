//! Monotonic timing for diagnostic control frames, independent of admission.

use std::{
    cell::Cell,
    marker::PhantomData,
    rc::Rc,
    time::{Duration, Instant},
};

/// Aggregate observed intervals, not pure SQLite lock-wait or SQL time.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub(crate) struct ControlWriteTiming {
    pub(crate) attempted: u64,
    pub(crate) acquired: u64,
    pub(crate) wait: Duration,
    pub(crate) hold: Duration,
}

thread_local! {
    static CURRENT: Cell<Option<ControlWriteTiming>> = const { Cell::new(None) };
}

/// Only the synchronous diagnostic frame installs this scope. Nested request
/// allowance scopes are independent; unwind restores the previous observer.
/// A snapshot reads the innermost diagnostic scope; its counts do not propagate
/// to an outer scope when the inner scope closes.
pub(crate) struct ControlDiagnosticScope {
    previous: Option<ControlWriteTiming>,
    _thread: PhantomData<Rc<()>>,
}

impl ControlDiagnosticScope {
    pub(crate) fn enter() -> Self {
        Self {
            previous: CURRENT.with(|slot| slot.replace(Some(ControlWriteTiming::default()))),
            _thread: PhantomData,
        }
    }

    #[expect(
        clippy::unused_self,
        reason = "sampling borrows the thread-bound RAII guard to keep the observation scope alive"
    )]
    pub(crate) fn snapshot(&self) -> ControlWriteTiming {
        CURRENT.with(Cell::get).unwrap_or_default()
    }
}

impl Drop for ControlDiagnosticScope {
    fn drop(&mut self) {
        CURRENT.with(|slot| slot.set(self.previous));
    }
}

/// One observer of the neutral transaction guard. It has no independent Drop:
/// the owning guard settles it after SQLite cleanup, even during unwinding.
#[derive(Debug)]
pub(super) struct Observation {
    started: Instant,
    acquired: Option<Instant>,
}

impl Observation {
    pub(super) fn start() -> Option<Self> {
        CURRENT.with(Cell::get).map(|_| Self {
            started: now(),
            acquired: None,
        })
    }

    pub(super) fn acquired(&mut self) {
        self.acquired = Some(now());
    }
    pub(super) fn finish(self) {
        let ended = now();
        CURRENT.with(|slot| {
            if let Some(mut timing) = slot.get() {
                timing.attempted = timing.attempted.saturating_add(1);
                timing.wait = timing.wait.saturating_add(
                    self.acquired
                        .unwrap_or(ended)
                        .saturating_duration_since(self.started),
                );
                if let Some(acquired) = self.acquired {
                    timing.acquired = timing.acquired.saturating_add(1);
                    timing.hold = timing
                        .hold
                        .saturating_add(ended.saturating_duration_since(acquired));
                }
                slot.set(Some(timing));
            }
        });
    }
}

pub(crate) fn now() -> Instant {
    #[cfg(test)]
    if let Some(now) = test_clock::current() {
        return now;
    }
    Instant::now()
}

#[cfg(test)]
mod tests;

#[cfg(test)]
pub(crate) mod test_clock {
    use std::{
        cell::Cell,
        marker::PhantomData,
        rc::Rc,
        time::{Duration, Instant},
    };

    thread_local! {
        static NOW: Cell<Option<Instant>> = const { Cell::new(None) };
    }

    pub(crate) struct Clock {
        previous: Option<Instant>,
        _thread: PhantomData<Rc<()>>,
    }

    pub(crate) fn start() -> Clock {
        Clock {
            previous: NOW.with(|now| now.replace(Some(Instant::now()))),
            _thread: PhantomData,
        }
    }

    pub(crate) fn advance(duration: Duration) {
        NOW.with(|now| now.set(Some(now.get().expect("diagnostic test clock") + duration)));
    }

    pub(super) fn current() -> Option<Instant> {
        NOW.with(Cell::get)
    }

    impl Drop for Clock {
        fn drop(&mut self) {
            NOW.with(|now| now.set(self.previous));
        }
    }

    impl crate::storage::SqliteStore {
        pub(crate) fn test_control_diagnostic_delays(&self, begin: Duration, commit: Duration) {
            use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};
            self.connection
                .authorizer(Some(move |context: AuthContext<'_>| {
                    if let AuthAction::Transaction {
                        operation: TransactionOperation::Begin,
                    } = context.action
                    {
                        advance(begin);
                    }
                    Authorization::Allow
                }))
                .unwrap();
            self.connection
                .commit_hook(Some(move || {
                    advance(commit);
                    false
                }))
                .unwrap();
        }
    }
}
