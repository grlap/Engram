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

// Rewrites the stored head `id` as `new`. The record keeps its id, so every
// edge that names it still resolves: the reader must reject the semantic
// fault itself.
fn rewrite_head(store: &SqliteStore, id: &ObjectId, new: &RootExecutionDelta) {
    let object = CanonicalObject::identified(id, new).unwrap();
    assert_eq!(
        store
            .connection
            .execute(
                "UPDATE objects SET canonical_json = ?2 WHERE object_id = ?1",
                params![id.as_str(), object.bytes()],
            )
            .unwrap(),
        1
    );
}

// Points every event that names the head `old` at the stored head `new`.
fn rebind_events(store: &SqliteStore, old: &ObjectId, new: &ObjectId) {
    let ids: Vec<String> = store
        .connection
        .prepare(
            "SELECT object_id FROM objects WHERE object_kind = 'work_event'
         AND json_extract(canonical_json, '$.root_execution.head') = ?1",
        )
        .unwrap()
        .query_map([old.as_str()], |row| row.get(0))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap();
    assert!(!ids.is_empty());
    for id in ids {
        let id = ObjectId::from_stored(id).unwrap();
        let mut event: WorkEvent =
            load_typed_work_object(&store.connection, &id, "work_event").unwrap();
        event.root_execution.as_mut().unwrap().head = new.clone();
        let object = CanonicalObject::identified(&id, &event).unwrap();
        store
            .connection
            .execute(
                "UPDATE objects SET canonical_json = ?2 WHERE object_id = ?1",
                params![id.as_str(), object.bytes()],
            )
            .unwrap();
    }
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
    let object = CanonicalObject::mint(&branch).unwrap();
    SqliteStore::insert_object(&store.connection, KIND, &object).unwrap();
    let mut fork = witness.clone();
    fork.0.head = object.key().clone();
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
    rewrite_head(&store, &current.head, &broken);
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
    rebind_events(&store, &witness.0.head, &current.head);
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
                head.predecessor = Some(ObjectId::mint());
            }
            "sequence" => head.sequence += 1,
            "revision" => head.previous_revision = Some(0),
            "origin" => head.predecessor = None,
            _ => unreachable!(),
        }
        rewrite_head(&store, &current.head, &head);
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

fn audit_failures(store: &SqliteStore) -> Vec<String> {
    store.verify_work_projections().unwrap().1
}

#[test]
fn root_delta_audit_compares_the_last_checksum_and_names_the_first_mismatch() {
    let (mut store, root, id) = fixture();
    let witness = append_waiver(&mut store, &root, 1);
    advance_root(&mut store, &root);
    let (_, current) = projected(&store.connection, id).unwrap();
    let earlier = load_head(&store.connection, &witness.0).unwrap();
    let last = load_head(&store.connection, &current).unwrap();
    let mut false_earlier = earlier.clone();
    false_earlier.state_checksum = current.head.clone();
    let mut false_last = last.clone();
    false_last.state_checksum = witness.0.head.clone();
    let named = |head: &ObjectId, sequence: u64| {
        format!("work_root_delta:{head}:first_checksum_mismatch:{sequence}")
    };

    // The last head alone: the strict replay runs and names it.
    rewrite_head(&store, &current.head, &false_last);
    COST.with_borrow_mut(|cost| *cost = Cost::default());
    let failures = audit_failures(&store);
    assert_eq!(COST.with_borrow(|cost| cost.resolves), 1);
    assert!(
        failures.contains(&named(&current.head, last.sequence)),
        "{failures:?}"
    );
    assert!(
        failures.contains(&format!("work_root_execution:{}", id.0)),
        "{failures:?}"
    );
    assert!(!store.verify_all().unwrap().is_healthy());

    // An earlier head too: the report names the earlier one only.
    rewrite_head(&store, &witness.0.head, &false_earlier);
    let failures = audit_failures(&store);
    assert!(
        failures.contains(&named(&witness.0.head, earlier.sequence)),
        "{failures:?}"
    );
    assert!(
        !failures.contains(&named(&current.head, last.sequence)),
        "{failures:?}"
    );

    rewrite_head(&store, &witness.0.head, &earlier);
    rewrite_head(&store, &current.head, &last);
    COST.with_borrow_mut(|cost| *cost = Cost::default());
    assert_eq!(audit_failures(&store), Vec::<String>::new());
    assert_eq!(COST.with_borrow(|cost| cost.resolves), 0);

    // The events still name the generation when its projection row is gone.
    rewrite_head(&store, &current.head, &false_last);
    store
        .connection
        .execute_batch("PRAGMA foreign_keys = OFF")
        .unwrap();
    store
        .connection
        .execute(
            "DELETE FROM work_root_executions WHERE root_execution_id = ?1",
            [id.0.to_string()],
        )
        .unwrap();
    let failures = audit_failures(&store);
    assert!(
        failures.contains(&named(&current.head, last.sequence)),
        "{failures:?}"
    );
    assert!(
        failures.contains(&format!("work_root_execution:{}:missing_projection", id.0)),
        "{failures:?}"
    );
}

#[test]
fn root_delta_audit_answers_the_same_head_for_each_address_asked() {
    let (mut store, root, id) = fixture();
    advance_root(&mut store, &root);
    let (state, current) = projected(&store.connection, id).unwrap();
    let mut another_generation = current.clone();
    another_generation.generation += 1;
    let mut audit = Audit::default();
    assert!(
        audit
            .history(&store.connection, &another_generation)
            .is_none()
    );
    assert_eq!(
        audit
            .history(&store.connection, &current)
            .map(|history| &history.state),
        Some(&state)
    );
    assert!(
        audit
            .history(&store.connection, &another_generation)
            .is_none()
    );
}

#[test]
fn root_delta_audit_binds_a_waiver_event_to_the_state_at_its_own_head() {
    let (mut store, root, id) = fixture();
    let before = projected(&store.connection, id).unwrap().1;
    let witness = append_waiver(&mut store, &root, 1);
    let (original, _) = projected(&store.connection, id).unwrap();
    let transaction = store.connection.unchecked_transaction().unwrap();
    let mut removed = original.clone();
    removed.required_child_waivers.clear();
    removed.revision += 1;
    persist(&transaction, &removed).unwrap();
    let (_, removal) = projected(&transaction, id).unwrap();
    let mut restored = original.clone();
    restored.revision += 2;
    persist(&transaction, &restored).unwrap();
    let (_, readded) = projected(&transaction, id).unwrap();
    transaction.commit().unwrap();
    let predecessor = load_head(&store.connection, &witness.0)
        .unwrap()
        .predecessor
        .unwrap();
    assert_ne!(predecessor, before.head, "the fixture adds other deltas");

    // The waiver is a member from the head that adds it up to the head that
    // removes it, and again from the head that adds it back.
    let mut bound = witness.0.head.clone();
    for (head, member) in [
        (witness.0.head.clone(), true),
        (predecessor, false),
        (removal.head.clone(), false),
        (readded.head.clone(), true),
    ] {
        if head != bound {
            rebind_events(&store, &bound, &head);
            bound = head.clone();
        }
        let failures = audit_failures(&store);
        let refused = |suffix: &str| failures.iter().any(|label| label.ends_with(suffix));
        assert_eq!(
            refused(":invalid_required_child_waiver"),
            !member,
            "{head}: {failures:?}"
        );
        assert_eq!(
            refused(":invalid_required_child_waivers"),
            !member,
            "{head}: {failures:?}"
        );
    }
}

#[test]
fn root_delta_audit_refuses_conflicting_waivers_that_a_later_delta_removes() {
    let (store, root, id) = fixture();
    let (base, _) = projected(&store.connection, id).unwrap();
    let child_waiver = |work_id, reason: &str| {
        RootExecutionMember::ChildWaiver(RequiredChildWaiver {
            work_id,
            work_revision: root.revision,
            waived_by: "test".into(),
            reason: reason.into(),
        })
    };
    let participant_waiver = |participant: &str, reason: &str| {
        RootExecutionMember::Waiver(CompletionWaiver {
            participant: SessionId(participant.into()),
            waived_by: "test".into(),
            reason: reason.into(),
        })
    };
    let mut with_one_each = base.clone();
    with_one_each
        .required_child_waivers
        .push(RequiredChildWaiver {
            work_id: root.work_id,
            work_revision: root.revision,
            waived_by: "test".into(),
            reason: "first".into(),
        });
    with_one_each.waivers.push(CompletionWaiver {
        participant: SessionId("absent".into()),
        waived_by: "test".into(),
        reason: "first".into(),
    });
    with_one_each.revision += 1;
    let mut cleared = base.clone();
    cleared.revision += 2;
    let transaction = store.connection.unchecked_transaction().unwrap();
    persist(&transaction, &with_one_each).unwrap();
    let (_, added_ref) = projected(&transaction, id).unwrap();
    persist(&transaction, &cleared).unwrap();
    let (_, last) = projected(&transaction, id).unwrap();
    transaction.commit().unwrap();
    assert!(Audit::default().history(&store.connection, &last).is_some());
    let adding = load_head(&store.connection, &added_ref).unwrap();
    let removing = load_head(&store.connection, &last).unwrap();
    // The same two deltas carry one more waiver, removed again before the
    // last head, so the final state and its checksum stay as they were.
    for (second, accepted) in [
        (
            child_waiver(crate::domain::WorkId::new(), "another child"),
            true,
        ),
        (child_waiver(root.work_id, "second"), false),
        (participant_waiver("another", "another participant"), true),
        (participant_waiver("absent", "second"), false),
    ] {
        let mut adding = adding.clone();
        adding.added.push(second.clone());
        let mut removing = removing.clone();
        removing.removed.push(second.clone());
        rewrite_head(&store, &added_ref.head, &adding);
        rewrite_head(&store, &last.head, &removing);
        assert_eq!(
            Audit::default().history(&store.connection, &last).is_some(),
            accepted,
            "{second:?}"
        );
    }
}

#[test]
fn root_delta_waiver_live_proof_does_not_audit_historical_checksums() {
    let (mut store, root, id) = fixture();
    let witness = append_waiver(&mut store, &root, 1);
    advance_root(&mut store, &root);
    let (state, current) = projected(&store.connection, id).unwrap();
    let mut bad = load_head(&store.connection, &witness.0).unwrap();
    let original_delta = bad.clone();
    bad.state_checksum = current.head.clone();
    rewrite_head(&store, &witness.0.head, &bad);
    let successor = load_head(&store.connection, &current).unwrap();
    assert_eq!(successor.predecessor, Some(witness.0.head.clone()));
    let address = current.clone();
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
    // The full audit compares the last head's checksum, which is true here.
    assert!(store.verify_all().unwrap().is_healthy());
    assert_eq!(
        test_database_shape_snapshot(&store.connection).unwrap(),
        snapshot
    );
    store
        .save_work_graph_snapshot(
            &root.project_id,
            &actor("export"),
            None,
            crate::WorkGraphSnapshotDestinationKind::DefaultFile,
            at(20),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    // Repair only the intentionally false checksum: the strict read then
    // succeeds, which rules out another fixture fault as its cause.
    rewrite_head(&store, &witness.0.head, &original_delta);
    assert_eq!(resolve(&store.connection, &address).unwrap(), state);
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
        super::cost::assert_audit_is_linear(&store, "growing_waivers", count);
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
    // 100 is a second point on the growth curve for a reader of the log; no
    // assertion compares the sizes.
    check_growing_waivers(&[100, super::scale_size("growing_waiver_cost")]);
}
