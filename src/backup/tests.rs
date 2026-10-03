use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, Ordering},
};
use std::time::Instant;

use sha2::{Digest, Sha256};

use super::*;
use crate::{
    ActorContext, DevelopmentNoopRedactor, RememberProjectMemoryRequest,
    domain::{AssuranceLevel, ProjectMemoryRetiringTargetChange, ProvenanceLink},
    storage::CopyProbePoint,
    test_support::{TempHome, temp_home},
};

fn actor() -> ActorContext {
    ActorContext {
        actor_id: "capture-test".into(),
        actor_kind: "test_agent".into(),
        assurance: AssuranceLevel::Asserted,
        run_id: None,
        session_id: Some(crate::SessionId("capture-test".into())),
        source_tool: Some("backup_capture_test".into()),
        source_skill: None,
        provenance_chain: Vec::<ProvenanceLink>::new(),
        reason: "exercise backup capture".into(),
    }
}

fn remember(store: &mut SqliteStore, project: &ProjectId, key: &str, body: String) {
    store
        .remember_project_memory(
            &RememberProjectMemoryRequest {
                project_id: project.clone(),
                session_id: actor().session_id.unwrap(),
                key: Some(key.into()),
                revise: false,
                expected_revision: None,
                body,
                retiring_target: ProjectMemoryRetiringTargetChange::Keep,
                actor: actor(),
                created_at: Utc::now(),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
}

/// A closed on-disk store for `project` holding `memories` project memories
/// of about `body_bytes` each.
fn fixture(memories: usize, body_bytes: usize) -> (TempHome, ProjectId, PathBuf) {
    let home = temp_home().unwrap();
    let project = ProjectId("capture-project".into());
    let database = crate::project_database_path(home.path(), &project);
    std::fs::create_dir_all(database.parent().unwrap()).unwrap();
    let mut store = SqliteStore::open(&database).unwrap();
    for index in 0..memories {
        remember(
            &mut store,
            &project,
            &format!("fixture-{index}"),
            format!("{index} {}", "x".repeat(body_bytes)),
        );
    }
    drop(store);
    (home, project, database)
}

fn options(free_space: &dyn Fn(&Path) -> io::Result<u64>) -> CaptureOptions<'_> {
    CaptureOptions {
        deadline: Duration::from_secs(120),
        compressed_in_stage: false,
        host_name: Some("test-host".into()),
        free_space,
        observer: None,
        same_as: None,
        copy_probe: None,
    }
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "it stands in for a free-space reading, which can fail"
)]
fn plenty(_: &Path) -> io::Result<u64> {
    Ok(u64::MAX)
}

#[test]
fn only_equal_bytes_checked_by_this_build_skip_the_integrity_scan() {
    let (home, project, database) = fixture(20, 256);
    let scans = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let hashes = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = scans.clone();
    let hash_counter = hashes.clone();
    let mut observed = options(&plenty);
    observed.copy_probe = Some(Arc::new(move |point| {
        if point == CopyProbePoint::Scan {
            counter.fetch_add(1, Ordering::SeqCst);
        }
        if point == CopyProbePoint::Hash {
            hash_counter.fetch_add(1, Ordering::SeqCst);
        }
        false
    }));
    let first = capture_store(home.path(), &project, &observed).unwrap();
    assert_eq!(first.check, CaptureCheck::Full);
    assert!(scans.load(Ordering::SeqCst) > 0);
    assert_single_capture_hash(&first, &hashes);
    scans.store(0, Ordering::SeqCst);
    hashes.store(0, Ordering::SeqCst);
    observed.same_as = Some(&first.manifest);
    let equal = capture_store(home.path(), &project, &observed).unwrap();
    assert_eq!(equal.check, CaptureCheck::SameBytesAsNewest);
    assert_eq!(scans.load(Ordering::SeqCst), 0);
    assert_single_capture_hash(&equal, &hashes);
    equal.discard().unwrap();

    for build in [
        None,
        Some(crate::ObjectId::from_canonical_bytes(b"other build")),
    ] {
        let mut older = first.manifest.clone();
        older.build_fingerprint = build;
        let mut full = options(&plenty);
        full.same_as = Some(&older);
        full.copy_probe = observed.copy_probe.clone();
        scans.store(0, Ordering::SeqCst);
        hashes.store(0, Ordering::SeqCst);
        let checked = capture_store(home.path(), &project, &full).unwrap();
        assert_eq!(checked.check, CaptureCheck::Full);
        assert!(scans.load(Ordering::SeqCst) > 0);
        assert_single_capture_hash(&checked, &hashes);
        checked.discard().unwrap();
    }
    {
        let mut store = SqliteStore::open(&database).unwrap();
        remember(&mut store, &project, "changed", "new restored row".into());
    }
    observed.same_as = Some(&first.manifest);
    scans.store(0, Ordering::SeqCst);
    hashes.store(0, Ordering::SeqCst);
    let changed = capture_store(home.path(), &project, &observed).unwrap();
    assert_eq!(changed.check, CaptureCheck::Full);
    assert!(scans.load(Ordering::SeqCst) > 0);
    assert_ne!(changed.manifest.sha256, first.manifest.sha256);
    assert_single_capture_hash(&changed, &hashes);
    changed.discard().unwrap();
    first.discard().unwrap();
}

/// Count the file chunks plus EOF, and independently compare the exact staged
/// bytes after the full verifier's immutable opens with the recorded hash.
fn assert_single_capture_hash(copy: &StoreCapture, hashes: &std::sync::atomic::AtomicUsize) {
    let bytes = std::fs::read(&copy.staged).unwrap();
    assert_eq!(copy.manifest.bytes, bytes.len() as u64);
    assert_eq!(
        copy.manifest.sha256,
        format!("{:x}", Sha256::digest(&bytes))
    );
    assert_eq!(
        hashes.load(Ordering::SeqCst) as u64,
        copy.manifest.bytes.div_ceil(1 << 20) + 1,
        "the settled stage must be hashed once, including the EOF check"
    );
}

/// The cut read straight from a store file, without Engram's opener.
fn raw_cut(path: &Path, project: &ProjectId) -> WorkGraphSnapshotCut {
    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    let work_feed = connection
        .query_row(
            "SELECT position FROM work_feed_heads WHERE feed_kind = 'project' AND feed_id = ?1",
            [project.0.as_str()],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .unwrap()
        .unwrap_or(0);
    let project_memory = connection
        .query_row(
            "SELECT change_position FROM project_memory_state WHERE project_id = ?1",
            [project.0.as_str()],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .unwrap()
        .unwrap_or(0);
    WorkGraphSnapshotCut {
        work_feed,
        project_memory,
    }
}

fn stage_entries(home: &Path, project: &ProjectId) -> Vec<PathBuf> {
    let root = home
        .join(STAGE_DIRECTORY)
        .join(crate::project_digest(project));
    match std::fs::read_dir(&root) {
        Ok(entries) => entries.map(|entry| entry.unwrap().path()).collect(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("{error}"),
    }
}

use rusqlite::OptionalExtension;

#[test]
fn capture_refuses_a_missing_or_empty_store_and_creates_nothing() {
    let home = temp_home().unwrap();
    let project = ProjectId("capture-project".into());
    let error = capture_store(home.path(), &project, &options(&plenty)).unwrap_err();
    assert_eq!(error.code(), "store_not_initialized", "{error}");
    assert!(!home.path().join("projects").exists());
    assert!(!home.path().join(STAGE_DIRECTORY).exists());

    // An existing empty file is no store either: refused on admission, before
    // the copy could be settled into a fresh store, and its stage removed.
    let database = crate::project_database_path(home.path(), &project);
    std::fs::create_dir_all(database.parent().unwrap()).unwrap();
    std::fs::write(&database, b"").unwrap();
    let error = capture_store(home.path(), &project, &options(&plenty)).unwrap_err();
    assert_eq!(error.code(), "store_not_initialized", "{error}");
    assert_eq!(std::fs::metadata(&database).unwrap().len(), 0);
    assert_eq!(stage_entries(home.path(), &project), Vec::<PathBuf>::new());
}

#[test]
fn capture_reads_the_live_store_without_writing_it() {
    let (home, project, database) = fixture(3, 64);
    let before = std::fs::read(&database).unwrap();
    let capture = capture_store(home.path(), &project, &options(&plenty)).unwrap();
    assert_eq!(std::fs::read(&database).unwrap(), before);
    let log = sidecar(&database, "-wal");
    assert!(!log.exists() || std::fs::metadata(&log).unwrap().len() == 0);
    capture.discard().unwrap();
    assert_eq!(stage_entries(home.path(), &project), Vec::<PathBuf>::new());
}

#[test]
fn capture_manifest_names_each_identity_and_the_copy_s_own_cut() {
    let (home, project, database) = fixture(4, 64);
    let live_before = raw_cut(&database, &project);
    let copy_began = Mutex::new(None);
    // Once the copy is written, the live store moves on: the manifest must
    // keep the copy's cut, not the live store's.
    let observer = |phase: CapturePhase| match phase {
        CapturePhase::Copy => *copy_began.lock().unwrap() = Some(Utc::now()),
        CapturePhase::Verify => {
            let mut live = SqliteStore::open(&database).unwrap();
            remember(&mut live, &project, "after-copy", "later".into());
        }
        CapturePhase::Space => {}
    };
    let mut options = options(&plenty);
    options.observer = Some(&observer);
    let started = Utc::now();
    let capture = capture_store(home.path(), &project, &options).unwrap();
    let manifest = &capture.manifest;

    let staged_cut = raw_cut(&capture.staged, &project);
    assert_eq!(manifest.cut, staged_cut);
    assert_eq!(manifest.cut, live_before);
    let live_after = raw_cut(&database, &project);
    assert!(live_after.project_memory > manifest.cut.project_memory);
    assert!(live_after.work_feed >= manifest.cut.work_feed);

    let bytes = std::fs::read(&capture.staged).unwrap();
    assert_eq!(manifest.project_digest, crate::project_digest(&project));
    assert_eq!(
        database.parent().unwrap().file_name().unwrap().to_str(),
        Some(manifest.project_digest.as_str())
    );
    assert_eq!(manifest.kind, CopyKind::Store);
    assert_eq!(manifest.bytes, bytes.len() as u64);
    assert_eq!(manifest.sha256, format!("{:x}", Sha256::digest(&bytes)));
    assert_eq!(
        manifest.format_identity,
        crate::storage::running_schema_reference().unwrap()
    );
    assert_eq!(
        manifest.build_fingerprint,
        crate::build_identity::current().build_fingerprint
    );
    // The manifest carries exactly the revision the build reports.
    assert_eq!(
        manifest.source_revision.as_deref(),
        Some(
            crate::build_identity::current()
                .build
                .source_revision
                .as_str()
        )
    );
    assert_eq!(manifest.host_name.as_deref(), Some("test-host"));
    let copy_began = copy_began.lock().unwrap().unwrap();
    assert!(started <= manifest.capture_started_at);
    assert!(copy_began <= manifest.capture_started_at);

    let value = serde_json::to_value(manifest).unwrap();
    let mut fields = value
        .as_object()
        .unwrap()
        .keys()
        .cloned()
        .collect::<Vec<_>>();
    fields.sort();
    assert_eq!(
        fields,
        [
            "build_fingerprint",
            "bytes",
            "capture_started_at",
            "cut",
            "format_identity",
            "host_name",
            "kind",
            "project_digest",
            "sha256",
            "source_revision",
        ]
    );
    assert_eq!(value["kind"], "store");
    assert_eq!(
        serde_json::from_value::<CaptureManifest>(value).unwrap(),
        *manifest
    );
    capture.discard().unwrap();
}

#[test]
fn capture_checks_stage_space_before_writing_anything() {
    let (home, project, _database) = fixture(2, 64);
    let mut required_by_mode = Vec::new();
    for compressed in [false, true] {
        let probed = Mutex::new(Vec::new());
        let reading = Mutex::new(0_u64);
        let free_space = |path: &Path| {
            probed.lock().unwrap().push(path.to_path_buf());
            Ok(*reading.lock().unwrap())
        };
        let mut options = options(&free_space);
        options.compressed_in_stage = compressed;
        let error = capture_store(home.path(), &project, &options).unwrap_err();
        assert_eq!(error.code(), "backup_stage_no_space", "{error}");
        let BackupError::StageNoSpace { required, .. } = error else {
            unreachable!()
        };
        assert!(!home.path().join(STAGE_DIRECTORY).exists());
        // The reading is taken on an existing ancestor of the stage, which
        // is not created for it.
        let probed = probed.lock().unwrap().clone();
        assert_eq!(probed, [home.path().to_path_buf()]);

        *reading.lock().unwrap() = required - 1;
        let error = capture_store(home.path(), &project, &options).unwrap_err();
        assert_eq!(error.code(), "backup_stage_no_space", "{error}");
        assert!(!home.path().join(STAGE_DIRECTORY).exists());

        *reading.lock().unwrap() = required;
        capture_store(home.path(), &project, &options)
            .unwrap()
            .discard()
            .unwrap();
        std::fs::remove_dir_all(home.path().join(STAGE_DIRECTORY)).unwrap();
        required_by_mode.push(required);
    }
    assert_eq!(required_by_mode[1], 2 * required_by_mode[0]);

    let unreadable = |_: &Path| Err(io::Error::other("no reading on this share"));
    let error = capture_store(home.path(), &project, &options(&unreadable)).unwrap_err();
    assert_eq!(error.code(), "backup_stage_space_unknown", "{error}");
    assert!(!home.path().join(STAGE_DIRECTORY).exists());
}

#[test]
fn capture_lets_a_writer_commit_while_the_store_is_copied() {
    let (home, project, database) = fixture(400, 8000);
    // Each commit's start and duration, as the writer made them.
    let commits = Arc::new(Mutex::new(Vec::<(Instant, Duration)>::new()));
    let stop = Arc::new(AtomicBool::new(false));
    let writer = {
        let (commits, stop) = (commits.clone(), stop.clone());
        let (database, project) = (database.clone(), project.clone());
        std::thread::spawn(move || {
            let mut store = SqliteStore::open(&database).unwrap();
            let mut index = 0_usize;
            while !stop.load(Ordering::SeqCst) {
                let began = Instant::now();
                remember(&mut store, &project, &format!("writer-{index}"), "w".into());
                commits.lock().unwrap().push((began, began.elapsed()));
                index += 1;
                std::thread::sleep(Duration::from_millis(20));
            }
        })
    };
    let count = || commits.lock().unwrap().len();
    let give_up = Instant::now() + Duration::from_secs(60);
    while count() == 0 {
        if writer.is_finished() || Instant::now() >= give_up {
            stop.store(true, Ordering::SeqCst);
            match writer.join() {
                Err(panic) => std::panic::resume_unwind(panic),
                Ok(()) => panic!("the writer made no first commit"),
            }
        }
        std::thread::sleep(Duration::from_millis(1));
    }

    // Inside the copy's read transaction, wait for the writer's next commit.
    let probed = Arc::new(AtomicBool::new(false));
    let committed_during_copy = Arc::new(AtomicBool::new(false));
    let probe: Arc<dyn Fn(CopyProbePoint) -> bool + Send + Sync> = {
        let (commits, probed, committed) = (
            commits.clone(),
            probed.clone(),
            committed_during_copy.clone(),
        );
        Arc::new(move |point| {
            if point != CopyProbePoint::Copy || probed.swap(true, Ordering::SeqCst) {
                return false;
            }
            let seen = commits.lock().unwrap().len();
            let until = Instant::now() + Duration::from_secs(30);
            while Instant::now() < until {
                if commits.lock().unwrap().len() > seen {
                    committed.store(true, Ordering::SeqCst);
                    break;
                }
                std::thread::sleep(Duration::from_millis(1));
            }
            false
        })
    };
    let mut options = options(&plenty);
    options.copy_probe = Some(probe);
    let began = Instant::now();
    let capture = capture_store(home.path(), &project, &options);
    let ended = Instant::now();
    stop.store(true, Ordering::SeqCst);
    let joined = writer.join();
    let capture = capture.unwrap();
    if let Err(panic) = joined {
        std::panic::resume_unwind(panic);
    }
    // The commits that ran, wholly or partly, while the capture did.
    let during = commits
        .lock()
        .unwrap()
        .iter()
        .filter(|(start, took)| *start < ended && *start + *took > began)
        .map(|(_, took)| *took)
        .collect::<Vec<_>>();
    println!(
        "capture of {} bytes took {:?}; writer commits during it: {}; slowest of them: {:?}",
        capture.manifest.bytes,
        ended - began,
        during.len(),
        during.iter().max()
    );
    assert!(
        probed.load(Ordering::SeqCst),
        "the copy ran no progress check"
    );
    assert!(
        committed_during_copy.load(Ordering::SeqCst),
        "no writer commit landed while the copy held its read transaction"
    );
    assert_ne!(during, Vec::<Duration>::new());
    capture.discard().unwrap();
}

/// Captures with a probe that fires the interrupt at `point` once the staged
/// copy exists, and returns the failure and whether the probe fired.
fn capture_fired_at(point: CopyProbePoint) -> (BackupError, bool) {
    let (home, project, _database) = fixture(3, 64);
    let fired = Arc::new(AtomicBool::new(false));
    let probe: Arc<dyn Fn(CopyProbePoint) -> bool + Send + Sync> = {
        let fired = fired.clone();
        let root = home
            .path()
            .join(STAGE_DIRECTORY)
            .join(crate::project_digest(&project));
        Arc::new(move |at| {
            if at != point {
                return false;
            }
            let staged = std::fs::read_dir(&root).is_ok_and(|entries| {
                entries
                    .flatten()
                    .any(|entry| entry.path().join(STAGED_STORE).exists())
            });
            if staged {
                fired.store(true, Ordering::SeqCst);
            }
            staged
        })
    };
    let mut options = options(&plenty);
    options.copy_probe = Some(probe);
    let error = capture_store(home.path(), &project, &options).unwrap_err();
    assert_eq!(stage_entries(home.path(), &project), Vec::<PathBuf>::new());
    (error, fired.load(Ordering::SeqCst))
}

#[test]
fn capture_with_a_deadline_shorter_than_the_copy_fails_typed_and_removes_its_stage() {
    // A real configured deadline that passes while the copy is under way:
    // inside the copy, once the staged file exists, the copy is held past
    // the deadline. Nothing fires the interrupt but the clock.
    let (home, project, _database) = fixture(3, 64);
    let deadline = Duration::from_secs(5);
    let began = Instant::now();
    let staged_existed = Arc::new(AtomicBool::new(false));
    let probe: Arc<dyn Fn(CopyProbePoint) -> bool + Send + Sync> = {
        let staged_existed = staged_existed.clone();
        let root = home
            .path()
            .join(STAGE_DIRECTORY)
            .join(crate::project_digest(&project));
        Arc::new(move |point| {
            if point != CopyProbePoint::Copy || staged_existed.load(Ordering::SeqCst) {
                return false;
            }
            let staged = std::fs::read_dir(&root).is_ok_and(|entries| {
                entries
                    .flatten()
                    .any(|entry| entry.path().join(STAGED_STORE).exists())
            });
            if staged {
                staged_existed.store(true, Ordering::SeqCst);
                // The capture's deadline started before its copy did, so a
                // whole deadline from here passes it, however the threads
                // were scheduled before this point.
                std::thread::sleep(deadline + Duration::from_millis(100));
            }
            false
        })
    };
    let mut options = options(&plenty);
    options.deadline = deadline;
    options.copy_probe = Some(probe);
    let error = capture_store(home.path(), &project, &options).unwrap_err();
    assert!(began.elapsed() >= deadline);
    assert!(
        staged_existed.load(Ordering::SeqCst),
        "the deadline passed before the staged copy existed"
    );
    assert_eq!(error.code(), "backup_capture_deadline", "{error}");
    assert!(
        matches!(
            error,
            BackupError::CaptureDeadline {
                phase: CapturePhase::Copy
            }
        ),
        "{error}"
    );
    assert_eq!(stage_entries(home.path(), &project), Vec::<PathBuf>::new());
}

#[test]
fn capture_past_its_deadline_at_any_step_fails_typed_and_removes_its_stage() {
    for (point, phase) in [
        (CopyProbePoint::Copy, CapturePhase::Copy),
        (CopyProbePoint::Settle, CapturePhase::Verify),
        (CopyProbePoint::Admit, CapturePhase::Verify),
        (CopyProbePoint::Scan, CapturePhase::Verify),
        (CopyProbePoint::Hash, CapturePhase::Verify),
    ] {
        let (error, fired) = capture_fired_at(point);
        assert!(
            fired,
            "{point:?}: the staged copy did not exist when it fired"
        );
        assert_eq!(
            error.code(),
            "backup_capture_deadline",
            "{point:?}: {error}"
        );
        assert!(
            matches!(error, BackupError::CaptureDeadline { phase: at } if at == phase),
            "{point:?}: {error}"
        );
    }
}

#[test]
fn capture_deadline_bounds_a_wait_for_a_locked_store() {
    let (home, project, database) = fixture(3, 64);
    // A connection in exclusive locking mode that has written keeps every
    // other connection, readers included, out of the store.
    let holder = rusqlite::Connection::open(&database).unwrap();
    holder
        .execute_batch(
            "PRAGMA locking_mode = EXCLUSIVE;
             BEGIN EXCLUSIVE;
             CREATE TABLE capture_lock_probe(value INTEGER);
             DROP TABLE capture_lock_probe;
             COMMIT;",
        )
        .unwrap();
    let mut options = options(&plenty);
    options.deadline = Duration::from_millis(1500);
    let began = Instant::now();
    let error = capture_store(home.path(), &project, &options).unwrap_err();
    let took = began.elapsed();
    drop(holder);
    assert_eq!(error.code(), "backup_capture_deadline", "{error}");
    // The ordinary five-second lock wait was cut to the time left.
    assert!(took < Duration::from_secs(5), "{took:?}");
    assert_eq!(stage_entries(home.path(), &project), Vec::<PathBuf>::new());

    // A deadline too large for the clock means no deadline, not a panic.
    options.deadline = Duration::MAX;
    capture_store(home.path(), &project, &options)
        .unwrap()
        .discard()
        .unwrap();
}

/// The capture that the test below runs in a child process and kills: inside
/// the copy's read transaction it writes its ready file and waits. A child
/// whose test never kills it ends after two minutes, without cleanup. It runs
/// only in that exact filtered child.
#[test]
fn capture_killed_during_its_copy_process() {
    let Some(home) = std::env::var_os("ENGRAM_KILLED_CAPTURE_HOME") else {
        return;
    };
    let home = PathBuf::from(home);
    let ready = home.join("killed-capture-ready");
    let probe: Arc<dyn Fn(CopyProbePoint) -> bool + Send + Sync> = Arc::new(move |point| {
        if point == CopyProbePoint::Copy {
            std::fs::write(&ready, b"copying").unwrap();
            std::thread::sleep(Duration::from_secs(120));
            std::process::exit(3);
        }
        false
    });
    let mut options = options(&plenty);
    options.copy_probe = Some(probe);
    let capture = capture_store(&home, &ProjectId("capture-project".into()), &options);
    panic!("the capture ended before its copy was killed: {capture:?}");
}

#[test]
fn a_capture_killed_during_its_copy_leaves_the_live_store_usable_and_only_a_stage() {
    let (home, project, database) = fixture(3, 64);
    let ready = home.path().join("killed-capture-ready");
    let mut child = std::process::Command::new(std::env::current_exe().unwrap())
        .args([
            "--exact",
            "backup::tests::capture_killed_during_its_copy_process",
            "--nocapture",
        ])
        .env("ENGRAM_KILLED_CAPTURE_HOME", home.path())
        .stdout(std::process::Stdio::null())
        .spawn()
        .unwrap();
    let give_up = Instant::now() + Duration::from_secs(60);
    let stopped = loop {
        if ready.exists() {
            break Ok(());
        }
        if let Some(status) = child.try_wait().unwrap() {
            break Err(format!("the capture ended before its copy: {status}"));
        }
        if Instant::now() >= give_up {
            break Err("the capture never reached its copy".to_owned());
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let _ = child.kill();
    child.wait().unwrap();
    if let Err(reason) = stopped {
        panic!("{reason}");
    }

    // What the kill left in the stage is one attempt directory holding only
    // files a capture writes, which a push removes before its own capture.
    let stages = stage_entries(home.path(), &project);
    assert_eq!(stages.len(), 1, "{stages:?}");
    for entry in std::fs::read_dir(&stages[0]).unwrap() {
        let name = entry.unwrap().file_name().into_string().unwrap();
        assert!(
            [
                "store.db",
                "store.db-journal",
                "store.db-wal",
                "store.db-shm"
            ]
            .contains(&name.as_str()),
            "{name}"
        );
    }

    // The live store still opens, takes a write and can be captured whole.
    let mut store = SqliteStore::open(&database).unwrap();
    remember(&mut store, &project, "after-the-kill", "written".into());
    drop(store);
    let capture = capture_store(home.path(), &project, &options(&plenty)).unwrap();
    assert_eq!(capture.manifest.cut, raw_cut(&database, &project));
    capture.discard().unwrap();
}
