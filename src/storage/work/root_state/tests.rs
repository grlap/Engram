use super::*;
use crate::domain::{RootContribution, SessionId};
use crate::storage::work::test_support::*;

mod corrections;
mod cost;
mod reopen;
mod seal;
mod witnesses;

const CANONICAL_ROOT_WRITE_BUDGET: i64 = 4096;
const PROJECTION_ROOT_WRITE_BUDGET: i64 = 1024;

fn fixture() -> (SqliteStore, crate::WorkItem, RootExecutionId) {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let root = store
        .create_work(
            &root_request("root-deltas", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let id = store
        .get_work_run(root.active_run_id.unwrap())
        .unwrap()
        .root_execution_id;
    (store, root, id)
}

fn capture(store: &mut SqliteStore, root: &crate::WorkItem, index: u32) {
    store
        .add_expected_root_contributor_fixture(
            root.work_id,
            &SessionId(format!("participant-{index:08}")),
            at(i64::from(index)),
        )
        .unwrap();
}

fn canonical_bytes(store: &SqliteStore) -> i64 {
    store.connection.query_row("SELECT COALESCE(SUM(length(canonical_json)), 0) FROM objects WHERE object_kind IN ('work_event', 'work_root_delta')", [], |row| row.get(0)).unwrap()
}

// Observe actual SQL writes, not final-size differences or a hand-maintained
// production counter. Counts bound JSON/key/text payloads and 8 bytes per i64,
// not SQLite record encoding, B-tree indexes, pages, WAL frames or fsync traffic.
fn install_write_probe(store: &SqliteStore) {
    store.connection.execute_batch(
        "CREATE TEMP TABLE root_write_bytes (bytes INTEGER NOT NULL);
         CREATE TEMP TRIGGER root_header_insert AFTER INSERT ON main.work_root_executions BEGIN
             INSERT INTO root_write_bytes VALUES(length(NEW.header_json) + length(NEW.head_hash) + length(NEW.root_execution_id) + length(NEW.project_id) + length(NEW.root_id) + length(NEW.state) + 32); END;
         CREATE TEMP TRIGGER root_header_update AFTER UPDATE ON main.work_root_executions BEGIN
             INSERT INTO root_write_bytes VALUES(length(NEW.header_json) + length(NEW.head_hash) + length(NEW.root_execution_id) + length(NEW.state) + 16); END;
         CREATE TEMP TRIGGER root_member_insert AFTER INSERT ON main.work_root_members BEGIN
             INSERT INTO root_write_bytes VALUES(length(NEW.member_json) + length(NEW.member_hash) + length(NEW.root_execution_id)); END;
         CREATE TEMP TRIGGER root_member_update AFTER UPDATE ON main.work_root_members BEGIN
             INSERT INTO root_write_bytes VALUES(length(NEW.member_json) + length(NEW.member_hash) + length(NEW.root_execution_id)); END;
         CREATE TEMP TRIGGER root_member_delete AFTER DELETE ON main.work_root_members BEGIN
             INSERT INTO root_write_bytes VALUES(length(OLD.member_hash) + length(OLD.root_execution_id)); END;"
    ).unwrap();
}

fn written(store: &SqliteStore) -> i64 {
    store
        .connection
        .query_row(
            "SELECT COALESCE(SUM(bytes), 0) FROM root_write_bytes",
            [],
            |row| row.get(0),
        )
        .unwrap()
}

#[test]
fn root_delta_constant_fact_write_bytes_do_not_grow_with_history() {
    check_constant_fact_writes(&[10, 100]);
}

#[test]
#[ignore = "thousand-delta fixture belongs to the separate scale phase"]
fn root_delta_scale_constant_fact_write_bytes() {
    check_constant_fact_writes(&[1000]);
}

fn check_constant_fact_writes(lengths: &[u32]) {
    for &prior in lengths {
        let (mut store, root, id) = fixture();
        install_write_probe(&store);
        for index in 1..=prior {
            capture(&mut store, &root, index);
        }
        let before = (canonical_bytes(&store), written(&store));
        capture(&mut store, &root, prior + 1);
        let state = projected(&store.connection, id).unwrap().0;
        let added = (
            canonical_bytes(&store) - before.0,
            written(&store) - before.1,
        );
        let old_full_payload = CanonicalObject::freeze(&state).unwrap().bytes().len();
        eprintln!(
            "root_write prior={prior} canonical={} projection_payload={} old_full_root_payload={old_full_payload}",
            added.0, added.1
        );
        assert!(
            added.0 < CANONICAL_ROOT_WRITE_BUDGET,
            "canonical root write grew with history: {added:?}"
        );
        assert!(
            added.1 < PROJECTION_ROOT_WRITE_BUDGET,
            "projection root write grew with history: {added:?}"
        );
        assert_eq!(
            state.expected_contributors.len(),
            usize::try_from(prior + 1).unwrap()
        );
    }
}

#[test]
fn root_delta_full_copy_counterfactual_exceeds_shared_budgets() {
    let (store, root, id) = fixture();
    let mut full = projected(&store.connection, id).unwrap().0;
    // Construct the counterfactual full aggregate directly. No thousand-step
    // history is needed to prove that copying this payload breaks the budget.
    full.expected_contributors = (1..=1001)
        .map(|index| SessionId(format!("participant-{index:08}")))
        .collect();
    let entry = store
        .work_event_tail(root.work_id, 1)
        .unwrap()
        .pop()
        .unwrap();
    let mut json: serde_json::Value =
        load_typed_work_object(&store.connection, &entry.object_hash, "work_event").unwrap();
    json["root_execution"] = serde_json::to_value(&full).unwrap();
    let object = CanonicalObject::freeze(&json).unwrap();
    install_write_probe(&store);
    let before = canonical_bytes(&store);
    SqliteStore::insert_object(&store.connection, "work_event", &object).unwrap();
    store
        .connection
        .execute(
            "UPDATE work_root_executions SET header_json = ?1 WHERE root_execution_id = ?2",
            params![serde_json::to_vec(&full).unwrap(), id.0.to_string()],
        )
        .unwrap();
    let canonical = canonical_bytes(&store) - before;
    let projection = written(&store);
    eprintln!(
        "root_full_copy_counterfactual canonical={canonical} projection_payload={projection}"
    );
    assert!(canonical >= CANONICAL_ROOT_WRITE_BUDGET);
    assert!(projection >= PROJECTION_ROOT_WRITE_BUDGET);
    // This deliberately invalid, isolated fixture is never reopened or used
    // for another operation. These are real writes, not estimated sizes.
}

#[test]
fn root_delta_total_write_growth_is_linear_in_new_facts() {
    let (mut store, root, _) = fixture();
    install_write_probe(&store);
    let initial = canonical_bytes(&store);
    for index in 1..=100 {
        capture(&mut store, &root, index);
    }
    let n = (canonical_bytes(&store) - initial, written(&store));
    for index in 101..=200 {
        capture(&mut store, &root, index);
    }
    let twice = (canonical_bytes(&store) - initial, written(&store));
    eprintln!(
        "root_write N=100 canonical={} projection_payload={}; 2N=200 canonical={} projection_payload={}",
        n.0, n.1, twice.0, twice.1
    );
    assert!(twice.0 < n.0 * 21 / 10);
    assert!(twice.1 < n.1 * 21 / 10);
    assert!(twice.0 > n.0 * 19 / 10);
    assert!(twice.1 > n.1 * 19 / 10);
}

#[test]
fn root_delta_holder_note_writes_only_its_new_contribution() {
    for prior in [10, 100] {
        let (mut store, root, id) = fixture();
        for index in 1..=prior {
            capture(&mut store, &root, index);
        }
        let held = claim(&mut store, &root, "holder", "claim", 1001, 3600);
        let item = super::super::query::load_work_item(&store.connection, root.work_id).unwrap();
        install_write_probe(&store);
        let before = canonical_bytes(&store);
        let request = crate::domain::RecordWorkNoteRequest {
            status: false,
            work_id: item.work_id,
            run_id: held.run_id,
            expected_work_revision: item.revision,
            holder: held.holder.clone(),
            claim_id: held.claim_id,
            claim_fence: held.fence,
            summary: "one fixed-size finding".into(),
            refs: Vec::new(),
            actor: actor("holder"),
            idempotency_key: "note".into(),
            recorded_at: at(1002),
        };
        let capture = store
            .record_work_note(&request, &DevelopmentNoopRedactor)
            .unwrap();
        let bytes = (canonical_bytes(&store) - before, written(&store));
        eprintln!(
            "root_note prior={prior} canonical={} projection_payload={}",
            bytes.0, bytes.1
        );
        assert!(bytes.0 < 10_000, "note canonical copy: {bytes:?}");
        assert!(bytes.1 < 2_000, "note projection copy: {bytes:?}");
        let (state, address) = projected(&store.connection, id).unwrap();
        let mut expected = vec![
            RootContribution {
                participant: held.holder.clone(),
                object: capture.evidence.clone(),
            },
            RootContribution {
                participant: held.holder.clone(),
                object: capture.checkpoint.clone().unwrap(),
            },
        ];
        expected.sort_by(|a, b| a.object.cmp(&b.object));
        assert_eq!(state.contributions, expected);
        assert_eq!(resolve(&store.connection, &address).unwrap(), state);
        let unchanged = test_database_shape_snapshot(&store.connection).unwrap();
        assert_eq!(
            serde_json::to_value(
                store
                    .record_work_note(&request, &DevelopmentNoopRedactor)
                    .unwrap()
            )
            .unwrap(),
            serde_json::to_value(capture).unwrap()
        );
        assert_eq!(
            test_database_shape_snapshot(&store.connection).unwrap(),
            unchanged
        );
        assert!(store.verify_all().unwrap().is_healthy());
    }
}

#[test]
fn root_delta_all_member_kinds_add_remove_and_replay_exactly() {
    let (store, root, id) = fixture();
    let (base, old) = projected(&store.connection, id).unwrap();
    let mut changed = base.clone();
    changed.run_ids.clear();
    changed.required_child_seals.push(old.head.clone());
    changed
        .required_child_waivers
        .push(crate::domain::RequiredChildWaiver {
            work_id: root.work_id,
            work_revision: root.revision,
            waived_by: "test".into(),
            reason: "member codec only".into(),
        });
    changed.expected_contributors.push(SessionId("test".into()));
    changed.contributions.push(RootContribution {
        participant: SessionId("test".into()),
        object: old.head.clone(),
    });
    changed.waivers.push(crate::domain::CompletionWaiver {
        participant: SessionId("test".into()),
        waived_by: "test".into(),
        reason: "member codec only".into(),
    });
    // This is a substrate codec fixture, not an admitted completion/waiver.
    // Existing lifecycle fixtures cover the domain admission of these facts.
    let transaction = store.connection.unchecked_transaction().unwrap();
    persist(&transaction, &changed).unwrap();
    let (_, next) = projected(&transaction, id).unwrap();
    let delta = load_head(&transaction, &next).unwrap();
    assert_eq!(delta.added.len(), 5);
    assert_eq!(delta.removed.len(), 1);
    assert_eq!(resolve(&transaction, &next).unwrap(), changed);
    persist(&transaction, &base).unwrap();
    let (_, restored) = projected(&transaction, id).unwrap();
    let delta = load_head(&transaction, &restored).unwrap();
    assert_eq!(delta.added.len(), 1);
    assert_eq!(delta.removed.len(), 5);
    assert_eq!(resolve(&transaction, &restored).unwrap(), base);
    assert_eq!(resolve(&transaction, &next).unwrap(), changed);
    transaction.rollback().unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn root_delta_audit_checks_each_historical_event_reference() {
    let (mut store, root, id) = fixture();
    capture(&mut store, &root, 1);
    let (_, first) = projected(&store.connection, id).unwrap();
    capture(&mut store, &root, 2);
    let (_, last) = projected(&store.connection, id).unwrap();
    let expected = std::collections::HashMap::from([(id.0.to_string(), last.clone())]);
    let mut absent = first.clone();
    absent.head = CanonicalObject::freeze(&"absent root head")
        .unwrap()
        .hash()
        .clone();
    let mut wrong_generation = first.clone();
    wrong_generation.generation += 1;
    for refs in [
        vec![last.clone(), first.clone()],
        vec![absent, last.clone()],
        vec![wrong_generation, last.clone()],
    ] {
        let mut failures = Vec::new();
        verify_projections(&store.connection, &expected, &refs, &mut 0, &mut failures).unwrap();
        assert!(
            failures
                .iter()
                .any(|label| label.contains("invalid_history")),
            "{failures:?}"
        );
    }
    let mut failures = Vec::new();
    verify_projections(
        &store.connection,
        &expected,
        &[first.clone(), first, last],
        &mut 0,
        &mut failures,
    )
    .unwrap();
    assert!(failures.is_empty(), "{failures:?}");
}

#[test]
fn root_delta_doctor_rejects_orphan_projection_members() {
    let (store, _, _) = fixture();
    // Simulate external corruption, which is not constrained by the writer's
    // foreign-key enforcement. Doctor must check these rows independently.
    store
        .connection
        .execute_batch("PRAGMA foreign_keys = OFF")
        .unwrap();
    let member = RootExecutionMember::Contributor(SessionId("orphan".into()));
    store
        .connection
        .execute(
            "INSERT INTO work_root_members VALUES (?1, ?2, ?3)",
            params![
                RootExecutionId::new().0.to_string(),
                member_hash(&member).unwrap().as_str(),
                serde_json::to_vec(&member).unwrap()
            ],
        )
        .unwrap();
    let mut failures = Vec::new();
    verify_projections(
        &store.connection,
        &std::collections::HashMap::new(),
        &[],
        &mut 0,
        &mut failures,
    )
    .unwrap();
    assert!(
        failures
            .iter()
            .any(|label| label.starts_with("work_root_member:")
                && label.ends_with(":missing_generation")),
        "{failures:?}"
    );
    assert!(!store.verify_all().unwrap().is_healthy());
}

#[test]
fn root_delta_history_and_current_projection_have_independent_integrity() {
    let (mut store, root, id) = fixture();
    capture(&mut store, &root, 1);
    let (old, old_ref) = projected(&store.connection, id).unwrap();
    capture(&mut store, &root, 2);
    let (current, current_ref) = projected(&store.connection, id).unwrap();
    assert_eq!(resolve(&store.connection, &old_ref).unwrap(), old);
    assert_eq!(resolve(&store.connection, &current_ref).unwrap(), current);
    assert!(store.verify_all().unwrap().is_healthy());
    for fault in [
        "missing_row",
        "extra_row",
        "missing_delta",
        "wrong_generation",
        "orphan_delta",
    ] {
        store.connection.execute_batch("SAVEPOINT corrupt").unwrap();
        match fault {
            "missing_row" => {
                store.connection.execute("DELETE FROM work_root_members WHERE root_execution_id = ?1 AND member_hash = (SELECT MIN(member_hash) FROM work_root_members WHERE root_execution_id = ?1)", [id.0.to_string()]).unwrap();
            }
            "extra_row" => {
                let member = RootExecutionMember::Contributor(SessionId("unrecorded".into()));
                store
                    .connection
                    .execute(
                        "INSERT INTO work_root_members VALUES (?1, ?2, ?3)",
                        params![
                            id.0.to_string(),
                            member_hash(&member).unwrap().as_str(),
                            serde_json::to_vec(&member).unwrap()
                        ],
                    )
                    .unwrap();
            }
            "missing_delta" => {
                store
                    .connection
                    .execute(
                        "DELETE FROM objects WHERE object_hash = ?1",
                        [old_ref.head.as_str()],
                    )
                    .unwrap();
            }
            "wrong_generation" => {
                store
                    .connection
                    .execute(
                        "UPDATE work_root_executions SET generation = generation + 1",
                        [],
                    )
                    .unwrap();
            }
            "orphan_delta" => {
                let mut head = load_head(&store.connection, &current_ref).unwrap();
                head.header.revision += 1;
                let object = CanonicalObject::freeze(&head).unwrap();
                SqliteStore::insert_object(&store.connection, KIND, &object).unwrap();
            }
            _ => unreachable!(),
        }
        assert!(
            !store.verify_all().unwrap().is_healthy(),
            "doctor missed {fault}"
        );
        if matches!(fault, "missing_row" | "extra_row" | "wrong_generation") {
            assert!(
                projected(&store.connection, id).is_err(),
                "current read missed {fault}"
            );
        }
        if fault == "missing_delta" {
            assert!(resolve(&store.connection, &current_ref).is_err());
        }
        restore_savepoint(&store);
        assert!(store.verify_all().unwrap().is_healthy());
    }
}

#[test]
fn root_delta_removal_replay_and_wrong_chain_refuse_without_projection_writes() {
    let (mut store, root, id) = fixture();
    capture(&mut store, &root, 1);
    let (before, address) = projected(&store.connection, id).unwrap();
    let mut after = before.clone();
    after.expected_contributors.clear();
    after.revision += 1;
    let transaction = store.connection.unchecked_transaction().unwrap();
    persist(&transaction, &after).unwrap();
    let (_, removed_ref) = projected(&transaction, id).unwrap();
    assert_eq!(resolve(&transaction, &removed_ref).unwrap(), after);
    assert_eq!(resolve(&transaction, &address).unwrap(), before);
    let snapshot = test_database_shape_snapshot(&transaction).unwrap();
    persist(&transaction, &after).unwrap();
    assert_eq!(
        test_database_shape_snapshot(&transaction).unwrap(),
        snapshot
    );
    for fault in [
        "sequence",
        "predecessor",
        "generation",
        "checksum",
        "remove_absent",
    ] {
        let mut head = load_head(&transaction, &removed_ref).unwrap();
        match fault {
            "sequence" => head.sequence += 1,
            "predecessor" => head.predecessor = None,
            "generation" => head.header.generation += 1,
            "checksum" => head.state_checksum = address.head.clone(),
            "remove_absent" => {
                head.removed
                    .push(RootExecutionMember::Contribution(RootContribution {
                        participant: SessionId("absent".into()),
                        object: address.head.clone(),
                    }));
            }
            _ => unreachable!(),
        }
        let object = CanonicalObject::freeze(&head).unwrap();
        SqliteStore::insert_object(&transaction, KIND, &object).unwrap();
        let mut damaged_ref = removed_ref.clone();
        damaged_ref.head = object.hash().clone();
        assert!(
            resolve(&transaction, &damaged_ref).is_err(),
            "accepted {fault}"
        );
    }
    transaction.rollback().unwrap();
    assert_eq!(projected(&store.connection, id).unwrap().0, before);
    assert!(store.verify_all().unwrap().is_healthy());
}
