use super::*;
use crate::domain::{
    ChildRequirement, DecomposeWorkRequest, DisposeWorkRequest, WaiveRequiredChildRequest,
    WorkDisposition,
};

fn reset() {
    COST.with_borrow_mut(|cost| *cost = Cost::default());
    crate::canonical::reset_canonical_decode_count();
}

fn report(operation: &str, prior: u32) -> Cost {
    let cost = COST.with_borrow(|cost| *cost);
    eprintln!(
        "root_cost operation={operation} prior={prior} {cost:?} canonical_decodes={}",
        crate::canonical::canonical_decode_count()
    );
    cost
}

#[test]
fn root_delta_mutation_cost_measurement() {
    for prior in [10, 100] {
        let (mut store, root, id) = fixture();
        for index in 1..=prior {
            capture(&mut store, &root, index);
        }
        let before = members(&projected(&store.connection, id).unwrap().0)
            .unwrap()
            .len();
        reset();
        capture(&mut store, &root, prior + 1);
        let cost = report("contributor", prior);
        // One validated read plus one pass hashing the resulting membership.
        assert!(cost.member_hashes <= 2 * before + 1, "{cost:?}");
        assert!(cost.checksums <= 2, "{cost:?}");
        let held = claim(&mut store, &root, "holder", "claim", 1001, 3600);
        let item = store.get_work_item(root.work_id).unwrap();
        let before = members(&projected(&store.connection, id).unwrap().0)
            .unwrap()
            .len();
        reset();
        let proof = evidence(&mut store, &item, &held, "holder", "evidence", 1002);
        let cost = report("evidence", prior);
        // The additional read is the existing independent claim admission.
        assert!(cost.member_hashes <= 3 * before + 1, "{cost:?}");
        assert!(cost.checksums <= 3, "{cost:?}");
        let before = members(&projected(&store.connection, id).unwrap().0)
            .unwrap()
            .len();
        reset();
        checkpoint(
            &mut store,
            &item,
            &held,
            "holder",
            "checkpoint",
            1003,
            &[proof],
        );
        let cost = report("checkpoint", prior);
        assert!(cost.member_hashes <= 3 * before + 1, "{cost:?}");
        assert!(cost.checksums <= 3, "{cost:?}");
    }
}

// Exhaustive audit intentionally retains intermediate full-state checksums.
// Keep its measured no-regression ceiling separate from the algorithm-derived
// live bounds below. These numbers do not claim a bounded audit algorithm.
fn assert_audit_replay_ceiling(prior: u32) {
    let (bytes, loads, decodes) = match prior {
        10 => (92_525, 206, 441),
        100 => (3_003_835, 1_286, 2_601),
        1000 => (257_098_620, 12_086, 24_201),
        _ => panic!("unmeasured audit fixture: {prior}"),
    };
    let cost = report("verify_all", prior);
    assert!(
        cost.checksum_bytes <= bytes,
        "temporary checksum regression: {cost:?}"
    );
    assert!(
        cost.head_loads <= loads,
        "temporary head-load regression: {cost:?}"
    );
    assert!(
        crate::canonical::canonical_decode_count() <= decodes,
        "audit canonical-decode regression: {prior}"
    );
}

struct LiveBudget {
    execution: RootExecutionId,
    state_bytes: usize,
    deltas: usize,
    events: usize,
    waivers: usize,
}

impl LiveBudget {
    fn before(store: &SqliteStore, item: &crate::WorkItem) -> Self {
        let execution = store
            .get_work_run(item.active_run_id.unwrap())
            .unwrap()
            .root_execution_id;
        let state = projected(&store.connection, execution).unwrap().0;
        let deltas: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM objects WHERE object_kind = 'work_root_delta'",
                [],
                |row| row.get(0),
            )
            .unwrap();
        let events: i64 = store.connection.query_row("SELECT COUNT(*) FROM work_feed_entries WHERE feed_kind = 'root_work' AND object_kind = 'work_event'", [], |row| row.get(0)).unwrap();
        let direct = state.required_child_waivers.iter().any(|waiver| {
            store.get_work_item(waiver.work_id).unwrap().parent_id == Some(item.work_id)
        });
        Self {
            execution,
            state_bytes: CanonicalObject::freeze(&state).unwrap().bytes().len(),
            deltas: if direct {
                usize::try_from(deltas).unwrap()
            } else {
                0
            },
            events: usize::try_from(events).unwrap(),
            waivers: state.required_child_waivers.len(),
        }
    }

    fn assert_after(self, store: &SqliteStore, operation: &str, prior: u32) {
        let cost = report(operation, prior);
        let decodes = crate::canonical::canonical_decode_count();
        let after = projected(&store.connection, self.execution).unwrap().0;
        let state_bytes = self
            .state_bytes
            .max(CanonicalObject::freeze(&after).unwrap().bytes().len());
        // At most six current-state hashes: claim, root, witness anchor,
        // persistence read/write, event binding. The witness scans D heads once;
        // the remaining four reads load a current head, not its history.
        assert!(cost.checksums <= 6, "{cost:?}");
        assert!(cost.checksum_bytes <= 6 * state_bytes, "{cost:?}");
        assert!(cost.head_loads <= self.deltas + 4, "{cost:?}");
        // This deliberately looser decode guard is not as sensitive as the
        // head-load bound; it does not promise to catch every small regression.
        assert!(
            decodes <= self.deltas + self.events + 8 * self.waivers + 64,
            "{decodes}"
        );
    }
}

#[test]
#[ignore = "root history cost measurement belongs to the separate scale phase"]
fn root_delta_scale_history_cost_measurement() {
    for prior in [10, 100, 1000] {
        let (mut store, root, _) = fixture();
        let plan = store
            .decompose_work(
                &DecomposeWorkRequest {
                    parent_id: root.work_id,
                    expected_parent_revision: root.revision,
                    children: vec![
                        child("waived-a", ChildRequirement::Required, "Waived A"),
                        child("waived-b", ChildRequirement::Required, "Waived B"),
                        child(
                            "completed-child",
                            ChildRequirement::Optional,
                            "Independent completion",
                        ),
                    ],
                    prerequisites: Vec::new(),
                    authority: delegated(&root.project_id.0, "planner"),
                    actor: actor("planner"),
                    idempotency_key: "plan".into(),
                    created_at: at(1),
                },
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        let held = claim(&mut store, &plan.parent, "holder", "claim", 2, 3600);
        let root = store.get_work_item(root.work_id).unwrap();
        let proof = evidence(&mut store, &root, &held, "holder", "root-evidence", 3);
        for index in 1..=prior {
            checkpoint(
                &mut store,
                &root,
                &held,
                "holder",
                &format!("cp-{index}"),
                4 + i64::from(index),
                std::slice::from_ref(&proof),
            );
        }
        for (index, child) in plan.children[..2].iter().enumerate() {
            store
                .dispose_work(
                    &DisposeWorkRequest {
                        work_id: child.work_id,
                        expected_work_revision: child.revision,
                        disposition: WorkDisposition::Cancelled,
                        replacement_id: None,
                        reason: "deliberate measured waiver".into(),
                        actor: actor("holder"),
                        idempotency_key: format!("cancel-{index}"),
                        disposed_at: at(1100),
                    },
                    &DevelopmentNoopRedactor,
                )
                .unwrap();
            store
                .waive_required_child(
                    &WaiveRequiredChildRequest {
                        parent_id: root.work_id,
                        child_id: child.work_id,
                        expected_parent_revision: root.revision,
                        reason: "deliberate measured waiver".into(),
                        actor: actor("holder"),
                        idempotency_key: format!("waive-{index}"),
                        waived_at: at(1101),
                    },
                    &DevelopmentNoopRedactor,
                )
                .unwrap();
        }
        reset();
        let audit = store.verify_all().unwrap();
        assert_audit_replay_ceiling(prior);
        assert!(audit.is_healthy(), "{audit:?}");

        // The waivers belong to the root, not this optional child. Its done
        // nevertheless reaches the generation-wide waiver validator.
        let child = &plan.children[2];
        let child_claim = claim(&mut store, child, "holder", "child-claim", 1102, 3600);
        let child = store.get_work_item(child.work_id).unwrap();
        let child_proof = evidence(
            &mut store,
            &child,
            &child_claim,
            "holder",
            "child-evidence",
            1103,
        );
        checkpoint(
            &mut store,
            &child,
            &child_claim,
            "holder",
            "child-cp",
            1104,
            std::slice::from_ref(&child_proof),
        );
        let budget = LiveBudget::before(&store, &child);
        reset();
        complete(
            &mut store,
            &child,
            &child_claim,
            "holder",
            &child_proof,
            "child-done",
            1105,
        )
        .unwrap();
        budget.assert_after(&store, "complete_child_with_other_parent_waiver", prior);
        checkpoint(
            &mut store,
            &root,
            &held,
            "holder",
            "root-final-cp",
            1106,
            std::slice::from_ref(&proof),
        );
        let budget = LiveBudget::before(&store, &root);
        reset();
        complete(
            &mut store,
            &root,
            &held,
            "holder",
            &proof,
            "root-done",
            1107,
        )
        .unwrap();
        budget.assert_after(&store, "complete_root", prior);
        assert!(store.verify_all().unwrap().is_healthy());
    }
}
