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
fn interrupted_exact_publication_recovers_without_reminting_its_revision() {
    let (_home, reservation) = fixture();
    let before = reservation.journal().unwrap().unwrap();
    let operation = uuid::Uuid::new_v4();
    let write = WriteAhead {
        operation,
        family: Family::Commit,
        request: serde_json::json!("selected"),
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
    assert!(reservation.journal().is_err());
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
