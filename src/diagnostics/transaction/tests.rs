use super::*;
use crate::diagnostics::{
    Surface,
    tests::{drain, records},
};
use serde_json::Value;

fn context(home: &std::path::Path, full: bool, operation: &str) -> Context {
    let mut sink = Diagnostics::start(home, "session-12329", Surface::Mcp);
    sink.full_transactions = full;
    Context::new(
        sink,
        operation,
        Correlation::default(),
        Fingerprint::store(&home.join("private-store.db")),
    )
}
fn transactions(home: &std::path::Path) -> Vec<Value> {
    records(home)
        .into_iter()
        .filter(|row| row["event"]["kind"] == "store_transaction")
        .collect()
}
fn observe(context: Context, wait: u64, held: u64) {
    let started = Instant::now();
    let mut observation =
        Observation::start_at(context, Boundary::WorkMutation, Budget::default(), started).unwrap();
    observation.acquired_at(started + Duration::from_millis(wait));
    observation.finish_at(
        Disposition::Committed,
        Some(true),
        started + Duration::from_millis(wait + held),
    );
}

#[test]
fn only_slow_intervals_emit_retrospective_pairs_with_separate_wait_and_hold() {
    let home = crate::test_support::temp_home().unwrap();
    let context = context(home.path(), false, "note");
    observe(context.clone(), 1000, 1000);
    observe(context.clone(), 1001, 3);
    observe(context.clone(), 2, 1001);
    drain(&context.sink);
    let rows = transactions(home.path());
    assert_eq!(rows.len(), 4);
    for pair in rows.as_chunks::<2>().0 {
        let a = &pair[0]["event"];
        let b = &pair[1]["event"];
        assert_eq!(a["stage"], "acquired");
        assert_eq!(b["stage"], "settled");
        assert_eq!(
            a["store_transaction"]["transaction_seq"],
            b["store_transaction"]["transaction_seq"]
        );
        assert_eq!(a["store_transaction"]["retrospective"], true);
        assert_eq!(b["store_transaction"]["connection_autocommit"], true);
        assert_eq!(b["store_transaction"]["disposition"], "committed");
    }
    assert_eq!(rows[1]["event"]["store_transaction"]["admission_ms"], 1001);
    assert_eq!(
        rows[1]["event"]["store_transaction"]["acquired_interval_ms"],
        3
    );
    assert_eq!(rows[3]["event"]["store_transaction"]["admission_ms"], 2);
    assert_eq!(
        rows[3]["event"]["store_transaction"]["acquired_interval_ms"],
        1001
    );
}

#[test]
fn failed_begin_is_always_observed_and_never_invents_acquisition() {
    let home = crate::test_support::temp_home().unwrap();
    let context = context(home.path(), false, "claim");
    let now = Instant::now();
    Observation::start_at(
        context.clone(),
        Boundary::WorkMutation,
        Budget {
            configured_ms: Some(5000),
            allowance_ms: Some(0),
            shared_window_ms: Some(10000),
        },
        now,
    )
    .unwrap()
    .finish_at(Disposition::AdmissionRefused, None, now);
    drain(&context.sink);
    let rows = transactions(home.path());
    assert_eq!(rows.len(), 1);
    let event = &rows[0]["event"];
    assert_eq!(event["stage"], "acquisition_failed");
    assert!(event["store_transaction"]["acquired_at"].is_null());
    assert!(event["store_transaction"]["acquired_interval_ms"].is_null());
    assert!(event["store_transaction"]["connection_autocommit"].is_null());
    assert_eq!(event["store_transaction"]["budget"]["allowance_ms"], 0);
}

#[test]
fn full_capture_retains_acquisition_before_completion_or_lost_settlement() {
    let home = crate::test_support::temp_home().unwrap();
    let context = context(home.path(), true, "update");
    let now = Instant::now();
    let mut observation = Observation::start_at(
        context.clone(),
        Boundary::ControlMutation,
        Budget::default(),
        now,
    )
    .unwrap();
    observation.acquired_at(now);
    drain(&context.sink);
    assert_eq!(transactions(home.path()).len(), 1);
    // Simulates missing settlement evidence, not proof that a process died.
    drop(observation);
    observe(context.clone(), 0, 0);
    drain(&context.sink);
    let rows = transactions(home.path());
    assert_eq!(rows.len(), 3);
    assert_eq!(
        rows[0]["event"]["store_transaction"]["retrospective"],
        false
    );
    assert_eq!(rows[0]["event"]["stage"], "acquired");
}

#[test]
fn release_unknown_and_sequence_exhaustion_are_explicit() {
    let home = crate::test_support::temp_home().unwrap();
    let context = context(home.path(), true, "gate");
    let now = Instant::now();
    let mut observation = Observation::start_at(
        context.clone(),
        Boundary::ControlMutation,
        Budget::default(),
        now,
    )
    .unwrap();
    observation.acquired_at(now);
    observation.finish_at(Disposition::RollbackFailed, Some(false), now);
    context
        .sink
        .transaction_sequence
        .store(u64::MAX, Ordering::Relaxed);
    assert!(
        Observation::start_at(
            context.clone(),
            Boundary::WorkMutation,
            Budget::default(),
            now
        )
        .is_none()
    );
    drain(&context.sink);
    let rows = transactions(home.path());
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows[1]["event"]["store_transaction"]["disposition"],
        "rollback_failed"
    );
    assert_eq!(
        rows[1]["event"]["store_transaction"]["connection_autocommit"],
        false
    );
    assert!(
        records(home.path())
            .iter()
            .any(|row| row["loss"]["tracking"].as_u64().unwrap_or(0) > 0)
    );
}

#[tokio::test]
async fn task_local_contexts_do_not_cross_awaits_or_concurrent_calls() {
    let home = crate::test_support::temp_home().unwrap();
    let a = context(home.path(), true, "note");
    let mut b = a.clone();
    b.operation = "claim";
    b.correlation.frame_seq = Some(22);
    let run = || async {
        tokio::task::yield_now().await;
        let mut observation =
            Observation::start(Boundary::WorkMutation, Budget::default()).unwrap();
        observation.acquired();
        tokio::task::yield_now().await;
        observation.finish(Disposition::Committed, Some(true));
    };
    tokio::join!(scoped(a.clone(), run()), scoped(b, run()));
    assert!(current().is_none());
    drain(&a.sink);
    let rows = transactions(home.path());
    assert_eq!(rows.len(), 4);
    assert_eq!(
        rows.iter()
            .filter(|r| r["event"]["operation"] == "note")
            .count(),
        2
    );
    assert_eq!(
        rows.iter()
            .filter(|r| r["event"]["operation"] == "claim" && r["event"]["frame_seq"] == 22)
            .count(),
        2
    );
    let text = serde_json::to_string(&rows).unwrap();
    assert!(!text.contains("private-store.db"));
    assert!(!text.contains(&home.path().display().to_string()));
}

#[test]
fn synchronous_context_is_restored_on_unwind() {
    let home = crate::test_support::temp_home().unwrap();
    let a = context(home.path(), true, "note");
    let outer = Scope::enter(a.clone());
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let mut b = a.clone();
        b.operation = "claim";
        let _inner = Scope::enter(b);
        assert_eq!(current().unwrap().operation, "claim");
        panic!("unwind scope");
    }));
    assert!(result.is_err());
    assert_eq!(current().unwrap().operation, "note");
    drop(outer);
    assert!(current().is_none());
}

#[test]
fn mutex_wait_is_separate_and_store_comparison_never_emits_paths() {
    assert!(mutex_start().is_none());
    let home = crate::test_support::temp_home().unwrap();
    let context = context(home.path(), true, "note");
    let scope = Scope::enter(context.clone());
    mutex_finished(Some(
        Instant::now()
            .checked_sub(Duration::from_millis(7))
            .expect("test clock subtraction"),
    ));
    let mut observation = Observation::start(Boundary::WorkMutation, Budget::default()).unwrap();
    observation.acquired();
    observation.finish(Disposition::Committed, Some(true));
    drop(scope);
    drain(&context.sink);
    let rows = records(home.path());
    let mutex = rows
        .iter()
        .find(|r| r["event"]["kind"] == "store_mutex")
        .unwrap();
    let transaction = rows
        .iter()
        .find(|r| r["event"]["stage"] == "settled")
        .unwrap();
    assert!(mutex["event"]["store_transaction"].is_null());
    assert!(mutex["event"]["store_mutex"]["wait_ms"].as_u64().unwrap() >= 7);
    assert_eq!(
        mutex["event"]["store_mutex"]["store_path_fingerprint"],
        transaction["event"]["store_transaction"]["store_path_fingerprint"]
    );
    assert_eq!(
        mutex["event"]["store_mutex"]["wait_ms"],
        transaction["event"]["store_transaction"]["store_mutex_wait_ms"]
    );
    assert!(
        !serde_json::to_string(&rows)
            .unwrap()
            .contains("private-store.db")
    );
    assert_ne!(
        Fingerprint::store(&home.path().join("private-store.db")),
        Fingerprint::store(&home.path().join("different-store.db"))
    );
}

#[test]
fn concurrent_ring_writer_loss_is_counted_when_an_acquisition_cannot_be_retained() {
    use std::{fs::OpenOptions, io::Write};
    let home = crate::test_support::temp_home().unwrap();
    let directory = home.path().join("diagnostics");
    std::fs::create_dir(&directory).unwrap();
    let lock = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(directory.join("engram.lock"))
        .unwrap();
    // A cooperating writer owns the actual OS ring lock and writes a complete
    // fixture line. Keep its critical section open while the diagnostic
    // producer attempts its acquisition record; no scheduling race is needed.
    lock.lock().unwrap();
    let mut competing_writer = OpenOptions::new()
        .create_new(true)
        .append(true)
        .open(directory.join("engram-0.jsonl"))
        .unwrap();
    competing_writer
        .write_all(b"{\"test_writer\":true}\n")
        .unwrap();
    competing_writer.flush().unwrap();
    let context = context(home.path(), true, "note");
    let mut observation = Observation::start_at(
        context.clone(),
        Boundary::WorkMutation,
        Budget::default(),
        Instant::now(),
    )
    .unwrap();
    observation.acquired();
    assert!(!context.sink.flush(crate::diagnostics::tests::RECORD_DRAIN));
    drop(competing_writer);
    lock.unlock().unwrap();
    observation.finish(Disposition::Committed, Some(true));
    // Prior loss remains sticky even after later records drain successfully.
    assert!(!context.sink.flush(crate::diagnostics::tests::RECORD_DRAIN));
    let rows = transactions(home.path());
    assert_eq!(rows.len(), 1, "{rows:?}");
    assert_eq!(rows[0]["event"]["stage"], "settled");
    assert!(rows[0]["loss"]["lock"].as_u64().unwrap() > 0);
    assert_eq!(rows[0]["loss"]["flush_timeout"], 0);
    let summary = records(home.path())
        .into_iter()
        .find(|row| row["event"]["kind"] == "summary")
        .unwrap();
    assert!(summary["loss"]["lock"].as_u64().unwrap() > 0);
}
