//! Deterministic transaction delay injection; no elapsed wall-clock sleeps.

use super::*;
use crate::storage::SqliteStore;
use rusqlite::hooks::{AuthAction, AuthContext, Authorization};

thread_local! {
    static CLOCK: Cell<Instant> = Cell::new(Instant::now());
}

fn now() -> Instant {
    CLOCK.with(Cell::get)
}

pub(crate) fn scope() -> ControlRequestScope {
    ControlRequestScope::with_clock(now)
}

pub(crate) fn expire() {
    CLOCK.with(|clock| clock.set(clock.get() + CONTROL_REQUEST_ALLOWANCE));
}

/// The operation result is inserted after the operation's durable writes.
/// Expire there so the wrapper must undo the complete transaction.
pub(crate) fn expire_at_operation_write(store: &SqliteStore) {
    store
        .connection
        .authorizer(Some(|context: AuthContext<'_>| {
            if matches!(
                context.action,
                AuthAction::Insert {
                    table_name: "control_operation_results"
                }
            ) {
                expire();
            }
            Authorization::Allow
        }))
        .unwrap();
}

pub(crate) fn clear_hook(store: &SqliteStore) {
    store
        .connection
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .unwrap();
}
