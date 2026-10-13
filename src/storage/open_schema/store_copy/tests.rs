use super::*;
use crate::storage::copy_trace;

#[test]
fn a_copy_waits_for_locks_through_its_deadline_handler_and_leaves_no_scope() {
    let directory = crate::test_support::temp_home().expect("temporary store directory");
    let source = directory.path().join("engram.db");
    drop(SqliteStore::open(&source).expect("initialize current store"));
    let target = directory.path().join("copy.db");
    let interrupt = CopyInterrupt::after(Duration::from_secs(60));

    copy_trace::start();
    let copied = SqliteStore::copy_existing_read_only(&source, &target, &interrupt);
    let trace = copy_trace::take();
    copied.expect("copy the store");
    assert!(
        trace
            .iter()
            .any(|step| step.ends_with("open: lock wait CopyDeadline set")),
        "the admission waits through the copy's handler: {trace:?}"
    );
    // SQLite reads a busy timeout of 0 while a custom busy handler is set.
    assert!(
        trace
            .iter()
            .any(|step| step.ends_with("copy: vacuum begins, busy timeout 0 ms")),
        "the copy statement waits through the copy's handler: {trace:?}"
    );
    assert!(
        !super::super::lock_wait::in_copy_scope(),
        "the copy's scope ends with it"
    );

    SqliteStore::settle_store_copy(&target, &interrupt).expect("settle the copy");
    assert!(
        !super::super::lock_wait::in_copy_scope(),
        "the settlement's scope ends with it"
    );
}
