//! Scoped elapsed time for a real Git query, with one controlled pending poll.

use std::{cell::Cell, cell::RefCell, rc::Rc, time::Duration, time::Instant};

struct Delay {
    command: &'static str,
    elapsed: Duration,
    hits: Rc<Cell<usize>>,
}

thread_local! {
    static DELAY: RefCell<Option<Delay>> = const { RefCell::new(None) };
}

pub(crate) struct PendingQuery {
    previous: Option<Delay>,
    hits: Rc<Cell<usize>>,
}

impl PendingQuery {
    pub(crate) fn at(command: &'static str, elapsed: Duration) -> Self {
        let hits = Rc::new(Cell::new(0));
        let previous = DELAY.with(|delay| {
            delay.replace(Some(Delay {
                command,
                elapsed,
                hits: Rc::clone(&hits),
            }))
        });
        Self { previous, hits }
    }

    pub(crate) fn hits(&self) -> usize {
        self.hits.get()
    }
}

impl Drop for PendingQuery {
    fn drop(&mut self) {
        DELAY.with(|delay| delay.replace(self.previous.take()));
    }
}

pub(super) fn begin(command: &str, start: Instant) -> (Instant, bool) {
    DELAY.with(|delay| {
        let delay = delay.borrow();
        let Some(delay) = delay.as_ref().filter(|delay| delay.command == command) else {
            return (start, false);
        };
        delay.hits.set(delay.hits.get() + 1);
        (start.checked_sub(delay.elapsed).unwrap(), true)
    })
}
