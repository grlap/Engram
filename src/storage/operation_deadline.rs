//! One operation's absolute deadline, shared by every wait it makes.
//!
//! An operation that opens the store and then writes to it would otherwise
//! give each wait its own full timeout: the store's open lock, each
//! statement's busy timeout, the open's retries, a Git read and the writer's
//! `BEGIN IMMEDIATE`. Under concurrent writers those waits add up to several
//! minutes. Inside this scope they all end at one instant, taken when the
//! operation began: the operation limit, at least
//! [`crate::operation_time::MINIMUM`]. Crossing it ends the waiting step with
//! its ordinary busy refusal, before any write transaction begins; nothing is
//! retried and no limit is raised.
//!
//! No write begins after the deadline, even with the writer free, since the
//! operation's answer is due by then; a read that needs no wait still runs.
//! An operation that writes leaves nothing to write once its write commits,
//! so no wait can stand between a recorded result and its answer.
//!
//! The deadline bounds the waits named here; reads on an already open
//! connection keep its own busy timeout, which in write-ahead-log mode they
//! seldom need.

use std::cell::Cell;
use std::marker::PhantomData;
use std::rc::Rc;
use std::time::{Duration, Instant};

thread_local! {
    static DEADLINE: Cell<Option<Instant>> = const { Cell::new(None) };
}

#[cfg(test)]
thread_local! {
    static DISABLED: Cell<bool> = const { Cell::new(false) };
}

/// Runs `act` with operations on this thread taking no deadline of their
/// own, so each wait has only its own ordinary limit, as every wait had
/// before operations shared one: the counterfactual a test compares with.
#[cfg(test)]
pub(crate) fn without_operation_deadlines<T>(act: impl FnOnce() -> T) -> T {
    struct Restore(bool);
    impl Drop for Restore {
        fn drop(&mut self) {
            DISABLED.with(|slot| slot.set(self.0));
        }
    }
    let _restore = Restore(DISABLED.with(|slot| slot.replace(true)));
    act()
}

/// While it lives, the waits this thread makes end at its deadline. A nested
/// scope keeps the earlier deadline, so an inner step never extends the
/// operation that contains it. Dropping it, by return or by unwinding, puts
/// back what the thread had before.
#[must_use = "the deadline ends when the scope is dropped"]
pub(crate) struct OperationDeadlineScope {
    previous: Option<Instant>,
    _thread: PhantomData<Rc<()>>,
}

impl OperationDeadlineScope {
    /// The operation limit from now: [`crate::operation_time::MINIMUM`].
    pub(crate) fn operation() -> Self {
        #[cfg(test)]
        if DISABLED.with(Cell::get) {
            return Self::until(None);
        }
        Self::lasting(crate::operation_time::MINIMUM)
    }

    /// A deadline `budget` from now, for a test that waits the whole budget
    /// out on the real clock without waiting a minute.
    pub(crate) fn lasting(budget: Duration) -> Self {
        Self::until(Instant::now().checked_add(budget))
    }

    fn until(deadline: Option<Instant>) -> Self {
        let previous = DEADLINE.with(Cell::get);
        DEADLINE.with(|slot| slot.set(earliest(previous, deadline)));
        Self {
            previous,
            _thread: PhantomData,
        }
    }
}

impl Drop for OperationDeadlineScope {
    fn drop(&mut self) {
        DEADLINE.with(|slot| slot.set(self.previous));
    }
}

/// When the current operation's waits end; `None` outside an operation.
pub(crate) fn current() -> Option<Instant> {
    DEADLINE.try_with(Cell::get).ok().flatten()
}

/// How long a wait that begins now may last: at most `ordinary`, and never
/// past the current operation's deadline.
pub(crate) fn cap(ordinary: Duration) -> Duration {
    current().map_or(ordinary, |deadline| {
        ordinary.min(deadline.saturating_duration_since(Instant::now()))
    })
}

/// The earlier of two optional deadlines.
pub(crate) fn earliest(first: Option<Instant>, second: Option<Instant>) -> Option<Instant> {
    match (first, second) {
        (Some(first), Some(second)) => Some(first.min(second)),
        (first, second) => first.or(second),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_nested_scope_never_extends_the_operation_and_drop_restores_it() {
        assert_eq!(current(), None);
        let outer = OperationDeadlineScope::lasting(Duration::from_secs(5));
        let outer_deadline = current().expect("an operation deadline");
        {
            let _inner = OperationDeadlineScope::lasting(Duration::from_secs(600));
            assert_eq!(current(), Some(outer_deadline));
        }
        {
            let _inner = OperationDeadlineScope::lasting(Duration::from_millis(1));
            assert!(current().expect("an inner deadline") < outer_deadline);
        }
        assert_eq!(current(), Some(outer_deadline));
        assert!(cap(Duration::from_secs(60)) <= Duration::from_secs(5));
        assert_eq!(cap(Duration::from_millis(3)), Duration::from_millis(3));
        drop(outer);
        assert_eq!(current(), None);
        assert_eq!(cap(Duration::from_secs(60)), Duration::from_secs(60));
    }
}
