use super::*;
use crate::storage::copy_trace;

fn capture_source_with_memory() -> (crate::test_support::TempHome, ProjectId, std::path::PathBuf) {
    let home = crate::test_support::temp_home().unwrap();
    let project = ProjectId("settle-opened-expiry".into());
    let source = crate::project_database_path(home.path(), &project);
    std::fs::create_dir_all(source.parent().unwrap()).unwrap();
    let mut store = SqliteStore::open(&source).unwrap();
    let actor = crate::storage::test_support::actor("settle-opened-expiry");
    store
        .remember_project_memory(
            &crate::RememberProjectMemoryRequest {
                project_id: project.clone(),
                session_id: actor.session_id.clone().unwrap(),
                key: Some("retained-memory".into()),
                revise: false,
                expected_revision: None,
                body: "the capture must preserve this source".into(),
                retiring_target: crate::domain::ProjectMemoryRetiringTargetChange::Keep,
                actor,
                created_at: chrono::Utc::now(),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    drop(store);
    (home, project, source)
}

#[test]
fn settle_opened_expiry_refuses_before_admission_and_releases_its_stage() {
    use std::cell::Cell;
    use std::sync::Mutex;

    use crate::backup::{
        BackupError, CaptureOptions, CapturePhase, STAGE_DIRECTORY, capture_store,
    };

    let (home, project, source) = capture_source_with_memory();
    let source_bytes = std::fs::read(&source).unwrap();
    let stage_root = home
        .path()
        .join(STAGE_DIRECTORY)
        .join(crate::project_digest(&project));
    let sibling = stage_root.join("another-attempt");
    std::fs::create_dir_all(&sibling).unwrap();
    let sentinel = sibling.join("store.db");
    std::fs::write(&sentinel, b"another capture owns this").unwrap();

    let opened_attempt = Arc::new(Mutex::new(None));
    let seen_points = Arc::new(Mutex::new(Vec::new()));
    let probe = {
        let opened_attempt = opened_attempt.clone();
        let seen_points = seen_points.clone();
        let stage_root = stage_root.clone();
        let sibling = sibling.clone();
        Arc::new(move |point| {
            seen_points.lock().unwrap().push(point);
            if point != CopyProbePoint::SettleOpened {
                return false;
            }
            assert!(super::super::lock_wait::in_copy_scope());
            let attempts: Vec<_> = std::fs::read_dir(&stage_root)
                .unwrap()
                .map(|entry| entry.unwrap().path())
                .filter(|path| path != &sibling)
                .collect();
            assert_eq!(attempts.len(), 1);
            let staged = attempts[0].join("store.db");
            assert!(staged.is_file());
            // This positive control and subsequent cleanup prove handle release
            // on Windows. Unix unlink would not establish that a handle closed.
            #[cfg(windows)]
            assert_eq!(
                std::fs::remove_file(&staged).unwrap_err().raw_os_error(),
                Some(32)
            );
            assert!(
                opened_attempt
                    .lock()
                    .unwrap()
                    .replace(attempts[0].clone())
                    .is_none()
            );
            true
        })
    };
    let verify_seen = Cell::new(false);
    let observer = |phase| {
        if phase == CapturePhase::Verify {
            assert!(!verify_seen.replace(true));
            // Exclude source-copy admission from the settlement observation.
            copy_trace::start();
        }
    };
    let options = CaptureOptions {
        deadline: Duration::MAX,
        compressed_in_stage: false,
        host_name: None,
        free_space: &|_| Ok(u64::MAX),
        observer: Some(&observer),
        same_as: None,
        copy_probe: Some(probe),
    };
    let captured = capture_store(home.path(), &project, &options);
    let admissions = copy_trace::admission_count();
    let _ = copy_trace::take();
    assert!(verify_seen.get());
    let attempt = opened_attempt
        .lock()
        .unwrap()
        .clone()
        .expect("settle opened");
    assert!(matches!(
        captured,
        Err(BackupError::CaptureDeadline {
            phase: CapturePhase::Verify
        })
    ));
    assert_eq!(admissions, Some(0), "settlement must not start admission");
    let points = seen_points.lock().unwrap();
    assert_eq!(points.last(), Some(&CopyProbePoint::SettleOpened));
    assert_eq!(
        points
            .iter()
            .filter(|&&point| point == CopyProbePoint::SettleOpened)
            .count(),
        1
    );
    assert!(!super::super::lock_wait::in_copy_scope());
    assert!(
        !attempt.exists(),
        "the opened handle must not prevent cleanup"
    );
    let remaining: Vec<_> = std::fs::read_dir(&stage_root)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(remaining, [sibling]);
    assert_eq!(
        std::fs::read(&sentinel).unwrap(),
        b"another capture owns this"
    );
    assert_eq!(std::fs::read(&source).unwrap(), source_bytes);
}

#[test]
fn copy_boundary_expiry_releases_the_connection_without_starting_later_work() {
    for point in [
        CopyProbePoint::Opened,
        CopyProbePoint::BeforeAdmission,
        CopyProbePoint::BeforeCopy,
    ] {
        let directory = crate::test_support::temp_home().unwrap();
        let source = directory.path().join("engram.db");
        drop(SqliteStore::open(&source).unwrap());
        let target = directory.path().join("copy.db");
        let interrupt =
            CopyInterrupt::after(Duration::MAX).with_probe(Arc::new(move |at| at == point));
        copy_trace::start();
        let error = SqliteStore::copy_existing_read_only(&source, &target, &interrupt)
            .expect_err("the selected boundary fires the interrupt");
        let trace = copy_trace::take();
        assert!(interrupt.fired(), "{point:?}");
        assert!(
            matches!(error, StoreError::InvalidWork(ref message) if message == "the copy passed its deadline"),
            "{point:?}: {error}"
        );
        assert!(!target.exists(), "{point:?}: no copy statement ran");
        assert!(
            !trace
                .iter()
                .any(|step| step.contains("copy: vacuum begins")),
            "{point:?}: {trace:?}"
        );
        if point != CopyProbePoint::BeforeCopy {
            assert!(
                !trace
                    .iter()
                    .any(|step| step.ends_with("copy: admission begins")),
                "{point:?}: {trace:?}"
            );
        }
        assert!(!super::super::lock_wait::in_copy_scope());
        assert!(trace.iter().any(|step| step.ends_with(": close begins")));
        assert!(trace.iter().any(|step| step.ends_with(": closed")));
        // A leaked read transaction would prevent this exclusive lock.
        let reopened = Connection::open(&source).unwrap();
        reopened.busy_timeout(Duration::ZERO).unwrap();
        reopened
            .execute_batch(
                "PRAGMA locking_mode = EXCLUSIVE;
                 BEGIN EXCLUSIVE;
                 CREATE TABLE release_probe(value INTEGER);
                 DROP TABLE release_probe;
                 ROLLBACK;",
            )
            .unwrap();
    }
}

#[test]
fn failed_admission_records_the_original_sqlite_error_before_disposal() {
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("engram.db");
    drop(SqliteStore::open(&source).unwrap());
    let holder = Connection::open(&source).unwrap();
    holder
        .execute_batch(
            "PRAGMA locking_mode = EXCLUSIVE;
             BEGIN EXCLUSIVE;
             CREATE TABLE lock_probe(value INTEGER);
             DROP TABLE lock_probe;
             COMMIT;",
        )
        .unwrap();
    let connection = SqliteStore::open_existing_read_only_connection(&source).unwrap();
    copy_trace::start();
    // Without a copy scope the callback refuses immediately, without a sleep.
    let Err(error) = SqliteStore::from_connection_with_lock_wait(
        connection,
        None,
        None,
        LockWait::CopyDeadline,
        ProtocolRetry::Never,
    ) else {
        panic!("the locked store must refuse admission");
    };
    let trace = copy_trace::take();
    assert!(
        matches!(error, StoreError::Sqlite(rusqlite::Error::SqliteFailure(ref failure, _))
            if failure.code == rusqlite::ErrorCode::DatabaseBusy),
        "{error}"
    );
    let produced = trace
        .iter()
        .position(|step| step.contains("admission error produced:"))
        .unwrap();
    let closing = trace
        .iter()
        .position(|step| step.ends_with("open admission failure: close begins"))
        .unwrap();
    let closed = trace
        .iter()
        .position(|step| step.ends_with("open admission failure: closed"))
        .unwrap();
    assert!(produced < closing && closing < closed, "{trace:?}");
    assert!(
        trace[produced].contains("sqlite code DatabaseBusy"),
        "{trace:?}"
    );
    assert!(trace.last().unwrap().contains("no copy scope"), "{trace:?}");
    assert!(!super::super::lock_wait::in_copy_scope());
}

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
