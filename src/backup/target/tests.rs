use std::{
    path::Path,
    process::{Child, Command},
    time::{Duration, Instant},
};

use chrono::TimeZone;

use super::*;
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
    let other_state = serde_json::to_vec(&TargetState {
        format_version: RECORD_FORMAT_VERSION,
        target_identity: set_target(home.path(), &project(), &request(), at(9))
            .unwrap()
            .identity,
    })
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
