//! Operation-scoped SQL work in library tests, across retained and new connections.
//!
//! This module is compiled only by the test-only parent. The connection callback
//! remains installed between samples; outside a scope it records nothing. SQL
//! text and bound values are never retained or reported.
//!
//! Attribution covers this executing thread and the `SqliteStore`/`open_sqlite_file`
//! funnels used by the measured operations. An ad-hoc `Connection::open` needs
//! explicit installation; adding another opener requires a coverage test.
//! Within a scope, extend the dispatcher instead of replacing `trace_v2`: a
//! replacement can silently hide a connection's work. Callback consumers must
//! never execute SQL or re-enter the collector's `RefCell`.
use std::borrow::Cow;
use std::cell::RefCell;
use std::collections::BTreeMap;

use rusqlite::trace::{StmtRef, TraceEvent, TraceEventCodes};
use rusqlite::{Connection, StatementStatus};

#[derive(Clone, Copy, Debug, Default, Eq, PartialEq, serde::Serialize)]
pub(crate) struct Counters {
    pub(crate) statements: u64,
    pub(crate) fullscan_steps: u64,
    pub(crate) sorts: u64,
    pub(crate) vm_steps: u64,
}

impl Counters {
    fn add(&mut self, other: Self) {
        self.statements += other.statements;
        self.fullscan_steps += other.fullscan_steps;
        self.sorts += other.sorts;
        self.vm_steps += other.vm_steps;
    }
}

#[derive(Clone, Debug, Default, serde::Serialize)]
pub(crate) struct Metrics {
    pub(crate) total: Counters,
    pub(crate) by_kind: BTreeMap<&'static str, Counters>,
}

#[derive(Clone, Copy)]
struct Status {
    fullscan_steps: i32,
    sorts: i32,
    vm_steps: i32,
}

impl Status {
    fn read(statement: &StmtRef<'_>) -> Self {
        Self {
            fullscan_steps: statement.get_status(StatementStatus::FullscanStep),
            sorts: statement.get_status(StatementStatus::Sort),
            vm_steps: statement.get_status(StatementStatus::VmStep),
        }
    }

    fn since(self, before: Self) -> Option<Counters> {
        // A reset or overflow invalidates the observation rather than silently
        // clamping it. SQLite preserves these counters across cached executions.
        if [
            self.fullscan_steps,
            self.sorts,
            self.vm_steps,
            before.fullscan_steps,
            before.sorts,
            before.vm_steps,
        ]
        .iter()
        .any(|value| *value < 0)
        {
            return None;
        }
        Some(Counters {
            statements: 1,
            fullscan_steps: u64::try_from(self.fullscan_steps.checked_sub(before.fullscan_steps)?)
                .ok()?,
            sorts: u64::try_from(self.sorts.checked_sub(before.sorts)?).ok()?,
            vm_steps: u64::try_from(self.vm_steps.checked_sub(before.vm_steps)?).ok()?,
        })
    }
}

#[derive(Default)]
struct Capture {
    metrics: Metrics,
    pending: BTreeMap<usize, (Status, &'static str)>,
    invalid: bool,
}

thread_local! {
    static CURRENT: RefCell<Option<Capture>> = const { RefCell::new(None) };
}

/// A synchronous executing-thread sample. Drop clears attribution on unwind.
/// The marker keeps the guard on the thread whose collector it owns.
pub(crate) struct Scope(std::marker::PhantomData<std::rc::Rc<()>>);

pub(crate) fn start() -> Scope {
    CURRENT.with_borrow_mut(|current| {
        assert!(current.is_none(), "SQLite cost scopes must not nest");
        *current = Some(Capture::default());
    });
    Scope(std::marker::PhantomData)
}

/// Run the same independent scan/sort control before any scale numbers.
/// Its fixed unindexed 128-row query must yield 127 steps and one sort per
/// execution, including reuse of a cached statement from an earlier scope.
pub(crate) fn assert_calibrated() {
    tests::calibrate();
}

impl Scope {
    pub(crate) fn finish(self) -> Metrics {
        let captured = CURRENT
            .with_borrow_mut(Option::take)
            .expect("active SQLite cost scope");
        assert!(
            !captured.invalid,
            "SQLite cost observation lost statement attribution or counters"
        );
        assert!(
            captured.pending.is_empty(),
            "SQLite statement outlived its cost scope"
        );
        drop(self);
        captured.metrics
    }
}

impl Drop for Scope {
    fn drop(&mut self) {
        CURRENT.with_borrow_mut(|current| *current = None);
    }
}

/// Observe costs without enabling MCP or control phase recording. Their
/// production registration paths replace this callback with dispatchers
/// that also forward costs in test builds.
pub(crate) fn install(connection: &Connection) {
    connection.trace_v2(
        TraceEventCodes::SQLITE_TRACE_STMT | TraceEventCodes::SQLITE_TRACE_PROFILE,
        Some(collect),
    );
}

#[expect(
    clippy::needless_pass_by_value,
    reason = "rusqlite requires a by-value trace callback; the collector borrows its event"
)]
fn collect(event: TraceEvent<'_>) {
    observe(&event);
}

fn kind(sql: &str) -> &'static str {
    match sql
        .split_ascii_whitespace()
        .next()
        .unwrap_or("")
        .trim_end_matches(';')
        .to_ascii_uppercase()
        .as_str()
    {
        "SELECT" => "select",
        "WITH" => "with",
        "INSERT" => "insert",
        "UPDATE" => "update",
        "DELETE" => "delete",
        "BEGIN" => "begin",
        "COMMIT" => "commit",
        "ROLLBACK" => "rollback",
        "PRAGMA" => "pragma",
        _ => "other",
    }
}

pub(crate) fn observe(event: &TraceEvent<'_>) {
    CURRENT.with_borrow_mut(|current| {
        let Some(capture) = current.as_mut() else {
            return;
        };
        let (statement, starting) = match event {
            TraceEvent::Stmt(statement, _) => (statement, true),
            TraceEvent::Profile(statement, _) => (statement, false),
            _ => return,
        };
        // rusqlite borrows sqlite3_sql's statement-owned UTF-8 buffer. Its
        // address distinguishes simultaneous identical SQL on different
        // connections; no pointer is dereferenced or kept past PROFILE. An
        // owned lossy conversion cannot supply that identity, so refuse it.
        let sql = statement.sql();
        let Cow::Borrowed(sql) = sql else {
            capture.invalid = true;
            return;
        };
        // NULL sqlite3_sql becomes a shared empty Cow in rusqlite, not a
        // statement-owned buffer whose address can serve as an identity.
        if sql.is_empty() {
            capture.invalid = true;
            return;
        }
        let identity = sql.as_ptr() as usize;
        if starting {
            // Trigger subprograms emit more STMT events with the same parent
            // statement. Preserve its original baseline and count it once at
            // PROFILE; distinct nested statements retain independent entries.
            capture
                .pending
                .entry(identity)
                .or_insert_with(|| (Status::read(statement), kind(sql)));
        } else if let Some((before, category)) = capture.pending.remove(&identity) {
            if let Some(delta) = Status::read(statement).since(before) {
                capture.metrics.total.add(delta);
                capture
                    .metrics
                    .by_kind
                    .entry(category)
                    .or_default()
                    .add(delta);
            } else {
                capture.invalid = true;
            }
        } else {
            capture.invalid = true;
        }
    });
}

#[cfg(test)]
mod tests;
