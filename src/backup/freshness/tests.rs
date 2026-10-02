use chrono::TimeZone;

use super::*;
use crate::backup::{
    CaptureManifest,
    record::{
        Acknowledgement, AttemptOutcome, BackupReceipt, Encoding, LastAttempt, OffHost,
        STORED_FORMAT_VERSION, StoredManifest,
    },
    target::{
        AdapterKind, PushLock, RECORD_FORMAT_VERSION, Statement, TargetRequest, set_target,
        write_state,
    },
};
use crate::test_support::temp_home;

/// Hours after a fixed instant.
fn at(hours: f64) -> DateTime<Utc> {
    #[expect(
        clippy::cast_possible_truncation,
        reason = "test offsets are small whole seconds"
    )]
    let seconds = (hours * 3600.0).round() as i64;
    Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0)
        .single()
        .expect("fixed test timestamp")
        + Duration::seconds(seconds)
}

fn project() -> ProjectId {
    ProjectId("freshness-project".into())
}

fn schema() -> ObjectId {
    ObjectId::from_canonical_bytes(b"store schema")
}

fn formats() -> AcceptedFormats {
    AcceptedFormats {
        store: Some(schema()),
    }
}

fn config() -> TargetConfig {
    TargetConfig {
        format_version: RECORD_FORMAT_VERSION,
        project: project().0,
        kind: CopyKind::Store,
        adapter: AdapterKind::Directory,
        dir: if cfg!(windows) {
            r"D:\copies".into()
        } else {
            "/srv/copies".into()
        },
        window_hours: 24,
        keep: 3,
        disclosure_authorized: Statement {
            by: "greg".into(),
            at: at(0.0),
        },
        off_host_asserted: Some(Statement {
            by: "greg".into(),
            at: at(0.0),
        }),
    }
}

fn receipt(identity: &ObjectId, captured: DateTime<Utc>, received: DateTime<Utc>) -> BackupReceipt {
    BackupReceipt {
        sha256: "a".repeat(64),
        target_identity: identity.clone(),
        at: received,
        acknowledgement: Acknowledgement::ReadBack,
        off_host: OffHost::Asserted,
        manifest: StoredManifest {
            format_version: STORED_FORMAT_VERSION,
            copy: "20261001T000000Z-copy".into(),
            target_identity: identity.clone(),
            encoding: Encoding::Gzip,
            stored_bytes: 100,
            capture: CaptureManifest {
                project_digest: crate::project_digest(&project()),
                kind: CopyKind::Store,
                cut: WorkGraphSnapshotCut {
                    work_feed: 7,
                    project_memory: 3,
                },
                capture_started_at: captured,
                bytes: 4096,
                sha256: "a".repeat(64),
                format_identity: schema(),
                build_fingerprint: None,
                source_revision: Some("unavailable".into()),
                host_name: None,
            },
        },
    }
}

/// Records of a copy captured at hour 9, put and confirmed at hour 10, for
/// a 24-hour window: qualifying at hour 20.
fn qualifying() -> (TargetConfig, ObjectId, TargetState) {
    let config = config();
    let identity = config.identity().unwrap();
    let mut state = TargetState::empty(identity.clone());
    state.record_receipt(receipt(&identity, at(9.0), at(10.0)), at(9.0));
    state.last_attempt = Some(LastAttempt {
        started_at: at(9.0),
        ended_at: at(10.0),
        outcome: AttemptOutcome::Uploaded,
        code: None,
        message: None,
    });
    (config, identity, state)
}

fn records(config: TargetConfig, identity: ObjectId, state: TargetState) -> KindRecords {
    KindRecords::Configured {
        config: Box::new(config),
        identity,
        state: Box::new(state),
    }
}

fn reason_of(
    records: &KindRecords,
    formats: &AcceptedFormats,
    now: DateTime<Utc>,
) -> Option<Reason> {
    kind_reason(CopyKind::Store, records, formats, now)
}

#[test]
fn the_rule_gives_each_reason_and_local_backed_up_in_the_order_of_the_brief() {
    let now = at(20.0);
    let other_identity = ObjectId::from_canonical_bytes(b"another target");
    let (config, identity, base) = qualifying();
    let with = |edit: &dyn Fn(&mut TargetState)| {
        let mut state = base.clone();
        edit(&mut state);
        records(config.clone(), identity.clone(), state)
    };
    let cases: Vec<(&str, KindRecords, AcceptedFormats, Option<Reason>)> = vec![
        (
            "a fresh, confirmed copy",
            records(config.clone(), identity.clone(), base.clone()),
            formats(),
            None,
        ),
        (
            "an unreadable record",
            KindRecords::Unreadable {
                path: PathBuf::from("store.state.json"),
                reason: "it is not JSON".into(),
            },
            formats(),
            Some(Reason::RecordUnreadable),
        ),
        (
            "no target",
            KindRecords::NotConfigured,
            formats(),
            Some(Reason::NotConfigured),
        ),
        (
            "a target and no receipt",
            records(
                config.clone(),
                identity.clone(),
                TargetState::empty(identity.clone()),
            ),
            formats(),
            Some(Reason::NeverConfirmed),
        ),
        (
            "a receipt for an earlier identity",
            with(&|state| {
                let earlier = receipt(&other_identity, at(9.0), at(10.0));
                *state = TargetState::empty(identity.clone());
                state.record_receipt(earlier, at(9.0));
            }),
            formats(),
            Some(Reason::TargetChanged),
        ),
        (
            "a format the running build does not accept",
            records(config.clone(), identity.clone(), base.clone()),
            AcceptedFormats {
                store: Some(ObjectId::from_canonical_bytes(b"another schema")),
            },
            Some(Reason::OtherFormat),
        ),
        (
            "a running build that names no format",
            records(config.clone(), identity.clone(), base.clone()),
            AcceptedFormats { store: None },
            Some(Reason::OtherFormat),
        ),
        (
            "a future timestamp",
            with(&|state| {
                state.last_attempt.as_mut().unwrap().ended_at = at(21.0);
            }),
            formats(),
            Some(Reason::ClockInvalid),
        ),
        (
            "a copy the target no longer holds",
            with(&|state| state.mark_newest_missing(at(15.0), "gone".into())),
            formats(),
            Some(Reason::CopyMissing),
        ),
        (
            "a confirmation and a capture both older than the window",
            with(&|state| {
                state.last_confirmation.as_mut().unwrap().at = at(20.0 - 25.0);
                state.observed_equal_at = Some(at(20.0 - 30.0));
            }),
            formats(),
            Some(Reason::ConfirmationExpired),
        ),
        (
            "an old store copy on an unchanged store, confirmed a minute ago",
            with(&|state| {
                state.observed_equal_at = Some(at(20.0 - 48.0));
                state.confirm_newest(at(20.0 - 1.0 / 60.0));
            }),
            formats(),
            Some(Reason::Stale),
        ),
    ];
    for (label, records, formats, expected) in &cases {
        assert_eq!(reason_of(records, formats, now), *expected, "{label}");
    }
    // Every reason of the brief's table appears, and each in its place: the
    // reasons are declared in the table's order.
    let mut seen: Vec<Reason> = cases.iter().filter_map(|case| case.3).collect();
    seen.dedup();
    assert_eq!(
        seen,
        [
            Reason::RecordUnreadable,
            Reason::NotConfigured,
            Reason::NeverConfirmed,
            Reason::TargetChanged,
            Reason::OtherFormat,
            Reason::ClockInvalid,
            Reason::CopyMissing,
            Reason::ConfirmationExpired,
            Reason::Stale,
        ]
    );
    let codes: Vec<_> = seen.iter().map(|reason| reason.code()).collect();
    assert_eq!(
        codes,
        [
            "backup_record_unreadable",
            "backup_not_configured",
            "backup_never_confirmed",
            "backup_target_changed",
            "backup_other_format",
            "backup_clock_invalid",
            "backup_copy_missing",
            "backup_confirmation_expired",
            "backup_stale",
        ]
    );

    // The mode: local_backed_up only when a kind qualifies.
    let qualifying = evaluate(&[(CopyKind::Store, cases[0].1.clone())], &formats(), now);
    assert_eq!(qualifying.mode, Mode::LocalBackedUp);
    assert_eq!(qualifying.mode.as_str(), "local_backed_up");
    assert!(qualifying.kinds[0].qualifies());
    let stale = evaluate(
        &[(CopyKind::Store, cases.last().unwrap().1.clone())],
        &formats(),
        now,
    );
    assert_eq!(stale.mode, Mode::Local);
    assert_eq!(stale.mode.as_str(), "local");
    assert_eq!(stale.kinds[0].reason, Some(Reason::Stale));
}

#[test]
fn the_first_reason_that_applies_is_the_one_reported() {
    let now = at(20.0);
    let (config, identity, mut state) = qualifying();
    // Missing, expired and stale at once: missing comes first.
    state.mark_newest_missing(at(15.0), "gone".into());
    state.last_confirmation.as_mut().unwrap().at = at(-10.0);
    state.observed_equal_at = Some(at(-10.0));
    let all = records(config.clone(), identity.clone(), state.clone());
    assert_eq!(reason_of(&all, &formats(), now), Some(Reason::CopyMissing));
    // A future time comes before the missing copy.
    let mut future = state.clone();
    future.missing_copy.as_mut().unwrap().at = at(30.0);
    let future = records(config.clone(), identity.clone(), future);
    assert_eq!(
        reason_of(&future, &formats(), now),
        Some(Reason::ClockInvalid)
    );
    // Another format comes before the future time.
    let other = AcceptedFormats {
        store: Some(ObjectId::from_canonical_bytes(b"another schema")),
    };
    assert_eq!(reason_of(&future, &other, now), Some(Reason::OtherFormat));
}

#[test]
fn the_window_is_inclusive_and_counts_content_age_from_the_capture_start() {
    let (config, identity, state) = qualifying();
    let records = records(config, identity, state);
    // Confirmed at hour 10, captured at hour 9, with a 24-hour window.
    assert_eq!(reason_of(&records, &formats(), at(33.0)), None);
    assert_eq!(
        reason_of(&records, &formats(), at(33.0) + Duration::seconds(1)),
        Some(Reason::Stale)
    );
    assert_eq!(
        reason_of(&records, &formats(), at(34.0) + Duration::seconds(1)),
        Some(Reason::ConfirmationExpired)
    );
}

#[test]
fn the_collector_reads_absent_usable_and_unreadable_records_and_the_cut_from_local_files() {
    let home = temp_home().unwrap();
    let database = crate::project_database_path(home.path(), &project());

    // Nothing configured, and no store yet: the cut says why it is missing.
    let collected = collect(home.path(), &project(), &database);
    assert_eq!(
        collected.kinds,
        [(CopyKind::Store, KindRecords::NotConfigured)]
    );
    assert_eq!(
        collected.cut.as_ref().unwrap_err().code,
        "store_not_initialized"
    );
    assert!(!home.path().join("backup-records").exists());

    // A store and a target with a recorded receipt.
    std::fs::create_dir_all(database.parent().unwrap()).unwrap();
    drop(SqliteStore::open(&database).unwrap());
    let view = set_target(
        home.path(),
        &project(),
        &TargetRequest {
            kind: CopyKind::Store,
            adapter: AdapterKind::Directory,
            dir: PathBuf::from(config().dir),
            disclosure_authorized_by: "greg".into(),
            off_host_asserted_by: Some("greg".into()),
            window_hours: 24,
            keep: 3,
        },
        at(0.0),
    )
    .unwrap();
    let paths = RecordPaths::new(home.path(), &project(), CopyKind::Store);
    let mut state = TargetState::empty(view.identity.clone());
    state.record_receipt(receipt(&view.identity, at(9.0), at(10.0)), at(9.0));
    let lock = PushLock::try_acquire(&paths).unwrap();
    write_state(&paths, &lock, &state).unwrap();
    drop(lock);
    let before = std::fs::read(&paths.state).unwrap();
    let collected = collect(home.path(), &project(), &database);
    assert_eq!(
        collected.kinds,
        [(
            CopyKind::Store,
            KindRecords::Configured {
                config: Box::new(view.config.clone()),
                identity: view.identity.clone(),
                state: Box::new(state.clone()),
            }
        )]
    );
    // The cut is the store's own: one work item and one memory move both
    // positions, read here straight from their tables.
    let verbs = crate::verbs::AgentVerbs::new(
        database.clone(),
        project(),
        "freshness-test".into(),
        crate::SessionId("freshness-test".into()),
        None,
    );
    verbs
        .add(
            crate::verbs::AddInput {
                title: "Moves the work feed".into(),
                acceptance: vec!["It is added".into()],
                ..crate::verbs::AddInput::default()
            },
            at(1.0),
        )
        .unwrap();
    drop(verbs);
    crate::LocalWorkService::new(
        database.clone(),
        project(),
        "freshness-test".into(),
        crate::SessionId("freshness-test".into()),
        None,
    )
    .remember_project_memory(
        "a memory".into(),
        Some("moves".into()),
        false,
        None,
        at(1.0),
    )
    .unwrap();
    let raw = rusqlite::Connection::open_with_flags(
        &database,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let expected = WorkGraphSnapshotCut {
        work_feed: raw
            .query_row(
                "SELECT position FROM work_feed_heads WHERE feed_kind = 'project' AND feed_id = ?1",
                [project().0.as_str()],
                |row| row.get(0),
            )
            .unwrap(),
        project_memory: raw
            .query_row(
                "SELECT change_position FROM project_memory_state WHERE project_id = ?1",
                [project().0.as_str()],
                |row| row.get(0),
            )
            .unwrap(),
    };
    drop(raw);
    assert!(
        expected.work_feed > 0 && expected.project_memory > 0,
        "{expected:?}"
    );
    let collected = collect(home.path(), &project(), &database);
    assert_eq!(collected.cut, Ok(expected));
    // Collecting wrote nothing.
    assert_eq!(std::fs::read(&paths.state).unwrap(), before);

    // A state this build cannot use is reported, not raised.
    std::fs::write(&paths.state, b"not json").unwrap();
    let collected = collect(home.path(), &project(), &database);
    match &collected.kinds[0].1 {
        KindRecords::Unreadable { path, reason } => {
            assert_eq!(path, &paths.state);
            assert!(reason.contains("not JSON"), "{reason}");
        }
        other => panic!("{other:?}"),
    }
    assert_eq!(
        reason_of(&collected.kinds[0].1, &formats(), at(20.0)),
        Some(Reason::RecordUnreadable)
    );
}

#[test]
fn a_future_time_anywhere_in_the_records_makes_the_clock_invalid() {
    let now = at(20.0);
    let (config, identity, base) = qualifying();
    assert_eq!(
        reason_of(
            &records(config.clone(), identity.clone(), base.clone()),
            &formats(),
            now
        ),
        None
    );
    let future = at(21.0);
    let later_attempt = |state: &TargetState| {
        let mut attempt = crate::backup::record::Attempt {
            id: uuid::Uuid::nil(),
            manifest: state.newest_receipt.as_ref().unwrap().manifest.clone(),
            data_file: "copy.db.gz".into(),
            temporary_data_file: ".copy.db.gz.tmp".into(),
        };
        attempt.manifest.capture.capture_started_at = future;
        attempt
    };
    let cases: Vec<(&str, TargetState)> = vec![
        ("a pending attempt captured in the future", {
            let mut state = base.clone();
            state.pending = Some(later_attempt(&state));
            state
        }),
        ("a set-aside attempt captured in the future", {
            let mut state = base.clone();
            state.set_aside.push(later_attempt(&state));
            state
        }),
        ("an older receipt in the ledger received in the future", {
            let mut state = base.clone();
            let mut older = state.receipts[0].clone();
            older.manifest.copy = "older".into();
            older.at = future;
            state.receipts.insert(0, older);
            state
        }),
        ("an older receipt in the ledger captured in the future", {
            let mut state = base.clone();
            let mut older = state.receipts[0].clone();
            older.manifest.copy = "older".into();
            older.manifest.capture.capture_started_at = future;
            state.receipts.insert(0, older);
            state
        }),
    ];
    for (label, state) in cases {
        assert_eq!(
            reason_of(
                &records(config.clone(), identity.clone(), state),
                &formats(),
                now
            ),
            Some(Reason::ClockInvalid),
            "{label}"
        );
    }
}
