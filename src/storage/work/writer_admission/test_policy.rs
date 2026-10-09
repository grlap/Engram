//! Test-only coordination immediately before a real SQLite acquisition.
use std::cell::RefCell;
use std::time::Duration;

struct Policy {
    budget: Option<Duration>,
    before: Box<dyn FnMut()>,
    last_allowance: Option<Duration>,
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

pub(super) fn before_acquisition() {
    // A hook may use a second store's ordinary typed mutation. That mutation
    // must not recursively run this connection's admission hook.
    let mut policy = POLICY.with(|slot| slot.borrow_mut().take());
    if let Some(policy) = policy.as_mut() {
        (policy.before)();
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
    let _restore = Restore(POLICY.with(|slot| {
        slot.borrow_mut().replace(Policy {
            budget,
            before: Box::new(before),
            last_allowance: None,
        })
    }));
    act()
}
