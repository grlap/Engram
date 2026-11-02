use super::*;
use crate::CanonicalObject;
use crate::storage::work::{
    task_sync::tests::{candidate, ledger::binding, state},
    test_support::{actor, at},
};
use crate::task_sync::ledger::PublicationObservation;

#[test]
fn negative_tip_observations_remain_unresolved_across_restart_and_block_supersession() {
    let home = crate::test_support::temp_home().unwrap();
    let database = home.path().join("work.db");
    let mut store = SqliteStore::open(&database).unwrap();
    let binding = binding(home.path());
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let mut tree = candidate();
    tree.tasks.clear();
    let mut selected = super::super::tests::input(&store, &id, &binding, tree);
    selected.intent.direction = Direction::Outgoing;
    store.prepare_task_sync_exchange(&selected, at(6)).unwrap();
    let initial = state(&store);
    assert!(
        store
            .record_task_sync_exchange_progress(&selected, OutgoingProgress::PushStarted, at(7))
            .is_err()
    );
    assert_eq!(state(&store), initial);
    let committed = store
        .record_task_sync_exchange_progress(&selected, OutgoingProgress::LocalCommitRecorded, at(8))
        .unwrap();
    let pushing = store
        .record_task_sync_exchange_progress(&selected, OutgoingProgress::PushStarted, at(9))
        .unwrap();
    let unknown =
        OutgoingProgress::Observed(PublicationObservation::Unknown { observed_tip: None });
    store
        .record_task_sync_exchange_progress(&selected, unknown.clone(), at(10))
        .unwrap();
    drop(store);
    let mut store = SqliteStore::open(&database).unwrap();
    // Neither an absent tip nor another current tip proves that the prior push
    // never applied. Both observations leave the same operation unresolved.
    let other_tip = CanonicalObject::freeze(&"another current tip")
        .unwrap()
        .key()
        .as_str()
        .to_owned();
    let negative = OutgoingProgress::Observed(PublicationObservation::Unknown {
        observed_tip: Some(other_tip.clone()),
    });
    let last = store
        .record_task_sync_exchange_progress(&selected, negative.clone(), at(11))
        .unwrap();
    let rows = state(&store);
    assert_eq!(
        store
            .record_task_sync_exchange_progress(&selected, negative, at(12))
            .unwrap(),
        last
    );
    assert_eq!(
        store
            .record_task_sync_exchange_progress(
                &selected,
                OutgoingProgress::LocalCommitRecorded,
                at(12)
            )
            .unwrap(),
        committed
    );
    assert_eq!(
        store
            .record_task_sync_exchange_progress(&selected, OutgoingProgress::PushStarted, at(12))
            .unwrap(),
        pushing
    );
    assert_eq!(state(&store), rows);
    let events = store.task_sync_exchange_events(&id).unwrap();
    assert_eq!(events.len(), 6);
    for (index, (_, event)) in events.iter().enumerate() {
        assert_eq!(event.sequence, u64::try_from(index + 1).unwrap());
        assert_eq!(
            event.predecessor.as_ref(),
            index.checked_sub(1).map(|previous| &events[previous].0)
        );
    }
    assert!(matches!(
        &events.last().unwrap().1.transition,
        Transition::PushObserved {
            observation: PublicationObservation::Unknown { .. },
            ..
        }
    ));
    let mut successor = binding.clone();
    successor.supersedes = Some(id.clone());
    successor.superseded_tail = Some(last);
    successor.store.restore_occurrence = Some(uuid::Uuid::new_v4());
    assert!(
        store
            .activate_task_sync_binding(&successor)
            .unwrap_err()
            .to_string()
            .contains("unresolved")
    );
    let false_confirmation = OutgoingProgress::Observed(PublicationObservation::Confirmed {
        observed_tip: other_tip,
    });
    assert!(
        store
            .record_task_sync_exchange_progress(&selected, false_confirmation, at(13))
            .is_err()
    );
    assert_eq!(state(&store), rows);
    let exact = OutgoingProgress::Observed(PublicationObservation::Confirmed {
        observed_tip: selected.intent.commit.oid.clone(),
    });
    let confirmed = store
        .record_task_sync_exchange_progress(&selected, exact.clone(), at(14))
        .unwrap();
    let settled = state(&store);
    assert_eq!(
        store
            .record_task_sync_exchange_progress(&selected, exact, at(15))
            .unwrap(),
        confirmed
    );
    assert_eq!(state(&store), settled);
    successor.superseded_tail = Some(confirmed);
    store.activate_task_sync_binding(&successor).unwrap();
    assert!(
        store
            .record_task_sync_exchange_progress(&selected, unknown, at(16))
            .is_err()
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn outgoing_progress_refuses_changed_request_binding_or_stale_candidate_without_writes() {
    let home = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(home.path().join("work.db")).unwrap();
    let binding = binding(home.path());
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let mut selected = super::super::tests::input(&store, &id, &binding, candidate());
    selected.intent.direction = Direction::Outgoing;
    let before = state(&store);
    assert!(
        store
            .prepare_task_sync_exchange(&selected, at(6))
            .unwrap_err()
            .to_string()
            .contains("outgoing candidate differs")
    );
    assert_eq!(state(&store), before);
    // Use the retained request as an incoming import to settle its artifacts,
    // then admit a new, exact outgoing snapshot.
    selected.intent.direction = Direction::Incoming;
    store.prepare_task_sync_exchange(&selected, at(6)).unwrap();
    store
        .apply_task_sync_exchange(&selected, false, at(7))
        .unwrap();
    let mut outgoing = super::super::tests::input(&store, &id, &binding, selected.tree.clone());
    outgoing.intent.direction = Direction::Outgoing;
    store.prepare_task_sync_exchange(&outgoing, at(8)).unwrap();
    let before = state(&store);
    for scenario in ["actor", "key", "branch", "profile", "restore"] {
        let mut changed = outgoing.clone();
        match scenario {
            "actor" => changed.intent.actor = actor("other-session"),
            "key" => changed.intent.key.push_str("-changed"),
            "branch" => changed.binding.branch = "refs/heads/other".into(),
            "profile" => changed.binding.profile.include_notes = true,
            "restore" => changed.binding.store.restore_occurrence = Some(uuid::Uuid::new_v4()),
            _ => unreachable!(),
        }
        assert!(
            store
                .record_task_sync_exchange_progress(
                    &changed,
                    OutgoingProgress::LocalCommitRecorded,
                    at(9)
                )
                .is_err(),
            "{scenario}"
        );
        assert_eq!(state(&store), before, "{scenario}");
    }
}

#[test]
fn publication_witness_preserves_sql_rollback_rewind_and_missing_artifact_uncertainty() {
    for scenario in ["rollback", "rewind", "missing", "partial"] {
        let home = crate::test_support::temp_home().unwrap();
        let database = home.path().join("work.db");
        let mut store = SqliteStore::open(&database).unwrap();
        let binding = binding(home.path());
        let id = store.activate_task_sync_binding(&binding).unwrap();
        let mut tree = candidate();
        tree.tasks.clear();
        let mut selected = super::super::tests::input(&store, &id, &binding, tree);
        selected.intent.direction = Direction::Outgoing;
        let prepared = store.prepare_task_sync_exchange(&selected, at(6)).unwrap();
        store
            .record_task_sync_exchange_progress(
                &selected,
                OutgoingProgress::LocalCommitRecorded,
                at(7),
            )
            .unwrap();
        let witness_path = binding
            .repository
            .git_directory
            .join("engram-task-sync-publication-attempts")
            .join(format!("{}.json", selected.intent.operation));
        match scenario {
            "rollback" => {
                let transaction = store.begin_work_mutation().unwrap();
                let mut ledger = Ledger::load(&transaction).unwrap();
                let object = append(
                    &transaction,
                    &mut ledger,
                    &selected,
                    Transition::PushStarted { prepared },
                    at(8),
                )
                .unwrap();
                retention::retain_publication_witness(
                    &binding.repository.git_directory,
                    &retention::PublicationWitness {
                        operation: selected.intent.operation,
                        event_id: object.key().clone(),
                        event: object.decode().unwrap(),
                    },
                )
                .unwrap();
                // Crash after the external witness was flushed, before SQL COMMIT.
            }
            "rewind" => {
                let copy = home.path().join("before-publication.db");
                store.backup_to(&copy).unwrap();
                store
                    .record_task_sync_exchange_progress(
                        &selected,
                        OutgoingProgress::PushStarted,
                        at(8),
                    )
                    .unwrap();
                drop(store);
                assert!(!home.path().join("work.db-wal").exists());
                assert!(!home.path().join("work.db-shm").exists());
                std::fs::copy(copy, &database).unwrap();
                store = SqliteStore::open(&database).unwrap();
            }
            "missing" | "partial" => {
                store
                    .record_task_sync_exchange_progress(
                        &selected,
                        OutgoingProgress::PushStarted,
                        at(8),
                    )
                    .unwrap();
                if scenario == "missing" {
                    std::fs::remove_file(&witness_path).unwrap();
                } else {
                    std::fs::write(&witness_path, b"{").unwrap();
                }
            }
            _ => unreachable!(),
        }
        let files_before = std::fs::read(&witness_path).ok();
        let before = state(&store);
        assert!(
            store
                .record_task_sync_exchange_progress(&selected, OutgoingProgress::PushStarted, at(9))
                .is_err(),
            "{scenario}"
        );
        assert!(
            store.prepare_task_sync_exchange(&selected, at(10)).is_err(),
            "{scenario}"
        );
        assert_eq!(state(&store), before, "{scenario}");
        assert_eq!(
            std::fs::read(&witness_path).ok(),
            files_before,
            "{scenario}"
        );
    }
}

/// An outgoing exchange publishes what preparation exported, so a native
/// store with shared notes and an origin alias is admitted on the same basis
/// as its prepared candidate, and its progress can be recorded.
#[test]
fn an_outgoing_exchange_of_native_work_with_notes_is_admitted() {
    let home = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(home.path().join("work.db")).unwrap();
    let mut binding = binding(home.path());
    binding.profile = crate::task_sync::progress::ExportProfile {
        include_notes: true,
        origin_machine: Some("host-a".into()),
    };
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let mut request =
        crate::storage::work::test_support::root_request(&binding.project.0, "native", 0);
    request.notes = vec!["a native note shared by this host".into()];
    store
        .create_work(&request, &crate::DevelopmentNoopRedactor)
        .unwrap();
    let exported = store
        .export_task_tree_with_options(
            &binding.project,
            &crate::task_sync::TaskSyncExportOptions {
                include_notes: true,
                origin_machine: Some("host-a".into()),
            },
        )
        .unwrap();
    assert!(exported.tree.project.notes_included);
    let mut selected = super::super::tests::input(&store, &id, &binding, exported.tree);
    selected.intent.direction = Direction::Outgoing;
    selected.intent.local_planning_basis = selected.intent.candidate.planning_fingerprint.clone();
    store.prepare_task_sync_exchange(&selected, at(6)).unwrap();
    store
        .record_task_sync_exchange_progress(&selected, OutgoingProgress::LocalCommitRecorded, at(7))
        .unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
}
