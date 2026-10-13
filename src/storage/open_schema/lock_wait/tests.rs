use super::*;

fn at(start: Instant, offset: Duration) -> Instant {
    start.checked_add(offset).expect("a fixed instant")
}

const fn ms(millis: u64) -> Duration {
    Duration::from_millis(millis)
}

const fn ns(nanos: u64) -> Duration {
    Duration::from_nanos(nanos)
}

#[test]
fn a_copy_wait_ends_at_the_deadline_or_five_seconds_after_it_began() {
    let start = Instant::now();
    let cases = [
        // A remainder under a millisecond still waits for it.
        (ms(0), Some(ns(1)), Decision::Wait(ns(1))),
        (ms(0), Some(ns(999_999)), Decision::Wait(ns(999_999))),
        // A longer remainder waits a short poll at a time.
        (ms(0), Some(ms(1500)), Decision::Wait(POLL)),
        (ms(1499), Some(ms(1500)), Decision::Wait(ms(1))),
        // The deadline ends the wait, however long the sleeps lasted.
        (ms(1500), Some(ms(1500)), Decision::Refuse),
        (ms(1712), Some(ms(1500)), Decision::Refuse),
        (ms(0), Some(ms(0)), Decision::Refuse),
        // Without a deadline, or with a later one, a wait lasts five seconds.
        (ms(4999), None, Decision::Wait(ms(1))),
        (LOCK_WAIT, None, Decision::Refuse),
        (LOCK_WAIT, Some(ms(6000)), Decision::Refuse),
        (ms(4000), Some(ms(6000)), Decision::Wait(POLL)),
    ];
    for (elapsed, deadline, expected) in cases {
        assert_eq!(
            decide(
                at(start, elapsed),
                start,
                deadline.map(|left| at(start, left))
            ),
            expected,
            "after {elapsed:?} with deadline {deadline:?}"
        );
    }
}

#[test]
fn a_new_lock_wait_restarts_its_five_seconds_but_never_the_deadline() {
    let start = Instant::now();
    let deadline = Some(at(start, Duration::from_secs(8)));
    // A second wait that begins six seconds in has its own five seconds...
    let second = at(start, Duration::from_secs(6));
    assert_eq!(
        decide(at(second, Duration::from_secs(1)), second, deadline),
        Decision::Wait(POLL)
    );
    // ...but not past the copy's deadline.
    assert_eq!(
        decide(at(second, Duration::from_secs(2)), second, deadline),
        Decision::Refuse
    );
    // A wait already past the deadline refuses, however recently it began.
    let late = at(start, Duration::from_secs(9));
    assert_eq!(decide(late, late, deadline), Decision::Refuse);
}

#[test]
fn the_busy_handler_refuses_without_a_scope() {
    assert!(!in_copy_scope());
    assert!(!copy_busy(0));
    assert!(!copy_busy(7));
    assert!(!in_copy_scope(), "refusing must not leave a scope behind");
}

#[test]
fn the_busy_handler_waits_inside_a_scope_until_the_deadline() {
    let scope = CopyWaitScope::enter(Instant::now().checked_add(Duration::from_secs(60)));
    assert!(copy_busy(0), "a fresh wait inside the deadline goes on");
    assert!(copy_busy(1));
    drop(scope);

    let expired = CopyWaitScope::enter(Instant::now().checked_sub(Duration::from_nanos(1)));
    assert!(!copy_busy(0), "a copy past its deadline waits for no lock");
    assert!(!copy_busy(3));
    drop(expired);
}

#[test]
fn the_first_call_for_a_lock_starts_a_new_wait() {
    let _scope = CopyWaitScope::enter(None);
    let long_ago = Instant::now()
        .checked_sub(Duration::from_secs(10))
        .expect("an instant ten seconds ago");
    replace(Some(CopyWait {
        deadline: None,
        episode_start: long_ago,
    }));
    assert!(
        !copy_busy(3),
        "a wait that began ten seconds ago has run out"
    );
    assert!(copy_busy(0), "a new lock starts a new wait");
    let wait = current().expect("the scope");
    assert!(wait.episode_start > long_ago);
    assert!(copy_busy(1), "the new wait goes on");
}

#[test]
fn a_scope_puts_back_what_was_there_when_it_ends() {
    assert_eq!(current(), None);
    let outer_deadline = Instant::now().checked_add(Duration::from_secs(30));
    let outer = CopyWaitScope::enter(outer_deadline);
    let outer_wait = current().expect("the outer scope");
    assert_eq!(outer_wait.deadline, outer_deadline);
    {
        let _inner = CopyWaitScope::enter(None);
        assert_eq!(current().expect("the inner scope").deadline, None);
    }
    assert_eq!(
        current(),
        Some(outer_wait),
        "the inner scope restores the outer one"
    );
    drop(outer);
    assert_eq!(current(), None);

    let unwound = std::panic::catch_unwind(|| {
        let _scope = CopyWaitScope::enter(None);
        assert!(in_copy_scope());
        panic!("a step of the copy panics");
    });
    assert!(unwound.is_err());
    assert_eq!(current(), None, "unwinding ends the scope too");
}

#[test]
fn a_scope_covers_only_its_own_thread() {
    let _scope = CopyWaitScope::enter(None);
    let elsewhere = std::thread::spawn(|| (in_copy_scope(), copy_busy(0)))
        .join()
        .expect("the other thread");
    assert_eq!(elsewhere, (false, false));
    assert!(in_copy_scope());
}

#[test]
fn a_retry_outside_the_busy_handler_keeps_to_the_same_limit() {
    let start = Instant::now();
    let ordinary = LockWait::Timeout(Duration::from_secs(5));
    let limit = ordinary.retry_limit(start);
    assert_eq!(limit, Some(at(start, Duration::from_secs(5))));
    assert_eq!(
        ordinary.retry_pause(limit, at(start, Duration::from_millis(4999))),
        Some(JOURNAL_RETRY_PAUSE),
        "an ordinary open keeps its fixed pause"
    );
    assert_eq!(ordinary.retry_pause(limit, at(start, LOCK_WAIT)), None);

    let copy = LockWait::CopyDeadline;
    assert_eq!(copy.retry_limit(start), None, "no scope, no retry");
    assert_eq!(copy.retry_pause(None, start), None);
    {
        let deadline = at(start, Duration::from_millis(1500));
        let _scope = CopyWaitScope::enter(Some(deadline));
        assert_eq!(copy.retry_limit(start), Some(deadline));
        assert_eq!(
            copy.retry_pause(Some(deadline), at(start, Duration::from_millis(1498))),
            Some(Duration::from_millis(2)),
            "a copy's pause stops at its limit"
        );
        assert_eq!(copy.retry_pause(Some(deadline), deadline), None);
    }
    {
        let _scope = CopyWaitScope::enter(None);
        assert_eq!(
            copy.retry_limit(start),
            Some(at(start, LOCK_WAIT)),
            "without a deadline a copy retries for five seconds"
        );
    }
}

#[test]
fn installing_a_copy_wait_replaces_the_busy_timeout() {
    let connection = rusqlite::Connection::open_in_memory().expect("in-memory SQLite");
    let timeout_ms = |connection: &rusqlite::Connection| -> i64 {
        connection
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .expect("SQLite timeout milliseconds")
    };
    LockWait::Timeout(Duration::from_millis(25))
        .install(&connection)
        .expect("install the timeout");
    assert_eq!(timeout_ms(&connection), 25);
    LockWait::CopyDeadline
        .install(&connection)
        .expect("install the copy's handler");
    assert_eq!(
        timeout_ms(&connection),
        0,
        "the copy's handler replaced the timeout"
    );
}
