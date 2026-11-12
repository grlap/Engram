//! Observe the lock boundary at actual hardened Git process launches.

use super::*;
use std::{cell::Cell, rc::Rc};

pub(super) struct Probe {
    queries: Rc<Cell<usize>>,
    pins: Rc<Cell<usize>>,
}

impl Probe {
    pub(super) fn new(database: &std::path::Path) -> Self {
        let database = database.to_owned();
        let queries = Rc::new(Cell::new(0));
        let pins = Rc::new(Cell::new(0));
        let counts = (queries.clone(), pins.clone());
        crate::task_sync::git::BEFORE_QUERY.with(|slot| {
            assert!(slot.borrow().is_none());
            *slot.borrow_mut() = Some(Box::new(move |command| {
                let writer = rusqlite::Connection::open(&database).unwrap();
                writer.busy_timeout(std::time::Duration::ZERO).unwrap();
                writer.execute_batch("BEGIN IMMEDIATE; ROLLBACK;").unwrap_or_else(|error| {
                    panic!("Git {command} launched while another connection held the writer: {error}")
                });
                counts.0.set(counts.0.get() + 1);
                if matches!(command, "for-each-ref" | "cat-file") {
                    counts.1.set(counts.1.get() + 1);
                }
            }));
        });
        Self { queries, pins }
    }

    pub(super) fn assert_observed(&self) {
        assert!(self.queries.get() > 0, "no actual Git query was exercised");
        assert!(self.pins.get() > 0, "no abandoned pin query was exercised");
    }
}

impl Drop for Probe {
    fn drop(&mut self) {
        crate::task_sync::git::BEFORE_QUERY.with(|slot| *slot.borrow_mut() = None);
    }
}

pub(super) async fn abandoned() -> (
    crate::test_support::TempHome,
    SqliteStore,
    Binding,
    ObjectId,
) {
    let (home, mut store, binding, id, mut request) = super::data_bootstrap::fixture().await;
    let CustodySeedSelection::IncomingDataCommit { commit } = request.seed else {
        unreachable!()
    };
    request.reason = CheckpointReason::NamedAudit;
    request.seed = CustodySeedSelection::AbandonFetchedCommit {
        binding: id.clone(),
        commit,
    };
    let probe = Probe::new(&binding.store.database);
    store
        .audit_task_sync_custody(&request, &binding.actor, at(30))
        .unwrap();
    probe.assert_observed();
    drop(probe);
    (home, store, binding, id)
}

#[tokio::test]
async fn abandoned_carrier_git_queries_allow_a_second_sqlite_writer() {
    let (_home, mut store, binding, id) = abandoned().await;
    let probe = Probe::new(&binding.store.database);
    unrelated(&mut store, &binding.project);
    audit_healthy(&mut store, &binding, &id);
    let guard = store.prepare_task_sync_origin(&binding.project).unwrap();
    let transaction = store.begin_work_mutation().unwrap();
    producing_origin::check_on(&transaction, &binding.project, &guard)
        .unwrap()
        .require_on(&transaction)
        .unwrap();
    drop(transaction);
    probe.assert_observed();
}

#[tokio::test]
async fn abandoned_selection_changed_after_capture_refuses_without_state_changes() {
    let (_home, mut store, binding, _) = abandoned().await;
    let root = &binding.repository.git_directory;
    let observed = super::super::super::custody::Observed::capture(root).unwrap();
    let ledger = Ledger::load(&store.connection).unwrap();
    let artifact = ledger
        .events
        .values()
        .find_map(|event| match &event.transition {
            Transition::CustodyCheckpoint { checkpoint } => checkpoint.abandoned_commit(),
            _ => None,
        })
        .unwrap();
    let path = root
        .join("engram-task-sync-commits")
        .join(format!("{}.json", artifact.operation));
    let original = std::fs::read(&path).unwrap();
    let mut commit: crate::task_sync::ledger::RetainedCommit =
        serde_json::from_slice(&original).unwrap();
    commit.raw_bytes_fingerprint = CanonicalObject::freeze(&"changed observed selection")
        .unwrap()
        .key()
        .clone();
    std::fs::write(&path, crate::canonical::canonical_bytes(&commit).unwrap()).unwrap();
    let before = state(&store);
    let private_before =
        crate::task_sync::origin::private::read(&binding.store.database, &binding.project).unwrap();
    let inventory = retention::inventory(root).unwrap();
    let transaction = store.begin_work_mutation().unwrap();
    assert!(
        super::super::super::custody::check(&ledger, root, &observed, None, |a, b| a == b,)
            .is_err()
    );
    drop(transaction);
    assert_eq!(state(&store), before);
    assert_eq!(retention::inventory(root).unwrap(), inventory);
    assert_eq!(
        crate::task_sync::origin::private::read(&binding.store.database, &binding.project).unwrap(),
        private_before
    );
    std::fs::write(path, original).unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
}

#[tokio::test]
async fn an_additional_abandoned_owner_requires_a_new_pre_writer_observation() {
    let (_home, mut store, binding, id) = abandoned().await;
    let root = &binding.repository.git_directory;
    let observed = super::super::super::custody::Observed::capture(root).unwrap();
    let mut request = audit_request(&store, &binding, &id, CustodySeedSelection::CustodyOnly);
    let data = crate::task_sync::data_worktree::DataWorktree::from_binding(&binding).unwrap();
    let commit = crate::task_sync::exchange::retain_commit(
        &binding.repository.main_checkout,
        request.operation,
        data.head.as_ref().unwrap(),
        crate::task_sync::exchange::RunLimits::default(),
        &crate::task_sync::exchange::Cancellation::default(),
    )
    .await
    .unwrap();
    request.seed = CustodySeedSelection::AbandonFetchedCommit {
        binding: id,
        commit,
    };
    store
        .audit_task_sync_custody(&request, &binding.actor, at(31))
        .unwrap();
    let ledger = Ledger::load(&store.connection).unwrap();
    let before = state(&store);
    let inventory = retention::inventory(root).unwrap();
    let transaction = store.begin_work_mutation().unwrap();
    assert!(
        super::super::super::custody::check(&ledger, root, &observed, None, |a, b| a == b).is_err()
    );
    drop(transaction);
    assert_eq!(state(&store), before);
    assert_eq!(retention::inventory(root).unwrap(), inventory);
    assert!(store.verify_all().unwrap().is_healthy());
}
