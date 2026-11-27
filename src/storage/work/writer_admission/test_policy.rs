//! Test-only coordination immediately before a real SQLite acquisition.
use std::cell::RefCell;
use std::time::{Duration, Instant};

struct Policy {
    budget: Option<Duration>,
    before: Box<dyn FnMut()>,
    /// Run on the committing thread right after a work mutation commits,
    /// before the caller goes on.
    after_commit: Option<Box<dyn FnMut()>>,
    /// Run on the committing thread just before a work mutation settles.
    before_commit: Option<Box<dyn FnMut()>>,
    last_allowance: Option<Duration>,
    /// When each `BEGIN IMMEDIATE` under this policy began waiting and when
    /// it returned, admitted or refused, in order.
    acquisitions: Vec<(Instant, Instant)>,
    /// A test that sets its own budget also owns that window's time, so how
    /// fast the host runs never decides how much of the window is left. Only
    /// `advance_writer_admission_window` moves it. Without a budget the window
    /// is the production one, on the real clock.
    window_now: Option<Instant>,
}

thread_local! {
    static POLICY: RefCell<Option<Policy>> = const { RefCell::new(None) };
}

struct Restore(Option<Policy>);

impl Drop for Restore {
    fn drop(&mut self) {
        POLICY.with(|slot| *slot.borrow_mut() = self.0.take());
    }
}

pub(super) fn budget() -> Option<Duration> {
    POLICY.with(|slot| slot.borrow().as_ref().and_then(|policy| policy.budget))
}

pub(super) fn now() -> Option<Instant> {
    POLICY.with(|slot| slot.borrow().as_ref().and_then(|policy| policy.window_now))
}

pub(crate) fn advance_writer_admission_window(by: Duration) {
    POLICY.with(|slot| {
        let mut slot = slot.borrow_mut();
        let now = slot
            .as_mut()
            .and_then(|policy| policy.window_now.as_mut())
            .expect("the test set its own admission budget");
        *now += by;
    });
}

pub(super) fn before_acquisition() {
    // A hook may use a second store's ordinary typed mutation. That mutation
    // must not recursively run this connection's admission hook.
    let mut policy = POLICY.with(|slot| slot.borrow_mut().take());
    if let Some(policy) = policy.as_mut() {
        (policy.before)();
    }
    POLICY.with(|slot| *slot.borrow_mut() = policy);
}

pub(super) fn before_commit() {
    let mut policy = POLICY.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = policy
        .as_mut()
        .and_then(|policy| policy.before_commit.as_mut())
    {
        hook();
    }
    POLICY.with(|slot| *slot.borrow_mut() = policy);
}

pub(super) fn after_commit() {
    // As before an acquisition: the hook may write through another store.
    let mut policy = POLICY.with(|slot| slot.borrow_mut().take());
    if let Some(hook) = policy
        .as_mut()
        .and_then(|policy| policy.after_commit.as_mut())
    {
        hook();
    }
    POLICY.with(|slot| *slot.borrow_mut() = policy);
}

pub(super) fn observe_allowance(allowance: Duration) {
    POLICY.with(|slot| {
        if let Some(policy) = slot.borrow_mut().as_mut() {
            policy.last_allowance = Some(allowance);
        }
    });
}

pub(super) fn observe_acquisition(began: Instant, returned: Instant) {
    POLICY.with(|slot| {
        if let Some(policy) = slot.borrow_mut().as_mut() {
            policy.acquisitions.push((began, returned));
        }
    });
}

/// When each writer acquisition under the current policy began waiting and
/// when it returned.
pub(crate) fn writer_admission_acquisitions() -> Vec<(Instant, Instant)> {
    POLICY.with(|slot| {
        slot.borrow()
            .as_ref()
            .map_or_else(Vec::new, |policy| policy.acquisitions.clone())
    })
}

pub(crate) fn last_writer_admission_allowance() -> Option<Duration> {
    POLICY.with(|slot| {
        slot.borrow()
            .as_ref()
            .and_then(|policy| policy.last_allowance)
    })
}

pub(crate) fn with_writer_admission_test_policy<T>(
    budget: Option<Duration>,
    before: impl FnMut() + 'static,
    act: impl FnOnce() -> T,
) -> T {
    install(budget, Box::new(before), None, None, act)
}

/// [`with_writer_admission_test_policy`], with `after_commit` also run on
/// this thread right after each work mutation commits.
pub(crate) fn with_writer_commit_hook<T>(
    before: impl FnMut() + 'static,
    after_commit: impl FnMut() + 'static,
    act: impl FnOnce() -> T,
) -> T {
    install(
        None,
        Box::new(before),
        None,
        Some(Box::new(after_commit)),
        act,
    )
}

/// [`with_writer_admission_test_policy`], with `before_commit` also run on
/// this thread just before each work mutation settles.
pub(crate) fn with_writer_settle_hook<T>(
    before_commit: impl FnMut() + 'static,
    act: impl FnOnce() -> T,
) -> T {
    install(
        None,
        Box::new(|| {}),
        Some(Box::new(before_commit)),
        None,
        act,
    )
}

fn install<T>(
    budget: Option<Duration>,
    before: Box<dyn FnMut()>,
    before_commit: Option<Box<dyn FnMut()>>,
    after_commit: Option<Box<dyn FnMut()>>,
    act: impl FnOnce() -> T,
) -> T {
    let _restore = Restore(POLICY.with(|slot| {
        slot.borrow_mut().replace(Policy {
            budget,
            before,
            before_commit,
            after_commit,
            last_allowance: None,
            acquisitions: Vec::new(),
            window_now: budget.map(|_| Instant::now()),
        })
    }));
    act()
}
