use super::*;
use crate::task_sync::retention::audit::journal;

#[test]
fn managed_legacy_full_exchange_preserves_explicit_cold_custody() {
    use crate::task_sync::ledger::{OutgoingProgress, PublicationObservation};
    for outgoing in [false, true] {
        let (_home, mut store, binding, tree) = initial_fixture();
        let id = store.activate_task_sync_binding(&binding).unwrap();
        let mut request = audit_request(&store, &binding, &id, CustodySeedSelection::CustodyOnly);
        request.reason = CheckpointReason::ManagedAudit;
        store
            .audit_task_sync_custody(&request, &binding.actor, at(30))
            .unwrap();
        let tree = if outgoing {
            store.export_task_tree(&binding.project).unwrap()
        } else {
            tree
        };
        let cut = producing_origin::cut_on(&store.connection, &binding.project).unwrap();
        let mut input = super::super::tests::input_with_cut(&store, &id, &binding, tree, cut);
        if outgoing {
            input.intent.direction = Direction::Outgoing;
        }
        let prepared = store.prepare_task_sync_exchange(&input, at(40)).unwrap();
        assert_eq!(
            store.prepare_task_sync_exchange(&input, at(41)).unwrap(),
            prepared
        );
        if outgoing {
            store
                .record_task_sync_exchange_progress(
                    &input,
                    OutgoingProgress::LocalCommitRecorded,
                    at(42),
                )
                .unwrap();
            let (started, fresh) = store
                .record_task_sync_exchange_progress_once(
                    &input,
                    OutgoingProgress::PushStarted,
                    at(43),
                )
                .unwrap();
            assert!(fresh);
            assert_eq!(
                store
                    .record_task_sync_exchange_progress_once(
                        &input,
                        OutgoingProgress::PushStarted,
                        at(44)
                    )
                    .unwrap(),
                (started, false)
            );
            store
                .record_task_sync_exchange_progress(
                    &input,
                    OutgoingProgress::Observed(PublicationObservation::Confirmed {
                        observed_tip: input.intent.commit.oid.clone(),
                    }),
                    at(45),
                )
                .unwrap();
        } else {
            let first = store
                .apply_task_sync_exchange(&input, false, at(42))
                .unwrap();
            assert_eq!(
                store
                    .apply_task_sync_exchange(&input, false, at(43))
                    .unwrap(),
                first
            );
        }
        assert!(
            journal::observe(&binding.repository.git_directory)
                .unwrap()
                .unwrap()
                .writes
                .is_empty()
        );
        assert!(store.verify_all().unwrap().is_healthy());
    }
}

#[test]
fn managed_cold_checkpoint_selects_bounded_custody_and_named_audit_still_finds_damage() {
    let (_home, mut store, binding, _) = initial_fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let mut request = audit_request(&store, &binding, &id, CustodySeedSelection::CustodyOnly);
    request.reason = CheckpointReason::ManagedAudit;
    let receipt = store
        .audit_task_sync_custody(&request, &binding.actor, at(100))
        .unwrap();
    assert!(receipt.acknowledged);
    let root = &binding.repository.git_directory;
    let journal = journal::observe(root).unwrap().unwrap();
    assert_eq!(journal.checkpoint.checkpoint, receipt.checkpoint);
    assert!(journal.writes.is_empty());

    let commits = root.join("engram-task-sync-commits");
    std::fs::create_dir(&commits).unwrap();
    let mut files = Vec::new();
    for _ in 0..100 {
        let path = commits.join(format!("{}.json", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"partial old input").unwrap();
        files.push(path);
    }
    let observation = super::super::super::custody::Observed::capture(root).unwrap();
    super::super::super::custody::check_on(
        &store.connection,
        root,
        &observation,
        None,
        None,
        None,
        &mut super::super::super::view::read::ReadBudget::default(),
    )
    .unwrap();
    let audit = audit_request(&store, &binding, &id, CustodySeedSelection::CustodyOnly);
    assert!(
        store
            .audit_task_sync_custody(&audit, &binding.actor, at(101))
            .is_err()
    );
    for path in files {
        assert_eq!(std::fs::read(path).unwrap(), b"partial old input");
    }
    assert_eq!(journal::observe(root).unwrap().unwrap(), journal);
}

#[test]
fn managed_checkpoint_projection_mismatch_refuses_without_cold_fallback() {
    let (_home, mut store, binding, _) = initial_fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let mut request = audit_request(&store, &binding, &id, CustodySeedSelection::CustodyOnly);
    request.reason = CheckpointReason::ManagedAudit;
    store
        .audit_task_sync_custody(&request, &binding.actor, at(100))
        .unwrap();
    let root = &binding.repository.git_directory;
    let observation = super::super::super::custody::Observed::capture(root).unwrap();
    store
        .connection
        .execute(
            "UPDATE work_task_sync_exchange_heads SET last_checkpoint_id=NULL WHERE binding_id=?1",
            [id.as_str()],
        )
        .unwrap();
    let error = super::super::super::custody::check_on(
        &store.connection,
        root,
        &observation,
        None,
        None,
        None,
        &mut super::super::super::view::read::ReadBudget::default(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("repair projections"), "{error}");
}

#[test]
fn managed_checkpoint_missing_index_requires_explicit_projection_repair() {
    let (_home, mut store, binding, _) = initial_fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let mut request = audit_request(&store, &binding, &id, CustodySeedSelection::CustodyOnly);
    request.reason = CheckpointReason::ManagedAudit;
    store
        .audit_task_sync_custody(&request, &binding.actor, at(100))
        .unwrap();
    let root = &binding.repository.git_directory;
    let before = journal::observe(root).unwrap();
    let observed = super::super::super::custody::Observed::capture(root).unwrap();
    store
        .connection
        .execute_batch("DROP INDEX objects_task_sync_custody_checkpoint")
        .unwrap();
    let error = super::super::super::custody::check_on(
        &store.connection,
        root,
        &observed,
        None,
        None,
        None,
        &mut super::super::super::view::read::ReadBudget::default(),
    )
    .unwrap_err();
    assert!(error.to_string().contains("repair projections"), "{error}");
    assert_eq!(journal::observe(root).unwrap(), before);
    // Rebuild through the index's existing projection schema owner; no private
    // proof or new checkpoint is created by repairing its lookup structure.
    super::super::super::custody::managed::create_schema(&store.connection).unwrap();
    super::super::super::custody::check_on(
        &store.connection,
        root,
        &observed,
        None,
        None,
        None,
        &mut super::super::super::view::read::ReadBudget::default(),
    )
    .unwrap();
    assert_eq!(journal::observe(root).unwrap(), before);
}
