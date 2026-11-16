use super::*;
use crate::task_sync::retention::{self, audit::AuditReservation};

fn managed() -> (
    crate::test_support::TempHome,
    SqliteStore,
    Binding,
    ObjectId,
    crate::task_sync::TaskTree,
) {
    let (home, mut store, binding, tree) = initial_fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let mut request = audit_request(&store, &binding, &id, CustodySeedSelection::CustodyOnly);
    request.reason = CheckpointReason::ManagedAudit;
    store
        .audit_task_sync_custody(&request, &binding.actor, at(30))
        .unwrap();
    (home, store, binding, id, tree)
}

#[test]
fn nonmanaged_audit_cannot_replace_an_active_managed_checkpoint() {
    let (_home, mut store, binding, id, _) = managed();
    let root = &binding.repository.git_directory;
    let before = journal::observe(root).unwrap().unwrap();
    let database = state(&store);
    let mut request = audit_request(&store, &binding, &id, CustodySeedSelection::CustodyOnly);
    let error = store
        .audit_task_sync_custody(&request, &binding.actor, at(40))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("run a custody audit with reason managed_audit"),
        "{error}"
    );
    assert_eq!(state(&store), database);
    assert_eq!(journal::observe(root).unwrap().unwrap(), before);
    request.reason = CheckpointReason::ManagedAudit;
    let receipt = store
        .audit_task_sync_custody(&request, &binding.actor, at(41))
        .unwrap();
    assert_eq!(
        journal::observe(root)
            .unwrap()
            .unwrap()
            .checkpoint
            .checkpoint,
        receipt.checkpoint
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn a_recorded_audit_only_request_replays_after_managed_custody_activation() {
    let (_home, mut store, binding, _) = initial_fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let request = audit_request(&store, &binding, &id, CustodySeedSelection::CustodyOnly);
    assert_eq!(request.reason, CheckpointReason::NamedAudit);
    let original = store
        .audit_task_sync_custody(&request, &binding.actor, at(30))
        .unwrap();
    let mut activation = audit_request(&store, &binding, &id, CustodySeedSelection::CustodyOnly);
    activation.reason = CheckpointReason::ManagedAudit;
    store
        .audit_task_sync_custody(&activation, &binding.actor, at(31))
        .unwrap();
    let root = &binding.repository.git_directory;
    let journal = journal::observe(root).unwrap().unwrap();
    let database = state(&store);
    let repeated = store
        .audit_task_sync_custody(&request, &binding.actor, at(32))
        .unwrap();
    assert!(repeated.repeated && repeated.acknowledged);
    assert_eq!(repeated.checkpoint, original.checkpoint);
    assert_eq!(state(&store), database);
    assert_eq!(journal::observe(root).unwrap().unwrap(), journal);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn settlement_reobserves_artifacts_created_after_a_concurrent_no_effect_snapshot() {
    let (_home, mut store, binding, id, tree) = managed();
    let root = binding.repository.git_directory.clone();
    let cut = producing_origin::cut_on(&store.connection, &binding.project).unwrap();
    let input = super::super::super::tests::input_with_cut(&store, &id, &binding, tree, cut);
    store.prepare_task_sync_exchange(&input, at(40)).unwrap();
    let mut guard = store.prepare_task_sync_origin(&binding.project).unwrap();
    let operation = uuid::Uuid::new_v4();
    // A writer's retained selection names its own operation.
    let mut retained = input.intent.commit.clone();
    retained.retention_ref = format!("refs/engram/task-sync/exchange/{operation}/commit");
    let retry = retained.clone();
    let artifact = root
        .join("engram-task-sync-commits")
        .join(format!("{operation}.json"));
    let bytes = crate::canonical::canonical_bytes(&retained).unwrap();
    let (reserved, arrived) = std::sync::mpsc::sync_channel(0);
    let (write, proceed) = std::sync::mpsc::sync_channel(0);
    let writer_root = root.clone();
    let writer_artifact = artifact.clone();
    let writer_bytes = bytes.clone();
    let stale_check = std::thread::scope(|scope| {
        scope.spawn(move || {
            let reservation = AuditReservation::acquire(&writer_root).unwrap();
            reservation
                .reserve_artifact(operation, journal::Family::Commit, &retained)
                .unwrap();
            reserved.send(()).unwrap();
            proceed.recv().unwrap();
            // Model a writer that retains its selection and then rolls back
            // SQL or exits, leaving the write ahead for an exact retry.
            std::fs::write(writer_artifact, writer_bytes).unwrap();
        });
        arrived.recv().unwrap();
        let result = (|| -> Result<(), StoreError> {
            guard.capture_custody(&store.connection, Some(&root), None, None)?;
            let transaction = store.begin_work_mutation()?;
            crate::storage::work::task_sync::ledger::check_origin_custody_on(
                &transaction,
                &binding.project,
                guard.custody(),
                None,
                None,
                None,
            )
        })();
        // Always release the writer before testing the captured result.
        write.send(()).unwrap();
        result
    });
    stale_check.unwrap();
    let before = journal::observe(&root).unwrap().unwrap();
    assert_eq!(before.writes.len(), 1);
    let database = state(&store);
    let error =
        producing_origin::settle_exchange_custody(&mut store, &binding.project, &root, &mut guard)
            .unwrap_err();
    assert!(
        error.to_string().contains("retained operations are ahead"),
        "{error}"
    );
    assert_eq!(journal::observe(&root).unwrap().unwrap(), before);
    assert_eq!(std::fs::read(&artifact).unwrap(), bytes);
    assert_eq!(state(&store), database);
    // Its reservation remains usable by the original writer, rather than
    // disappearing from managed selection after the other operation settles.
    let reservation = AuditReservation::acquire(&root).unwrap();
    reservation
        .reserve_artifact(operation, journal::Family::Commit, &retry)
        .unwrap();
    assert_eq!(reservation.journal().unwrap().unwrap(), before);
}

#[test]
fn managed_audit_recovers_only_a_complete_pending_successor() {
    for kind in ["complete", "partial", "divergent"] {
        let (_home, mut store, binding, id, _) = managed();
        let root = &binding.repository.git_directory;
        let before = journal::observe(root).unwrap().unwrap();
        let mut pending = before.clone();
        pending.revision = uuid::Uuid::new_v4();
        pending.previous_revision = Some(if kind == "divergent" {
            uuid::Uuid::new_v4()
        } else {
            before.revision
        });
        let bytes = if kind == "partial" {
            b"{".to_vec()
        } else {
            crate::canonical::canonical_bytes(&pending).unwrap()
        };
        let path = root.join("engram-task-sync-custody-audits/managed.pending");
        std::fs::write(&path, &bytes).unwrap();
        let mut request = audit_request(&store, &binding, &id, CustodySeedSelection::CustodyOnly);
        request.reason = CheckpointReason::ManagedAudit;
        let database = state(&store);
        let result = store.audit_task_sync_custody(&request, &binding.actor, at(40));
        if kind == "complete" {
            let receipt = result.unwrap();
            assert!(!path.exists());
            assert_eq!(
                journal::observe(root)
                    .unwrap()
                    .unwrap()
                    .checkpoint
                    .checkpoint,
                receipt.checkpoint
            );
            assert!(store.verify_all().unwrap().is_healthy());
        } else {
            let error = result.unwrap_err();
            assert!(error.to_string().contains("managed_audit"), "{error}");
            assert_eq!(std::fs::read(&path).unwrap(), bytes);
            assert_eq!(state(&store), database);
            assert!(journal::observe(root).is_err());
            let published: journal::Journal = serde_json::from_slice(
                &std::fs::read(root.join("engram-task-sync-custody-audits/managed.json")).unwrap(),
            )
            .unwrap();
            assert_eq!(published, before);
        }
    }
}

#[test]
fn reservations_without_artifact_effects_settle_but_partial_artifacts_refuse() {
    for partial in [false, true] {
        let (_home, mut store, binding, id, tree) = managed();
        let root = &binding.repository.git_directory;
        let operation = uuid::Uuid::new_v4();
        {
            let reservation = AuditReservation::acquire(root).unwrap();
            for family in [
                journal::Family::Candidate,
                journal::Family::Delta,
                journal::Family::Commit,
                journal::Family::Import,
                journal::Family::Publication,
                journal::Family::Evidence,
                journal::Family::Successor,
            ] {
                reservation
                    .reserve_artifact(operation, family, &"reserved before effect")
                    .unwrap();
            }
        }
        let reserved = journal::observe(root).unwrap().unwrap();
        assert_eq!(reserved.writes.len(), 7);
        let file = root
            .join("engram-task-sync-commits")
            .join(format!("{operation}.json"));
        if partial {
            std::fs::create_dir_all(file.parent().unwrap()).unwrap();
            std::fs::write(&file, b"{").unwrap();
        }
        let cut = producing_origin::cut_on(&store.connection, &binding.project).unwrap();
        let input = super::super::super::tests::input_with_cut(&store, &id, &binding, tree, cut);
        // The input fixture retains its own candidate and commit. Capture the
        // refusal boundary after that setup, keeping every earlier reservation.
        let pending = journal::observe(root).unwrap().unwrap();
        for (key, write) in &reserved.writes {
            assert_eq!(pending.writes.get(key), Some(write));
        }
        let database = state(&store);
        let result = store.prepare_task_sync_exchange(&input, at(40));
        if partial {
            assert!(result.is_err());
            assert_eq!(std::fs::read(&file).unwrap(), b"{");
            assert_eq!(journal::observe(root).unwrap().unwrap(), pending);
            assert_eq!(state(&store), database);
            // Repair only this fixture's malformed file, then recover the same
            // prepared request without replacing its reserved operation ids.
            std::fs::remove_file(&file).unwrap();
            let receipt = store.prepare_task_sync_exchange(&input, at(41)).unwrap();
            assert_eq!(
                store.prepare_task_sync_exchange(&input, at(42)).unwrap(),
                receipt
            );
            assert!(journal::observe(root).unwrap().unwrap().writes.is_empty());
            assert!(store.verify_all().unwrap().is_healthy());
        } else {
            let receipt = result.unwrap();
            assert_eq!(
                store.prepare_task_sync_exchange(&input, at(41)).unwrap(),
                receipt
            );
            assert!(!file.exists());
            assert!(journal::observe(root).unwrap().unwrap().writes.is_empty());
            assert!(store.verify_all().unwrap().is_healthy());
        }
    }
}

fn prepared_candidate(
    home: &crate::test_support::TempHome,
    store: &SqliteStore,
    binding: &Binding,
    tree: crate::task_sync::TaskTree,
    padding: usize,
) -> crate::task_sync::progress::PreparedCandidate {
    use crate::task_sync::{
        notes::NoteOmissions,
        progress::{PreparedCandidate, PreparedProvenance},
    };
    let mut documents = tree.documents().unwrap();
    documents
        .get_mut("project.json")
        .unwrap()
        .extend(std::iter::repeat_n(b' ', padding));
    PreparedCandidate {
        documents,
        tree,
        provenance: PreparedProvenance {
            sequence: 1,
            operation: uuid::Uuid::new_v4(),
            predecessor: None,
            store: binding.store.database.clone(),
            destination: home.path().join("prepared"),
            project: binding.project.clone(),
            profile: binding.profile.clone(),
            producer: CanonicalObject::freeze(&"candidate producer")
                .unwrap()
                .key()
                .clone(),
            cut: producing_origin::cut_on(&store.connection, &binding.project).unwrap(),
            omissions: NoteOmissions::default(),
            activity_omissions: NoteOmissions::default(),
        },
    }
}

#[test]
fn a_different_request_for_a_settled_artifact_is_refused_without_poisoning_the_journal() {
    let (home, store, binding, _id, tree) = managed();
    let root = &binding.repository.git_directory;
    let original = prepared_candidate(&home, &store, &binding, tree.clone(), 0);
    let operation = uuid::Uuid::new_v4();
    let retained = retention::retain_prepared(root, operation, &original).unwrap();
    {
        // Settle the original write, as a checked exchange does after commit.
        let reservation = AuditReservation::acquire(root).unwrap();
        let observed = reservation.journal().unwrap().unwrap();
        assert_eq!(observed.writes.len(), 1);
        reservation.settle_journal(&observed).unwrap();
    }
    let settled = journal::observe(root).unwrap().unwrap();
    assert!(settled.writes.is_empty());
    let mut changed = original.clone();
    changed.provenance.producer = CanonicalObject::freeze(&"changed producer")
        .unwrap()
        .key()
        .clone();
    let error = retention::retain_prepared(root, operation, &changed).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("retry with the exact original request"),
        "{error}"
    );
    // Nothing was published for the refused request.
    assert_eq!(journal::observe(root).unwrap().unwrap(), settled);
    retention::validate_journal(root, &settled).unwrap();
    // The original request still replays, and an unrelated operation proceeds.
    assert_eq!(
        retention::retain_prepared(root, operation, &original).unwrap(),
        retained
    );
    let other = prepared_candidate(&home, &store, &binding, tree, 1);
    retention::retain_prepared(root, uuid::Uuid::new_v4(), &other).unwrap();
    retention::validate_journal(root, &journal::observe(root).unwrap().unwrap()).unwrap();
}

#[test]
fn legacy_candidate_payload_stays_out_of_the_selected_journal() {
    let (home, store, binding, _id, tree) = managed();
    let root = &binding.repository.git_directory;
    let mut prepared = prepared_candidate(&home, &store, &binding, tree, 512 * 1024);
    let operation = uuid::Uuid::new_v4();
    let retained = retention::retain_prepared(root, operation, &prepared).unwrap();
    assert!(
        std::fs::metadata(root.join("engram-task-sync-custody-audits/managed.json"))
            .unwrap()
            .len()
            < 16 * 1024
    );
    assert_eq!(
        retention::read_retained_documents(root, operation, &retained).unwrap(),
        prepared.documents
    );
    assert_eq!(
        retention::retain_prepared(root, operation, &prepared).unwrap(),
        retained
    );
    let before = journal::observe(root).unwrap().unwrap();
    prepared.provenance.producer = CanonicalObject::freeze(&"changed producer")
        .unwrap()
        .key()
        .clone();
    assert!(retention::retain_prepared(root, operation, &prepared).is_err());
    assert_eq!(journal::observe(root).unwrap().unwrap(), before);
}
