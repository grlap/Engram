//! A control frame's cooperative allowance, separate from authority and intent.

use std::cell::Cell;
use std::marker::PhantomData;
use std::rc::Rc;
use std::time::{Duration, Instant};

use crate::storage::StoreError;

pub(crate) const CONTROL_REQUEST_ALLOWANCE: Duration = Duration::from_secs(15);

#[derive(Clone, Copy)]
pub(crate) struct Allowance {
    started: Instant,
    now: fn() -> Instant,
    duration: Duration,
}

impl Allowance {
    pub(crate) fn new(duration: Duration) -> Self {
        Self::with_clock(duration, Instant::now)
    }

    pub(crate) fn with_clock(duration: Duration, now: fn() -> Instant) -> Self {
        Self {
            started: now(),
            now,
            duration,
        }
    }

    pub(crate) fn elapsed(self) -> Duration {
        (self.now)().saturating_duration_since(self.started)
    }

    pub(crate) fn remaining(self) -> Duration {
        self.duration.saturating_sub(self.elapsed())
    }
}

#[derive(Clone, Copy)]
pub(crate) struct ControlRequestBudget {
    allowance: Allowance,
}

impl ControlRequestBudget {
    fn new() -> Self {
        let now = current().map_or(Instant::now as fn() -> Instant, |budget| {
            budget.allowance.now
        });
        Self {
            allowance: Allowance::with_clock(CONTROL_REQUEST_ALLOWANCE, now),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_clock(now: fn() -> Instant) -> Self {
        Self {
            allowance: Allowance::with_clock(CONTROL_REQUEST_ALLOWANCE, now),
        }
    }

    pub(super) fn remaining(self) -> Duration {
        self.allowance.remaining()
    }

    pub(super) fn check(self) -> Result<(), StoreError> {
        if self.remaining().is_zero() {
            Err(StoreError::ControlRequestExpired)
        } else {
            Ok(())
        }
    }
}

thread_local! {
    static CURRENT: Cell<Option<ControlRequestBudget>> = const { Cell::new(None) };
}

pub(super) fn current() -> Option<ControlRequestBudget> {
    CURRENT.with(Cell::get)
}

/// This synchronous scope restores the previous frame on return or unwind.
/// It carries no work claim, timestamp, retry key, or note admission policy.
pub(crate) struct ControlRequestScope {
    previous: Option<ControlRequestBudget>,
    _thread: PhantomData<Rc<()>>,
}

impl ControlRequestScope {
    pub(crate) fn frame() -> Self {
        Self::enter(ControlRequestBudget::new())
    }

    pub(crate) fn ensure() -> Result<Self, StoreError> {
        let budget = current().unwrap_or_else(ControlRequestBudget::new);
        budget.check()?;
        Ok(Self::enter(budget))
    }

    fn enter(budget: ControlRequestBudget) -> Self {
        Self {
            previous: CURRENT.with(|slot| slot.replace(Some(budget))),
            _thread: PhantomData,
        }
    }

    #[cfg(test)]
    pub(crate) fn with_clock(now: fn() -> Instant) -> Self {
        Self::enter(ControlRequestBudget::with_clock(now))
    }
}

impl Drop for ControlRequestScope {
    fn drop(&mut self) {
        CURRENT.with(|slot| slot.set(self.previous));
    }
}

#[cfg(test)]
pub(crate) mod test_clock;

#[cfg(test)]
impl crate::storage::SqliteStore {
    pub(crate) fn test_control_commit_clock(&self, advance: fn()) {
        self.connection
            .commit_hook(Some(move || {
                advance();
                false
            }))
            .unwrap();
    }
}
