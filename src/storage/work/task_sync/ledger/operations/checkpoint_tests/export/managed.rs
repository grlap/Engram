//! Real origin and canonical SQL lifecycle with an admitted managed checkpoint.
//! As in the export fixtures, only the selected Git commit facts are substituted.

use super::*;
use crate::task_sync::{
    ledger::{OutgoingProgress, PublicationObservation},
    retention::audit::journal,
};

pub(super) fn fixture() -> ExportFixture {
    let (home, mut store, binding, _) = initial_fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    ExportFixture::from_store_mode(home, store, binding, id, 2, true)
}

fn journal_of(f: &ExportFixture) -> journal::Journal {
    journal::observe(&f.binding.repository.git_directory)
        .unwrap()
        .unwrap()
}

#[test]
fn managed_export_preparation_commit_and_publication_replay_without_disconnected_history() {
    let mut f = fixture();
    let checkpoint = journal_of(&f).checkpoint;
    let (input, selection) = commit::selected(&mut f);
    assert_eq!(journal_of(&f).writes.len(), 1);
    // These disconnected malformed files would fail exhaustive discovery. They
    // are outside the selected operation after the complete cold checkpoint.
    let root = f
        .binding
        .repository
        .git_directory
        .join("engram-task-sync-commits");
    let mut old_files = Vec::new();
    for _ in 0..100 {
        let path = root.join(format!("{}.json", uuid::Uuid::new_v4()));
        std::fs::write(&path, b"old partial selection").unwrap();
        old_files.push(path);
    }
    let committed = f
        .store
        .record_task_sync_delta_export_commit(&input, &selection, at(41))
        .unwrap();
    let (started, fresh) = f
        .store
        .record_task_sync_delta_progress_once(
            &input,
            &selection,
            OutgoingProgress::PushStarted,
            at(42),
        )
        .unwrap();
    assert!(fresh);
    let unknown =
        OutgoingProgress::Observed(PublicationObservation::Unknown { observed_tip: None });
    let (observed, _) = f
        .store
        .record_task_sync_delta_progress_once(&input, &selection, unknown.clone(), at(43))
        .unwrap();
    assert!(journal_of(&f).writes.is_empty());
    drop(f.store);
    let mut store = SqliteStore::open(&f.binding.store.database).unwrap();
    let before = state(&store);
    assert_eq!(
        store
            .record_task_sync_delta_export_commit(&input, &selection, at(44))
            .unwrap(),
        committed
    );
    assert_eq!(
        store
            .record_task_sync_delta_progress_once(
                &input,
                &selection,
                OutgoingProgress::PushStarted,
                at(45)
            )
            .unwrap(),
        (started, false)
    );
    assert_eq!(
        store
            .record_task_sync_delta_progress_once(&input, &selection, unknown, at(46))
            .unwrap(),
        (observed, false)
    );
    assert_eq!(state(&store), before);
    let confirmed = OutgoingProgress::Observed(PublicationObservation::Confirmed {
        observed_tip: selection.commit.oid.clone(),
    });
    let (outcome, fresh) = store
        .record_task_sync_delta_progress_once(&input, &selection, confirmed.clone(), at(47))
        .unwrap();
    assert!(fresh);
    assert_eq!(
        store
            .record_task_sync_delta_progress_once(&input, &selection, confirmed, at(48))
            .unwrap(),
        (outcome, false)
    );
    let current = journal::observe(&f.binding.repository.git_directory)
        .unwrap()
        .unwrap();
    assert_eq!(current.checkpoint, checkpoint);
    assert!(current.writes.is_empty());
    for path in old_files {
        assert_eq!(std::fs::read(path).unwrap(), b"old partial selection");
    }
    assert!(retention::inventory(&f.binding.repository.git_directory).is_err());
}

#[test]
fn managed_preparation_sql_rollback_preserves_exact_write_ahead_for_retry() {
    let mut f = fixture();
    let before = state(&f.store);
    f.store.connection.execute_batch("CREATE TEMP TRIGGER refuse_managed_prepare BEFORE INSERT ON objects
        WHEN NEW.object_kind='task_sync_exchange_event' AND json_extract(NEW.canonical_json,'$.transition.transition')='delta_prepared'
        BEGIN SELECT RAISE(ABORT,'fixture preparation rollback'); END;").unwrap();
    assert!(
        f.store
            .prepare_task_sync_delta_export(&f.id, &f.intent, at(40))
            .is_err()
    );
    assert_eq!(state(&f.store), before);
    let pending = journal_of(&f);
    assert_eq!(pending.writes.len(), 1);
    let retained = retention::delta::read_candidate(&f.id, &f.binding, &f.intent)
        .unwrap()
        .unwrap();
    assert_eq!(retained, f.payload);
    f.store
        .connection
        .execute_batch("DROP TRIGGER refuse_managed_prepare")
        .unwrap();
    let prepared = f
        .store
        .prepare_task_sync_delta_export(&f.id, &f.intent, at(41))
        .unwrap();
    assert!(journal_of(&f).writes.is_empty());
    let after = state(&f.store);
    assert_eq!(
        f.store
            .prepare_task_sync_delta_export(&f.id, &f.intent, at(42))
            .unwrap(),
        prepared
    );
    assert_eq!(state(&f.store), after);
    assert_eq!(
        retention::delta::read_candidate(&f.id, &f.binding, &f.intent).unwrap(),
        Some(retained)
    );
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[test]
fn managed_publication_rollback_cannot_grant_another_dispatch() {
    let mut f = fixture();
    let (input, selection) = commit::selected(&mut f);
    f.store
        .record_task_sync_delta_export_commit(&input, &selection, at(41))
        .unwrap();
    let before = state(&f.store);
    {
        let transaction = f.store.begin_work_mutation().unwrap();
        let tail = super::super::super::super::view::read::tail_on(
            &transaction,
            &f.id,
            &f.binding,
            &mut super::super::super::super::view::read::ReadBudget::default(),
        )
        .unwrap();
        let object = CanonicalObject::mint(&Event {
            schema_version: 1,
            binding: f.id.clone(),
            sequence: tail.event.sequence + 1,
            predecessor: Some(tail.id),
            actor: f.intent.actor.clone(),
            recorded_at: at(42),
            transition: Transition::PushStarted {
                prepared: selection.prepared.clone(),
            },
        })
        .unwrap();
        super::super::super::super::operations::store_event_keyed(
            &transaction,
            &f.binding,
            &object,
        )
        .unwrap();
        retention::retain_publication_witness(
            &f.binding.repository.git_directory,
            &retention::PublicationWitness {
                operation: f.intent.operation,
                event_id: object.key().clone(),
                event: object.decode().unwrap(),
            },
        )
        .unwrap();
        // Lose the SQL transaction only after the write-ahead request and the
        // exact external publication witness have both been flushed.
    }
    let pending = journal_of(&f);
    let witness = retention::read_publication_witness(
        &f.binding.repository.git_directory,
        f.intent.operation,
    )
    .unwrap();
    assert!(
        f.store
            .record_task_sync_delta_progress_once(
                &input,
                &selection,
                OutgoingProgress::PushStarted,
                at(43)
            )
            .is_err()
    );
    assert_eq!(state(&f.store), before);
    assert_eq!(journal_of(&f), pending);
    assert_eq!(
        retention::read_publication_witness(
            &f.binding.repository.git_directory,
            f.intent.operation
        )
        .unwrap(),
        witness
    );
}
