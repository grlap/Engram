use super::*;
use crate::task_sync::{exchange::delta::InspectedDelta, ledger::RetainedCommit, origin::private};

mod attempt;
mod progress;
mod resolution;

#[cfg(windows)]
#[tokio::test]
async fn delta_commit_and_progress_observe_abandoned_git_before_the_writer() {
    use super::super::writer_probe::{Probe, abandoned};
    let (home, store, binding, id) = abandoned().await;
    let mut f = ExportFixture::from_store(home, store, binding, id, 1);
    let probe = Probe::new(&f.binding.store.database);
    let (input, selection) = selected(&mut f);
    f.store
        .record_task_sync_delta_export_commit(&input, &selection, at(41))
        .unwrap();
    let (started, _) = f
        .store
        .record_task_sync_delta_progress_once(
            &input,
            &selection,
            crate::task_sync::ledger::OutgoingProgress::PushStarted,
            at(42),
        )
        .unwrap();
    f.store
        .record_task_sync_delta_progress_once(
            &input,
            &selection,
            crate::task_sync::ledger::OutgoingProgress::PushRejected {
                push_started: started,
            },
            at(43),
        )
        .unwrap();
    probe.assert_observed();
    assert!(f.store.verify_all().unwrap().is_healthy());
}

pub(super) fn selected(f: &mut ExportFixture) -> (InspectedDelta, DeltaCommitSelection) {
    let prepared = f
        .store
        .prepare_task_sync_delta_export(&f.id, &f.intent, at(40))
        .unwrap();
    // Substitute only Git observation here. The writer still uses real retained
    // request bytes, private custody, SQLite and the common canonical reducer.
    let fingerprint = |label| {
        CanonicalObject::freeze(&(f.intent.operation, label))
            .unwrap()
            .key()
            .to_string()
    };
    let commit = RetainedCommit {
        oid: fingerprint("selected commit"),
        tree: fingerprint("selected tree"),
        parents: Vec::new(),
        retention_ref: format!(
            "refs/engram/task-sync/exchange/{}/commit",
            f.intent.operation
        ),
        raw_bytes_fingerprint: CanonicalObject::freeze(&(f.intent.operation, "raw commit"))
            .unwrap()
            .key()
            .clone(),
    };
    let path = f
        .binding
        .repository
        .git_directory
        .join("engram-task-sync-commits");
    std::fs::create_dir_all(&path).unwrap();
    std::fs::write(
        path.join(format!("{}.json", f.intent.operation)),
        crate::canonical::canonical_bytes(&commit).unwrap(),
    )
    .unwrap();
    (
        InspectedDelta {
            binding_id: f.id.clone(),
            binding: f.binding.clone(),
            intent: f.intent.clone(),
            payload: f.payload.clone(),
        },
        DeltaCommitSelection { prepared, commit },
    )
}

#[test]
fn outgoing_delta_commit_recovers_lost_ack_and_replays_after_native_edits() {
    let mut f = ExportFixture::new(2);
    let (input, selection) = selected(&mut f);
    let before_witness = private::read(&f.binding.store.database, &f.binding.project)
        .unwrap()
        .witness
        .unwrap();
    let committed = f
        .store
        .record_task_sync_delta_export_commit(&input, &selection, at(41))
        .unwrap();
    let after_witness = private::read(&f.binding.store.database, &f.binding.project)
        .unwrap()
        .witness
        .unwrap();
    let request =
        retention::delta::commit_request_fingerprint(&f.id, &f.intent, &selection).unwrap();
    let mut pending = before_witness;
    pending.pending_exchange = Some(request);
    private::advance(&f.binding.store.database, &after_witness, &pending).unwrap();
    let before = state(&f.store);
    drop(f.store);
    let mut store = SqliteStore::open(&f.binding.store.database).unwrap();
    assert_eq!(
        store
            .record_task_sync_delta_export_commit(&input, &selection, at(42))
            .unwrap(),
        committed
    );
    assert_eq!(state(&store), before);
    assert_eq!(
        private::read(&f.binding.store.database, &f.binding.project)
            .unwrap()
            .witness,
        Some(after_witness)
    );
    store
        .create_work(
            &crate::storage::work::test_support::root_request(
                &f.binding.project.0,
                "Native edit after outgoing commit",
                50,
            ),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let before = state(&store);
    assert_eq!(
        store
            .record_task_sync_delta_export_commit(&input, &selection, at(51))
            .unwrap(),
        committed
    );
    assert_eq!(state(&store), before);
    let mut changed = selection.clone();
    changed.prepared = ObjectId::mint();
    assert!(
        store
            .record_task_sync_delta_export_commit(&input, &changed, at(52))
            .is_err()
    );
    assert_eq!(state(&store), before);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn outgoing_delta_commit_sql_failure_preserves_selection_and_exact_retry() {
    let mut f = ExportFixture::new(2);
    let (input, selection) = selected(&mut f);
    let before = state(&f.store);
    let mut changed = selection.clone();
    changed.commit.tree = CanonicalObject::freeze(&"different retained tree")
        .unwrap()
        .key()
        .to_string();
    assert!(
        f.store
            .record_task_sync_delta_export_commit(&input, &changed, at(41))
            .is_err()
    );
    assert_eq!(state(&f.store), before);
    f.store.connection.execute_batch("CREATE TEMP TRIGGER fail_delta_commit BEFORE INSERT ON objects
        WHEN NEW.object_kind='task_sync_exchange_event' AND json_extract(NEW.canonical_json,'$.transition.transition')='delta_local_commit_recorded'
        BEGIN SELECT RAISE(ABORT,'fixture delta commit failure'); END;").unwrap();
    assert!(
        f.store
            .record_task_sync_delta_export_commit(&input, &selection, at(42))
            .is_err()
    );
    assert_eq!(state(&f.store), before);
    assert_eq!(
        retention::commit_selection(&f.binding.repository.git_directory, f.intent.operation)
            .unwrap(),
        Some(selection.commit.clone())
    );
    f.store
        .connection
        .execute_batch("DROP TRIGGER fail_delta_commit")
        .unwrap();
    let committed = f
        .store
        .record_task_sync_delta_export_commit(&input, &selection, at(43))
        .unwrap();
    let before = state(&f.store);
    assert_eq!(
        f.store
            .record_task_sync_delta_export_commit(&input, &selection, at(44))
            .unwrap(),
        committed
    );
    assert_eq!(state(&f.store), before);
    assert!(f.store.verify_all().unwrap().is_healthy());
}
