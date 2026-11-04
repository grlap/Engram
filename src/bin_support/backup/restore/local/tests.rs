use std::time::Duration;

use engram::backup::restore::restore_record_path;

use super::*;
use crate::{
    bin_support::backup::fetch::ReadSettings,
    test_support::{TempHome, temp_home},
};

struct Fixture {
    home: TempHome,
    project: ProjectId,
    database: PathBuf,
    source: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let home = temp_home().unwrap();
        let project = ProjectId("local-restore-unit".into());
        let source = home.path().join("source.db");
        let origin = home.path().join("origin.db");
        let store = SqliteStore::open(&origin).unwrap();
        store.backup_to(&source).unwrap();
        drop(store);
        let database = engram::project_database_path(home.path(), &project);
        Self {
            home,
            project,
            database,
            source,
        }
    }

    fn run(&self, replace: bool, stop: Option<Stop>) -> Result<BackupManifest> {
        let mut settings = RestoreSettings::new(ReadSettings::new(Duration::from_secs(5)));
        settings.stop = stop;
        restore_local(
            self.home.path(),
            &self.project,
            &self.database,
            &self.source,
            replace,
            &settings,
        )
    }

    fn record(&self) -> RestoreRecord {
        let RestoreRecords::Recorded(record) = read_restore_record(self.home.path(), &self.project)
        else {
            panic!("recorded occurrence")
        };
        *record
    }
}

#[test]
fn repeated_local_restores_keep_bytes_but_mint_distinct_occurrences() {
    let fixture = Fixture::new();
    let first = fixture.run(false, None).unwrap();
    let record = fixture.record();
    assert_eq!(record.state, RestoreState::Completed);
    assert_eq!(
        first.file_sha256,
        SqliteStore::verify_backup(&fixture.source)
            .unwrap()
            .file_sha256
    );
    assert!(matches!(
        record.source,
        RestoreSource::LocalFile { replace: false, .. }
    ));
    fixture.run(true, None).unwrap();
    let second = fixture.record();
    assert_ne!(record.occurrence_id, second.occurrence_id);
    assert_eq!(record.sha256, second.sha256);
    assert_eq!(
        fs::read(&fixture.database).unwrap(),
        fs::read(&fixture.source).unwrap()
    );
    assert_eq!(record.source.copy(), None);
}

#[test]
fn local_failure_windows_retry_the_same_pending_occurrence() {
    for stop in [
        Stop::AfterPending,
        Stop::AfterMove,
        Stop::FailCompletionWrite,
    ] {
        let fixture = Fixture::new();
        assert!(fixture.run(false, Some(stop)).is_err());
        let pending = fixture.record();
        assert_eq!(pending.state, RestoreState::Pending);
        assert_eq!(fixture.database.exists(), stop != Stop::AfterPending);
        fixture.run(false, None).unwrap();
        let completed = fixture.record();
        assert_eq!(completed.occurrence_id, pending.occurrence_id);
        assert_eq!(completed.pending_at, pending.pending_at);
        assert_eq!(completed.source, pending.source);
        assert_eq!(completed.state, RestoreState::Completed);
        assert_eq!(
            fs::read(&fixture.database).unwrap(),
            fs::read(&fixture.source).unwrap()
        );
    }
}

#[test]
fn replacement_publishes_before_checkpoint_and_preserves_retry_intent() {
    for stop in [Stop::AfterPending, Stop::AfterCheckpoint] {
        let fixture = Fixture::new();
        fixture.run(false, None).unwrap();
        let before = fs::read(&fixture.database).unwrap();
        assert!(fixture.run(true, Some(stop)).is_err());
        let pending = fixture.record();
        assert_eq!(pending.state, RestoreState::Pending);
        assert_eq!(fs::read(&fixture.database).unwrap(), before);
        assert!(Path::new(&pending.staging).is_file());
        assert!(
            fixture.run(false, None).is_err(),
            "replacement intent cannot change on retry"
        );
        assert_eq!(fixture.record(), pending);
        fixture.run(true, None).unwrap();
        assert_eq!(fixture.record().occurrence_id, pending.occurrence_id);
    }
}

#[test]
fn prepublication_failures_leave_no_occurrence_or_destination() {
    for stop in [Stop::FailCheck, Stop::FailPendingWrite] {
        let fixture = Fixture::new();
        assert!(fixture.run(false, Some(stop)).is_err());
        assert_eq!(
            read_restore_record(fixture.home.path(), &fixture.project),
            RestoreRecords::None
        );
        assert!(!fixture.database.exists());
    }
}

#[test]
fn changed_staging_or_installed_bytes_never_complete_the_pending_occurrence() {
    for moved in [false, true] {
        let fixture = Fixture::new();
        let stop = if moved {
            Stop::AfterMove
        } else {
            Stop::AfterPending
        };
        assert!(fixture.run(false, Some(stop)).is_err());
        let pending = fixture.record();
        let path = if moved {
            fixture.database.clone()
        } else {
            PathBuf::from(&pending.staging)
        };
        fs::write(&path, b"changed bytes").unwrap();
        assert!(fixture.run(false, None).is_err());
        assert_eq!(fixture.record(), pending);
        assert_eq!(fs::read(path).unwrap(), b"changed bytes");
    }
}

#[test]
fn a_changed_source_or_unreadable_record_refuses_without_destination_writes() {
    let fixture = Fixture::new();
    assert!(fixture.run(false, Some(Stop::AfterPending)).is_err());
    let pending = fixture.record();
    let another = fixture.home.path().join("another.db");
    fs::copy(&fixture.source, &another).unwrap();
    let settings = RestoreSettings::new(ReadSettings::new(Duration::from_secs(5)));
    assert!(
        restore_local(
            fixture.home.path(),
            &fixture.project,
            &fixture.database,
            &another,
            false,
            &settings
        )
        .is_err()
    );
    assert_eq!(fixture.record(), pending);
    let path = restore_record_path(fixture.home.path(), &fixture.project);
    fs::write(&path, b"unreadable").unwrap();
    assert!(fixture.run(false, None).is_err());
    assert_eq!(fs::read(path).unwrap(), b"unreadable");
    assert!(!fixture.database.exists());
}

#[test]
fn an_ambiguous_replacement_with_missing_stage_keeps_the_old_destination() {
    let fixture = Fixture::new();
    fixture.run(false, None).unwrap();
    assert!(fixture.run(true, Some(Stop::AfterPending)).is_err());
    let pending = fixture.record();
    fs::remove_file(&pending.staging).unwrap();
    fs::write(&fixture.database, b"not the pending bytes").unwrap();
    assert!(fixture.run(true, None).is_err());
    assert_eq!(fixture.record(), pending);
    assert_eq!(
        fs::read(&fixture.database).unwrap(),
        b"not the pending bytes"
    );
}

fn settings(stop: Option<Stop>) -> RestoreSettings {
    let mut settings = RestoreSettings::new(ReadSettings::new(Duration::from_secs(5)));
    settings.stop = stop;
    settings
}

/// VACUUM changes SQLite's file layout/counter, retaining a valid current store.
fn different_healthy_copy(fixture: &Fixture) -> PathBuf {
    let different = fixture.home.path().join("different.db");
    fs::copy(&fixture.source, &different).unwrap();
    let connection = rusqlite::Connection::open(&different).unwrap();
    connection.execute_batch("VACUUM").unwrap();
    drop(connection);
    let manifest = SqliteStore::verify_backup(&different).unwrap();
    assert_ne!(
        manifest.file_sha256,
        SqliteStore::verify_backup(&fixture.source)
            .unwrap()
            .file_sha256
    );
    different
}

#[test]
fn completion_and_checked_staging_do_not_require_the_original_source() {
    for replace in [false, true] {
        for stop in [
            Stop::AfterPending,
            Stop::AfterMove,
            Stop::FailCompletionWrite,
        ] {
            for remove in [false, true] {
                let fixture = Fixture::new();
                if replace {
                    fixture.run(false, None).unwrap();
                }
                assert!(fixture.run(replace, Some(stop)).is_err());
                let pending = fixture.record();
                if remove {
                    fs::remove_file(&fixture.source).unwrap();
                } else {
                    fs::write(&fixture.source, b"rotated source").unwrap();
                }
                let restored = fixture.run(replace, None).unwrap();
                assert_eq!(restored.file_sha256, pending.sha256);
                let completed = fixture.record();
                assert_eq!(completed.occurrence_id, pending.occurrence_id);
                assert_eq!(completed.pending_at, pending.pending_at);
                assert_eq!(completed.state, RestoreState::Completed);
            }
        }
    }
}

#[test]
fn restaging_requires_the_pending_bytes_and_preserves_the_occurrence() {
    let fixture = Fixture::new();
    assert!(fixture.run(false, Some(Stop::AfterPending)).is_err());
    let pending = fixture.record();
    fs::remove_file(&pending.staging).unwrap();
    fixture.run(false, None).unwrap();
    assert_eq!(fixture.record().occurrence_id, pending.occurrence_id);
    assert_eq!(fixture.record().pending_at, pending.pending_at);

    let fixture = Fixture::new();
    let different = different_healthy_copy(&fixture);
    assert!(fixture.run(false, Some(Stop::AfterPending)).is_err());
    let pending = fixture.record();
    fs::remove_file(&pending.staging).unwrap();
    fs::copy(different, &fixture.source).unwrap();
    let error = fixture.run(false, None).unwrap_err().to_string();
    assert!(
        error.contains("source bytes differ from the pending restore"),
        "{error}"
    );
    assert!(error.contains("--abandon-pending"), "{error}");
    assert_eq!(fixture.record(), pending);
    assert!(!fixture.database.exists());
}

#[test]
fn hard_link_leftovers_never_turn_a_retry_into_an_overwrite() {
    for replace in [false, true] {
        let fixture = Fixture::new();
        let different = different_healthy_copy(&fixture);
        let mut options = settings(Some(Stop::AfterMove));
        options.read.leave_staging = true;
        assert!(
            restore_local(
                fixture.home.path(),
                &fixture.project,
                &fixture.database,
                &fixture.source,
                replace,
                &options
            )
            .is_err()
        );
        let pending = fixture.record();
        assert_eq!(
            Path::new(&pending.staging).exists(),
            !replace,
            "replace always renames; no-replace exercises the real move-leaving-source seam"
        );
        let refused = abandon_local(
            fixture.home.path(),
            &fixture.project,
            &fixture.database,
            &fixture.source,
            Some("operator"),
            &settings(None),
        )
        .unwrap_err()
        .to_string();
        assert!(refused.contains("may already be installed"), "{refused}");
        assert_eq!(fixture.record(), pending);
        fs::remove_file(&fixture.database).unwrap();
        fs::copy(different, &fixture.database).unwrap();
        let before = fs::read(&fixture.database).unwrap();
        let error = fixture.run(replace, None).unwrap_err().to_string();
        assert!(
            error.contains("does not hold the restored copy's bytes"),
            "{error}"
        );
        assert_eq!(fixture.record(), pending);
        assert_eq!(fs::read(&fixture.database).unwrap(), before);
    }
}

#[test]
fn refused_replacement_can_be_abandoned_without_changing_the_old_store() {
    let fixture = Fixture::new();
    fixture.run(false, None).unwrap();
    let original = fixture.record();
    let before = fs::read(&fixture.database).unwrap();
    let blocker = rusqlite::Connection::open(&fixture.database).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let error = fixture.run(true, None).unwrap_err().to_string();
    assert!(error.contains("--abandon-pending"), "{error}");
    let pending = fixture.record();
    assert_eq!(pending.state, RestoreState::Pending);
    let abandoned = abandon_local(
        fixture.home.path(),
        &fixture.project,
        &fixture.database,
        &fixture.source,
        Some("operator"),
        &settings(None),
    )
    .unwrap();
    assert_eq!(
        abandoned["occurrence_id"],
        pending.occurrence_id.to_string()
    );
    assert_eq!(
        read_restore_record(fixture.home.path(), &fixture.project),
        RestoreRecords::Recorded(Box::new(original))
    );
    assert!(!Path::new(&pending.staging).exists());
    assert_eq!(fs::read(&fixture.database).unwrap(), before);
    drop(blocker);
    fixture.run(true, None).unwrap();
    assert_ne!(fixture.record().occurrence_id, pending.occurrence_id);
}

#[test]
fn abandonment_archive_failure_is_retryable_and_cleanup_failure_is_visible() {
    for stop in [Stop::FailArchive, Stop::FailCleanup] {
        let fixture = Fixture::new();
        fixture.run(false, None).unwrap();
        let original = fixture.record();
        assert!(fixture.run(true, Some(Stop::AfterPending)).is_err());
        let pending = fixture.record();
        let before = fs::read(&fixture.database).unwrap();
        let result = abandon_local(
            fixture.home.path(),
            &fixture.project,
            &fixture.database,
            &fixture.source,
            Some("operator"),
            &settings(Some(stop)),
        );
        if stop == Stop::FailArchive {
            assert!(result.is_err());
            assert_eq!(fixture.record(), pending);
            assert!(Path::new(&pending.staging).exists());
            abandon_local(
                fixture.home.path(),
                &fixture.project,
                &fixture.database,
                &fixture.source,
                Some("operator"),
                &settings(None),
            )
            .unwrap();
        } else {
            let abandoned = result.unwrap();
            assert_eq!(abandoned["staging_removed"], false);
            assert_ne!(
                abandoned["warnings"].as_array().unwrap(),
                &Vec::<serde_json::Value>::new()
            );
            assert!(Path::new(&pending.staging).exists());
        }
        assert_eq!(
            read_restore_record(fixture.home.path(), &fixture.project),
            RestoreRecords::Recorded(Box::new(original))
        );
        assert_eq!(fs::read(&fixture.database).unwrap(), before);
    }
}

#[test]
fn possibly_installed_occurrences_cannot_be_abandoned() {
    for replace in [false, true] {
        let fixture = Fixture::new();
        assert!(fixture.run(replace, Some(Stop::AfterMove)).is_err());
        let pending = fixture.record();
        let before = fs::read(&fixture.database).unwrap();
        let error = abandon_local(
            fixture.home.path(),
            &fixture.project,
            &fixture.database,
            &fixture.source,
            Some("operator"),
            &settings(None),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("may already be installed"), "{error}");
        assert_eq!(fixture.record(), pending);
        assert_eq!(fs::read(&fixture.database).unwrap(), before);
        fixture.run(replace, None).unwrap();
        assert_eq!(fixture.record().occurrence_id, pending.occurrence_id);
    }
}

#[test]
fn refusing_pending_publication_keeps_the_previous_completed_observation() {
    let fixture = Fixture::new();
    fixture.run(false, None).unwrap();
    let original = fixture.record();
    let before = fs::read(&fixture.database).unwrap();
    assert!(fixture.run(true, Some(Stop::FailPendingWrite)).is_err());
    assert_eq!(fixture.record(), original);
    assert_eq!(fs::read(&fixture.database).unwrap(), before);
}

#[test]
fn cancellation_without_a_previous_restore_truthfully_returns_none() {
    let fixture = Fixture::new();
    fs::create_dir_all(fixture.database.parent().unwrap()).unwrap();
    fs::copy(&fixture.source, &fixture.database).unwrap();
    assert!(fixture.run(true, Some(Stop::AfterPending)).is_err());
    assert!(fixture.record().previous_completed.is_none());
    let before = fs::read(&fixture.database).unwrap();
    abandon_local(
        fixture.home.path(),
        &fixture.project,
        &fixture.database,
        &fixture.source,
        Some("operator"),
        &settings(None),
    )
    .unwrap();
    assert_eq!(
        read_restore_record(fixture.home.path(), &fixture.project),
        RestoreRecords::None
    );
    assert_eq!(fs::read(&fixture.database).unwrap(), before);
}

#[test]
fn control_characters_in_source_selectors_are_refused_before_a_record_is_published() {
    let fixture = Fixture::new();
    let source = fixture.home.path().join("unsafe\nsource.db");
    let error = restore_local(
        fixture.home.path(),
        &fixture.project,
        &fixture.database,
        &source,
        false,
        &settings(None),
    )
    .unwrap_err()
    .to_string();
    assert!(
        error.contains("source path must not contain control characters"),
        "{error}"
    );
    assert_eq!(
        read_restore_record(fixture.home.path(), &fixture.project),
        RestoreRecords::None
    );
    assert!(!fixture.database.exists());
}
