use super::super::super::tests::input;
use super::*;
use crate::task_sync::ledger::{CompletedExchangeRequest, OutgoingFailure, OutgoingReconciliation};
use std::path::Path;

mod rejected;

fn progress(prepared: &ObjectId, reconciliation: OutgoingReconciliation) -> OutgoingProgress {
    OutgoingProgress::Reconciled {
        prepared: prepared.clone(),
        reconciliation: Box::new(reconciliation),
    }
}

fn replay_after_reopen(
    mut store: SqliteStore,
    database: &Path,
    input: &InspectedExchange,
    progress: OutgoingProgress,
) -> (SqliteStore, ObjectId) {
    let id = store
        .record_task_sync_exchange_progress(input, progress.clone(), at(20))
        .unwrap();
    let committed = state(&store);
    drop(store); // The committed response was lost; the caller retains its exact request.
    let mut store = SqliteStore::open(database).unwrap();
    assert_eq!(
        store
            .record_task_sync_exchange_progress(input, progress, at(30))
            .unwrap(),
        id
    );
    assert_eq!(state(&store), committed);
    assert!(store.verify_all().unwrap().is_healthy());
    (store, id)
}

fn check_keyed_custody(store: &SqliteStore, root: &Path) -> Result<(), StoreError> {
    use crate::storage::work::task_sync::ledger::{custody, view::read};
    let observed = custody::Observed::capture(root)?;
    custody::check_on(
        &store.connection,
        root,
        &observed,
        None,
        None,
        None,
        &mut read::ReadBudget::default(),
    )
}

fn resolved_input(
    store: &SqliteStore,
    original: &InspectedExchange,
    tip: &str,
) -> InspectedExchange {
    let mut resolved = input(
        store,
        &original.binding_id,
        &original.binding,
        original.tree.clone(),
    );
    resolved.intent.direction = Direction::Outgoing;
    resolved.intent.expected_head = Some(original.intent.commit.oid.clone());
    // This fixture substitutes Git observation only. Its raw recipe still uses
    // the real retained files, canonical metadata and storage transaction path.
    resolved.intent.commit.oid =
        CanonicalObject::freeze(&(resolved.intent.operation, "resolved commit"))
            .unwrap()
            .key()
            .to_string();
    resolved.intent.commit.parents = vec![original.intent.commit.oid.clone(), tip.to_owned()];
    let path = original
        .binding
        .repository
        .git_directory
        .join("engram-task-sync-commits")
        .join(format!("{}.json", resolved.intent.operation));
    std::fs::write(
        path,
        crate::canonical::canonical_bytes(&resolved.intent.commit).unwrap(),
    )
    .unwrap();
    resolved
}

#[test]
fn outgoing_retry_steps_replay_after_restart_and_keep_both_publication_witnesses() {
    for pushed in [false, true] {
        for terminal in ["confirmed", "stopped", "unknown"] {
            let home = crate::test_support::temp_home().unwrap();
            let database = home.path().join("work.db");
            let mut store = SqliteStore::open(&database).unwrap();
            let binding = binding(home.path());
            let binding_id = store.activate_task_sync_binding(&binding).unwrap();
            let mut tree = candidate();
            tree.tasks.clear();
            let mut original = input(&store, &binding_id, &binding, tree);
            original.intent.direction = Direction::Outgoing;
            let prepared = store.prepare_task_sync_exchange(&original, at(6)).unwrap();
            let (mut store, _) = replay_after_reopen(
                store,
                &database,
                &original,
                OutgoingProgress::LocalCommitRecorded,
            );
            let tip = CanonicalObject::freeze(&"fetched competing data tip")
                .unwrap()
                .key()
                .to_string();
            let failure = if pushed {
                let (next, push_started) =
                    replay_after_reopen(store, &database, &original, OutgoingProgress::PushStarted);
                let (next, _) = replay_after_reopen(
                    next,
                    &database,
                    &original,
                    OutgoingProgress::PushRejected {
                        push_started: push_started.clone(),
                    },
                );
                store = next;
                OutgoingFailure::NonFastForward {
                    push_started,
                    fetched_tip: tip.clone(),
                }
            } else {
                OutgoingFailure::AncestryChanged {
                    fetched_tip: tip.clone(),
                }
            };
            let old_witness = retention::read_publication_witness(
                &binding.repository.git_directory,
                original.intent.operation,
            )
            .unwrap();
            let resolved = resolved_input(&store, &original, &tip);
            let (next, reconciliation) = replay_after_reopen(
                store,
                &database,
                &resolved,
                progress(
                    &prepared,
                    OutgoingReconciliation::Resolved {
                        failure,
                        resolved: Box::new(resolved.intent.clone()),
                    },
                ),
            );
            store = next;
            check_keyed_custody(&store, &binding.repository.git_directory).unwrap();
            let (mut store, retry_started) = replay_after_reopen(
                store,
                &database,
                &resolved,
                progress(
                    &prepared,
                    OutgoingReconciliation::PushStarted { reconciliation },
                ),
            );
            check_keyed_custody(&store, &binding.repository.git_directory).unwrap();
            assert_eq!(
                retention::read_publication_witness(
                    &binding.repository.git_directory,
                    original.intent.operation
                )
                .unwrap(),
                old_witness
            );
            let retry_witness = retention::read_publication_witness(
                &binding.repository.git_directory,
                resolved.intent.operation,
            )
            .unwrap()
            .unwrap();
            assert_eq!(retry_witness.event_id, retry_started);
            assert_ne!(Some(&retry_witness), old_witness.as_ref());
            if terminal == "stopped" {
                let (next, _) = replay_after_reopen(
                    store,
                    &database,
                    &resolved,
                    OutgoingProgress::PushRejected {
                        push_started: retry_started.clone(),
                    },
                );
                store = next;
            }
            let final_progress = match terminal {
                "confirmed" => OutgoingProgress::Observed(PublicationObservation::Confirmed {
                    observed_tip: resolved.intent.commit.oid.clone(),
                }),
                "unknown" => OutgoingProgress::Observed(PublicationObservation::Unknown {
                    observed_tip: None,
                }),
                "stopped" => progress(
                    &prepared,
                    OutgoingReconciliation::Stopped {
                        failure: OutgoingFailure::NonFastForward {
                            push_started: retry_started,
                            fetched_tip: tip.clone(),
                        },
                    },
                ),
                _ => unreachable!(),
            };
            let (next, outcome) = replay_after_reopen(store, &database, &resolved, final_progress);
            store = next;
            check_keyed_custody(&store, &binding.repository.git_directory).unwrap();
            let request = CompletedExchangeRequest {
                binding: binding_id.clone(),
                intent: original.intent.clone(),
            };
            let completed = store.completed_task_sync_exchange(&request).unwrap();
            assert_eq!(
                completed.as_ref().map(|value| &value.outcome),
                (terminal == "confirmed").then_some(&outcome)
            );
            let before = state(&store);
            assert!(
                store.prepare_task_sync_exchange(&resolved, at(40)).is_err(),
                "a retained retry recipe cannot become a second ledger operation"
            );
            assert_eq!(state(&store), before);
            let mut independent = input(&store, &binding_id, &binding, original.tree.clone());
            independent.intent.direction = Direction::Outgoing;
            assert_eq!(
                store
                    .prepare_task_sync_exchange(&independent, at(41))
                    .is_ok(),
                terminal != "unknown"
            );
        }
    }
}

#[test]
fn retry_custody_refuses_a_first_attempt_witness_relabelled_as_the_retry() {
    let home = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(home.path().join("work.db")).unwrap();
    let binding = binding(home.path());
    let binding_id = store.activate_task_sync_binding(&binding).unwrap();
    let mut tree = candidate();
    tree.tasks.clear();
    let mut original = input(&store, &binding_id, &binding, tree);
    original.intent.direction = Direction::Outgoing;
    let prepared = store.prepare_task_sync_exchange(&original, at(6)).unwrap();
    store
        .record_task_sync_exchange_progress(&original, OutgoingProgress::LocalCommitRecorded, at(7))
        .unwrap();
    let push_started = store
        .record_task_sync_exchange_progress(&original, OutgoingProgress::PushStarted, at(8))
        .unwrap();
    store
        .record_task_sync_exchange_progress(
            &original,
            OutgoingProgress::PushRejected {
                push_started: push_started.clone(),
            },
            at(8),
        )
        .unwrap();
    let tip = CanonicalObject::freeze(&"competing tip")
        .unwrap()
        .key()
        .to_string();
    let resolved = resolved_input(&store, &original, &tip);
    let reconciliation = store
        .record_task_sync_exchange_progress(
            &resolved,
            progress(
                &prepared,
                OutgoingReconciliation::Resolved {
                    failure: OutgoingFailure::NonFastForward {
                        push_started,
                        fetched_tip: tip,
                    },
                    resolved: Box::new(resolved.intent.clone()),
                },
            ),
            at(9),
        )
        .unwrap();
    store
        .record_task_sync_exchange_progress(
            &resolved,
            progress(
                &prepared,
                OutgoingReconciliation::PushStarted { reconciliation },
            ),
            at(10),
        )
        .unwrap();
    let mut counterfeit = retention::read_publication_witness(
        &binding.repository.git_directory,
        original.intent.operation,
    )
    .unwrap()
    .unwrap();
    counterfeit.operation = resolved.intent.operation;
    let path = binding
        .repository
        .git_directory
        .join("engram-task-sync-publication-attempts")
        .join(format!("{}.json", resolved.intent.operation));
    assert!(path.exists());
    let bytes = crate::canonical::canonical_bytes(&counterfeit).unwrap();
    std::fs::write(&path, &bytes).unwrap();
    assert!(check_keyed_custody(&store, &binding.repository.git_directory).is_err());
    let before = state(&store);
    assert!(
        store
            .record_task_sync_exchange_progress(
                &resolved,
                OutgoingProgress::Observed(PublicationObservation::Confirmed {
                    observed_tip: resolved.intent.commit.oid.clone()
                }),
                at(11)
            )
            .is_err()
    );
    assert_eq!(state(&store), before);
    assert_eq!(std::fs::read(path).unwrap(), bytes);
}

#[test]
fn an_uncommitted_or_rewound_retry_witness_preserves_uncertainty_without_reminting() {
    for scenario in ["rollback", "rewind"] {
        let home = crate::test_support::temp_home().unwrap();
        let database = home.path().join("work.db");
        let mut store = SqliteStore::open(&database).unwrap();
        let binding = binding(home.path());
        let binding_id = store.activate_task_sync_binding(&binding).unwrap();
        let mut tree = candidate();
        tree.tasks.clear();
        let mut original = input(&store, &binding_id, &binding, tree);
        original.intent.direction = Direction::Outgoing;
        let prepared = store.prepare_task_sync_exchange(&original, at(6)).unwrap();
        store
            .record_task_sync_exchange_progress(
                &original,
                OutgoingProgress::LocalCommitRecorded,
                at(7),
            )
            .unwrap();
        let tip = CanonicalObject::freeze(&"competing data tip")
            .unwrap()
            .key()
            .to_string();
        let resolved = resolved_input(&store, &original, &tip);
        let reconciliation = store
            .record_task_sync_exchange_progress(
                &resolved,
                progress(
                    &prepared,
                    OutgoingReconciliation::Resolved {
                        failure: OutgoingFailure::AncestryChanged { fetched_tip: tip },
                        resolved: Box::new(resolved.intent.clone()),
                    },
                ),
                at(8),
            )
            .unwrap();
        let start = OutgoingReconciliation::PushStarted { reconciliation };
        if scenario == "rollback" {
            let transaction = store.begin_work_mutation().unwrap();
            let mut ledger = Ledger::load(&transaction).unwrap();
            let object = append(
                &transaction,
                &mut ledger,
                &resolved,
                Transition::OutgoingReconciled {
                    prepared: prepared.clone(),
                    reconciliation: Box::new(start.clone()),
                },
                at(9),
            )
            .unwrap();
            retention::retain_publication_witness(
                &binding.repository.git_directory,
                &retention::PublicationWitness {
                    operation: resolved.intent.operation,
                    event_id: object.key().clone(),
                    event: object.decode().unwrap(),
                },
            )
            .unwrap();
            // The real retention boundary succeeded, but SQL COMMIT never ran.
        } else {
            let backup = home.path().join("before-retry.db");
            store.backup_to(&backup).unwrap();
            store
                .record_task_sync_exchange_progress(
                    &resolved,
                    progress(&prepared, start.clone()),
                    at(9),
                )
                .unwrap();
            drop(store);
            assert!(!home.path().join("work.db-wal").exists());
            assert!(!home.path().join("work.db-shm").exists());
            std::fs::copy(backup, &database).unwrap();
            store = SqliteStore::open(&database).unwrap();
        }
        let witness = retention::read_publication_witness(
            &binding.repository.git_directory,
            resolved.intent.operation,
        )
        .unwrap()
        .unwrap();
        let before = state(&store);
        let error = store
            .record_task_sync_exchange_progress(&resolved, progress(&prepared, start), at(10))
            .unwrap_err();
        assert!(
            error.to_string().contains("publication witness"),
            "{scenario}: {error}"
        );
        assert_eq!(state(&store), before);
        assert_eq!(
            retention::read_publication_witness(
                &binding.repository.git_directory,
                resolved.intent.operation
            )
            .unwrap(),
            Some(witness)
        );
        assert!(check_keyed_custody(&store, &binding.repository.git_directory).is_err());
    }
}
