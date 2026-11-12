use super::super::super::view;
use super::*;
use crate::storage::work::test_support::root_request;
use crate::task_sync::ledger::{
    CompletedExchangeRequest, OutgoingProgress, PublicationObservation,
};

fn request(input: &InspectedExchange) -> CompletedExchangeRequest {
    CompletedExchangeRequest {
        binding: input.binding_id.clone(),
        intent: input.intent.clone(),
    }
}

fn read(
    store: &SqliteStore,
    request: &CompletedExchangeRequest,
) -> crate::task_sync::ledger::CompletedExchange {
    store
        .completed_task_sync_exchange(request)
        .unwrap()
        .unwrap()
}

#[test]
fn original_completed_receipt_survives_later_exchange_local_edit_and_supersession() {
    let home = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(home.path().join("work.db")).unwrap();
    let binding = binding(home.path());
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let selected = input(&store, &id, &binding, candidate());
    assert_eq!(
        store
            .completed_task_sync_exchange(&request(&selected))
            .unwrap(),
        None
    );
    let prepared = store.prepare_task_sync_exchange(&selected, at(6)).unwrap();
    assert_eq!(
        store
            .completed_task_sync_exchange(&request(&selected))
            .unwrap(),
        None
    );
    let receipt = store
        .apply_task_sync_exchange(&selected, false, at(7))
        .unwrap();
    let original = read(&store, &request(&selected));
    assert_eq!(original.prepared, prepared);
    assert!(
        matches!(&original.event.transition,Transition::ImportApplied{receipt: actual,..} if actual == &receipt)
    );
    let bytes =
        SqliteStore::get_canonical_object_on(&store.connection, &original.outcome, EVENT_KIND)
            .unwrap()
            .unwrap();
    let current = store.export_task_tree(&binding.project).unwrap();
    let next = input(&store, &id, &binding, current);
    store.prepare_task_sync_exchange(&next, at(8)).unwrap();
    store.apply_task_sync_exchange(&next, false, at(9)).unwrap();
    let latest = read(&store, &request(&next));
    assert_ne!(latest.outcome, original.outcome);
    store
        .create_work(
            &root_request(&binding.project.0, "independent local task", 10),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let mut successor = binding.clone();
    successor.supersedes = Some(id.clone());
    successor.superseded_tail = Some(latest.outcome);
    successor.store.restore_occurrence = Some(uuid::Uuid::new_v4());
    store.activate_task_sync_binding(&successor).unwrap();
    let before = state(&store);
    assert_eq!(read(&store, &request(&selected)), original);
    assert_eq!(
        SqliteStore::get_canonical_object_on(&store.connection, &original.outcome, EVENT_KIND)
            .unwrap()
            .unwrap()
            .bytes(),
        bytes.bytes()
    );
    assert_eq!(state(&store), before);
    // No retained-file, origin-payload or current task decoding is permitted on
    // this historical read. The independent canonical receipt remains readable.
    std::fs::rename(
        &binding.repository.git_directory,
        home.path().join("offline-git"),
    )
    .unwrap();
    store
        .connection
        .execute(
            "UPDATE objects SET canonical_json=x'00' WHERE object_kind=?1",
            [ORIGIN_KIND],
        )
        .unwrap();
    store
        .connection
        .execute("UPDATE work_items SET item_json=x'00'", [])
        .unwrap();
    assert_eq!(read(&store, &request(&selected)), original);
}

#[test]
fn historical_selection_compares_every_intent_field_and_never_mutates() {
    let home = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(home.path().join("work.db")).unwrap();
    let binding = binding(home.path());
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let selected = input(&store, &id, &binding, candidate());
    store.prepare_task_sync_exchange(&selected, at(6)).unwrap();
    store
        .apply_task_sync_exchange(&selected, false, at(7))
        .unwrap();
    let original = serde_json::to_value(&selected.intent).unwrap();
    let other = CanonicalObject::freeze(&"different comparison")
        .unwrap()
        .key()
        .clone();
    let fields = [
        ("operation", serde_json::json!(uuid::Uuid::new_v4())),
        ("key", serde_json::json!("different key")),
        ("direction", serde_json::json!("outgoing")),
        (
            "actor",
            serde_json::to_value(actor("different actor")).unwrap(),
        ),
        ("producer", serde_json::to_value(&other).unwrap()),
        (
            "expected_head",
            serde_json::json!(selected.intent.commit.oid),
        ),
        (
            "common_commit",
            serde_json::to_value(&selected.intent.commit).unwrap(),
        ),
        (
            "local_planning_basis",
            serde_json::to_value(&other).unwrap(),
        ),
    ];
    let mut cases = Vec::new();
    for (field, value) in fields {
        let mut changed = original.clone();
        changed[field] = value;
        cases.push(changed);
    }
    for owner in ["candidate", "commit"] {
        for (field, value) in original[owner].as_object().unwrap() {
            let mut changed = original.clone();
            changed[owner][field] = match field.as_str() {
                "operation" => serde_json::json!(uuid::Uuid::new_v4()),
                "directory" => serde_json::json!(home.path().join("other candidate")),
                "source_cut" => {
                    serde_json::json!({"position":1,"anchor":{"object_kind":"work_event","object_id":ObjectId::mint()}})
                }
                "retention_ref" => {
                    serde_json::json!("refs/engram/task-sync/exchange/different/commit")
                }
                "parents" => serde_json::json!([selected.intent.commit.oid]),
                _ => serde_json::to_value(&other).unwrap(),
            };
            assert_ne!(&changed[owner][field], value);
            cases.push(changed);
        }
    }
    let before = state(&store);
    for changed in cases {
        let intent: Intent = serde_json::from_value(changed).unwrap();
        intent.validate().unwrap();
        assert!(
            store
                .completed_task_sync_exchange(&CompletedExchangeRequest {
                    binding: id.clone(),
                    intent
                })
                .is_err()
        );
    }
    assert_eq!(state(&store), before);
}

#[test]
fn only_confirmed_outgoing_progress_is_a_completed_outcome() {
    let home = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(home.path().join("work.db")).unwrap();
    let binding = binding(home.path());
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let mut tree = candidate();
    tree.tasks.clear();
    let mut selected = input(&store, &id, &binding, tree);
    selected.intent.direction = Direction::Outgoing;
    store.prepare_task_sync_exchange(&selected, at(6)).unwrap();
    for step in [
        OutgoingProgress::LocalCommitRecorded,
        OutgoingProgress::PushStarted,
        OutgoingProgress::Observed(PublicationObservation::Unknown { observed_tip: None }),
    ] {
        store
            .record_task_sync_exchange_progress(&selected, step, at(7))
            .unwrap();
        assert_eq!(
            store
                .completed_task_sync_exchange(&request(&selected))
                .unwrap(),
            None
        );
    }
    let outcome = store
        .record_task_sync_exchange_progress(
            &selected,
            OutgoingProgress::Observed(PublicationObservation::Confirmed {
                observed_tip: selected.intent.commit.oid.clone(),
            }),
            at(8),
        )
        .unwrap();
    assert_eq!(read(&store, &request(&selected)).outcome, outcome);
}

// Repair preserves canonical bytes and derives the same logical exchange
// rows/schema. SQLite root pages and FTS5 segments are physical rebuild output,
// not a byte-stability contract. Refusal/rollback assertions still use the full
// unmodified database-shape snapshot.
fn logical_ledger_state(store: &SqliteStore) -> crate::storage::TestDatabaseShapeSnapshot {
    let tables = [
        "objects",
        "work_task_sync_exchange_operations",
        "work_task_sync_exchange_heads",
    ];
    let mut snapshot = state(store);
    snapshot
        .schema
        .retain(|(_, _, table, _, _)| tables.contains(&table.as_str()));
    for (_, _, _, root_page, _) in &mut snapshot.schema {
        *root_page = 0;
    }
    snapshot
        .table_info
        .retain(|(table, _)| tables.contains(&table.as_str()));
    snapshot
        .rows
        .retain(|(table, _)| tables.contains(&table.as_str()));
    snapshot
}

#[test]
fn missing_stale_or_forked_projection_refuses_and_repair_is_deterministic() {
    let home = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(home.path().join("work.db")).unwrap();
    let binding = binding(home.path());
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let selected = input(&store, &id, &binding, candidate());
    store.prepare_task_sync_exchange(&selected, at(6)).unwrap();
    store
        .apply_task_sync_exchange(&selected, false, at(7))
        .unwrap();
    let original = read(&store, &request(&selected));
    let before = logical_ledger_state(&store);
    for damage in [
        "DELETE FROM work_task_sync_exchange_operations",
        "DELETE FROM work_task_sync_exchange_heads",
        "UPDATE work_task_sync_exchange_operations SET completed_event_id=NULL",
        "UPDATE work_task_sync_exchange_operations SET latest_event_id=prepared_id",
        "UPDATE work_task_sync_exchange_heads SET sequence=sequence+1",
    ] {
        store.connection.execute(damage, []).unwrap();
        let damaged = state(&store);
        assert!(
            store
                .completed_task_sync_exchange(&request(&selected))
                .unwrap_err()
                .to_string()
                .contains("repair projections explicitly")
        );
        assert_eq!(state(&store), damaged);
        assert!(!store.verify_all().unwrap().is_healthy());
        let transaction = store.begin_work_mutation().unwrap();
        crate::storage::work::schema::repair_rebuildable_schema_on(&transaction).unwrap();
        transaction.commit().unwrap();
        assert_eq!(logical_ledger_state(&store), before);
        assert!(store.verify_all().unwrap().is_healthy());
        assert_eq!(read(&store, &request(&selected)), original);
    }
    // A broken immutable chain must roll back the entire schema/data repair.
    let mut fork = original.event.clone();
    fork.predecessor = Some(ObjectId::mint());
    let object = CanonicalObject::mint(&fork).unwrap();
    SqliteStore::insert_object(&store.connection, EVENT_KIND, &object).unwrap();
    let damaged = state(&store);
    {
        let transaction = store.begin_work_mutation().unwrap();
        let findings =
            crate::storage::work::schema::repair_rebuildable_schema_on(&transaction).unwrap();
        assert!(
            findings
                .iter()
                .any(|finding| finding.starts_with("task_sync_exchange:"))
        );
    }
    assert_eq!(state(&store), damaged);
}

#[test]
fn canonical_and_view_writes_rollback_together_when_projection_insertion_fails() {
    let home = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(home.path().join("work.db")).unwrap();
    let binding = binding(home.path());
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let selected = input(&store, &id, &binding, candidate());
    store.prepare_task_sync_exchange(&selected, at(6)).unwrap();
    store.connection.execute_batch("CREATE TEMP TRIGGER fail_exchange_view BEFORE UPDATE ON work_task_sync_exchange_operations BEGIN SELECT RAISE(ABORT,'injected view failure'); END;").unwrap();
    let before = state(&store);
    assert!(
        store
            .apply_task_sync_exchange(&selected, false, at(7))
            .unwrap_err()
            .to_string()
            .contains("injected view failure")
    );
    assert_eq!(state(&store), before);
    store
        .connection
        .execute_batch("DROP TRIGGER fail_exchange_view")
        .unwrap();
    store
        .apply_task_sync_exchange(&selected, false, at(8))
        .unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn keyed_reads_do_not_grow_with_two_thousand_unrelated_ledger_operations() {
    let home = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(home.path().join("work.db")).unwrap();
    let binding = binding(home.path());
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let selected = input(&store, &id, &binding, candidate());
    store.prepare_task_sync_exchange(&selected, at(6)).unwrap();
    store
        .apply_task_sync_exchange(&selected, false, at(7))
        .unwrap();
    view::READS.with(|count| count.set(0));
    let original = read(&store, &request(&selected));
    let small = view::READS.with(std::cell::Cell::get);
    assert!(small > 0);
    view::READS.with(|count| count.set(0));
    let tail = view::read::tail_on(
        &store.connection,
        &id,
        &binding,
        &mut view::read::ReadBudget::default(),
    )
    .unwrap();
    assert_eq!(tail.id, original.outcome);
    let tail_reads = view::READS.with(std::cell::Cell::get);
    assert!(tail_reads > 0);
    let mut next_intent = selected.intent.clone();
    next_intent.operation = uuid::Uuid::new_v4();
    next_intent.key = next_intent.operation.to_string();
    let proposal = |tail: ObjectId, sequence: u64| {
        CanonicalObject::mint(&Event {
            schema_version: 1,
            binding: id.clone(),
            sequence: sequence + 1,
            predecessor: Some(tail),
            actor: next_intent.actor.clone(),
            recorded_at: at(10),
            transition: Transition::Prepared {
                intent: Box::new(next_intent.clone()),
            },
        })
        .unwrap()
    };
    let proposed = proposal(original.outcome.clone(), original.event.sequence);
    view::READS.with(|count| count.set(0));
    let next = view::append::propose(&store.connection, &binding, &proposed).unwrap();
    let append_reads = view::READS.with(std::cell::Cell::get);
    assert!(append_reads > 0);
    assert_eq!(next.head.last_incoming, Some(original.outcome.clone()));
    // Extend genuine canonical chains with structurally valid empty receipts.
    // Bulk fixture insertion avoids re-running the old full-ledger writer for
    // every synthetic history row; production repair validates the result.
    let transaction = store.begin_work_mutation().unwrap();
    let mut tail = original.outcome.clone();
    let mut sequence = original.event.sequence;
    for _ in 0..2000 {
        let mut intent = selected.intent.clone();
        intent.operation = uuid::Uuid::new_v4();
        intent.key = intent.operation.to_string();
        let prepared_event = Event {
            schema_version: 1,
            binding: id.clone(),
            sequence: sequence + 1,
            predecessor: Some(tail),
            actor: intent.actor.clone(),
            recorded_at: at(8),
            transition: Transition::Prepared {
                intent: Box::new(intent.clone()),
            },
        };
        let prepared = CanonicalObject::mint(&prepared_event).unwrap();
        SqliteStore::insert_object(&transaction, EVENT_KIND, &prepared).unwrap();
        let event = Event {
            schema_version: 1,
            binding: id.clone(),
            sequence: sequence + 2,
            predecessor: Some(prepared.key().clone()),
            actor: intent.actor,
            recorded_at: at(9),
            transition: Transition::ImportApplied {
                prepared: prepared.key().clone(),
                receipt: ImportReceipt {
                    source_commit: intent.commit.oid,
                    source_tree: intent.commit.tree,
                    before_basis: intent.local_planning_basis,
                    after_basis: intent.candidate.planning_fingerprint,
                    notes_included: false,
                    tasks: Vec::new(),
                },
            },
        };
        let object = CanonicalObject::mint(&event).unwrap();
        SqliteStore::insert_object(&transaction, EVENT_KIND, &object).unwrap();
        tail = object.key().clone();
        sequence += 2;
    }
    transaction
        .execute("DELETE FROM work_task_sync_exchange_operations", [])
        .unwrap();
    transaction
        .execute("DELETE FROM work_task_sync_exchange_heads", [])
        .unwrap();
    view::rebuild(&transaction).unwrap();
    transaction.commit().unwrap();
    view::READS.with(|count| count.set(0));
    assert_eq!(read(&store, &request(&selected)), original);
    assert_eq!(view::READS.with(std::cell::Cell::get), small);
    view::READS.with(|count| count.set(0));
    let latest = view::read::tail_on(
        &store.connection,
        &id,
        &binding,
        &mut view::read::ReadBudget::default(),
    )
    .unwrap();
    assert_ne!(latest.id, original.outcome);
    assert_eq!(view::READS.with(std::cell::Cell::get), tail_reads);
    let proposed = proposal(latest.id.clone(), latest.event.sequence);
    view::READS.with(|count| count.set(0));
    let next = view::append::propose(&store.connection, &binding, &proposed).unwrap();
    assert_eq!(view::READS.with(std::cell::Cell::get), append_reads);
    assert_eq!(next.head.last_incoming, Some(latest.id.clone()));
    // Malformed unrelated ledger payload proves the hot read has no hidden
    // full-discovery fallback. Doctor still reports that unrelated corruption.
    let unrelated = CanonicalObject::mint(&serde_json::json!({"broken":"unrelated"})).unwrap();
    SqliteStore::insert_object(&store.connection, EVENT_KIND, &unrelated).unwrap();
    assert_eq!(read(&store, &request(&selected)), original);
    let before = state(&store);
    view::READS.with(|count| count.set(0));
    let after_damage = view::read::tail_on(
        &store.connection,
        &id,
        &binding,
        &mut view::read::ReadBudget::default(),
    )
    .unwrap();
    assert_eq!(after_damage.id, latest.id);
    assert_eq!(view::READS.with(std::cell::Cell::get), tail_reads);
    assert_eq!(state(&store), before);
    view::READS.with(|count| count.set(0));
    view::append::propose(&store.connection, &binding, &proposed).unwrap();
    assert_eq!(view::READS.with(std::cell::Cell::get), append_reads);
    assert_eq!(state(&store), before);
    assert!(!store.verify_all().unwrap().is_healthy());
}

#[test]
fn missing_canonical_history_never_suggests_projection_repair() {
    let home = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(home.path().join("work.db")).unwrap();
    let binding = binding(home.path());
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let selected = input(&store, &id, &binding, candidate());
    let prepared = store.prepare_task_sync_exchange(&selected, at(6)).unwrap();
    store
        .apply_task_sync_exchange(&selected, false, at(7))
        .unwrap();
    let mut unknown = request(&selected);
    unknown.binding = ObjectId::mint();
    let before = state(&store);
    let error = store
        .completed_task_sync_exchange(&unknown)
        .unwrap_err()
        .to_string();
    assert!(error.contains("exchange binding is missing"));
    assert!(!error.contains("repair"));
    assert_eq!(state(&store), before);

    let mut event: Event =
        SqliteStore::get_typed_object_on(&store.connection, &prepared, EVENT_KIND)
            .unwrap()
            .unwrap();
    event.predecessor = Some(ObjectId::mint());
    let broken = CanonicalObject::identified(&prepared, &event).unwrap();
    store
        .connection
        .execute(
            "UPDATE objects SET canonical_json=?1 WHERE object_id=?2",
            rusqlite::params![broken.bytes(), prepared.as_str()],
        )
        .unwrap();
    let damaged = state(&store);
    let error = store
        .completed_task_sync_exchange(&request(&selected))
        .unwrap_err()
        .to_string();
    assert!(error.contains("exchange canonical record is missing"));
    assert!(!error.contains("repair"));
    assert_eq!(state(&store), damaged);
}

#[test]
fn repair_retains_labelled_ledger_and_other_findings_and_propagates_database_errors() {
    let home = crate::test_support::temp_home().unwrap();
    let database = home.path().join("work.db");
    let mut store = SqliteStore::open(&database).unwrap();
    let binding = binding(home.path());
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let selected = input(&store, &id, &binding, candidate());
    store.prepare_task_sync_exchange(&selected, at(6)).unwrap();
    store
        .apply_task_sync_exchange(&selected, false, at(7))
        .unwrap();
    let outcome = read(&store, &request(&selected)).event;
    let mut fork = outcome.clone();
    fork.predecessor = Some(ObjectId::mint());
    let fork = CanonicalObject::mint(&fork).unwrap();
    let mut unreadable = serde_json::to_value(&outcome).unwrap();
    unreadable["transition"] = serde_json::json!({"transition":"private-body-sentinel"});
    let unreadable = CanonicalObject::mint(&unreadable).unwrap();
    let malformed = CanonicalObject::mint(&serde_json::json!({})).unwrap();
    SqliteStore::insert_object(&store.connection, "work_restored_record", &malformed).unwrap();
    // Both semantic and JSON ledger failures must have one shared, redacted
    // diagnostic in the complete refusal, alongside the other owner's label.
    for defect in [&fork, &unreadable] {
        SqliteStore::insert_object(&store.connection, EVENT_KIND, defect).unwrap();
        let damaged = state(&store);
        let error = SqliteStore::repair_rebuildable_projections(&database)
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid labels:"), "{error}");
        assert_eq!(error.matches("task_sync_exchange:").count(), 1, "{error}");
        assert!(error.contains("and 2 invalid work record(s);"), "{error}");
        assert!(!error.contains("private-body-sentinel"), "{error}");
        assert!(
            error.contains(&format!("work_restored_record:{}", malformed.key())),
            "{error}"
        );
        assert_eq!(
            state(&store),
            damaged,
            "the complete repair refusal rolls back every owner"
        );
        store
            .connection
            .execute(
                "DELETE FROM objects WHERE object_id=?1",
                [defect.key().as_str()],
            )
            .unwrap();
    }
    store
        .connection
        .execute(
            "DELETE FROM objects WHERE object_id=?1",
            [malformed.key().as_str()],
        )
        .unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
    store.connection.execute_batch("CREATE TEMP TRIGGER fail_ledger_rebuild BEFORE INSERT ON work_task_sync_exchange_heads BEGIN SELECT RAISE(ABORT,'injected database failure'); END;").unwrap();
    let before = state(&store);
    assert!(matches!(
        view::rebuild_for_repair(&store.connection),
        Err(StoreError::Sqlite(_))
    ));
    assert_eq!(state(&store), before);
}

#[test]
fn malformed_event_json_repairs_with_labels_and_selected_bad_sequence_has_guidance() {
    let home = crate::test_support::temp_home().unwrap();
    let database = home.path().join("work.db");
    let mut store = SqliteStore::open(&database).unwrap();
    let binding = binding(home.path());
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let selected = input(&store, &id, &binding, candidate());
    store.prepare_task_sync_exchange(&selected, at(6)).unwrap();
    store
        .apply_task_sync_exchange(&selected, false, at(7))
        .unwrap();
    let completed = read(&store, &request(&selected));
    let original = CanonicalObject::identified(&completed.outcome, &completed.event).unwrap();
    let mut bad_sequence = serde_json::to_value(&completed.event).unwrap();
    bad_sequence["sequence"] = serde_json::json!("private-sequence-sentinel");
    let bad_sequence = serde_json::to_vec(&bad_sequence).unwrap();
    for (defect, bytes) in [
        ("noninteger sequence", bad_sequence.as_slice()),
        ("malformed JSON", b"{private-json-sentinel".as_slice()),
    ] {
        // Simulate an invalid canonical event while its old expression indexes
        // are absent; explicit repair must recreate guarded indexes and then
        // report the canonical defect, rather than fail in CREATE INDEX.
        store
            .connection
            .execute_batch(
                "DROP INDEX objects_task_sync_exchange_sequence;
             DROP INDEX objects_task_sync_exchange_operation;
             DROP INDEX objects_task_sync_exchange_key;
             DROP INDEX objects_task_sync_exchange_prepared;",
            )
            .unwrap();
        store
            .connection
            .execute(
                "UPDATE objects SET canonical_json=?1 WHERE object_id=?2",
                rusqlite::params![bytes, completed.outcome.as_str()],
            )
            .unwrap_or_else(|error| panic!("damage {defect}: {error}"));
        if bytes == bad_sequence.as_slice() {
            view::create_schema(&store.connection).unwrap();
            let error = store
                .completed_task_sync_exchange(&request(&selected))
                .unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("exchange sequence is invalid; preserve the store and recover its canonical history"),
                "{error}"
            );
            assert!(!matches!(error, StoreError::Sqlite(_)));
        }
        let damaged = state(&store);
        let error = SqliteStore::repair_rebuildable_projections(&database)
            .unwrap_err()
            .to_string();
        assert!(error.contains("invalid labels:"), "{error}");
        assert_eq!(error.matches("task_sync_exchange:").count(), 1, "{error}");
        assert!(!error.contains("private-sequence-sentinel"), "{error}");
        assert!(!error.contains("private-json-sentinel"), "{error}");
        assert_eq!(state(&store), damaged);
        store
            .connection
            .execute(
                "UPDATE objects SET canonical_json=?1 WHERE object_id=?2",
                rusqlite::params![original.bytes(), completed.outcome.as_str()],
            )
            .unwrap();
        view::create_schema(&store.connection).unwrap();
    }
    assert!(store.verify_all().unwrap().is_healthy());
    assert_eq!(read(&store, &request(&selected)), completed);
}
