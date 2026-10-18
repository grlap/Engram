//! How long an open's connection waits for a lock another connection holds.
//!
//! Ordinary opens give SQLite a busy timeout. SQLite counts that timeout as
//! the sum of the sleeps it asks for, not as the time that passes, and a
//! sleep can last longer than asked, so a wait can overrun its timeout. A
//! copy for a backup has a deadline to keep, so its connections wait through
//! a busy handler that reads the clock instead: each wait ends at the copy's
//! deadline, and at most [`LOCK_WAIT`] after it began.

use std::cell::Cell;
use std::time::{Duration, Instant};

/// The longest one lock wait lasts on a copy's connection, as long as an
/// ordinary open waits.
pub(super) const LOCK_WAIT: Duration = Duration::from_secs(5);

/// The longest a copy's connection sleeps between two tries for a lock.
const POLL: Duration = Duration::from_millis(2);

/// The pause between two tries to switch an ordinary open's journal mode.
const JOURNAL_RETRY_PAUSE: Duration = Duration::from_millis(5);

/// How an open's connection waits for a lock.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum LockWait {
    /// SQLite's own busy timeout, for ordinary opens.
    Timeout(Duration),
    /// The clock-read busy handler of a copy, bounded by the deadline of the
    /// [`CopyWaitScope`] the connection is used in.
    CopyDeadline,
}

impl LockWait {
    /// Installs the wait on `connection`, replacing any busy handler or
    /// timeout it had.
    pub(super) fn install(self, connection: &rusqlite::Connection) -> rusqlite::Result<()> {
        match self {
            Self::Timeout(timeout) => connection.busy_timeout(timeout),
            Self::CopyDeadline => connection.busy_handler(Some(copy_busy)),
        }
    }

    /// When a retry that SQLite does not send through the busy handler must
    /// stop, for a retry loop that begins `now`. A copy without a scope has
    /// no limit, and so no retry.
    pub(super) fn retry_limit(self, now: Instant) -> Option<Instant> {
        match self {
            Self::Timeout(timeout) => Some(now + timeout),
            Self::CopyDeadline => current().and_then(|wait| limit(now, wait.deadline)),
        }
    }

    /// The pause before the next such retry at `now`, or `None` once `limit`
    /// is reached. A copy's pause never sleeps past its limit.
    pub(super) fn retry_pause(self, limit: Option<Instant>, now: Instant) -> Option<Duration> {
        let limit = limit?;
        match self {
            Self::Timeout(_) => (now < limit).then_some(JOURNAL_RETRY_PAUSE),
            Self::CopyDeadline => {
                let left = limit.saturating_duration_since(now);
                (!left.is_zero()).then(|| left.min(JOURNAL_RETRY_PAUSE))
            }
        }
    }
}

/// The copy whose connections this thread is using: its deadline, and when
/// the current lock wait began.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct CopyWait {
    /// `None` when the deadline lies beyond what the clock can represent.
    deadline: Option<Instant>,
    episode_start: Instant,
}

thread_local! {
    static COPY_WAIT: Cell<Option<CopyWait>> = const { Cell::new(None) };
}

fn current() -> Option<CopyWait> {
    COPY_WAIT.try_with(Cell::get).ok().flatten()
}

fn replace(wait: Option<CopyWait>) -> Option<CopyWait> {
    COPY_WAIT.try_with(|cell| cell.replace(wait)).ok().flatten()
}

/// While it lives, the copy connections this thread uses wait for a lock
/// until `deadline`. Dropping it, by return or by unwinding, puts back what
/// the thread had before. Declare it before the connections it covers, so
/// that it outlives them: a copy connection used without a scope waits for
/// no lock at all.
#[must_use = "a copy connection waits for no lock once its scope is dropped"]
pub(super) struct CopyWaitScope {
    previous: Option<CopyWait>,
}

impl CopyWaitScope {
    pub(super) fn enter(deadline: Option<Instant>) -> Self {
        let previous = replace(Some(CopyWait {
            deadline,
            episode_start: Instant::now(),
        }));
        Self { previous }
    }
}

impl Drop for CopyWaitScope {
    fn drop(&mut self) {
        replace(self.previous);
    }
}

/// What a copy's busy handler does next.
#[derive(Debug, Eq, PartialEq)]
enum Decision {
    /// Sleep this long, then try the lock again.
    Wait(Duration),
    /// Give up the wait; SQLite reports the store busy.
    Refuse,
}

/// When a lock wait that began at `episode_start` ends: at the copy's
/// deadline, and at most [`LOCK_WAIT`] after it began.
fn limit(episode_start: Instant, deadline: Option<Instant>) -> Option<Instant> {
    let cap = episode_start.checked_add(LOCK_WAIT);
    match (cap, deadline) {
        (Some(cap), Some(deadline)) => Some(cap.min(deadline)),
        (cap, deadline) => cap.or(deadline),
    }
}

fn decide(now: Instant, episode_start: Instant, deadline: Option<Instant>) -> Decision {
    let Some(end) = limit(episode_start, deadline) else {
        return Decision::Refuse;
    };
    let left = end.saturating_duration_since(now);
    if left.is_zero() {
        Decision::Refuse
    } else {
        Decision::Wait(left.min(POLL))
    }
}

/// The busy handler of a copy's connections. SQLite passes how often it has
/// called it since that count last restarted, which happens for each
/// statement step rather than for each lock; a count of 0 starts a new wait.
/// A count that was not restarted can only shorten a wait, never lengthen
/// it. It reads the clock again after sleeping, so it never asks SQLite to try
/// the lock once the wait has ended. Without a scope it refuses at once.
fn copy_busy(count: i32) -> bool {
    let now = Instant::now();
    #[cfg(test)]
    super::super::copy_trace::busy_enter(count, now);
    let Some(mut wait) = current() else {
        #[cfg(test)]
        super::super::copy_trace::busy_return(Instant::now(), Some("no copy scope"));
        return false;
    };
    if count == 0 {
        wait.episode_start = now;
        replace(Some(wait));
    }
    let retry = match decide(now, wait.episode_start, wait.deadline) {
        Decision::Refuse => false,
        Decision::Wait(pause) => {
            #[cfg(test)]
            let sleep_began = Instant::now();
            std::thread::sleep(pause);
            #[cfg(test)]
            super::super::copy_trace::busy_sleep(sleep_began.elapsed());
            decide(Instant::now(), wait.episode_start, wait.deadline) != Decision::Refuse
        }
    };
    #[cfg(test)]
    super::super::copy_trace::busy_return(
        Instant::now(),
        (!retry).then_some(if wait.deadline.is_some_and(|end| Instant::now() >= end) {
            "copy deadline"
        } else {
            "lock wait limit"
        }),
    );
    retry
}

/// Whether this thread is inside a copy's scope.
#[cfg(test)]
pub(super) fn in_copy_scope() -> bool {
    current().is_some()
}

#[cfg(test)]
mod tests;
