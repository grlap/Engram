//! Test support that commits from a second connection in the middle of a
//! read, between two of its statements. It shows that a read which compares
//! rows from several statements takes them from one commit, instead of
//! reporting another connection's commit as corruption.

use super::SqliteStore;

/// A second connection's write, committed from inside the reading
/// connection's trace hook just as the reader starts the first statement
/// whose SQL contains every fragment of `before`.
struct ConcurrentWrite {
    before: &'static [&'static str],
    write: Box<dyn FnOnce() -> Result<(), String>>,
}

thread_local! {
    static CONCURRENT_WRITE: std::cell::RefCell<Option<ConcurrentWrite>> =
        const { std::cell::RefCell::new(None) };
    /// What the hook's write returned, with its error; none until it fires.
    static CONCURRENT_WRITE_OUTCOME: std::cell::RefCell<Option<Result<(), String>>> =
        const { std::cell::RefCell::new(None) };
}

/// Runs the waiting write, once, when the reader starts its statement.
fn commit_before_the_statement(event: &rusqlite::trace::TraceEvent<'_>) {
    let rusqlite::trace::TraceEvent::Stmt(_, sql) = event else {
        return;
    };
    let Some(write) = CONCURRENT_WRITE.with(|slot| {
        let mut slot = slot.borrow_mut();
        let starts = slot
            .as_ref()
            .is_some_and(|write| write.before.iter().all(|fragment| sql.contains(fragment)));
        if starts { slot.take() } else { None }
    }) else {
        return;
    };
    let outcome = (write.write)();
    CONCURRENT_WRITE_OUTCOME.with(|slot| *slot.borrow_mut() = Some(outcome));
}

/// Runs `read` on `reader`, in autocommit, while `write` commits from
/// another connection just before the reader starts the statement `before`
/// names. Panics with its cause when the write fails or never runs.
pub(crate) fn read_across_a_concurrent_commit<T>(
    reader: &SqliteStore,
    read: impl FnOnce(&SqliteStore) -> T,
    before: &'static [&'static str],
    write: impl FnOnce() -> Result<(), String> + 'static,
) -> T {
    assert!(reader.connection.is_autocommit());
    CONCURRENT_WRITE.with(|slot| {
        *slot.borrow_mut() = Some(ConcurrentWrite {
            before,
            write: Box::new(write),
        });
    });
    CONCURRENT_WRITE_OUTCOME.with(|slot| *slot.borrow_mut() = None);
    reader.connection.trace_v2(
        rusqlite::trace::TraceEventCodes::SQLITE_TRACE_STMT,
        Some(|event| commit_before_the_statement(&event)),
    );
    let read = read(reader);
    reader
        .connection
        .trace_v2(rusqlite::trace::TraceEventCodes::empty(), None);
    CONCURRENT_WRITE.with(|slot| slot.borrow_mut().take());
    match CONCURRENT_WRITE_OUTCOME.with(|slot| slot.borrow_mut().take()) {
        Some(Ok(())) => {}
        Some(Err(error)) => panic!("the concurrent write failed: {error}"),
        None => panic!("the reader never started {before:?}, so nothing committed mid-read"),
    }
    read
}

/// The statement every canonical feed-head lookup runs: the latest event on
/// one feed that carries a given snapshot (run, claim or root execution).
pub(crate) const FEED_SNAPSHOT_HEAD: &[&str] =
    &["json_type(object.canonical_json, ?3) IS NOT NULL"];

/// The statement that reads one item's latest canonical event on the
/// project feed.
pub(crate) const ITEM_FEED_HEAD: &[&str] = &[
    "entry.feed_kind = 'project'",
    "ORDER BY entry.position DESC LIMIT 1",
];
