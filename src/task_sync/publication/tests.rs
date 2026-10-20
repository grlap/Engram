use super::*;
use std::fs;

fn stage(root: &Path) -> std::path::PathBuf {
    let staging = root.join("staging");
    fs::create_dir(&staging).unwrap();
    fs::write(staging.join("candidate"), b"complete candidate").unwrap();
    staging
}

fn assert_unsupported(error: &anyhow::Error) {
    let refusal = error.downcast_ref::<crate::storage::StoreError>().unwrap();
    assert_eq!(
        crate::store_error_value(refusal)["error"]["code"],
        "task_sync_publication_unsupported"
    );
}

#[test]
fn task_sync_directory_publication_moves_the_complete_stage() {
    let fixture = crate::test_support::temp_home().unwrap();
    let staging = stage(fixture.path());
    let destination = fixture.path().join("destination");
    publish(&staging, &destination).unwrap();
    assert!(!staging.exists());
    assert_eq!(
        fs::read(destination.join("candidate")).unwrap(),
        b"complete candidate"
    );
    assert_eq!(fs::read_dir(fixture.path()).unwrap().count(), 1);
}

#[test]
fn task_sync_directory_publication_preserves_an_empty_destination_at_the_move() {
    let fixture = crate::test_support::temp_home().unwrap();
    let staging = stage(fixture.path());
    let destination = fixture.path().join("destination");
    assert!(!destination.exists());
    let mut witness = None;
    let error = publish_with(&staging, &destination, |source, target| {
        // Insert the collision at the operation itself, after any caller's
        // preflight. The directory's identity must survive, even while empty.
        fs::create_dir(target)?;
        witness = Some(same_file::Handle::from_path(target)?);
        native_move(source, target)
    })
    .unwrap_err();
    assert!(error.downcast_ref::<io::Error>().is_some());
    assert_eq!(
        witness.unwrap(),
        same_file::Handle::from_path(&destination).unwrap()
    );
    assert_eq!(fs::read_dir(&destination).unwrap().count(), 0);
    assert_eq!(
        fs::read(staging.join("candidate")).unwrap(),
        b"complete candidate"
    );
}

#[cfg(any(unix, windows))]
#[test]
fn task_sync_directory_publication_preserves_a_dangling_link_at_the_move() {
    let fixture = crate::test_support::temp_home().unwrap();
    let staging = stage(fixture.path());
    let destination = fixture.path().join("destination");
    let missing = fixture.path().join("missing");
    let error = publish_with(&staging, &destination, |source, target| {
        #[cfg(unix)]
        std::os::unix::fs::symlink(&missing, target)?;
        #[cfg(windows)]
        std::os::windows::fs::symlink_dir(&missing, target)?;
        native_move(source, target)
    })
    .unwrap_err();
    assert!(error.downcast_ref::<io::Error>().is_some());
    assert_eq!(fs::read_link(&destination).unwrap(), missing);
    assert!(!missing.exists());
    assert_eq!(
        fs::read(staging.join("candidate")).unwrap(),
        b"complete candidate"
    );
}

#[test]
fn task_sync_directory_publication_unsupported_move_preserves_both_paths() {
    let fixture = crate::test_support::temp_home().unwrap();
    let staging = stage(fixture.path());
    let destination = fixture.path().join("destination");
    for occupied in [false, true] {
        if occupied {
            fs::create_dir(&destination).unwrap();
            fs::write(destination.join("sentinel"), b"existing bytes").unwrap();
        }
        let mut calls = 0;
        let error = publish_with(&staging, &destination, |_, _| {
            calls += 1;
            Err(io::ErrorKind::Unsupported.into())
        })
        .unwrap_err();
        assert_eq!(calls, 1);
        assert_unsupported(&error);
        assert!(error.to_string().contains("without replacement"));
        assert_eq!(
            fs::read(staging.join("candidate")).unwrap(),
            b"complete candidate"
        );
        if occupied {
            assert_eq!(
                fs::read(destination.join("sentinel")).unwrap(),
                b"existing bytes"
            );
            assert_eq!(fs::read_dir(&destination).unwrap().count(), 1);
        } else {
            assert!(!destination.exists());
        }
    }
}

#[test]
fn task_sync_directory_publication_keeps_io_failures_distinct() {
    for kind in [
        io::ErrorKind::PermissionDenied,
        io::ErrorKind::AlreadyExists,
        io::ErrorKind::NotFound,
        io::ErrorKind::CrossesDevices,
        io::ErrorKind::Interrupted,
    ] {
        let error = publish_with(Path::new("stage"), Path::new("destination"), |_, _| {
            Err(io::Error::new(kind, "original failure"))
        })
        .unwrap_err();
        assert_eq!(error.downcast_ref::<io::Error>().unwrap().kind(), kind);
        assert_eq!(error.to_string(), "original failure");
    }
}

#[test]
fn task_sync_directory_publication_preserves_occupied_destination_bytes() {
    let fixture = crate::test_support::temp_home().unwrap();
    let staging = stage(fixture.path());
    for is_directory in [false, true] {
        let destination = fixture
            .path()
            .join(if is_directory { "directory" } else { "file" });
        let sentinel = if is_directory {
            fs::create_dir(&destination).unwrap();
            destination.join("sentinel")
        } else {
            destination.clone()
        };
        fs::write(&sentinel, b"existing bytes").unwrap();
        assert!(publish(&staging, &destination).is_err());
        assert_eq!(fs::read(&sentinel).unwrap(), b"existing bytes");
        assert_eq!(
            fs::read(staging.join("candidate")).unwrap(),
            b"complete candidate"
        );
        if is_directory {
            assert_eq!(fs::read_dir(&destination).unwrap().count(), 1);
        }
    }
}

#[test]
fn task_sync_directory_publication_concurrent_writers_keep_one_complete_candidate() {
    let fixture = crate::test_support::temp_home().unwrap();
    let destination = fixture.path().join("destination");
    let starts = std::sync::Barrier::new(2);
    let stages: Vec<_> = [b"first".as_slice(), b"second".as_slice()]
        .into_iter()
        .enumerate()
        .map(|(index, bytes)| {
            let staging = fixture.path().join(format!("staging-{index}"));
            fs::create_dir(&staging).unwrap();
            fs::write(staging.join("candidate"), bytes).unwrap();
            staging
        })
        .collect();
    let results = std::thread::scope(|scope| {
        let handles: Vec<_> = stages
            .iter()
            .map(|staging| {
                let destination = &destination;
                let starts = &starts;
                scope.spawn(move || {
                    starts.wait();
                    publish(staging, destination)
                })
            })
            .collect();
        handles
            .into_iter()
            .map(|thread| thread.join().unwrap())
            .collect::<Vec<_>>()
    });
    assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
    let winner = results.iter().position(Result::is_ok).unwrap();
    let loser = 1 - winner;
    let expected = [b"first".as_slice(), b"second".as_slice()];
    assert_eq!(
        fs::read(destination.join("candidate")).unwrap(),
        expected[winner]
    );
    assert_eq!(fs::read_dir(&destination).unwrap().count(), 1);
    assert!(!stages[winner].exists());
    assert_eq!(
        fs::read(stages[loser].join("candidate")).unwrap(),
        expected[loser]
    );
}

#[cfg(unix)]
#[test]
fn task_sync_directory_publication_reports_missing_native_capability_per_operation() {
    for errno in [
        rustix::io::Errno::INVAL,
        rustix::io::Errno::NOSYS,
        rustix::io::Errno::OPNOTSUPP,
        #[cfg(not(target_os = "redox"))]
        rustix::io::Errno::NOTSUP,
    ] {
        let error = publish_with(Path::new("stage"), Path::new("destination"), |_, _| {
            Err(errno.into())
        })
        .unwrap_err();
        assert_unsupported(&error);
    }
}
