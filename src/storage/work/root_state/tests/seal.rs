use super::*;
use crate::domain::{CompletionSeal, RootExecutionRef, WorkClaim, WorkEvent};

fn ready(prior: u32) -> (SqliteStore, crate::WorkItem, WorkClaim, ObjectHash) {
    let (mut store, root, _) = fixture();
    let held = claim(&mut store, &root, "holder", "claim", 1, 3600);
    let root = store.get_work_item(root.work_id).unwrap();
    let proof = evidence(&mut store, &root, &held, "holder", "proof", 2);
    for index in 0..=prior {
        checkpoint(
            &mut store,
            &root,
            &held,
            "holder",
            &format!("cp-{index}"),
            3 + i64::from(index),
            std::slice::from_ref(&proof),
        );
    }
    (store, root, held, proof)
}

// Keep every ordinary admission binding sound while changing ONLY the root's
// accounting. Generic deltas can remove these facts; no product writer does so
// between the current-fence checkpoint and completion.
fn remove_accounting(store: &mut SqliteStore, root: &crate::WorkItem, contributor: bool) {
    let run = store.get_work_run(root.active_run_id.unwrap()).unwrap();
    let mut state = projected(&store.connection, run.root_execution_id)
        .unwrap()
        .0;
    if contributor {
        state.expected_contributors.clear();
    } else {
        let checkpoint = run.last_checkpoint.unwrap();
        state
            .contributions
            .retain(|entry| entry.object != checkpoint);
    }
    state.revision += 1;
    let transaction = store.begin_work_mutation().unwrap();
    persist(&transaction, &state).unwrap();
    transaction.commit().unwrap();
    let (_, address) = projected(&store.connection, run.root_execution_id).unwrap();
    let old: String = store
        .connection
        .query_row(
            "SELECT latest_event_hash FROM work_items WHERE work_id = ?1",
            [root.work_id.0.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    let old = ObjectHash::from_stored(old).unwrap();
    let mut event: WorkEvent =
        load_typed_work_object(&store.connection, &old, "work_event").unwrap();
    event.root_execution = Some(address);
    let new = CanonicalObject::freeze(&event).unwrap();
    SqliteStore::insert_object(&store.connection, "work_event", &new).unwrap();
    store
        .connection
        .execute(
            "UPDATE work_feed_entries SET object_hash = ?2 WHERE object_hash = ?1",
            params![old.as_str(), new.hash().as_str()],
        )
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE work_items SET latest_event_hash = ?2 WHERE latest_event_hash = ?1",
            params![old.as_str(), new.hash().as_str()],
        )
        .unwrap();
    store
        .connection
        .execute("DELETE FROM objects WHERE object_hash = ?1", [old.as_str()])
        .unwrap();
    // In particular, failure must not come from checksum, ancestry or binding.
    let loaded =
        super::super::super::query::load_root_execution(&store.connection, run.root_execution_id)
            .unwrap();
    assert_eq!(loaded, state);
    assert_eq!(
        resolve(&store.connection, event.root_execution.as_ref().unwrap()).unwrap(),
        state
    );
    assert_eq!(store.get_work_item(root.work_id).unwrap(), *root);
    assert_eq!(
        store.get_work_run(run.run_id).unwrap().last_checkpoint,
        event.run.unwrap().last_checkpoint
    );
}

#[test]
fn root_delta_seal_missing_accounting_refuses_exactly_without_writes() {
    for contributor in [true, false] {
        let (mut store, root, held, proof) = ready(0);
        remove_accounting(&mut store, &root, contributor);
        let before = test_database_shape_snapshot(&store.connection).unwrap();
        let error = complete(&mut store, &root, &held, "holder", &proof, "done", 20).unwrap_err();
        assert!(
            matches!(&error, StoreError::InvalidWorkProjection(reason)
            if reason == "completion root accounting is missing the holder or current checkpoint; run `engram doctor` and inspect the recorded history before restoring a verified store; completion does not repair canonical accounting"),
            "{error}"
        );
        assert_eq!(
            test_database_shape_snapshot(&store.connection).unwrap(),
            before
        );
    }
    let (mut store, root, held, proof) = ready(0);
    complete(&mut store, &root, &held, "holder", &proof, "done", 20).unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn root_delta_seal_keeps_exact_predecessor_and_history_after_later_changes() {
    let (mut store, root, held, proof) = ready(4);
    let id = store.get_work_run(held.run_id).unwrap().root_execution_id;
    let (before, address) = projected(&store.connection, id).unwrap();
    let seal = complete(&mut store, &root, &held, "holder", &proof, "done", 20).unwrap();
    assert_eq!(seal.root_execution, address);
    let hash = CanonicalObject::freeze(&seal).unwrap().hash().clone();
    let event = super::super::super::query::latest_canonical_work_event_for_item_optional(
        &store.connection,
        root.work_id,
    )
    .unwrap()
    .unwrap();
    let completed = event.root_execution.clone().unwrap();
    assert_eq!(
        load_head(&store.connection, &completed)
            .unwrap()
            .predecessor,
        Some(address.head.clone())
    );
    assert_eq!(store.completion_root_execution(&hash).unwrap(), before);

    // Reopen creates another generation. It must not replace the old seal's
    // accounting, even though current authority now belongs to another head.
    let item = store.get_work_item(root.work_id).unwrap();
    store
        .reopen_work(
            &crate::domain::ReopenWorkRequest {
                work_id: item.work_id,
                expected_work_revision: item.revision,
                reason: "new independent execution".into(),
                actor: actor("holder"),
                idempotency_key: "reopen".into(),
                reopened_at: at(21),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    assert_eq!(store.completion_root_execution(&hash).unwrap(), before);
    assert!(store.verify_all().unwrap().is_healthy());

    for wrong in [
        completed.clone(),
        RootExecutionRef {
            head: load_head(&store.connection, &address)
                .unwrap()
                .predecessor
                .unwrap(),
            ..address.clone()
        },
    ] {
        let mut altered = seal.clone();
        altered.root_execution = wrong;
        let mut altered_event = event.clone();
        let new_hash = CanonicalObject::freeze(&altered).unwrap().hash().clone();
        altered_event.transition = crate::domain::WorkTransition::Completed {
            seal: new_hash.clone(),
        };
        altered_event.run.as_mut().unwrap().completion_seal = Some(new_hash.clone());
        let snapshot = test_database_shape_snapshot(&store.connection).unwrap();
        assert!(
            super::super::super::completion::validate_seal_root_event(
                &store.connection,
                &altered,
                &new_hash,
                &altered_event
            )
            .is_err()
        );
        assert_eq!(
            test_database_shape_snapshot(&store.connection).unwrap(),
            snapshot
        );
    }
}

#[test]
fn root_delta_seal_accounting_address_does_not_copy_growing_collections() {
    let mut sizes = Vec::new();
    let mut cuts = Vec::new();
    for prior in [1, 16] {
        // Grow accounting through another real run. Checkpoints in the root
        // run itself would also change the decimal width of its sealed cut.
        let (mut store, root, _) = fixture();
        let plan = store
            .decompose_work(
                &crate::domain::DecomposeWorkRequest {
                    parent_id: root.work_id,
                    expected_parent_revision: root.revision,
                    children: vec![child(
                        "peer",
                        crate::domain::ChildRequirement::Optional,
                        "Peer work",
                    )],
                    prerequisites: Vec::new(),
                    authority: delegated(&root.project_id.0, "planner"),
                    actor: actor("planner"),
                    idempotency_key: "plan".into(),
                    created_at: at(1),
                },
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        let peer = &plan.children[0];
        let peer_claim = claim(&mut store, peer, "peer", "peer-claim", 2, 3600);
        let peer = store.get_work_item(peer.work_id).unwrap();
        let peer_proof = evidence(&mut store, &peer, &peer_claim, "peer", "peer-proof", 3);
        for index in 0..=prior {
            checkpoint(
                &mut store,
                &peer,
                &peer_claim,
                "peer",
                &format!("peer-cp-{index}"),
                4 + i64::from(index),
                std::slice::from_ref(&peer_proof),
            );
        }
        complete(
            &mut store,
            &peer,
            &peer_claim,
            "peer",
            &peer_proof,
            "peer-done",
            100,
        )
        .unwrap();
        let root = store.get_work_item(root.work_id).unwrap();
        let held = claim(&mut store, &root, "holder", "claim", 101, 3600);
        let root = store.get_work_item(root.work_id).unwrap();
        let proof = evidence(&mut store, &root, &held, "holder", "proof", 102);
        checkpoint(
            &mut store,
            &root,
            &held,
            "holder",
            "cp",
            103,
            std::slice::from_ref(&proof),
        );
        let seal: CompletionSeal =
            complete(&mut store, &root, &held, "holder", &proof, "done", 104).unwrap();
        cuts.push(seal.completion_cut.position);
        let json = serde_json::to_value(&seal).unwrap();
        for removed in ["expected_contributors", "contributions", "waivers"] {
            assert!(json.get(removed).is_none());
        }
        let frozen = CanonicalObject::freeze(&seal).unwrap();
        let accounting = store.completion_root_execution(frozen.hash()).unwrap();
        assert!(accounting.contributions.len() > usize::try_from(prior).unwrap());
        eprintln!(
            "seal_size prior_peer_checkpoints={prior} contributions={} root_bytes={} seal_bytes={} root_run_cut={}",
            accounting.contributions.len(),
            CanonicalObject::freeze(&accounting).unwrap().bytes().len(),
            frozen.bytes().len(),
            seal.completion_cut.position
        );
        sizes.push(frozen.bytes().len());
    }
    // UUIDs and object hashes have fixed spellings. These otherwise identical
    // seals keep all per-item fields fixed while only root history grows.
    assert_eq!(sizes[0], sizes[1]);
    assert_eq!(cuts[0], cuts[1]);
}

#[test]
fn root_delta_seal_own_run_growth_is_only_cut_decimal_width() {
    let mut without_cut_digits = Vec::new();
    let mut root_sizes = Vec::new();
    let mut cut_widths = Vec::new();
    for prior in [1, 16] {
        let (mut store, root, held, proof) = ready(prior);
        let seal = complete(&mut store, &root, &held, "holder", &proof, "done", 30).unwrap();
        let frozen = CanonicalObject::freeze(&seal).unwrap();
        let accounting = store.completion_root_execution(frozen.hash()).unwrap();
        let cut_width = seal.completion_cut.position.to_string().len();
        let root_bytes = CanonicalObject::freeze(&accounting).unwrap().bytes().len();
        eprintln!(
            "seal_own_run_size prior_checkpoints={prior} root_bytes={root_bytes} seal_bytes={} cut={} cut_digits={cut_width}",
            frozen.bytes().len(),
            seal.completion_cut.position
        );
        // Only the named scalar is subtracted. No blanket byte allowance and
        // no normalization of accounting fields can hide their reintroduction.
        without_cut_digits.push(frozen.bytes().len() - cut_width);
        root_sizes.push(root_bytes);
        cut_widths.push(cut_width);
    }
    assert_eq!(without_cut_digits[0], without_cut_digits[1]);
    assert!(root_sizes[1] > root_sizes[0]);
    assert!(
        cut_widths[1] > cut_widths[0],
        "exercise the decimal-width boundary"
    );
}
