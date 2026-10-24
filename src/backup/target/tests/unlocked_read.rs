use super::*;
use crate::backup::freshness::{AcceptedFormats, KindRecords, Reason, kind_reason, kind_records};

fn files(paths: &RecordPaths) -> (Vec<u8>, Vec<u8>) {
    (
        fs::read(&paths.config).unwrap(),
        fs::read(&paths.state).unwrap(),
    )
}

fn configured(records: KindRecords) -> (TargetConfig, ObjectId, TargetState) {
    match records {
        KindRecords::Configured {
            config,
            identity,
            state,
        } => (*config, identity, *state),
        other => panic!("expected configured records, got {other:?}"),
    }
}

fn mismatch(records: KindRecords, paths: &RecordPaths) {
    match records {
        KindRecords::Unreadable { path, reason } => {
            assert_eq!(path, paths.state);
            assert!(reason.contains("recorded for another target"), "{reason}");
            assert!(
                reason.contains("a target set may be in progress"),
                "{reason}"
            );
            assert!(reason.contains("retry the read"), "{reason}");
        }
        other => panic!("expected an unresolved mismatch, got {other:?}"),
    }
}

#[test]
fn a_completed_set_between_the_status_readers_two_reads_reloads_both_records() {
    let home = temp_home().unwrap();
    let paths = RecordPaths::new(home.path(), &project(), CopyKind::Store);
    let first = set_target(home.path(), &project(), &request(), at(0)).unwrap();
    let old_state = full_state(&first.identity, &first.identity);
    let lock = PushLock::try_acquire(&paths).unwrap();
    write_state(&paths, &lock, &old_state).unwrap();
    drop(lock);

    let writer_home = home.path().to_path_buf();
    let hook = ReadHook::on_read(&paths, 1, move || {
        // The reader already retained config A. The real writer publishes B
        // fully before the reader fetches state, leaving A/B in that first pair.
        set_target(&writer_home, &project(), &request(), at(1)).unwrap();
    });
    let actual = kind_records(home.path(), &project(), CopyKind::Store);
    assert_eq!(hook.reads(), 2);
    let (config, identity, state) = configured(actual.clone());
    assert_eq!(config.disclosure_authorized.at, at(1));
    assert_eq!(config.off_host_asserted.as_ref().unwrap().at, at(1));
    assert_ne!(identity, first.identity);
    assert_eq!(config.identity().unwrap(), identity);
    assert_eq!(
        state,
        TargetState {
            target_identity: identity,
            ..old_state
        }
    );
    // Keeping A's historical receipts must not qualify them for target B.
    assert_eq!(
        kind_reason(
            CopyKind::Store,
            &actual,
            &AcceptedFormats { store: None },
            at(100)
        ),
        Some(Reason::TargetChanged)
    );
}

#[test]
fn publication_before_the_retry_is_read_without_contending_with_its_writer() {
    let home = temp_home().unwrap();
    let paths = RecordPaths::new(home.path(), &project(), CopyKind::Store);
    let first = set_target(home.path(), &project(), &request(), at(0)).unwrap();
    let mut next = first.config;
    next.disclosure_authorized.at = at(1);
    let identity = next.identity().unwrap();
    let state = TargetState::empty(identity.clone());
    let lock = PushLock::try_acquire(&paths).unwrap();
    write_state(&paths, &lock, &state).unwrap();
    let publish_path = paths.config.clone();
    let publish_config = next.clone();
    let hook = ReadHook::on_read(&paths, 1, move || {
        write_record(&publish_path, &publish_config).unwrap();
    });
    // Keep the real writer lock live. A reader taking it would self-contend,
    // including when this helper is used by restore's fetch path.
    let actual = configured(kind_records(home.path(), &project(), CopyKind::Store));
    assert_eq!(actual, (next, identity, state));
    assert_eq!(hook.reads(), 2);
    drop(hook);
    let before = files(&paths);
    let hook = ReadHook::count(&paths);
    let _ = configured(kind_records(home.path(), &project(), CopyKind::Store));
    assert_eq!(hook.reads(), 1, "a consistent pair needs no retry");
    assert_eq!(files(&paths), before);
    drop(lock);
}

#[test]
fn a_paused_publication_or_persistent_mismatch_stops_after_one_reread() {
    let home = temp_home().unwrap();
    let paths = RecordPaths::new(home.path(), &project(), CopyKind::Store);
    let first = set_target(home.path(), &project(), &request(), at(0)).unwrap();
    let mut next = first.config;
    next.disclosure_authorized.at = at(1);
    let lock = PushLock::try_acquire(&paths).unwrap();
    write_state(&paths, &lock, &TargetState::empty(next.identity().unwrap())).unwrap();
    let before = files(&paths);
    let hook = ReadHook::count(&paths);
    mismatch(
        kind_records(home.path(), &project(), CopyKind::Store),
        &paths,
    );
    assert_eq!(hook.reads(), 2);
    assert_eq!(files(&paths), before);
    drop(hook);
    // A caller holding the lock stays strict and keeps its original message.
    let error = read_for_push(&paths, &project(), CopyKind::Store, &lock).unwrap_err();
    assert_eq!(error.code(), "backup_record_unreadable");
    assert!(!error.to_string().contains("retry the read"));
    drop(lock);
    let hook = ReadHook::count(&paths);
    let error = read_kind_records(&paths, &project(), CopyKind::Store).unwrap_err();
    assert_eq!(error.code(), "backup_record_unreadable");
    assert!(error.to_string().contains("retry the read"));
    assert_eq!(hook.reads(), 2);
    assert_eq!(files(&paths), before);
}

#[test]
fn publication_after_the_retry_sampled_config_can_still_leave_a_mismatch() {
    let home = temp_home().unwrap();
    let paths = RecordPaths::new(home.path(), &project(), CopyKind::Store);
    let first = set_target(home.path(), &project(), &request(), at(0)).unwrap();
    let mut next = first.config;
    next.disclosure_authorized.at = at(1);
    let identity = next.identity().unwrap();
    let state = TargetState::empty(identity.clone());
    let lock = PushLock::try_acquire(&paths).unwrap();
    write_state(&paths, &lock, &state).unwrap();
    let publish_path = paths.config.clone();
    let publish_config = next.clone();
    let hook = ReadHook::on_read(&paths, 2, move || {
        write_record(&publish_path, &publish_config).unwrap();
    });
    mismatch(
        kind_records(home.path(), &project(), CopyKind::Store),
        &paths,
    );
    assert_eq!(
        hook.reads(),
        2,
        "no third pair read follows late publication"
    );
    drop(hook);
    assert_eq!(
        configured(kind_records(home.path(), &project(), CopyKind::Store)),
        (next, identity, state)
    );
    drop(lock);
}

#[test]
fn a_fresh_pair_error_is_not_replaced_with_the_original_mismatch() {
    let home = temp_home().unwrap();
    let paths = RecordPaths::new(home.path(), &project(), CopyKind::Store);
    let first = set_target(home.path(), &project(), &request(), at(0)).unwrap();
    let mut next = first.config;
    next.disclosure_authorized.at = at(1);
    let lock = PushLock::try_acquire(&paths).unwrap();
    write_state(&paths, &lock, &TargetState::empty(next.identity().unwrap())).unwrap();
    let config_path = paths.config.clone();
    let hook = ReadHook::on_read(&paths, 1, move || {
        fs::write(config_path, b"not json").unwrap();
    });
    match kind_records(home.path(), &project(), CopyKind::Store) {
        KindRecords::Unreadable { path, reason } => {
            assert_eq!(path, paths.config);
            assert!(reason.contains("not JSON"), "{reason}");
            assert!(!reason.contains("retry the read"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(hook.reads(), 1, "the retry fails before config is sampled");
    assert_eq!(fs::read(&paths.config).unwrap(), b"not json");
    drop(lock);
}

#[test]
fn the_retry_observes_an_absent_config_without_reusing_the_old_one() {
    let home = temp_home().unwrap();
    let paths = RecordPaths::new(home.path(), &project(), CopyKind::Store);
    let first = set_target(home.path(), &project(), &request(), at(0)).unwrap();
    let mut next = first.config;
    next.disclosure_authorized.at = at(1);
    let lock = PushLock::try_acquire(&paths).unwrap();
    write_state(&paths, &lock, &TargetState::empty(next.identity().unwrap())).unwrap();
    let state_before = fs::read(&paths.state).unwrap();
    let config_path = paths.config.clone();
    let hook = ReadHook::on_read(&paths, 1, move || {
        fs::remove_file(config_path).unwrap();
    });
    assert_eq!(
        kind_records(home.path(), &project(), CopyKind::Store),
        KindRecords::NotConfigured
    );
    assert_eq!(hook.reads(), 2);
    assert!(!paths.config.exists());
    assert_eq!(fs::read(&paths.state).unwrap(), state_before);
    drop(lock);
}

#[test]
fn ordinary_absence_and_invalid_content_keep_their_existing_meaning() {
    let home = temp_home().unwrap();
    let paths = RecordPaths::new(home.path(), &project(), CopyKind::Store);
    assert_eq!(
        kind_records(home.path(), &project(), CopyKind::Store),
        KindRecords::NotConfigured
    );
    assert!(
        !paths.directory.exists(),
        "a reader creates no directory or lock"
    );
    let first = set_target(home.path(), &project(), &request(), at(0)).unwrap();
    fs::remove_file(&paths.state).unwrap();
    let hook = ReadHook::count(&paths);
    assert_eq!(
        configured(kind_records(home.path(), &project(), CopyKind::Store)),
        (
            first.config.clone(),
            first.identity.clone(),
            TargetState::empty(first.identity.clone())
        )
    );
    assert_eq!(hook.reads(), 1);
    drop(hook);

    let mut invalid_evidence = full_state(&first.identity, &first.identity);
    invalid_evidence.newest_receipt = None;
    let cases = [
        b"not json".to_vec(),
        serde_json::to_vec(&invalid_evidence).unwrap(),
    ];
    for bytes in cases {
        fs::write(&paths.state, &bytes).unwrap();
        let error = read_kind_records(&paths, &project(), CopyKind::Store).unwrap_err();
        assert_eq!(error.code(), "backup_record_unreadable");
        assert!(!error.to_string().contains("retry the read"));
        assert_eq!(fs::read(&paths.state).unwrap(), bytes);
    }
}
