use super::*;
use crate::ObjectId;

fn fixture() -> (crate::test_support::TempHome, AuditReservation) {
    let home = crate::test_support::temp_home().unwrap();
    let root = home.path().join("git");
    std::fs::create_dir(&root).unwrap();
    let reservation = AuditReservation::acquire(&root).unwrap();
    // Only the private write-ahead state machine is under test here. Storage's
    // activation tests independently establish canonical checkpoint authority.
    let checkpoint = AuditAcknowledgement {
        binding: ObjectId::mint(),
        origin: ObjectId::mint(),
        restore_occurrence: None,
        checkpoint: ObjectId::mint(),
        event_fingerprint: ObjectId::mint(),
        mode: super::super::AuditMode::Managed,
    };
    let bytes = crate::canonical::canonical_bytes(&checkpoint).unwrap();
    write_new(&reservation.directory, Path::new("head.json"), &bytes).unwrap();
    write_new(
        &reservation.directory,
        Path::new(&format!("{}.ack.json", checkpoint.checkpoint)),
        &bytes,
    )
    .unwrap();
    reservation
        .publish_journal(&Journal {
            checkpoint,
            revision: uuid::Uuid::new_v4(),
            previous_revision: None,
            writes: BTreeMap::new(),
        })
        .unwrap();
    (home, reservation)
}

#[test]
fn exact_pending_write_replays_and_divergent_or_stale_settlement_preserves_it() {
    let (_home, reservation) = fixture();
    let operation = uuid::Uuid::new_v4();
    reservation
        .reserve_artifact(operation, Family::Commit, &"first")
        .unwrap();
    let first = reservation.journal().unwrap().unwrap();
    reservation
        .reserve_artifact(operation, Family::Commit, &"first")
        .unwrap();
    assert_eq!(reservation.journal().unwrap().unwrap(), first);
    assert!(
        reservation
            .reserve_artifact(operation, Family::Commit, &"different")
            .is_err()
    );
    assert_eq!(reservation.journal().unwrap().unwrap(), first);
    reservation
        .reserve_artifact(uuid::Uuid::new_v4(), Family::Delta, &"next")
        .unwrap();
    let next = reservation.journal().unwrap().unwrap();
    assert!(reservation.settle_journal(&first).is_err());
    assert_eq!(reservation.journal().unwrap().unwrap(), next);
    reservation.settle_journal(&next).unwrap();
    reservation.settle_journal(&next).unwrap();
    assert!(reservation.journal().unwrap().unwrap().writes.is_empty());
}

#[test]
fn ordinary_sync_cannot_advance_an_unsettled_managed_request() {
    let (_home, reservation) = fixture();
    reservation.require_idle_for_ordinary_sync().unwrap();
    reservation
        .reserve_artifact(
            uuid::Uuid::new_v4(),
            Family::Delta,
            &"managed request before SQL",
        )
        .unwrap();
    let pending = reservation.journal_state().unwrap().unwrap();

    assert!(reservation.require_idle_for_ordinary_sync().is_err());
    assert_eq!(reservation.journal_state().unwrap(), Some(pending.clone()));

    reservation.settle_journal(&pending).unwrap();
    reservation.require_idle_for_ordinary_sync().unwrap();
}

#[test]
fn interrupted_exact_publication_recovers_without_reminting_its_revision() {
    let (home, reservation) = fixture();
    let before = reservation.journal().unwrap().unwrap();
    let operation = uuid::Uuid::new_v4();
    let write = WriteAhead {
        operation,
        family: Family::Commit,
        request_fingerprint: CanonicalObject::freeze(&"selected").unwrap().key().clone(),
    };
    let mut pending = before.clone();
    pending.previous_revision = Some(before.revision);
    pending.revision = uuid::Uuid::new_v4();
    pending.writes.insert(write.key().unwrap(), write);
    write_new(
        &reservation.directory,
        Path::new(PENDING),
        &pending.bytes().unwrap(),
    )
    .unwrap();
    assert_eq!(
        observe(&home.path().join("git")).unwrap(),
        Some(pending.clone())
    );
    assert_eq!(reservation.journal().unwrap(), Some(pending.clone()));
    assert!(
        reservation
            .reserve_artifact(operation, Family::Commit, &"another")
            .is_err()
    );
    reservation
        .reserve_artifact(operation, Family::Commit, &"selected")
        .unwrap();
    assert_eq!(reservation.journal().unwrap().unwrap(), pending);
    assert!(
        read_bytes(&reservation.directory, Path::new(PENDING), LIMIT)
            .unwrap()
            .is_none()
    );
}

#[test]
fn partial_publication_and_missing_managed_state_never_allow_a_writer() {
    let (_home, reservation) = fixture();
    write_new(&reservation.directory, Path::new(PENDING), b"{").unwrap();
    let original = read_bytes(&reservation.directory, Path::new(FILE), LIMIT).unwrap();
    assert!(
        reservation
            .reserve_artifact(uuid::Uuid::new_v4(), Family::Commit, &"request")
            .is_err()
    );
    assert_eq!(
        read_bytes(&reservation.directory, Path::new(FILE), LIMIT).unwrap(),
        original
    );
    assert_eq!(
        read_bytes(&reservation.directory, Path::new(PENDING), LIMIT).unwrap(),
        Some(b"{".to_vec())
    );
    reservation.directory.dir().remove_file(PENDING).unwrap();
    reservation.directory.dir().remove_file(FILE).unwrap();
    assert!(
        reservation
            .reserve_artifact(uuid::Uuid::new_v4(), Family::Commit, &"request")
            .is_err()
    );
    assert!(
        read_bytes(&reservation.directory, Path::new(FILE), LIMIT)
            .unwrap()
            .is_none()
    );
}

#[test]
fn selected_observation_ignores_disconnected_artifact_names() {
    let (home, reservation) = fixture();
    let root = home.path().join("git");
    let artifacts = root.join("engram-task-sync-commits");
    std::fs::create_dir(&artifacts).unwrap();
    for _ in 0..100 {
        std::fs::write(
            artifacts.join(format!("{}.json", uuid::Uuid::new_v4())),
            b"partial",
        )
        .unwrap();
    }
    let expected = reservation.journal().unwrap().unwrap();
    drop(reservation);
    assert_eq!(observe(&root).unwrap(), Some(expected));
    assert!(crate::task_sync::retention::inventory(&root).is_err());
    let selected =
        crate::task_sync::retention::selected_inventory(&root, &std::collections::BTreeSet::new())
            .unwrap();
    assert_eq!(
        selected,
        crate::task_sync::retention::RetainedInventory::default()
    );
}

/// A writer holding the reservation leaves `managed.pending` half written
/// until the reader first waits. Then it completes the file only when
/// `complete`, and keeps the reservation until the reader has returned.
/// Returns the reader's result, the journal the writer publishes, and how
/// often the reader waited.
fn read_during_publication(complete: bool) -> (Result<Option<Journal>>, Journal, usize) {
    let (home, reservation) = fixture();
    let root = home.path().join("git");
    let mut pending = reservation.journal().unwrap().unwrap();
    pending.previous_revision = Some(pending.revision);
    pending.revision = uuid::Uuid::new_v4();
    let expected = pending.clone();
    let (published, arrived) = std::sync::mpsc::sync_channel(0);
    let (command, proceed) = std::sync::mpsc::sync_channel::<bool>(0);
    let (written, done) = std::sync::mpsc::sync_channel(0);
    let (result, waits) = std::thread::scope(|scope| {
        scope.spawn(move || {
            use std::io::Write;
            let mut file = reservation
                .directory
                .open_file(Path::new(PENDING), true, true)
                .unwrap();
            file.write_all(b"{").unwrap();
            published.send(()).unwrap();
            if proceed.recv().unwrap() {
                file.write_all(&pending.bytes().unwrap()[1..]).unwrap();
                file.sync_all().unwrap();
                // A finished writer closes the file before it publishes, as
                // the real one does; an open write handle keeps a Windows
                // reader out of it until then.
                drop(file);
            }
            written.send(()).unwrap();
            // The reservation stays held until the reader has returned.
            proceed.recv().unwrap();
        });
        arrived.recv().unwrap();
        let mut waits = 0;
        let result = observe_waiting(&root, &mut || {
            waits += 1;
            if waits == 1 {
                command.send(complete).unwrap();
                done.recv().unwrap();
                complete
            } else {
                false
            }
        });
        if waits == 0 {
            command.send(false).unwrap();
            done.recv().unwrap();
        }
        command.send(false).unwrap();
        (result, waits)
    });
    drop(home);
    (result, expected, waits)
}

#[test]
fn a_reader_waits_out_a_publication_in_progress_and_reads_it_complete() {
    let (result, expected, waits) = read_during_publication(true);
    assert_eq!(result.unwrap(), Some(expected));
    assert_eq!(waits, 1);
}

#[test]
fn a_publication_that_stays_unreadable_refuses_as_busy_and_names_both_remedies() {
    let (result, _, waits) = read_during_publication(false);
    let error = result.unwrap_err().to_string();
    assert_eq!(waits, 1);
    assert!(error.contains("stayed busy"), "{error}");
    assert!(error.contains("Retry the exact request"), "{error}");
    assert!(error.contains("managed_audit"), "{error}");
}

#[test]
fn an_abandoned_partial_pending_write_still_requires_exact_recovery() {
    let (home, reservation) = fixture();
    let root = home.path().join("git");
    write_new(&reservation.directory, Path::new(PENDING), b"{").unwrap();
    drop(reservation);
    let error = observe(&root).unwrap_err();
    assert!(error.to_string().contains("managed_audit"), "{error}");
    assert_eq!(
        std::fs::read(root.join(super::super::ROOT).join(PENDING)).unwrap(),
        b"{"
    );
}
