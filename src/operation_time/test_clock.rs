//! Thread-local clock and isolated production warning queue for real operations.

use super::{FINISHED, Pending, Queue, WARN_AFTER};
use std::{
    cell::{Cell, RefCell},
    rc::Rc,
    sync::{Arc, Weak, atomic::Ordering},
    time::{Duration, Instant},
};

type Event = (&'static str, u64, Duration);

struct State {
    start: Instant,
    elapsed: Cell<Duration>,
    queue: RefCell<Queue>,
    registered: RefCell<Vec<Weak<Pending>>>,
    events: RefCell<Vec<Event>>,
}

thread_local! {
    static CLOCK: RefCell<Option<Rc<State>>> = const { RefCell::new(None) };
}

pub(crate) struct Probe {
    state: Rc<State>,
    previous: Option<Rc<State>>,
}

impl Probe {
    pub(crate) fn install() -> Self {
        let state = Rc::new(State {
            start: Instant::now(),
            elapsed: Cell::new(Duration::ZERO),
            queue: RefCell::new(Queue::default()),
            registered: RefCell::new(Vec::new()),
            events: RefCell::new(Vec::new()),
        });
        let previous = CLOCK.with(|clock| clock.replace(Some(Rc::clone(&state))));
        Self { state, previous }
    }

    pub(crate) fn advance(&self, elapsed: Duration) {
        let previous = self.state.elapsed.get();
        let next = previous + elapsed;
        // Visit the operation's warning boundary even when a query spans it.
        if previous < WARN_AFTER && next >= WARN_AFTER {
            self.state.elapsed.set(WARN_AFTER);
            self.collect();
        }
        self.state.elapsed.set(next);
        self.collect();
    }

    fn collect(&self) {
        let now = self.state.start + self.state.elapsed.get();
        let events = self.state.queue.borrow_mut().collect(now).0;
        self.state.events.borrow_mut().extend(events);
    }

    pub(crate) fn elapsed(&self) -> Duration {
        self.state.elapsed.get()
    }

    pub(crate) fn events(&self) -> Vec<Event> {
        self.state.events.borrow().clone()
    }

    pub(crate) fn active(&self, operation: &str) -> bool {
        self.state.registered.borrow().iter().any(|weak| {
            weak.upgrade().is_some_and(|pending| {
                pending.operation == operation && pending.state.load(Ordering::Acquire) != FINISHED
            })
        })
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        CLOCK.with(|clock| clock.replace(self.previous.take()));
    }
}

pub(super) fn now() -> Option<Instant> {
    CLOCK.with(|clock| {
        clock
            .borrow()
            .as_ref()
            .map(|clock| clock.start + clock.elapsed.get())
    })
}

pub(super) fn register(pending: &Arc<Pending>) -> bool {
    CLOCK.with(|clock| {
        let clock = clock.borrow();
        let Some(clock) = clock.as_ref() else {
            return false;
        };
        clock.queue.borrow_mut().0.push(Arc::downgrade(pending));
        clock.registered.borrow_mut().push(Arc::downgrade(pending));
        true
    })
}

pub(super) fn owns(pending: &Arc<Pending>) -> bool {
    CLOCK.with(|clock| {
        clock.borrow().as_ref().is_some_and(|clock| {
            clock
                .registered
                .borrow()
                .iter()
                .any(|weak| weak.ptr_eq(&Arc::downgrade(pending)))
        })
    })
}
