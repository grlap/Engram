use chrono::{Duration, TimeZone};

use super::*;
use crate::backup::{
    CaptureManifest,
    record::{
        Attempt, AttemptOutcome, BackupReceipt, Encoding, OffHost, STORED_FORMAT_VERSION,
        StoredManifest,
    },
    target::{RECORD_FORMAT_VERSION, TargetConfig, TargetState},
};

fn at(hours: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 10, 1, 0, 0, 0)
        .single()
        .expect("fixed test timestamp")
        + Duration::hours(hours)
}

fn schema() -> ObjectId {
    ObjectId::from_canonical_bytes(b"store schema")
}

fn running(fingerprint: &[u8]) -> RunningBuild {
    RunningBuild {
        formats: AcceptedFormats {
            store: Some(schema()),
        },
        fingerprint: Some(ObjectId::from_canonical_bytes(fingerprint)),
    }
}

fn config() -> TargetConfig {
    TargetConfig {
        format_version: RECORD_FORMAT_VERSION,
        project: "status-project".into(),
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
            at: at(0),
        },
        off_host_asserted: Some(Statement {
            by: "ann".into(),
            at: at(0),
        }),
    }
}

fn manifest(identity: &ObjectId, copy: &str, captured: DateTime<Utc>) -> StoredManifest {
    StoredManifest {
        format_version: STORED_FORMAT_VERSION,
        copy: copy.into(),
        target_identity: identity.clone(),
        encoding: Encoding::Gzip,
        stored_bytes: 100,
        capture: CaptureManifest {
            project_digest: "digest".into(),
            kind: CopyKind::Store,
            cut: WorkGraphSnapshotCut {
                work_feed: 7,
                project_memory: 3,
            },
            capture_started_at: captured,
            bytes: 4096,
            sha256: "a".repeat(64),
            format_identity: schema(),
            build_fingerprint: Some(ObjectId::from_canonical_bytes(b"capturing build")),
            source_revision: Some("unavailable".into()),
            host_name: None,
        },
    }
}

/// A copy captured at hour 9 and confirmed at hour 10, a pending attempt,
/// and a failed last attempt.
fn configured() -> KindRecords {
    let config = config();
    let identity = config.identity().unwrap();
    let mut state = TargetState::empty(identity.clone());
    state.record_receipt(
        BackupReceipt {
            sha256: "a".repeat(64),
            target_identity: identity.clone(),
            at: at(10),
            acknowledgement: Acknowledgement::ReadBack,
            off_host: OffHost::Asserted,
            manifest: manifest(&identity, "copy-1", at(9)),
        },
        at(9),
    );
    state.pending = Some(Attempt {
        id: uuid::Uuid::nil(),
        manifest: manifest(&identity, "copy-2", at(11)),
        data_file: "copy-2.db.gz".into(),
        temporary_data_file: ".copy-2.db.gz.tmp".into(),
    });
    state.last_attempt = Some(LastAttempt {
        started_at: at(11),
        ended_at: at(12),
        outcome: AttemptOutcome::Failed,
        code: Some("backup_target_unreachable".into()),
        message: Some("the target cannot be reached".into()),
    });
    KindRecords::Configured {
        config: Box::new(config),
        identity,
        state: Box::new(state),
    }
}

fn collected(records: KindRecords) -> Collected {
    Collected {
        kinds: vec![(CopyKind::Store, records)],
        cut: Ok(WorkGraphSnapshotCut {
            work_feed: 12,
            project_memory: 4,
        }),
    }
}

#[test]
fn the_status_reports_the_mode_with_its_off_host_text_and_each_piece_of_evidence() {
    let status = build_status(&collected(configured()), &running(b"running build"), at(20));
    assert_eq!(status.schema_version, STATUS_SCHEMA_VERSION);
    assert_eq!(status.durability.mode, Mode::LocalBackedUp);
    assert_eq!(status.durability.off_host.len(), 1);
    assert_eq!(status.durability.off_host[0].off_host, DIRECTORY_OFF_HOST);
    assert_eq!(DIRECTORY_OFF_HOST, "off-host asserted; not verified");
    assert_eq!(status.durability.off_host[0].restores, STORE_RESTORES);
    let kind = &status.kinds[0];
    assert!(kind.qualifies);
    assert_eq!(kind.reason, None);
    let target = kind.target.as_ref().unwrap();
    assert_eq!(target.off_host, DIRECTORY_OFF_HOST);
    assert_eq!(target.disclosure_authorized.by, "greg");
    assert_eq!(target.off_host_asserted.as_ref().unwrap().by, "ann");
    let copy = target.copy.as_ref().unwrap();
    assert_eq!(copy.copy, "copy-1");
    assert_eq!(copy.acknowledgement, Acknowledgement::ReadBack);
    assert_eq!(copy.capture_started_at, at(9));
    assert_eq!(copy.capture_age_seconds, 11 * 3600);
    assert_eq!(
        copy.store_moved,
        Some(CutMovement {
            work_feed: 5,
            project_memory: 1
        })
    );
    assert_eq!(copy.last_confirmation, Some(at(10)));
    // Checked by a build that is not the running one, so it is named.
    assert_eq!(
        copy.checking_build,
        Some(ObjectId::from_canonical_bytes(b"capturing build"))
    );
    assert_eq!(target.pending.as_ref().unwrap().copy, "copy-2");
    let attempt = target.last_attempt.as_ref().unwrap();
    assert_eq!(attempt.code.as_deref(), Some("backup_target_unreachable"));

    let text = render_status(&status);
    for expected in [
        "backup mode: local_backed_up (store copy: off-host asserted; not verified; restores the same store at the copy's cut)",
        "store: qualifies",
        "disclosure authorized by greg at",
        "off-host asserted; not verified: stated by ann at",
        "copy: copy-1 (read back at",
        "(age 11 h 0 min)",
        "cut: work feed 7, memory 3; the store has moved 5 work-feed and 1 memory positions since",
        "last confirmed at",
        "checked by build",
        "pending attempt: copy-2",
        "last attempt: failed: backup_target_unreachable: the target cannot be reached",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }

    // The same build that captured the copy is not named again.
    let same = build_status(
        &collected(configured()),
        &running(b"capturing build"),
        at(20),
    );
    let copy = same.kinds[0]
        .target
        .as_ref()
        .unwrap()
        .copy
        .as_ref()
        .unwrap();
    assert_eq!(copy.checking_build, None);
    assert!(!render_status(&same).contains("checked by build"));

    // Reasons serialize as their codes.
    // At hour 34 the confirmation (hour 10) is just within the window, the
    // capture (hour 9) just beyond it.
    let stale = build_status(&collected(configured()), &running(b"x"), at(34));
    let json = serde_json::to_value(&stale).unwrap();
    assert_eq!(json["kinds"][0]["reason"], "backup_stale");
    assert_eq!(
        serde_json::to_value(Reason::NeverConfirmed).unwrap(),
        "backup_never_confirmed"
    );
}

#[test]
fn an_unreadable_record_is_reported_as_its_reason_not_a_failure() {
    let records = KindRecords::Unreadable {
        path: "store.state.json".into(),
        reason: "it is not JSON".into(),
    };
    let status = build_status(&collected(records), &running(b"x"), at(20));
    assert_eq!(status.durability.mode, Mode::Local);
    assert_eq!(status.kinds[0].reason, Some("backup_record_unreadable"));
    assert!(
        status.kinds[0]
            .unreadable
            .as_deref()
            .unwrap()
            .contains("it is not JSON")
    );
    let text = render_status(&status);
    assert!(
        text.contains("store: does not qualify: backup_record_unreadable"),
        "{text}"
    );
    assert!(
        text.contains("unreadable record: store.state.json: it is not JSON"),
        "{text}"
    );
}

#[test]
fn no_output_names_the_mode_without_its_off_host_field() {
    let scenarios: Vec<(&str, Collected)> = vec![
        ("qualifying", collected(configured())),
        ("not configured", collected(KindRecords::NotConfigured)),
        (
            "unreadable",
            collected(KindRecords::Unreadable {
                path: "x".into(),
                reason: "y".into(),
            }),
        ),
        (
            "store cut unreadable",
            Collected {
                cut: Err(CutUnavailable {
                    code: "store_not_initialized".into(),
                    message: "no store".into(),
                }),
                ..collected(configured())
            },
        ),
    ];
    for (label, collected) in scenarios {
        for now in [at(20), at(100)] {
            let status = build_status(&collected, &running(b"x"), now);
            // Text: the mode appears on one line only, and that line always
            // carries what backs it.
            let text = render_status(&status);
            let mode_lines: Vec<_> = text
                .lines()
                .filter(|line| line.contains("local_backed_up") || line.contains("backup mode"))
                .collect();
            assert_eq!(mode_lines.len(), 1, "{label}\n{text}");
            let line = mode_lines[0];
            if status.durability.mode == Mode::LocalBackedUp {
                assert!(line.contains(DIRECTORY_OFF_HOST), "{label}: {line}");
            } else {
                assert!(
                    line.contains("nothing is known to be held off this host"),
                    "{label}: {line}"
                );
            }
            // JSON: the mode exists only inside the durability object,
            // which always carries the off-host field.
            let json = serde_json::to_value(&status).unwrap();
            assert!(json.get("mode").is_none(), "{label}");
            assert!(json["durability"]["mode"].is_string(), "{label}");
            assert!(json["durability"]["off_host"].is_array(), "{label}");
            if status.durability.mode == Mode::LocalBackedUp {
                assert_eq!(
                    json["durability"]["off_host"][0]["off_host"],
                    "off-host asserted; not verified"
                );
            }
        }
    }
}

#[test]
fn records_for_an_earlier_target_and_an_unnamed_checking_build_are_labelled() {
    let KindRecords::Configured {
        config,
        identity,
        state,
    } = configured()
    else {
        unreachable!("configured")
    };
    let earlier = ObjectId::from_canonical_bytes(b"an earlier target");
    let mut state = *state;
    // The newest receipt and the pending attempt were made for an earlier
    // identity of the target, and the capturing build named no fingerprint.
    let mut receipt = state.newest_receipt.clone().unwrap();
    receipt.target_identity = earlier.clone();
    receipt.manifest.target_identity = earlier.clone();
    receipt.manifest.capture.build_fingerprint = None;
    let observed = state.observed_equal_at.unwrap();
    let mut fresh = TargetState::empty(identity.clone());
    fresh.record_receipt(receipt, observed);
    fresh.pending = state.pending.take().map(|mut attempt| {
        attempt.manifest.target_identity = earlier.clone();
        attempt
    });
    // A failed attempt recorded with no code.
    fresh.last_attempt = Some(LastAttempt {
        code: None,
        message: None,
        ..state.last_attempt.unwrap()
    });
    let records = KindRecords::Configured {
        config,
        identity,
        state: Box::new(fresh),
    };
    let mut collected = collected(records);
    // The store is behind the copy.
    collected.cut = Ok(WorkGraphSnapshotCut {
        work_feed: 5,
        project_memory: 3,
    });
    let status = build_status(&collected, &running(b"running build"), at(20));
    assert_eq!(status.kinds[0].reason, Some("backup_target_changed"));
    let target = status.kinds[0].target.as_ref().unwrap();
    let copy = target.copy.as_ref().unwrap();
    assert!(copy.for_earlier_target);
    assert!(copy.checking_build_unknown);
    assert_eq!(copy.checking_build, None);
    assert!(target.pending.as_ref().unwrap().for_earlier_target);
    let text = render_status(&status);
    for expected in [
        "copy (recorded for an earlier target identity): copy-1",
        "pending attempt (recorded for an earlier target identity): copy-2",
        "checked by a build that did not name itself",
        "the store is behind the copy: work feed -2, memory +0",
        "last attempt: failed (no code recorded)",
    ] {
        assert!(text.contains(expected), "{expected}\n{text}");
    }
}

#[test]
fn an_unreadable_store_cut_is_said_once_and_an_unnamed_running_build_is_not_compared() {
    let mut unreadable_cut = collected(configured());
    unreadable_cut.cut = Err(CutUnavailable {
        code: "store_not_initialized".into(),
        message: "the store does not exist".into(),
    });
    let anonymous = RunningBuild {
        formats: running(b"x").formats,
        fingerprint: None,
    };
    let status = build_status(&unreadable_cut, &anonymous, at(20));
    let text = render_status(&status);
    assert!(
        text.contains("store cut unavailable: store_not_initialized: the store does not exist"),
        "{text}"
    );
    assert!(
        text.contains("the store's own cut could not be read: store_not_initialized"),
        "{text}"
    );
    assert!(
        text.contains("checked by build ")
            && text.contains("the running build could not name itself"),
        "{text}"
    );
    assert!(!text.contains("not the running build"), "{text}");
    // An extreme recorded cut never overflows the movement.
    let mut extreme = collected(configured());
    extreme.cut = Ok(WorkGraphSnapshotCut {
        work_feed: i64::MIN,
        project_memory: 0,
    });
    let status = build_status(&extreme, &running(b"x"), at(20));
    let copy = status.kinds[0]
        .target
        .as_ref()
        .unwrap()
        .copy
        .as_ref()
        .unwrap();
    assert_eq!(copy.store_moved, None);
}

#[test]
fn a_recorded_restore_is_shown_pending_restored_or_unreadable_after_the_kinds() {
    use crate::backup::restore::{RestoreRecord, RestoreRecords, RestoreState};
    let record = RestoreRecord {
        format_version: RECORD_FORMAT_VERSION,
        occurrence_id: uuid::Uuid::now_v7(),
        project: "status-project".into(),
        copy: "20261001T000000Z-copy".into(),
        sha256: "cd".repeat(32),
        origin_host: Some("old-host".into()),
        origin_retired: Statement {
            by: "greg".into(),
            at: at(1),
        },
        staging: "staging".into(),
        state: RestoreState::Pending,
        pending_at: at(2),
        completed_at: None,
    };
    assert!(RestoreStatus::of(RestoreRecords::None).is_none());

    let mut status = build_status(&collected(configured()), &running(b"build"), at(3));
    assert!(status.restore.is_none());
    status.restore = RestoreStatus::of(RestoreRecords::Recorded(Box::new(record.clone())));
    let pending = status.restore.as_ref().unwrap();
    assert_eq!(pending.state, "pending");
    let text = render_status(&status);
    let last = text.lines().last().unwrap();
    assert_eq!(
        last,
        format!(
            "restore: pending since {}: 20261001T000000Z-copy (sha256 {}) from host old-host; origin retired by greg at {} (asserted); run `engram backup restore 20261001T000000Z-copy --origin-retired-by=greg` again to finish it, or `engram backup restore 20261001T000000Z-copy --abandon-pending --abandoned-by=NAME` to abandon it",
            at(2).to_rfc3339(),
            "cd".repeat(32),
            at(1).to_rfc3339()
        )
    );
    let value = serde_json::to_value(&status).unwrap();
    assert_eq!(value["restore"]["state"], "pending");
    assert_eq!(value["restore"]["record"]["copy"], "20261001T000000Z-copy");
    assert_eq!(
        value["restore"]["record"]["occurrence_id"],
        record.occurrence_id.to_string()
    );

    let mut completed = record;
    completed.state = RestoreState::Completed;
    completed.completed_at = Some(at(3));
    status.restore = RestoreStatus::of(RestoreRecords::Recorded(Box::new(completed)));
    let text = render_status(&status);
    assert!(
        text.lines()
            .last()
            .unwrap()
            .starts_with("restore: restored 20261001T000000Z-copy (sha256 ")
            && text.ends_with(&format!("; completed {}\n", at(3).to_rfc3339())),
        "{text}"
    );

    status.restore = RestoreStatus::of(RestoreRecords::Unreadable {
        path: "store.restore.json".into(),
        reason: "it is not JSON".into(),
    });
    assert_eq!(status.restore.as_ref().unwrap().state, "unreadable");
    assert!(
        render_status(&status)
            .ends_with("restore: unreadable record: store.restore.json: it is not JSON\n")
    );
}
