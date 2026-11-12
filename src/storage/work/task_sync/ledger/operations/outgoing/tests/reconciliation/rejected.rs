use super::*;

#[test]
fn legacy_reconciliation_refuses_unknown_dispatch_and_accepts_only_recorded_rejection() {
    for rejected in [false, true] {
        for observed_unknown in [false, true] {
            if rejected && observed_unknown {
                continue;
            }
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
                .record_task_sync_exchange_progress(
                    &original,
                    OutgoingProgress::LocalCommitRecorded,
                    at(7),
                )
                .unwrap();
            let start = store
                .record_task_sync_exchange_progress(&original, OutgoingProgress::PushStarted, at(8))
                .unwrap();
            if rejected {
                store
                    .record_task_sync_exchange_progress(
                        &original,
                        OutgoingProgress::PushRejected {
                            push_started: start.clone(),
                        },
                        at(9),
                    )
                    .unwrap();
            } else if observed_unknown {
                store
                    .record_task_sync_exchange_progress(
                        &original,
                        OutgoingProgress::Observed(PublicationObservation::Unknown {
                            observed_tip: None,
                        }),
                        at(9),
                    )
                    .unwrap();
            }
            let tip = CanonicalObject::freeze(&"competing unconfirmed tip")
                .unwrap()
                .key()
                .to_string();
            let resolved = resolved_input(&store, &original, &tip);
            let transition = progress(
                &prepared,
                OutgoingReconciliation::Resolved {
                    failure: OutgoingFailure::NonFastForward {
                        push_started: start,
                        fetched_tip: tip,
                    },
                    resolved: Box::new(resolved.intent.clone()),
                },
            );
            let before = state(&store);
            let inventory = retention::inventory(&binding.repository.git_directory).unwrap();
            let witness = retention::read_publication_witness(
                &binding.repository.git_directory,
                original.intent.operation,
            )
            .unwrap();
            let result =
                store.record_task_sync_exchange_progress(&resolved, transition.clone(), at(10));
            if rejected {
                let id = result.unwrap();
                assert_eq!(
                    store
                        .record_task_sync_exchange_progress(&resolved, transition, at(11))
                        .unwrap(),
                    id
                );
            } else {
                let error = result.unwrap_err();
                assert!(
                    error.to_string().contains("uncertainty stays unresolved"),
                    "{error}"
                );
                assert_eq!(state(&store), before);
            }
            assert_eq!(
                retention::inventory(&binding.repository.git_directory).unwrap(),
                inventory
            );
            assert_eq!(
                retention::read_publication_witness(
                    &binding.repository.git_directory,
                    original.intent.operation
                )
                .unwrap(),
                witness
            );
            assert!(store.verify_all().unwrap().is_healthy());
        }
    }
}
