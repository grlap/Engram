use super::*;

#[test]
fn deadline_lock_wait_rounds_up_to_sqlite_milliseconds_and_caps_at_five_seconds() {
    let now = Instant::now();
    let connection = Connection::open_in_memory().expect("in-memory SQLite");
    let cases = [
        (Some(Duration::ZERO), 0),
        (Some(Duration::from_nanos(1)), 1),
        (Some(Duration::from_nanos(999_999)), 1),
        (Some(Duration::from_millis(1)), 1),
        (Some(Duration::from_nanos(1_000_001)), 2),
        (Some(Duration::from_nanos(4_999_999_999)), 5000),
        (Some(Duration::from_secs(5)), 5000),
        (Some(Duration::from_secs(6)), 5000),
        (None, 5000),
    ];
    for (remaining, expected_ms) in cases {
        let interrupt = CopyInterrupt {
            deadline: remaining.map(|duration| now.checked_add(duration).expect("fixed deadline")),
            fired: Arc::new(AtomicBool::new(false)),
            probe: None,
        };
        let wait = interrupt.lock_wait_at(now);
        assert_eq!(wait, Duration::from_millis(expected_ms), "{remaining:?}");
        connection.busy_timeout(wait).expect("install timeout");
        let actual_ms: i64 = connection
            .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
            .expect("SQLite timeout milliseconds");
        assert_eq!(
            actual_ms,
            i64::try_from(expected_ms).expect("bounded timeout"),
            "{remaining:?}"
        );
        assert!(
            !interrupt.fired(),
            "calculating a wait must not fire the latch"
        );
    }

    let elapsed = CopyInterrupt {
        deadline: now.checked_sub(Duration::from_nanos(1)),
        fired: Arc::new(AtomicBool::new(false)),
        probe: None,
    };
    assert!(elapsed.deadline.is_some());
    let wait = elapsed.lock_wait_at(now);
    assert_eq!(wait, Duration::ZERO);
    connection.busy_timeout(wait).expect("disable elapsed wait");
    let actual_ms: i64 = connection
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .expect("SQLite timeout milliseconds");
    assert_eq!(actual_ms, 0);
}
