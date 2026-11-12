//! Recovery must account for one exact pin without bypassing other custody.

use super::*;

#[tokio::test]
async fn abandoned_fetch_refuses_other_unowned_operations_and_changed_selection() {
    let (_home, mut store, binding, id, mut request) = super::data_bootstrap::fixture().await;
    let CustodySeedSelection::IncomingDataCommit { commit } = request.seed.clone() else {
        unreachable!()
    };
    request.reason = CheckpointReason::NamedAudit;
    request.seed = CustodySeedSelection::AbandonFetchedCommit {
        binding: id.clone(),
        commit: commit.clone(),
    };
    let before = state(&store);
    let mut changed = request.clone();
    let CustodySeedSelection::AbandonFetchedCommit {
        commit: changed_commit,
        ..
    } = &mut changed.seed
    else {
        unreachable!()
    };
    changed_commit.raw_bytes_fingerprint = CanonicalObject::freeze(&"changed commit")
        .unwrap()
        .key()
        .clone();
    assert!(
        store
            .audit_task_sync_custody(&changed, &binding.actor, at(30))
            .is_err()
    );
    assert_eq!(state(&store), before);
    let unrelated = uuid::Uuid::new_v4();
    crate::task_sync::exchange::retain_commit(
        &binding.repository.main_checkout,
        unrelated,
        &commit.oid,
        crate::task_sync::exchange::RunLimits::default(),
        &crate::task_sync::exchange::Cancellation::default(),
    )
    .await
    .unwrap();
    let inventory =
        crate::task_sync::retention::inventory(&binding.repository.git_directory).unwrap();
    let error = store
        .audit_task_sync_custody(&request, &binding.actor, at(31))
        .unwrap_err();
    assert!(
        error.to_string().contains("retained operations are ahead"),
        "{error}"
    );
    assert_eq!(state(&store), before);
    assert_eq!(
        crate::task_sync::retention::inventory(&binding.repository.git_directory).unwrap(),
        inventory
    );
}

#[tokio::test]
async fn abandoned_fetch_recovers_exact_checkpoint_after_interrupted_ack_and_rebuild() {
    let (_home, mut store, binding, id, mut request) = super::data_bootstrap::fixture().await;
    let CustodySeedSelection::IncomingDataCommit { commit } = request.seed.clone() else {
        unreachable!()
    };
    request.reason = CheckpointReason::NamedAudit;
    request.seed = CustodySeedSelection::AbandonFetchedCommit {
        binding: id.clone(),
        commit,
    };
    super::super::super::audit::FAIL_STAGE.with(|failure| failure.set(Some("repository_ack")));
    assert!(
        store
            .audit_task_sync_custody(&request, &binding.actor, at(30))
            .is_err()
    );
    let pending =
        crate::task_sync::origin::private::read(&binding.store.database, &binding.project)
            .unwrap()
            .audit
            .unwrap();
    let receipt = store
        .audit_task_sync_custody(&request, &binding.actor, at(31))
        .unwrap();
    assert_eq!(receipt.checkpoint, pending.event_id);
    assert!(receipt.repeated && receipt.acknowledged);
    assert!(store.verify_all().unwrap().is_healthy());
    drop(store);
    SqliteStore::repair_rebuildable_projections(&binding.store.database).unwrap();
    let mut store = SqliteStore::open(&binding.store.database).unwrap();
    audit_healthy(&mut store, &binding, &id);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[tokio::test]
async fn abandoned_fetch_refuses_checkpoint_identity_reuse_and_pending_execution_authority() {
    use crate::task_sync::origin::private;
    let (_home, mut store, binding, id, original) = super::data_bootstrap::fixture().await;
    let CustodySeedSelection::IncomingDataCommit { commit } = original.seed.clone() else {
        unreachable!()
    };
    let mut request = original.clone();
    request.reason = CheckpointReason::NamedAudit;
    request.seed = CustodySeedSelection::AbandonFetchedCommit {
        binding: id.clone(),
        commit,
    };
    let witness = private::read(&binding.store.database, &binding.project)
        .unwrap()
        .witness
        .unwrap();
    let mut pending = witness.clone();
    pending.pending_exchange = Some(ObjectId::mint());
    private::advance(&binding.store.database, &witness, &pending).unwrap();
    let before = state(&store);
    let error = store
        .audit_task_sync_custody(&request, &binding.actor, at(30))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("exchange intent remains pending"),
        "{error}"
    );
    assert_eq!(state(&store), before);
    assert_eq!(
        private::read(&binding.store.database, &binding.project)
            .unwrap()
            .witness,
        Some(pending.clone())
    );
    // Only the test restores its injected witness. Recovery itself cannot
    // release a pending executor or publication intent.
    private::advance(&binding.store.database, &pending, &witness).unwrap();
    store
        .audit_task_sync_custody(&original, &binding.actor, at(31))
        .unwrap();
    let mut request = audit_request(&store, &binding, &id, request.seed);
    request.operation = original.operation;
    let before = state(&store);
    let error = store
        .audit_task_sync_custody(&request, &binding.actor, at(32))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("exchange request key or operation differs from recorded intent"),
        "{error}"
    );
    assert_eq!(state(&store), before);
    request.key = original.key;
    let error = store
        .audit_task_sync_custody(&request, &binding.actor, at(33))
        .unwrap_err();
    assert!(error.to_string().contains("original checkpoint"), "{error}");
    assert_eq!(state(&store), before);
    assert!(store.verify_all().unwrap().is_healthy());
}
