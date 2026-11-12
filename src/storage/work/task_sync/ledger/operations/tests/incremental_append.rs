use super::super::super::{reduce, view};
use super::*;

fn proposal(store: &SqliteStore, id: &ObjectId, intent: &Intent) -> CanonicalObject {
    let events = store.task_sync_exchange_events(id).unwrap();
    let (tail, event) = events.last().unwrap();
    CanonicalObject::mint(&Event {
        schema_version: 1,
        binding: id.clone(),
        sequence: event.sequence + 1,
        predecessor: Some(tail.clone()),
        actor: intent.actor.clone(),
        recorded_at: at(10),
        transition: Transition::Prepared {
            intent: Box::new(intent.clone()),
        },
    })
    .unwrap()
}

#[test]
fn incremental_append_matches_full_reducer_and_preserves_original_operation_rows() {
    let home = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(home.path().join("work.db")).unwrap();
    let binding = binding(home.path());
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let first = input(&store, &id, &binding, candidate());
    store.prepare_task_sync_exchange(&first, at(6)).unwrap();
    store
        .apply_task_sync_exchange(&first, false, at(7))
        .unwrap();
    let original = store
        .completed_task_sync_exchange(&crate::task_sync::ledger::CompletedExchangeRequest {
            binding: id.clone(),
            intent: first.intent.clone(),
        })
        .unwrap()
        .unwrap();
    let second = input(
        &store,
        &id,
        &binding,
        store.export_task_tree(&binding.project).unwrap(),
    );
    let object = proposal(&store, &id, &second.intent);
    let proposed = view::append::propose(&store.connection, &binding, &object).unwrap();
    let mut chain = store.task_sync_exchange_events(&id).unwrap();
    chain.push((object.key().clone(), object.decode().unwrap()));
    let refs = chain
        .iter()
        .map(|(id, event)| (id, event))
        .collect::<Vec<_>>();
    let full = reduce::reduce(Some(&store.connection), &binding, &refs).unwrap();
    assert_eq!(proposed.head, full.head);
    let operation = second.intent.operation.to_string();
    assert_eq!(proposed.operations[&operation], full.operations[&operation]);
    store.prepare_task_sync_exchange(&second, at(10)).unwrap();
    store
        .apply_task_sync_exchange(&second, false, at(11))
        .unwrap();
    assert_eq!(
        store
            .completed_task_sync_exchange(&crate::task_sync::ledger::CompletedExchangeRequest {
                binding: id,
                intent: first.intent
            })
            .unwrap(),
        Some(original)
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn append_refuses_false_completion_metadata_duplicate_key_and_broken_predecessor_without_writes() {
    let home = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(home.path().join("work.db")).unwrap();
    let binding = binding(home.path());
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let selected = input(&store, &id, &binding, candidate());
    let object = proposal(&store, &id, &selected.intent);
    store.connection.execute("UPDATE work_task_sync_exchange_heads SET last_incoming_completed_id=?1 WHERE binding_id=?1",
        [id.as_str()]).unwrap();
    let before = state(&store);
    assert!(
        view::append::propose(&store.connection, &binding, &object)
            .err()
            .unwrap()
            .to_string()
            .contains("repair projections")
    );
    assert_eq!(state(&store), before);
    store.connection.execute("UPDATE work_task_sync_exchange_heads SET last_incoming_completed_id=NULL WHERE binding_id=?1",
        [id.as_str()]).unwrap();
    let mut event: Event = object.decode().unwrap();
    event.predecessor = Some(ObjectId::mint());
    let broken = CanonicalObject::mint(&event).unwrap();
    let before = state(&store);
    assert!(view::append::propose(&store.connection, &binding, &broken).is_err());
    assert_eq!(state(&store), before);
    store.prepare_task_sync_exchange(&selected, at(6)).unwrap();
    store
        .apply_task_sync_exchange(&selected, false, at(7))
        .unwrap();
    let duplicate = proposal(&store, &id, &selected.intent);
    let before = state(&store);
    assert!(view::append::propose(&store.connection, &binding, &duplicate).is_err());
    assert_eq!(state(&store), before);
}
