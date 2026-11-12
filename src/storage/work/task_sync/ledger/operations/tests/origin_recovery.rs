use super::*;
use crate::storage::work::task_sync::{ledger::recovery, producing_origin};
use crate::task_sync::origin::{
    EnrollRequest,
    recovery::{RecoveryCause, RecoveryRequest},
};

fn enrolled() -> (
    crate::test_support::TempHome,
    SqliteStore,
    Binding,
    ObjectId,
) {
    let home = crate::test_support::temp_home().unwrap();
    let mut binding = binding(home.path());
    binding.store.database = crate::project_database_path(home.path(), &binding.project);
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
    binding.store.origin = Some(origin.origin_id);
    binding.store.cut = producing_origin::cut_on(&store.connection, &binding.project).unwrap();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    (home, store, binding, id)
}

fn recover(
    store: &mut SqliteStore,
    binding: &Binding,
) -> crate::task_sync::origin::recovery::RecoveryReceipt {
    store
        .recover_task_sync_origin(
            &binding.project,
            &binding.actor,
            &RecoveryRequest {
                operation: uuid::Uuid::new_v4(),
                predecessor: binding.store.origin.clone().unwrap(),
                cause: RecoveryCause::DbOnlyClone,
            },
            at(12),
        )
        .unwrap()
}

#[test]
fn recovery_quarantines_old_head_without_supersession_or_changing_its_bytes() {
    let (_home, mut store, binding, old) = enrolled();
    let prior_events = store.task_sync_exchange_events(&old).unwrap();
    let prior_bytes = SqliteStore::get_canonical_object_on(&store.connection, &old, BINDING_KIND)
        .unwrap()
        .unwrap();
    let receipt = recover(&mut store, &binding);
    assert!(!receipt.record.bindings[0].unresolved);
    let mut successor = binding.clone();
    successor.store.origin = Some(receipt.record.successor_origin.clone());
    successor.store.cut = producing_origin::cut_on(&store.connection, &binding.project).unwrap();
    let new = store.activate_task_sync_binding(&successor).unwrap();
    assert_ne!(new, old);
    assert!(successor.supersedes.is_none());
    assert_eq!(store.task_sync_exchange_events(&old).unwrap(), prior_events);
    assert_eq!(
        SqliteStore::get_canonical_object_on(&store.connection, &old, BINDING_KIND)
            .unwrap()
            .unwrap()
            .bytes(),
        prior_bytes.bytes()
    );
    assert_eq!(
        recovery::origin_recovery_on(
            &store.connection,
            &binding.project,
            receipt.record.request.operation
        )
        .unwrap(),
        Some(receipt)
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn recovery_preserves_unresolved_stage_and_holds_target_even_if_remote_alias_changes() {
    let (_home, mut store, binding, old) = enrolled();
    let selected = input(&store, &old, &binding, candidate());
    store.prepare_task_sync_exchange(&selected, at(6)).unwrap();
    let before = store.task_sync_exchange_events(&old).unwrap();
    let receipt = recover(&mut store, &binding);
    assert!(receipt.record.bindings[0].unresolved);
    let mut successor = binding.clone();
    successor.store.origin = Some(receipt.record.successor_origin);
    successor.store.cut = producing_origin::cut_on(&store.connection, &binding.project).unwrap();
    assert!(recovery::require_scope_on(&store.connection, &successor).is_err());
    successor.target.remote = "another-alias".into();
    assert!(recovery::require_scope_on(&store.connection, &successor).is_err());
    successor.branch = "refs/heads/unrelated".into();
    successor.target.reference = "refs/heads/unrelated".into();
    recovery::require_scope_on(&store.connection, &successor).unwrap();
    // Setup is local. Neither activation nor an unrelated target pretends the
    // predecessor's prepared operation completed or was cancelled.
    store.activate_task_sync_binding(&successor).unwrap();
    assert_eq!(store.task_sync_exchange_events(&old).unwrap(), before);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn recovery_projection_damage_refuses_and_explicit_repair_keeps_canonical_receipt() {
    let (_home, mut store, binding, old) = enrolled();
    let receipt = recover(&mut store, &binding);
    let bytes = SqliteStore::get_canonical_object_on(
        &store.connection,
        &receipt.recovery,
        crate::task_sync::origin::recovery::RECOVERY_KIND,
    )
    .unwrap()
    .unwrap();
    store
        .connection
        .execute("DELETE FROM work_task_sync_origin_recovery_bindings", [])
        .unwrap();
    let before = state(&store);
    assert!(
        store
            .task_sync_binding(&old)
            .unwrap_err()
            .to_string()
            .contains("repair projections")
    );
    assert_eq!(state(&store), before);
    assert!(!store.verify_all().unwrap().is_healthy());
    drop(store);
    assert!(
        SqliteStore::repair_rebuildable_projections(&binding.store.database)
            .unwrap()
            .is_healthy()
    );
    let store = SqliteStore::open(&binding.store.database).unwrap();
    assert_eq!(
        SqliteStore::get_canonical_object_on(
            &store.connection,
            &receipt.recovery,
            crate::task_sync::origin::recovery::RECOVERY_KIND
        )
        .unwrap()
        .unwrap()
        .bytes(),
        bytes.bytes()
    );
    assert_eq!(
        recovery::origin_recovery_on(
            &store.connection,
            &binding.project,
            receipt.record.request.operation
        )
        .unwrap(),
        Some(receipt)
    );
}

#[test]
fn recovery_projection_failure_rolls_back_successor_objects_and_project_feed() {
    let (_home, mut store, binding, _old) = enrolled();
    store.connection.execute_batch("CREATE TRIGGER refuse_recovery_projection BEFORE INSERT
        ON work_task_sync_origin_recovery_bindings BEGIN SELECT RAISE(ABORT,'injected recovery projection'); END;").unwrap();
    let request = RecoveryRequest {
        operation: uuid::Uuid::new_v4(),
        predecessor: binding.store.origin.clone().unwrap(),
        cause: RecoveryCause::DbOnlyClone,
    };
    let before = state(&store);
    assert!(
        store
            .recover_task_sync_origin(&binding.project, &binding.actor, &request, at(12))
            .unwrap_err()
            .to_string()
            .contains("injected recovery projection")
    );
    assert_eq!(state(&store), before);
    store
        .connection
        .execute_batch("DROP TRIGGER refuse_recovery_projection")
        .unwrap();
    let receipt = store
        .recover_task_sync_origin(&binding.project, &binding.actor, &request, at(13))
        .unwrap();
    assert_eq!(receipt.record.recovered_at, at(12));
    assert!(store.verify_all().unwrap().is_healthy());
}
