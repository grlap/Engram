use super::*;
use crate::storage::work::{
    task_sync::tests::{ledger::binding, state},
    test_support::at,
};
use crate::{
    project_database_path,
    task_sync::origin::{EnrollRequest, recovery::RecoveryCause},
};

fn fixture() -> (
    crate::test_support::TempHome,
    SqliteStore,
    crate::task_sync::ledger::Binding,
    crate::task_sync::origin::ProducingOrigin,
) {
    let home = crate::test_support::temp_home().unwrap();
    let mut binding = binding(home.path());
    binding.store.database = project_database_path(home.path(), &binding.project);
    std::fs::create_dir_all(binding.store.database.parent().unwrap()).unwrap();
    std::fs::create_dir_all(&binding.repository.git_directory).unwrap();
    let mut store = SqliteStore::open(&binding.store.database).unwrap();
    let origin = store
        .enroll_task_sync_origin(
            &binding.project,
            &binding.actor,
            &EnrollRequest {
                operation: uuid::Uuid::new_v4(),
                predecessor: None,
            },
            at(1),
        )
        .unwrap();
    binding.store.origin = Some(origin.origin_id.clone());
    binding.store.cut = cut_on(&store.connection, &binding.project).unwrap();
    (home, store, binding, origin)
}

fn original_bytes(store: &SqliteStore, id: &ObjectId) -> Vec<u8> {
    store
        .connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_id=?1",
            [id.as_str()],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn cloned_and_migrated_enrolled_stores_recover_without_adopting_old_custody() {
    for cause in [RecoveryCause::DbOnlyClone, RecoveryCause::MigrationImport] {
        let (home, store, binding, origin) = fixture();
        let destination = project_database_path(&home.path().join("destination"), &binding.project);
        std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
        let old_bytes = original_bytes(&store, &origin.origin_id);
        if cause == RecoveryCause::DbOnlyClone {
            store.backup_to(&destination).unwrap();
        } else {
            let export = home.path().join("migration.jsonl");
            crate::storage::migration::export_json(&binding.store.database, &export).unwrap();
            // Import adds its own staging suffix and SQLite sidecars. Keep that
            // path short, then use a coherent backup in the project layout.
            let migrated = home.path().join("m.db");
            crate::storage::migration::import_json(&export, &migrated).unwrap();
            let migrated_store = SqliteStore::open(&migrated).unwrap();
            assert!(migrated_store.verify_all().unwrap().is_healthy());
            assert_eq!(
                original_bytes(&migrated_store, &origin.origin_id),
                old_bytes
            );
            migrated_store.backup_to(&destination).unwrap();
        }
        let mut destination_store = SqliteStore::open(&destination).unwrap();
        let before = state(&destination_store);
        assert!(
            destination_store
                .enroll_task_sync_origin(
                    &binding.project,
                    &binding.actor,
                    &EnrollRequest {
                        operation: uuid::Uuid::new_v4(),
                        predecessor: Some(origin.origin_id.clone())
                    },
                    at(2)
                )
                .is_err()
        );
        assert_eq!(state(&destination_store), before);
        let request = RecoveryRequest {
            operation: uuid::Uuid::new_v4(),
            predecessor: origin.origin_id.clone(),
            cause,
        };
        let receipt = destination_store
            .recover_task_sync_origin(&binding.project, &binding.actor, &request, at(3))
            .unwrap();
        assert_ne!(receipt.record.successor_origin, origin.origin_id);
        assert_eq!(receipt.record.request, request);
        assert!(receipt.record.custody.registration.is_none());
        let current = load_on(&destination_store.connection, &binding.project)
            .unwrap()
            .unwrap();
        assert_eq!(current.origin_id, receipt.record.successor_origin);
        assert_eq!(
            current.enrollment.predecessor_origin_id,
            Some(origin.origin_id.clone())
        );
        assert_eq!(
            original_bytes(&destination_store, &origin.origin_id),
            old_bytes
        );
        let after = state(&destination_store);
        assert_eq!(
            destination_store
                .recover_task_sync_origin(&binding.project, &binding.actor, &request, at(9))
                .unwrap(),
            receipt
        );
        assert_eq!(state(&destination_store), after);
        assert!(destination_store.verify_all().unwrap().is_healthy());
        // Losing the new private proof does not let an ordinary receipt retry
        // recreate it. The receipt remains history, not admission authority.
        std::fs::remove_file(
            destination
                .parent()
                .unwrap()
                .join("task-sync-origin-registration.json"),
        )
        .unwrap();
        assert_eq!(
            destination_store
                .recover_task_sync_origin(&binding.project, &binding.actor, &request, at(10))
                .unwrap(),
            receipt
        );
        assert!(
            !destination
                .parent()
                .unwrap()
                .join("task-sync-origin-registration.json")
                .exists()
        );
    }
}

#[test]
fn same_origin_rewind_preserves_newer_witness_references_missing_from_old_database() {
    let (home, mut store, binding, origin) = fixture();
    let older = home.path().join("older.db");
    store.backup_to(&older).unwrap();
    let old_binding = store.activate_task_sync_binding(&binding).unwrap();
    let witness = std::fs::read(
        binding
            .store
            .database
            .parent()
            .unwrap()
            .join("task-sync-origin-registration.json"),
    )
    .unwrap();
    let restored = project_database_path(&home.path().join("restored"), &binding.project);
    std::fs::create_dir_all(restored.parent().unwrap()).unwrap();
    std::fs::copy(older, &restored).unwrap();
    std::fs::write(
        restored
            .parent()
            .unwrap()
            .join("task-sync-origin-registration.json"),
        &witness,
    )
    .unwrap();
    let mut restored_store = SqliteStore::open(&restored).unwrap();
    let request = RecoveryRequest {
        operation: uuid::Uuid::new_v4(),
        predecessor: origin.origin_id.clone(),
        cause: RecoveryCause::SameOriginRewind,
    };
    let receipt = restored_store
        .recover_task_sync_origin(&binding.project, &binding.actor, &request, at(5))
        .unwrap();
    let reported = receipt.record.custody.witness.as_ref().unwrap();
    assert_eq!(reported.origin, origin.origin_id);
    assert_eq!(reported.exchange.as_ref().unwrap().binding, old_binding);
    assert!(
        receipt.record.bindings.is_empty(),
        "the old DB never had that binding"
    );
    let archived = restored
        .parent()
        .unwrap()
        .join("task-sync-origin-recovery")
        .join(request.operation.to_string())
        .join("task-sync-origin-registration.json");
    assert_eq!(std::fs::read(archived).unwrap(), witness);
    assert_ne!(
        load_on(&restored_store.connection, &binding.project)
            .unwrap()
            .unwrap()
            .origin_id,
        origin.origin_id
    );
}

#[test]
fn interrupted_recovery_reuses_exact_recipe_before_and_after_sql_commit() {
    for point in [Fault::BeforeCommit, Fault::AfterCommit] {
        let (_home, mut store, binding, origin) = fixture();
        let before = state(&store);
        let request = RecoveryRequest {
            operation: uuid::Uuid::new_v4(),
            predecessor: origin.origin_id,
            cause: RecoveryCause::DbOnlyClone,
        };
        FAULT.with(|fault| fault.set(Some(point)));
        assert!(
            store
                .recover_task_sync_origin(&binding.project, &binding.actor, &request, at(4))
                .unwrap_err()
                .to_string()
                .contains("injected")
        );
        let reservation =
            RecoveryReservation::acquire(&binding.store.database, &binding.project).unwrap();
        let retained = reservation.recipe().unwrap().unwrap();
        drop(reservation);
        if point == Fault::BeforeCommit {
            assert_eq!(state(&store), before);
        }
        assert!(
            crate::task_sync::origin::private::read(&binding.store.database, &binding.project)
                .is_err()
        );
        let changed = RecoveryRequest {
            cause: RecoveryCause::MigrationImport,
            ..request.clone()
        };
        assert!(
            store
                .recover_task_sync_origin(&binding.project, &binding.actor, &changed, at(5))
                .is_err()
        );
        let receipt = store
            .recover_task_sync_origin(&binding.project, &binding.actor, &request, at(8))
            .unwrap();
        assert_eq!(receipt.recovery, retained.recovery_id);
        assert_eq!(receipt.record, retained.recovery);
        assert_eq!(
            original_bytes(&store, &receipt.recovery),
            retained.recovery_object().unwrap().bytes()
        );
        assert!(
            crate::task_sync::origin::private::read(&binding.store.database, &binding.project)
                .is_ok()
        );
        assert!(
            !binding
                .store
                .database
                .parent()
                .unwrap()
                .join("task-sync-origin-recovery-pending.json")
                .exists()
        );
    }
}

#[test]
fn recovery_preserves_uninterpreted_pending_private_bytes_before_replacing_active_slots() {
    let (_home, mut store, binding, origin) = fixture();
    let parent = binding.store.database.parent().unwrap();
    let old_pending = b"{\"retained_unknown_enrollment\":true}";
    let old_audit = b"{\"retained_unknown_audit\":true}";
    std::fs::write(parent.join("task-sync-origin-pending.json"), old_pending).unwrap();
    std::fs::write(
        parent.join("task-sync-custody-audit-pending.json"),
        old_audit,
    )
    .unwrap();
    let request = RecoveryRequest {
        operation: uuid::Uuid::new_v4(),
        predecessor: origin.origin_id,
        cause: RecoveryCause::MigrationImport,
    };
    let receipt = store
        .recover_task_sync_origin(&binding.project, &binding.actor, &request, at(4))
        .unwrap();
    assert!(receipt.record.custody.enrollment_intent.is_some());
    assert!(receipt.record.custody.audit_intent.is_some());
    let archive = parent
        .join("task-sync-origin-recovery")
        .join(request.operation.to_string());
    assert_eq!(
        std::fs::read(archive.join("task-sync-origin-pending.json")).unwrap(),
        old_pending
    );
    assert_eq!(
        std::fs::read(archive.join("task-sync-custody-audit-pending.json")).unwrap(),
        old_audit
    );
    assert!(!parent.join("task-sync-origin-pending.json").exists());
    assert!(!parent.join("task-sync-custody-audit-pending.json").exists());
    assert!(
        crate::task_sync::origin::private::read(&binding.store.database, &binding.project).is_ok()
    );
}

#[test]
fn stale_uncommitted_recovery_can_be_cancelled_without_losing_predecessor_custody() {
    let (_home, mut store, binding, origin) = fixture();
    let old_origin_bytes = original_bytes(&store, &origin.origin_id);
    let request = RecoveryRequest {
        operation: uuid::Uuid::new_v4(),
        predecessor: origin.origin_id.clone(),
        cause: RecoveryCause::DbOnlyClone,
    };
    let witness_path = binding
        .store
        .database
        .parent()
        .unwrap()
        .join("task-sync-origin-registration.json");
    let old_witness = std::fs::read(&witness_path).unwrap();
    FAULT.with(|fault| fault.set(Some(Fault::BeforeCommit)));
    assert!(
        store
            .recover_task_sync_origin(&binding.project, &binding.actor, &request, at(4))
            .is_err()
    );
    let work = store
        .create_work(
            &crate::storage::work::test_support::root_request(
                &binding.project.0,
                "ordinary-work",
                5,
            ),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    assert!(
        store
            .recover_task_sync_origin(&binding.project, &binding.actor, &request, at(6))
            .is_err(),
        "the frozen source cut moved, so retry must not silently rebase the recipe"
    );
    let before = state(&store);
    assert!(
        store
            .cancel_task_sync_origin_recovery(&binding.project, &binding.actor, &request)
            .unwrap()
    );
    assert!(
        !store
            .cancel_task_sync_origin_recovery(&binding.project, &binding.actor, &request)
            .unwrap()
    );
    assert_eq!(state(&store), before);
    assert_eq!(std::fs::read(&witness_path).unwrap(), old_witness);
    assert_eq!(original_bytes(&store, &origin.origin_id), old_origin_bytes);
    assert!(store.get_work_item(work.work_id).is_ok());
    let fresh = RecoveryRequest {
        operation: uuid::Uuid::new_v4(),
        ..request
    };
    let receipt = store
        .recover_task_sync_origin(&binding.project, &binding.actor, &fresh, at(7))
        .unwrap();
    assert_ne!(receipt.record.successor_origin, origin.origin_id);
    assert!(
        store
            .cancel_task_sync_origin_recovery(&binding.project, &binding.actor, &fresh)
            .is_err()
    );
}

#[test]
fn recovered_binding_activation_preserves_unknown_artifacts_while_managed_exchange_stays_held() {
    let (home, mut store, binding, origin) = fixture();
    let older = home.path().join("before-binding.db");
    store.backup_to(&older).unwrap();
    store.activate_task_sync_binding(&binding).unwrap();
    let ahead_witness = std::fs::read(
        binding
            .store
            .database
            .parent()
            .unwrap()
            .join("task-sync-origin-registration.json"),
    )
    .unwrap();
    let publication_root = binding
        .repository
        .git_directory
        .join("engram-task-sync-publication-attempts");
    std::fs::create_dir_all(&publication_root).unwrap();
    let unknown = publication_root.join(format!("{}.json", uuid::Uuid::new_v4()));
    let retained = b"unknown predecessor publication bytes";
    std::fs::write(&unknown, retained).unwrap();
    let restored = project_database_path(&home.path().join("rewound"), &binding.project);
    std::fs::create_dir_all(restored.parent().unwrap()).unwrap();
    std::fs::copy(older, &restored).unwrap();
    std::fs::write(
        restored
            .parent()
            .unwrap()
            .join("task-sync-origin-registration.json"),
        ahead_witness,
    )
    .unwrap();
    let mut restored_store = SqliteStore::open(&restored).unwrap();
    let request = RecoveryRequest {
        operation: uuid::Uuid::new_v4(),
        predecessor: origin.origin_id,
        cause: RecoveryCause::SameOriginRewind,
    };
    let receipt = restored_store
        .recover_task_sync_origin(&binding.project, &binding.actor, &request, at(8))
        .unwrap();
    let mut successor = binding.clone();
    successor.store.database = restored;
    successor.store.origin = Some(receipt.record.successor_origin.clone());
    successor.store.cut = cut_on(&restored_store.connection, &binding.project).unwrap();
    let id = restored_store
        .activate_task_sync_binding(&successor)
        .unwrap();
    assert_ne!(id, receipt.recovery);
    assert_eq!(std::fs::read(&unknown).unwrap(), retained);
    let before = state(&restored_store);
    let entered_effect = std::cell::Cell::new(false);
    let managed_request = CanonicalObject::freeze(&"managed exchange request").unwrap();
    assert!(
        super::super::with_exchange(
            &mut restored_store,
            &binding.project,
            &binding.repository.git_directory,
            Some(uuid::Uuid::new_v4()),
            managed_request.key(),
            |_, _| {
                entered_effect.set(true);
                Ok(())
            }
        )
        .is_err()
    );
    assert!(!entered_effect.get());
    assert_eq!(state(&restored_store), before);
    assert_eq!(std::fs::read(unknown).unwrap(), retained);
    assert!(restored_store.verify_all().unwrap().is_healthy());
}
