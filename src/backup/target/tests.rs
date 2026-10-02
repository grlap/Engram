use std::{
    path::Path,
    process::{Child, Command},
    time::{Duration, Instant},
};

use chrono::TimeZone;

use super::*;
use crate::backup::{
    CaptureManifest,
    record::{
        Acknowledgement, AttemptOutcome, CopyConfirmed, CopyMissing, CopyRef, Encoding, OffHost,
        STORED_FORMAT_VERSION, StoredManifest,
    },
};
use crate::test_support::temp_home;

fn at(second: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 2, 12, 0, 0)
        .single()
        .expect("fixed test timestamp")
        + chrono::Duration::seconds(second)
}

fn project() -> ProjectId {
    ProjectId("backup-target-project".into())
}

fn absolute_dir() -> PathBuf {
    if cfg!(windows) {
        PathBuf::from(r"D:\engram-copies")
    } else {
        PathBuf::from("/srv/engram-copies")
    }
}

fn request() -> TargetRequest {
    TargetRequest {
        kind: CopyKind::Store,
        adapter: AdapterKind::Directory,
        dir: absolute_dir(),
        disclosure_authorized_by: "greg".into(),
        off_host_asserted_by: Some("greg".into()),
        window_hours: DEFAULT_WINDOW_HOURS,
        keep: DEFAULT_KEEP,
    }
}

#[test]
fn target_identity_changes_with_each_input() {
    let disclosure = Statement {
        by: "greg".into(),
        at: at(0),
    };
    let off_host = Statement {
        by: "greg".into(),
        at: at(0),
    };
    let base = IdentityInput {
        project: "p",
        kind: "store",
        adapter: "directory",
        location: "D:\\copies",
        disclosure_authorized: &disclosure,
        off_host_asserted: Some(&off_host),
    };
    let other_disclosure_by = Statement {
        by: "ann".into(),
        ..disclosure.clone()
    };
    let other_disclosure_at = Statement {
        at: at(1),
        ..disclosure.clone()
    };
    let other_off_host_by = Statement {
        by: "ann".into(),
        ..off_host.clone()
    };
    let other_off_host_at = Statement {
        at: at(1),
        ..off_host.clone()
    };
    let variants = [
        IdentityInput {
            project: "q",
            ..base
        },
        IdentityInput {
            kind: "graph",
            ..base
        },
        IdentityInput {
            adapter: "git-ref",
            ..base
        },
        IdentityInput {
            location: "E:\\copies",
            ..base
        },
        IdentityInput {
            disclosure_authorized: &other_disclosure_by,
            ..base
        },
        IdentityInput {
            disclosure_authorized: &other_disclosure_at,
            ..base
        },
        IdentityInput {
            off_host_asserted: Some(&other_off_host_by),
            ..base
        },
        IdentityInput {
            off_host_asserted: Some(&other_off_host_at),
            ..base
        },
        IdentityInput {
            off_host_asserted: None,
            ..base
        },
    ];
    let reference = target_identity(&base).unwrap();
    assert_eq!(target_identity(&base).unwrap(), reference);
    let mut seen = vec![reference];
    for variant in &variants {
        let identity = target_identity(variant).unwrap();
        assert!(!seen.contains(&identity), "{variant:?}");
        seen.push(identity);
    }
}

#[test]
fn set_show_and_clear_a_target() {
    let home = temp_home().unwrap();
    assert_eq!(show_targets(home.path(), &project()).unwrap(), Vec::new());
    assert!(!home.path().join(RECORDS_DIRECTORY).exists());

    let view = set_target(home.path(), &project(), &request(), at(5)).unwrap();
    let config = &view.config;
    assert_eq!(config.window_hours, 24);
    assert_eq!(config.keep, 3);
    assert_eq!(config.disclosure_authorized.at, at(5));
    assert_eq!(
        config
            .off_host_asserted
            .as_ref()
            .map(|statement| &statement.by),
        Some(&"greg".to_owned())
    );
    assert_eq!(view.identity, config.identity().unwrap());
    assert!(view.state_recorded);
    assert_eq!(
        show_targets(home.path(), &project()).unwrap(),
        std::slice::from_ref(&view)
    );
    let paths = RecordPaths::new(home.path(), &project(), CopyKind::Store);
    let state: TargetState = serde_json::from_slice(&std::fs::read(&paths.state).unwrap()).unwrap();
    assert_eq!(state.target_identity, view.identity);
    // The configuration file holds no identity; it is derived when read.
    let stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&paths.config).unwrap()).unwrap();
    assert!(stored.get("identity").is_none(), "{stored}");

    // A later set with another statement time names another target.
    let again = set_target(home.path(), &project(), &request(), at(6)).unwrap();
    assert_ne!(again.identity, view.identity);

    assert!(clear_target(home.path(), &project(), CopyKind::Store).unwrap());
    assert_eq!(show_targets(home.path(), &project()).unwrap(), Vec::new());
    assert!(!paths.config.exists());
    assert!(!paths.state.exists());
    // The lock file stays, so every process keeps locking the same file.
    assert!(paths.lock.exists());
    assert!(!clear_target(home.path(), &project(), CopyKind::Store).unwrap());
}

#[test]
fn set_refuses_what_the_operator_did_not_state() {
    let home = temp_home().unwrap();
    let refused = |request: TargetRequest| {
        let error = set_target(home.path(), &project(), &request, at(0)).unwrap_err();
        assert_eq!(error.code(), "backup_target_invalid", "{error}");
        error.to_string()
    };
    let mut relative = request();
    relative.dir = PathBuf::from("copies");
    assert!(refused(relative).contains("absolute"));
    if cfg!(windows) {
        for spelling in [r"C:copies", r"\copies"] {
            let mut request = request();
            request.dir = PathBuf::from(spelling);
            assert!(refused(request).contains("absolute"), "{spelling}");
        }
    }
    let mut no_off_host = request();
    no_off_host.off_host_asserted_by = None;
    assert!(refused(no_off_host).contains("--off-host-asserted-by"));
    for blank in ["", "  ", "gr\u{7}eg"] {
        let mut request = request();
        request.disclosure_authorized_by = blank.into();
        assert!(refused(request).contains("--disclosure-authorized-by"));
        let mut request = self::request();
        request.off_host_asserted_by = Some(blank.into());
        assert!(refused(request).contains("--off-host-asserted-by"));
    }
    let mut no_window = request();
    no_window.window_hours = 0;
    assert!(refused(no_window).contains("--window-hours"));
    let mut no_keep = request();
    no_keep.keep = 0;
    assert!(refused(no_keep).contains("--keep"));
    assert!(!home.path().join(RECORDS_DIRECTORY).exists());

    if cfg!(windows) {
        let mut share = request();
        share.dir = PathBuf::from(r"\\backup-host\engram\copies");
        let view = set_target(home.path(), &project(), &share, at(1)).unwrap();
        assert_eq!(view.config.dir, r"\\backup-host\engram\copies");
    }
}

#[test]
fn records_this_build_cannot_use_are_refused_untouched_until_set_writes_them_anew() {
    let home = temp_home().unwrap();
    let paths = RecordPaths::new(home.path(), &project(), CopyKind::Store);
    let written = set_target(home.path(), &project(), &request(), at(0)).unwrap();
    let state = std::fs::read(&paths.state).unwrap();
    let config = std::fs::read(&paths.config).unwrap();
    // Configurations `set` would have refused, written by hand.
    let edited = |edit: &dyn Fn(&mut TargetConfig)| {
        let mut config = written.config.clone();
        edit(&mut config);
        serde_json::to_vec(&config).unwrap()
    };
    let no_keep = edited(&|config| config.keep = 0);
    let no_window = edited(&|config| config.window_hours = 0);
    let relative = edited(&|config| config.dir = "copies".into());
    let no_off_host = edited(&|config| config.off_host_asserted = None);
    let blank_name = edited(&|config| config.disclosure_authorized.by = " ".into());
    // A state recorded for another target, as a partial set leaves it.
    let other_state = serde_json::to_vec(&TargetState::empty(
        set_target(home.path(), &project(), &request(), at(9))
            .unwrap()
            .identity,
    ))
    .unwrap();
    let cases: Vec<(&PathBuf, Vec<u8>)> = vec![
        (&paths.config, b"{\"format_version\": 2}".to_vec()),
        (&paths.config, b"not json".to_vec()),
        (&paths.config, no_keep),
        (&paths.config, no_window),
        (&paths.config, relative),
        (&paths.config, no_off_host),
        (&paths.config, blank_name),
        (
            &paths.state,
            b"{\"format_version\": 7, \"anything\": 1}".to_vec(),
        ),
        (&paths.state, b"{}".to_vec()),
        (
            &paths.state,
            b"{\"format_version\": 1, \"unknown\": true}".to_vec(),
        ),
        (&paths.state, other_state),
    ];
    for (path, bytes) in cases {
        std::fs::write(&paths.config, &config).unwrap();
        std::fs::write(&paths.state, &state).unwrap();
        std::fs::write(path, &bytes).unwrap();
        let before = (
            std::fs::read(&paths.config).unwrap(),
            std::fs::read(&paths.state).unwrap(),
        );
        let label = String::from_utf8_lossy(&bytes).into_owned();
        let error = show_targets(home.path(), &project()).unwrap_err();
        assert_eq!(error.code(), "backup_record_unreadable", "{label}: {error}");
        assert!(
            error.to_string().contains(&path.display().to_string()),
            "{label}: {error}"
        );
        let error = clear_target(home.path(), &project(), CopyKind::Store).unwrap_err();
        assert_eq!(error.code(), "backup_record_unreadable", "{label}: {error}");
        let after = (
            std::fs::read(&paths.config).unwrap(),
            std::fs::read(&paths.state).unwrap(),
        );
        assert_eq!(after, before, "{label}");

        // The stated way on: set writes both files for the kind anew.
        let renewed = set_target(home.path(), &project(), &request(), at(1)).unwrap();
        assert_eq!(show_targets(home.path(), &project()).unwrap(), [renewed]);
    }

    // A record that exists but cannot be read at all, here because a
    // directory stands in its place, is refused rather than taken as absent,
    // by show and by clear, and stays where it is.
    for path in [&paths.config, &paths.state] {
        std::fs::write(&paths.config, &config).unwrap();
        std::fs::write(&paths.state, &state).unwrap();
        std::fs::remove_file(path).unwrap();
        std::fs::create_dir(path).unwrap();
        for error in [
            show_targets(home.path(), &project()).unwrap_err(),
            clear_target(home.path(), &project(), CopyKind::Store).unwrap_err(),
        ] {
            assert_eq!(error.code(), "backup_record_unreadable", "{error}");
            assert!(error.to_string().contains(&path.display().to_string()));
        }
        assert!(path.is_dir());
        std::fs::remove_dir(path).unwrap();
    }

    // An unreadable state refuses even with no configuration beside it.
    std::fs::remove_file(&paths.config).unwrap();
    std::fs::write(&paths.state, b"[]").unwrap();
    let error = show_targets(home.path(), &project()).unwrap_err();
    assert_eq!(error.code(), "backup_record_unreadable", "{error}");
}

/// Holds the push lock in a child process until it is killed, or until its
/// parent leaves a stop file or two minutes pass, so a holder whose test was
/// itself killed does not outlive it for long. It runs only in the exact
/// filtered child that the test below starts.
#[test]
fn push_lock_holder_process() {
    let Some(home) = std::env::var_os("ENGRAM_BACKUP_LOCK_HOLDER_HOME") else {
        return;
    };
    let home = PathBuf::from(home);
    let paths = RecordPaths::new(&home, &project(), CopyKind::Store);
    let _lock = PushLock::try_acquire(&paths).unwrap();
    std::fs::write(home.join("lock-held"), b"held").unwrap();
    let until = Instant::now() + Duration::from_secs(120);
    while Instant::now() < until && !home.join("lock-stop").exists() {
        std::thread::sleep(Duration::from_millis(50));
    }
}

/// Stops, kills and reaps the holder, whatever the test's outcome.
struct Holder {
    child: Child,
    stop: PathBuf,
}

impl Drop for Holder {
    fn drop(&mut self) {
        let _ = std::fs::write(&self.stop, b"stop");
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// Starts a holder of the push lock in another process and returns once it
/// holds the lock.
fn start_holder(home: &Path) -> Holder {
    let ready = home.join("lock-held");
    let stop = home.join("lock-stop");
    for leftover in [&ready, &stop] {
        match std::fs::remove_file(leftover) {
            Ok(()) => {}
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => panic!("{}: {error}", leftover.display()),
        }
    }
    let mut holder = Holder {
        child: Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "backup::target::tests::push_lock_holder_process",
                "--nocapture",
            ])
            .env("ENGRAM_BACKUP_LOCK_HOLDER_HOME", home)
            .spawn()
            .unwrap(),
        stop,
    };
    let give_up = Instant::now() + Duration::from_secs(60);
    while !ready.exists() {
        if let Some(status) = holder.child.try_wait().unwrap() {
            panic!("the lock holder ended before it held the lock: {status}");
        }
        assert!(
            Instant::now() < give_up,
            "the lock holder never held the lock"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    holder
}

/// Kills the holder and repeats `word` until the lock is free again. The
/// system releases a killed holder's lock, though not necessarily by the time
/// the wait returns: until it does, only the lock may refuse.
fn after_release<T>(mut holder: Holder, word: impl Fn() -> Result<T, TargetError>) -> T {
    holder.child.kill().unwrap();
    holder.child.wait().unwrap();
    let give_up = Instant::now() + Duration::from_secs(30);
    loop {
        match word() {
            Ok(value) => return value,
            Err(error) => {
                assert_eq!(error.code(), "backup_push_running", "{error}");
                assert!(
                    Instant::now() < give_up,
                    "the killed holder's lock was never released"
                );
                std::thread::sleep(Duration::from_millis(20));
            }
        }
    }
}

#[test]
fn target_words_refuse_while_another_process_holds_the_push_lock() {
    let home = temp_home().unwrap();

    // With no records at all, both words still refuse while the lock is held.
    let holder = start_holder(home.path());
    let error = set_target(home.path(), &project(), &request(), at(0)).unwrap_err();
    assert_eq!(error.code(), "backup_push_running", "{error}");
    let error = clear_target(home.path(), &project(), CopyKind::Store).unwrap_err();
    assert_eq!(error.code(), "backup_push_running", "{error}");
    assert_eq!(show_targets(home.path(), &project()).unwrap(), Vec::new());
    let view = after_release(holder, || {
        set_target(home.path(), &project(), &request(), at(1))
    });
    assert_eq!(view.config.disclosure_authorized.at, at(1));

    // With a target configured, nothing changes while the lock is held.
    let holder = start_holder(home.path());
    let error = set_target(home.path(), &project(), &request(), at(2)).unwrap_err();
    assert_eq!(error.code(), "backup_push_running", "{error}");
    let error = clear_target(home.path(), &project(), CopyKind::Store).unwrap_err();
    assert_eq!(error.code(), "backup_push_running", "{error}");
    assert_eq!(
        show_targets(home.path(), &project()).unwrap(),
        std::slice::from_ref(&view)
    );
    let view = after_release(holder, || {
        set_target(home.path(), &project(), &request(), at(3))
    });
    assert_eq!(view.config.disclosure_authorized.at, at(3));
    assert!(clear_target(home.path(), &project(), CopyKind::Store).unwrap());
}

fn stored_manifest(identity: &ObjectId, second: i64) -> StoredManifest {
    StoredManifest {
        format_version: STORED_FORMAT_VERSION,
        copy: format!("20261002T120000Z-copy-{second}"),
        target_identity: identity.clone(),
        encoding: Encoding::Gzip,
        stored_bytes: 1_000,
        capture: CaptureManifest {
            project_digest: crate::project_digest(&project()),
            kind: CopyKind::Store,
            cut: crate::WorkGraphSnapshotCut {
                work_feed: 7,
                project_memory: 3,
            },
            capture_started_at: at(second),
            bytes: 4_096,
            sha256: format!("{second:064x}"),
            format_identity: ObjectId::from_canonical_bytes(b"schema"),
            build_fingerprint: Some(ObjectId::from_canonical_bytes(b"build")),
            source_revision: Some("unavailable".into()),
            host_name: Some("test-host".into()),
        },
    }
}

fn receipt(identity: &ObjectId, second: i64) -> BackupReceipt {
    let manifest = stored_manifest(identity, second);
    BackupReceipt {
        sha256: manifest.capture.sha256.clone(),
        target_identity: identity.clone(),
        at: at(second + 1),
        acknowledgement: Acknowledgement::ReadBack,
        off_host: OffHost::Asserted,
        manifest,
    }
}

fn attempt(identity: &ObjectId, second: i64) -> Attempt {
    let manifest = stored_manifest(identity, second);
    Attempt {
        id: uuid::Uuid::from_u128(u128::try_from(second).unwrap()),
        data_file: format!("{}.db.gz", manifest.copy),
        temporary_data_file: format!(".{}.db.gz.tmp", manifest.copy),
        manifest,
    }
}

/// A state of its `own` identity with every field filled in, its receipts
/// and attempts made for `identity`.
fn full_state(own: &ObjectId, identity: &ObjectId) -> TargetState {
    TargetState {
        format_version: RECORD_FORMAT_VERSION,
        target_identity: own.clone(),
        newest_receipt: Some(receipt(identity, 20)),
        observed_equal_at: Some(at(30)),
        pending: Some(attempt(identity, 40)),
        last_attempt: Some(LastAttempt {
            started_at: at(41),
            ended_at: at(42),
            outcome: AttemptOutcome::Failed,
            code: Some("backup_target_unreachable".into()),
            message: Some("the target cannot be reached".into()),
        }),
        last_confirmation: Some(CopyConfirmed {
            copy: CopyRef::of(&receipt(identity, 20)),
            at: at(25),
        }),
        missing_copy: Some(CopyMissing {
            copy: CopyRef::of(&receipt(identity, 20)),
            at: at(26),
            reason: "the stored file is not at the target".into(),
        }),
        receipts: vec![receipt(identity, 10), receipt(identity, 20)],
        set_aside: vec![attempt(identity, 5)],
    }
}

#[test]
fn a_push_writes_the_whole_state_under_its_lock_and_reads_it_back() {
    let home = temp_home().unwrap();
    let paths = RecordPaths::new(home.path(), &project(), CopyKind::Store);
    let lock = PushLock::try_acquire(&paths).unwrap();
    // Without a target there is nothing to push.
    assert_eq!(
        read_for_push(&paths, &project(), CopyKind::Store, &lock).unwrap(),
        None
    );
    drop(lock);

    let view = set_target(home.path(), &project(), &request(), at(0)).unwrap();
    let lock = PushLock::try_acquire(&paths).unwrap();
    let records = read_for_push(&paths, &project(), CopyKind::Store, &lock)
        .unwrap()
        .unwrap();
    assert_eq!(records.identity, view.identity);
    assert_eq!(records.config, view.config);
    assert_eq!(records.state, TargetState::empty(view.identity.clone()));

    let state = full_state(&view.identity, &view.identity);
    write_state(&paths, &lock, &state).unwrap();
    let read = read_for_push(&paths, &project(), CopyKind::Store, &lock)
        .unwrap()
        .unwrap();
    assert_eq!(read.state, state);
    // Every field is in the file itself, not only in memory.
    let stored: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&paths.state).unwrap()).unwrap();
    for field in [
        "newest_receipt",
        "observed_equal_at",
        "pending",
        "last_attempt",
        "receipts",
        "set_aside",
    ] {
        assert!(!stored[field].is_null(), "{field}: {stored}");
    }
    assert_eq!(
        stored["pending"]["data_file"],
        state.pending.as_ref().unwrap().data_file
    );
    // No temporary file is left beside it.
    let mut files: Vec<_> = std::fs::read_dir(&paths.directory)
        .unwrap()
        .map(|entry| entry.unwrap().file_name().into_string().unwrap())
        .collect();
    files.sort();
    assert_eq!(
        files,
        ["store.lock", "store.state.json", "store.target.json"]
    );

    // The lock of another project does not let a push read or write this one.
    let other = RecordPaths::new(
        home.path(),
        &ProjectId("another-project".into()),
        CopyKind::Store,
    );
    let other_lock = PushLock::try_acquire(&other).unwrap();
    let error = write_state(
        &paths,
        &other_lock,
        &TargetState::empty(view.identity.clone()),
    )
    .unwrap_err();
    assert_eq!(error.code(), "backup_target_invalid", "{error}");
    let error = read_for_push(&paths, &project(), CopyKind::Store, &other_lock).unwrap_err();
    assert_eq!(error.code(), "backup_target_invalid", "{error}");
    assert_eq!(
        read_for_push(&paths, &project(), CopyKind::Store, &lock)
            .unwrap()
            .unwrap()
            .state,
        state
    );
}

#[test]
fn target_set_keeps_a_readable_state_under_its_earlier_identity_and_renews_an_unreadable_one() {
    let home = temp_home().unwrap();
    let paths = RecordPaths::new(home.path(), &project(), CopyKind::Store);
    let first = set_target(home.path(), &project(), &request(), at(0)).unwrap();
    let state = full_state(&first.identity, &first.identity);
    let lock = PushLock::try_acquire(&paths).unwrap();
    write_state(&paths, &lock, &state).unwrap();
    drop(lock);

    // Another operator authorizes the disclosure: a new target identity.
    let mut changed = request();
    changed.disclosure_authorized_by = "ann".into();
    let second = set_target(home.path(), &project(), &changed, at(1)).unwrap();
    assert_ne!(second.identity, first.identity);
    let lock = PushLock::try_acquire(&paths).unwrap();
    let kept = read_for_push(&paths, &project(), CopyKind::Store, &lock)
        .unwrap()
        .unwrap()
        .state;
    assert_eq!(kept.target_identity, second.identity);
    // Everything recorded before is still there and still names the
    // earlier identity, so none of it is the new target's.
    assert_eq!(
        kept,
        TargetState {
            target_identity: second.identity.clone(),
            ..state
        }
    );
    let newest = kept.newest_receipt.as_ref().unwrap();
    assert_eq!(newest.target_identity, first.identity);
    assert_eq!(newest.manifest.target_identity, first.identity);
    assert_eq!(
        kept.pending.as_ref().unwrap().manifest.target_identity,
        first.identity
    );
    drop(lock);
    assert_eq!(
        show_targets(home.path(), &project()).unwrap(),
        std::slice::from_ref(&second)
    );

    // A state whose content this build cannot use is started anew by set:
    // one that does not parse, one of a format version it does not know, and
    // ones whose fields do not match their version.
    let mut missing_field =
        serde_json::to_value(full_state(&second.identity, &second.identity)).unwrap();
    missing_field.as_object_mut().unwrap().remove("pending");
    let unusable: [(&str, Vec<u8>); 4] = [
        ("not json", b"not json".to_vec()),
        (
            "unknown format version",
            b"{\"format_version\": 7, \"anything\": 1}".to_vec(),
        ),
        (
            "unknown field",
            b"{\"format_version\": 1, \"unknown\": true}".to_vec(),
        ),
        ("missing field", serde_json::to_vec(&missing_field).unwrap()),
    ];
    for (second_offset, (label, bytes)) in (2_i64..).zip(unusable) {
        std::fs::write(&paths.state, bytes).unwrap();
        let renewed = set_target(home.path(), &project(), &request(), at(second_offset)).unwrap();
        let lock = PushLock::try_acquire(&paths).unwrap();
        assert_eq!(
            read_for_push(&paths, &project(), CopyKind::Store, &lock)
                .unwrap()
                .unwrap()
                .state,
            TargetState::empty(renewed.identity),
            "{label}"
        );
    }
}

#[test]
fn a_state_for_another_target_is_refused_while_earlier_identities_inside_it_are_not() {
    let home = temp_home().unwrap();
    let paths = RecordPaths::new(home.path(), &project(), CopyKind::Store);
    let view = set_target(home.path(), &project(), &request(), at(0)).unwrap();
    let earlier = ObjectId::from_canonical_bytes(b"an earlier target identity");
    let lock = PushLock::try_acquire(&paths).unwrap();

    // The state's own identity is the configured target's; what it recorded
    // for an earlier identity is accepted as history.
    let nested = full_state(&view.identity, &earlier);
    write_state(&paths, &lock, &nested).unwrap();
    assert_eq!(
        read_for_push(&paths, &project(), CopyKind::Store, &lock)
            .unwrap()
            .unwrap()
            .state,
        nested
    );
    drop(lock);
    assert!(show_targets(home.path(), &project()).is_ok());

    // A state whose own identity is another target's is refused, by a push
    // and by the target words, and stays as it was.
    let foreign = full_state(&earlier, &view.identity);
    let lock = PushLock::try_acquire(&paths).unwrap();
    write_state(&paths, &lock, &foreign).unwrap();
    let before = std::fs::read(&paths.state).unwrap();
    let error = read_for_push(&paths, &project(), CopyKind::Store, &lock).unwrap_err();
    assert_eq!(error.code(), "backup_record_unreadable", "{error}");
    drop(lock);
    let error = show_targets(home.path(), &project()).unwrap_err();
    assert_eq!(error.code(), "backup_record_unreadable", "{error}");
    assert_eq!(std::fs::read(&paths.state).unwrap(), before);
}

#[test]
fn a_state_missing_any_field_is_refused_rather_than_read_as_empty() {
    let home = temp_home().unwrap();
    let paths = RecordPaths::new(home.path(), &project(), CopyKind::Store);
    let view = set_target(home.path(), &project(), &request(), at(0)).unwrap();
    let full = serde_json::to_value(full_state(&view.identity, &view.identity)).unwrap();
    let lock = PushLock::try_acquire(&paths).unwrap();
    let mut cases: Vec<(String, serde_json::Value)> = Vec::new();
    for field in [
        "newest_receipt",
        "observed_equal_at",
        "pending",
        "last_attempt",
        "last_confirmation",
        "missing_copy",
        "receipts",
        "set_aside",
    ] {
        let mut state = full.clone();
        state.as_object_mut().unwrap().remove(field);
        cases.push((field.to_owned(), state));
    }
    for field in ["code", "message"] {
        let mut state = full.clone();
        state["last_attempt"].as_object_mut().unwrap().remove(field);
        cases.push((format!("last_attempt.{field}"), state));
    }
    // The same holds inside a recorded manifest.
    for field in ["build_fingerprint", "source_revision", "host_name"] {
        let mut state = full.clone();
        state["newest_receipt"]["manifest"]["capture"]
            .as_object_mut()
            .unwrap()
            .remove(field);
        cases.push((format!("newest_receipt.manifest.capture.{field}"), state));
    }
    for (label, state) in cases {
        std::fs::write(&paths.state, serde_json::to_vec(&state).unwrap()).unwrap();
        let error = read_for_push(&paths, &project(), CopyKind::Store, &lock).unwrap_err();
        assert_eq!(error.code(), "backup_record_unreadable", "{label}: {error}");
    }
    // Null is how a field says it holds nothing.
    let mut empty = full.clone();
    for field in [
        "newest_receipt",
        "observed_equal_at",
        "pending",
        "last_attempt",
        "last_confirmation",
        "missing_copy",
    ] {
        empty[field] = serde_json::Value::Null;
    }
    std::fs::write(&paths.state, serde_json::to_vec(&empty).unwrap()).unwrap();
    let state = read_for_push(&paths, &project(), CopyKind::Store, &lock)
        .unwrap()
        .unwrap()
        .state;
    assert_eq!(state.pending, None);
    assert_eq!(state.newest_receipt, None);
}

#[test]
fn target_set_refuses_rather_than_discards_a_state_it_cannot_read_at_all() {
    let home = temp_home().unwrap();
    let paths = RecordPaths::new(home.path(), &project(), CopyKind::Store);
    set_target(home.path(), &project(), &request(), at(0)).unwrap();
    let config = std::fs::read(&paths.config).unwrap();
    // A directory stands where the state file is: reading it fails for a
    // reason other than its content, so nothing is replaced.
    std::fs::remove_file(&paths.state).unwrap();
    std::fs::create_dir(&paths.state).unwrap();
    let error = set_target(home.path(), &project(), &request(), at(1)).unwrap_err();
    assert_eq!(error.code(), "backup_io", "{error}");
    assert!(paths.state.is_dir());
    assert_eq!(std::fs::read(&paths.config).unwrap(), config);
}

#[test]
fn evidence_that_names_another_copy_than_the_newest_receipt_s_is_refused() {
    let home = temp_home().unwrap();
    let paths = RecordPaths::new(home.path(), &project(), CopyKind::Store);
    let view = set_target(home.path(), &project(), &request(), at(0)).unwrap();
    let lock = PushLock::try_acquire(&paths).unwrap();
    let older = CopyRef::of(&receipt(&view.identity, 10));
    let cases: Vec<(&str, TargetState)> = vec![
        ("confirmation of an older copy", {
            let mut state = full_state(&view.identity, &view.identity);
            state.last_confirmation.as_mut().unwrap().copy = older.clone();
            state
        }),
        ("missing finding about an older copy", {
            let mut state = full_state(&view.identity, &view.identity);
            state.missing_copy.as_mut().unwrap().copy = older.clone();
            state
        }),
        ("evidence with no newest receipt", {
            let mut state = full_state(&view.identity, &view.identity);
            state.newest_receipt = None;
            state
        }),
    ];
    for (label, state) in cases {
        write_state(&paths, &lock, &state).unwrap();
        let error = read_for_push(&paths, &project(), CopyKind::Store, &lock).unwrap_err();
        assert_eq!(error.code(), "backup_record_unreadable", "{label}: {error}");
        assert!(
            error.to_string().contains("newest receipt"),
            "{label}: {error}"
        );
    }
    // Evidence that names the newest receipt's copy is read back as written.
    let good = full_state(&view.identity, &view.identity);
    write_state(&paths, &lock, &good).unwrap();
    assert_eq!(
        read_for_push(&paths, &project(), CopyKind::Store, &lock)
            .unwrap()
            .unwrap()
            .state,
        good
    );
}

#[test]
fn recording_a_receipt_confirms_it_and_clears_an_earlier_missing_finding() {
    let identity = ObjectId::from_canonical_bytes(b"a target");
    let mut state = TargetState::empty(identity.clone());
    let first = receipt(&identity, 10);
    state.record_receipt(first.clone(), at(9));
    assert_eq!(state.newest_receipt.as_ref(), Some(&first));
    assert_eq!(state.observed_equal_at, Some(at(9)));
    assert_eq!(
        state.last_confirmation,
        Some(CopyConfirmed {
            copy: CopyRef::of(&first),
            at: first.at,
        })
    );
    state.mark_newest_missing(at(30), "gone".into());
    assert_eq!(
        state.missing_copy.as_ref().unwrap().copy,
        CopyRef::of(&first)
    );
    // The last confirmation stays as recorded beside the finding.
    assert_eq!(state.last_confirmation.as_ref().unwrap().at, first.at);
    state.confirm_newest(at(40));
    assert_eq!(state.missing_copy, None);
    assert_eq!(state.last_confirmation.as_ref().unwrap().at, at(40));
    state.mark_newest_missing(at(50), "gone".into());
    let second = receipt(&identity, 60);
    state.record_receipt(second.clone(), at(59));
    assert_eq!(state.missing_copy, None);
    assert_eq!(state.last_confirmation.unwrap().copy, CopyRef::of(&second));
    assert_eq!(state.receipts, [first, second]);
}
