use super::*;
use crate::domain::{
    ChildRequirement, DecomposeWorkRequest, DisposeWorkRequest, WaiveRequiredChildRequest,
    WorkDisposition, WorkEvent,
};

fn append_waiver(
    store: &mut SqliteStore,
    root: &crate::WorkItem,
    index: u32,
) -> (RootExecutionRef, RequiredChildWaiver) {
    let parent = store.get_work_item(root.work_id).unwrap();
    let plan = store
        .decompose_work(
            &DecomposeWorkRequest {
                parent_id: parent.work_id,
                expected_parent_revision: parent.revision,
                children: vec![child(
                    "waived",
                    ChildRequirement::Required,
                    "Terminal child",
                )],
                prerequisites: Vec::new(),
                authority: delegated(&root.project_id.0, "planner"),
                actor: actor("planner"),
                idempotency_key: format!("plan-{index}"),
                created_at: at(i64::from(index) * 3),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let child = &plan.children[0];
    store
        .dispose_work(
            &DisposeWorkRequest {
                work_id: child.work_id,
                expected_work_revision: child.revision,
                disposition: WorkDisposition::Cancelled,
                replacement_id: None,
                reason: "measured terminal child".into(),
                actor: actor("planner"),
                idempotency_key: format!("cancel-{index}"),
                disposed_at: at(i64::from(index) * 3 + 1),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let waiver = store
        .waive_required_child(
            &WaiveRequiredChildRequest {
                parent_id: root.work_id,
                child_id: child.work_id,
                expected_parent_revision: plan.parent.revision,
                reason: "measured terminal child".into(),
                actor: actor("planner"),
                idempotency_key: format!("waive-{index}"),
                waived_at: at(i64::from(index) * 3 + 2),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let event = super::super::super::query::latest_canonical_work_event_for_item_optional(
        &store.connection,
        root.work_id,
    )
    .unwrap()
    .unwrap();
    (event.root_execution.unwrap(), waiver)
}

fn advance_root(store: &mut SqliteStore, root: &crate::WorkItem) {
    let item = store.get_work_item(root.work_id).unwrap();
    claim(store, &item, "holder", "claim", 10, 3600);
}

// Deliberately re-canonicalize a corrupted fixture and rebind its event edges.
// The reader must reject semantic faults, not merely a stale object hash.
fn replace_head(store: &SqliteStore, old: &ObjectHash, new: &RootExecutionDelta) -> ObjectHash {
    let frozen = CanonicalObject::freeze(new).unwrap();
    SqliteStore::insert_object(&store.connection, KIND, &frozen).unwrap();
    store
        .connection
        .execute(
            "UPDATE work_root_executions SET head_hash = ?2 WHERE head_hash = ?1",
            params![old.as_str(), frozen.hash().as_str()],
        )
        .unwrap();
    let hashes: Vec<String> = store
        .connection
        .prepare(
            "SELECT object_hash FROM objects WHERE object_kind = 'work_event'
         AND json_extract(canonical_json, '$.root_execution.head') = ?1",
        )
        .unwrap()
        .query_map([old.as_str()], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    for hash in hashes {
        let old_event = ObjectHash::from_stored(hash).unwrap();
        let mut event: WorkEvent =
            load_typed_work_object(&store.connection, &old_event, "work_event").unwrap();
        event.root_execution.as_mut().unwrap().head = frozen.hash().clone();
        let object = CanonicalObject::freeze(&event).unwrap();
        SqliteStore::insert_object(&store.connection, "work_event", &object).unwrap();
        store
            .connection
            .execute(
                "UPDATE work_feed_entries SET object_hash = ?2 WHERE object_hash = ?1",
                params![old_event.as_str(), object.hash().as_str()],
            )
            .unwrap();
        store
            .connection
            .execute(
                "UPDATE work_items SET latest_event_hash = ?2 WHERE latest_event_hash = ?1",
                params![old_event.as_str(), object.hash().as_str()],
            )
            .unwrap();
        if old_event != *object.hash() {
            store
                .connection
                .execute(
                    "DELETE FROM objects WHERE object_hash = ?1",
                    [old_event.as_str()],
                )
                .unwrap();
        }
    }
    frozen.hash().clone()
}

#[test]
fn root_delta_waiver_fact_proof_checks_ancestry_exact_addition_and_current_anchor() {
    let (mut store, root, id) = fixture();
    let witness = append_waiver(&mut store, &root, 1);
    advance_root(&mut store, &root);
    let (state, current) = projected(&store.connection, id).unwrap();
    verify_waiver_witnesses(&store.connection, &state, std::slice::from_ref(&witness)).unwrap();
    let snapshot = test_database_shape_snapshot(&store.connection).unwrap();
    for fault in [
        "different_reason",
        "different_actor",
        "different_revision",
        "wrong_generation",
        "not_addition",
        "duplicate",
    ] {
        let mut bad = witness.clone();
        match fault {
            "different_reason" => bad.1.reason.push_str(" changed"),
            "different_actor" => bad.1.waived_by.push_str(" changed"),
            "different_revision" => bad.1.work_revision += 1,
            "wrong_generation" => bad.0.generation += 1,
            "not_addition" => bad.0 = current.clone(),
            "duplicate" => {}
            _ => unreachable!(),
        }
        let requests = if fault == "duplicate" {
            vec![bad.clone(), bad]
        } else {
            vec![bad]
        };
        assert!(
            verify_waiver_witnesses(&store.connection, &state, &requests).is_err(),
            "{fault}"
        );
        assert_eq!(
            test_database_shape_snapshot(&store.connection).unwrap(),
            snapshot
        );
    }
    let mut branch = load_head(&store.connection, &witness.0).unwrap();
    branch.header.updated_at = at(500);
    let object = CanonicalObject::freeze(&branch).unwrap();
    SqliteStore::insert_object(&store.connection, KIND, &object).unwrap();
    let mut fork = witness.clone();
    fork.0.head = object.hash().clone();
    let snapshot = test_database_shape_snapshot(&store.connection).unwrap();
    assert!(
        verify_waiver_witnesses(&store.connection, &state, &[fork])
            .unwrap_err()
            .to_string()
            .contains("not an ancestor")
    );
    assert_eq!(
        test_database_shape_snapshot(&store.connection).unwrap(),
        snapshot
    );

    let mut broken = load_head(&store.connection, &current).unwrap();
    broken.state_checksum = witness.0.head.clone();
    replace_head(&store, &current.head, &broken);
    assert!(
        verify_waiver_witnesses(&store.connection, &state, &[witness])
            .unwrap_err()
            .to_string()
            .contains("full-state checksum")
    );
}

#[test]
fn root_delta_waiver_fact_proof_refuses_remove_then_readd() {
    let (mut store, root, id) = fixture();
    let witness = append_waiver(&mut store, &root, 1);
    let (original, _) = projected(&store.connection, id).unwrap();
    // The generic codec admits removals: prove the reader checks the chain,
    // rather than assuming production lifecycle writers never issue one.
    let transaction = store.connection.unchecked_transaction().unwrap();
    let mut removed = original.clone();
    removed.required_child_waivers.clear();
    removed.revision += 1;
    persist(&transaction, &removed).unwrap();
    let mut restored = original.clone();
    restored.revision += 2;
    persist(&transaction, &restored).unwrap();
    transaction.commit().unwrap();
    // Rebind the fixture's latest event to the resulting valid current state.
    let (_, current) = projected(&store.connection, id).unwrap();
    let head = load_head(&store.connection, &current).unwrap();
    replace_head(&store, &witness.0.head, &head);
    assert_eq!(resolve(&store.connection, &current).unwrap(), restored);
    let snapshot = test_database_shape_snapshot(&store.connection).unwrap();
    assert!(
        verify_waiver_witnesses(&store.connection, &restored, &[witness])
            .unwrap_err()
            .to_string()
            .contains("was removed")
    );
    assert_eq!(
        test_database_shape_snapshot(&store.connection).unwrap(),
        snapshot
    );
}

#[test]
fn root_delta_waiver_fact_proof_refuses_broken_chain_without_writes() {
    for fault in ["missing_predecessor", "sequence", "revision", "origin"] {
        let (mut store, root, id) = fixture();
        let witness = append_waiver(&mut store, &root, 1);
        advance_root(&mut store, &root);
        let (state, current) = projected(&store.connection, id).unwrap();
        let mut head = load_head(&store.connection, &current).unwrap();
        match fault {
            "missing_predecessor" => {
                head.predecessor = Some(
                    CanonicalObject::freeze(&"missing predecessor")
                        .unwrap()
                        .hash()
                        .clone(),
                );
            }
            "sequence" => head.sequence += 1,
            "revision" => head.previous_revision = Some(0),
            "origin" => head.predecessor = None,
            _ => unreachable!(),
        }
        replace_head(&store, &current.head, &head);
        let snapshot = test_database_shape_snapshot(&store.connection).unwrap();
        let error = verify_waiver_witnesses(&store.connection, &state, &[witness])
            .unwrap_err()
            .to_string();
        assert!(
            error.contains(match fault {
                "missing_predecessor" => "is missing",
                "origin" => "empty origin",
                _ => "discontinuity",
            }),
            "{fault}: {error}"
        );
        assert_eq!(
            test_database_shape_snapshot(&store.connection).unwrap(),
            snapshot
        );
    }
}

#[test]
fn root_delta_waiver_live_proof_does_not_audit_historical_checksums() {
    let (mut store, root, id) = fixture();
    let mut witness = append_waiver(&mut store, &root, 1);
    advance_root(&mut store, &root);
    let (state, current) = projected(&store.connection, id).unwrap();
    let mut bad = load_head(&store.connection, &witness.0).unwrap();
    let original_delta = bad.clone();
    let original_head = witness.0.head.clone();
    bad.state_checksum = current.head.clone();
    let bad_hash = replace_head(&store, &witness.0.head, &bad);
    witness.0.head = bad_hash.clone();
    let mut successor = load_head(&store.connection, &current).unwrap();
    assert_eq!(successor.predecessor, Some(original_head.clone()));
    successor.predecessor = Some(bad_hash.clone());
    let current_hash = replace_head(&store, &current.head, &successor);
    // Remove obsolete fixture objects: export must not fail merely because
    // re-canonicalization left an unbound old head behind.
    store
        .connection
        .execute(
            "DELETE FROM objects WHERE object_hash IN (?1, ?2)",
            params![original_head.as_str(), current.head.as_str()],
        )
        .unwrap();
    let address = RootExecutionRef {
        head: current_hash.clone(),
        ..current
    };
    let snapshot = test_database_shape_snapshot(&store.connection).unwrap();
    verify_waiver_witnesses(&store.connection, &state, std::slice::from_ref(&witness)).unwrap();
    assert_eq!(
        super::super::super::completion::validated_required_child_waivers(
            &store.connection,
            root.work_id,
            &state,
        )
        .unwrap(),
        vec![witness.1.clone()]
    );
    assert!(
        resolve(&store.connection, &address)
            .unwrap_err()
            .to_string()
            .contains("replayed state checksum mismatch")
    );
    assert!(!store.verify_all().unwrap().is_healthy());
    assert!(
        store
            .save_work_graph_snapshot(
                &root.project_id,
                &actor("export"),
                None,
                crate::WorkGraphSnapshotDestinationKind::DefaultFile,
                at(20),
                &DevelopmentNoopRedactor,
            )
            .is_err()
    );
    assert_eq!(
        test_database_shape_snapshot(&store.connection).unwrap(),
        snapshot
    );
    // Repair only the intentionally false checksum and its content addresses.
    // A healthy full audit now rules out another fixture fault as the cause.
    let repaired = replace_head(&store, &bad_hash, &original_delta);
    successor.predecessor = Some(repaired);
    replace_head(&store, &current_hash, &successor);
    store
        .connection
        .execute(
            "DELETE FROM objects WHERE object_hash IN (?1, ?2)",
            params![bad_hash.as_str(), current_hash.as_str()],
        )
        .unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
}

fn check_growing_waivers(counts: &[u32]) {
    for &count in counts {
        let (mut store, root, id) = fixture();
        let mut witnesses = Vec::new();
        for index in 1..=count {
            witnesses.push(append_waiver(&mut store, &root, index));
            let open_children: i64 = store.connection.query_row(
                "SELECT COUNT(*) FROM work_items WHERE parent_id = ?1 AND lifecycle IN ('proposed', 'open')",
                [root.work_id.0.to_string()], |row| row.get(0),
            ).unwrap();
            assert_eq!(
                open_children, 0,
                "terminal children do not consume the open-child budget"
            );
        }
        let (state, _) = projected(&store.connection, id).unwrap();
        let (deltas, changes): (i64, i64) = store.connection.query_row(
            "SELECT COUNT(*), SUM(json_array_length(canonical_json, '$.added') + json_array_length(canonical_json, '$.removed')) FROM objects WHERE object_kind = 'work_root_delta'",
            [], |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        let deltas = usize::try_from(deltas).unwrap();
        let changes = usize::try_from(changes).unwrap();
        let state_bytes = CanonicalObject::freeze(&state).unwrap().bytes().len();
        let member_count = members(&state).unwrap().len();
        let snapshot = test_database_shape_snapshot(&store.connection).unwrap();
        COST.with_borrow_mut(|cost| *cost = Cost::default());
        crate::canonical::reset_canonical_decode_count();
        verify_waiver_witnesses(&store.connection, &state, &witnesses).unwrap();
        let cost = COST.with_borrow(|cost| *cost);
        let decodes = crate::canonical::canonical_decode_count();
        eprintln!("waiver_fact_cost K={count} D={deltas} {cost:?} canonical_decodes={decodes}");
        // Derived from one backward pass and one validated current anchor.
        // K grows independently of the number of concurrently open children.
        assert!(cost.head_loads <= deltas, "{cost:?}");
        assert!(decodes <= deltas + 1, "{decodes}");
        assert_eq!(cost.checksums, 1);
        assert_eq!(cost.checksum_bytes, state_bytes);
        assert!(
            cost.member_hashes <= member_count + witnesses.len() + changes,
            "{cost:?}"
        );
        assert_eq!(
            test_database_shape_snapshot(&store.connection).unwrap(),
            snapshot
        );
        if count == 8 {
            // Automatic negative cost control: the previous per-witness full
            // replay cannot meet the same one-current-checksum budget.
            COST.with_borrow_mut(|cost| *cost = Cost::default());
            for (address, _) in &witnesses {
                resolve(&store.connection, address).unwrap();
            }
            assert!(COST.with_borrow(|cost| cost.checksum_bytes) > state_bytes);
        }

        // Measure the real successful completion too, not only its new helper.
        let parent = store.get_work_item(root.work_id).unwrap();
        let held = claim(&mut store, &parent, "holder", "claim", 4000, 3600);
        let parent = store.get_work_item(root.work_id).unwrap();
        let evidence = evidence(&mut store, &parent, &held, "holder", "proof", 4001);
        checkpoint(
            &mut store,
            &parent,
            &held,
            "holder",
            "cp",
            4002,
            std::slice::from_ref(&evidence),
        );
        let before = projected(&store.connection, id).unwrap().0;
        let state_bytes = CanonicalObject::freeze(&before).unwrap().bytes().len();
        let deltas: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM objects WHERE object_kind = 'work_root_delta'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let events: i64 = store.connection.query_row("SELECT COUNT(*) FROM work_feed_entries WHERE feed_kind = 'root_work' AND object_kind = 'work_event'", [], |row| row.get(0)).unwrap();
        let deltas = usize::try_from(deltas).unwrap();
        let events = usize::try_from(events).unwrap();
        COST.with_borrow_mut(|cost| *cost = Cost::default());
        crate::canonical::reset_canonical_decode_count();
        let seal = complete(
            &mut store, &parent, &held, "holder", &evidence, "done", 4003,
        )
        .unwrap();
        let cost = COST.with_borrow(|cost| *cost);
        let decodes = crate::canonical::canonical_decode_count();
        let after = projected(&store.connection, id).unwrap().0;
        let state_bound = state_bytes.max(CanonicalObject::freeze(&after).unwrap().bytes().len());
        eprintln!(
            "waiver_complete_cost K={count} D={deltas} E={events} {cost:?} canonical_decodes={decodes}"
        );
        // This fixture has no completed child seals, obligations, handoffs or
        // restored records. Claim admission, explicit root load, proof anchor,
        // persistence read/write and event binding each hash at most one state.
        assert!(cost.checksums <= 6, "{cost:?}");
        assert!(cost.checksum_bytes <= 6 * state_bound, "{cost:?}");
        assert!(cost.head_loads <= deltas + 4, "{cost:?}");
        // One feed scan, one proof chain, up to eight child binding reads per
        // waived child, and a fixed allowance for this fixture's admission/cut.
        // More headroom than the head-load bound, not equal sensitivity to
        // small regressions. Do not tighten it retrospectively to measurements.
        assert!(
            decodes <= deltas + events + 8 * witnesses.len() + 64,
            "{decodes}"
        );
        assert_eq!(seal.required_child_waivers.len(), witnesses.len());
    }
}

#[test]
fn root_delta_waiver_cost_grows_with_payload_not_repeated_full_states() {
    check_growing_waivers(&[1, 8]);
}

#[test]
#[ignore = "growing waiver history belongs to the separate scale phase"]
fn root_delta_scale_growing_waiver_cost() {
    check_growing_waivers(&[10, 100, 1000]);
}
