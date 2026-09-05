use super::*;
use crate::storage::work::canonical_work_events_for_item;
use crate::storage::work::query::{inspect_work_on, load_root_execution};
use crate::storage::work::test_support::*;
use crate::{
    AddWorkBlockerRequest, ChangeWorkPrerequisiteRequest, ClaimWorkRequest, DecomposeWorkRequest,
    ReviseWorkRequest, SessionId, WorkAvailability, WorkBlockerKind, WorkReadinessReason,
};

fn fixture(claim_child: bool) -> (SqliteStore, WorkItem, WorkItem) {
    fixture_with_handoff(claim_child, false)
}

fn fixture_with_handoff(claim_child: bool, handoff: bool) -> (SqliteStore, WorkItem, WorkItem) {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(&root_request("detach", "root", 0), &DevelopmentNoopRedactor)
        .expect("root");
    let plan = store
        .decompose_work(
            &DecomposeWorkRequest {
                parent_id: root.work_id,
                expected_parent_revision: root.revision,
                children: vec![child("follow-up", ChildRequirement::Optional, "Follow-up")],
                prerequisites: vec![],
                authority: WorkPlanningAuthority::Project,
                actor: actor("planner"),
                idempotency_key: "plan".into(),
                created_at: at(1),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("plan");
    let child = plan.children[0].clone();
    if claim_child {
        let owned = claim(&mut store, &child, "child", "claim-child", 2, 60);
        let proof = evidence(&mut store, &child, &owned, "child", "child-evidence", 3);
        checkpoint(
            &mut store,
            &child,
            &owned,
            "child",
            "child-checkpoint",
            4,
            &[proof],
        );
        if handoff {
            store
                .offer_work_handoff(
                    &crate::OfferWorkHandoffRequest {
                        work_id: child.work_id,
                        run_id: owned.run_id,
                        expected_work_revision: child.revision,
                        from: owned.holder.clone(),
                        to: SessionId("recipient".into()),
                        claim_id: owned.claim_id,
                        claim_fence: owned.fence,
                        ttl_seconds: 60,
                        checkpoint_summary: "Retained child offer".into(),
                        actor: actor("child"),
                        idempotency_key: "child-offer".into(),
                        offered_at: at(5),
                    },
                    &DevelopmentNoopRedactor,
                )
                .expect("offer");
        }
    }
    let root = plan.parent;
    let owned = claim(&mut store, &root, "parent", "claim-parent", 5000, 100);
    let proof = evidence(&mut store, &root, &owned, "parent", "parent-evidence", 5001);
    checkpoint(
        &mut store,
        &root,
        &owned,
        "parent",
        "parent-checkpoint",
        5002,
        std::slice::from_ref(&proof),
    );
    complete(
        &mut store,
        &root,
        &owned,
        "parent",
        &proof,
        "parent-done",
        5003,
    )
    .expect("parent done");
    assert!(store.verify_all().expect("verify fixture").is_healthy());
    (store, root, child)
}

fn request(child: &WorkItem) -> DetachWorkRequest {
    DetachWorkRequest {
        project_id: child.project_id.clone(),
        work_id: child.work_id,
        expected_work_revision: child.revision,
        reason: "independent continuation".into(),
        actor: actor("detacher"),
        idempotency_key: "detach".into(),
        detached_at: at(5004),
    }
}

#[test]
fn detach_refuses_a_canonically_bound_run_from_another_root() {
    let (mut store, _, child) = fixture(false);
    let foreign = store
        .create_work(
            &root_request("detach", "foreign", 5004),
            &DevelopmentNoopRedactor,
        )
        .expect("foreign root");
    let foreign_run = store
        .get_work_run(foreign.active_run_id.expect("foreign run"))
        .expect("foreign run");
    let mut event = canonical_work_events_for_item(&store.connection, child.work_id)
        .expect("child history")
        .pop()
        .expect("child event");
    let original = crate::CanonicalObject::freeze(&event).expect("original event");
    event.run.as_mut().expect("child run").root_execution_id = foreign_run.root_execution_id;
    let forged = crate::CanonicalObject::freeze(&event).expect("forged event");
    let run = event.run.as_ref().expect("forged run");
    // Model a self-consistent but cross-root canonical snapshot, not merely
    // projection drift that load_work_run already refuses before detach's guard.
    let transaction = store
        .connection
        .transaction()
        .expect("corruption transaction");
    SqliteStore::insert_object(&transaction, "work_event", &forged).expect("forged object");
    transaction
        .execute(
            "UPDATE work_runs SET root_execution_id = ?2, run_json = ?3 WHERE run_id = ?1",
            params![
                run.run_id.0.to_string(),
                run.root_execution_id.0.to_string(),
                serde_json::to_vec(run).expect("run JSON")
            ],
        )
        .expect("forged run binding");
    transaction
        .execute(
            "UPDATE work_feed_entries SET object_hash = ?2 WHERE object_hash = ?1",
            params![original.hash().as_str(), forged.hash().as_str()],
        )
        .expect("forged feed binding");
    transaction
        .execute(
            "UPDATE work_items SET latest_event_hash = ?2 WHERE work_id = ?1",
            params![child.work_id.0.to_string(), forged.hash().as_str()],
        )
        .expect("forged item binding");
    transaction.commit().expect("commit corruption");
    assert_eq!(
        *run,
        store.get_work_run(run.run_id).expect("canonical run loads")
    );
    let before = test_database_shape_snapshot(&store.connection).expect("before");
    let error = store
        .detach_work(&request(&child), &DevelopmentNoopRedactor)
        .expect_err("cross-root binding");
    assert!(
        matches!(error, StoreError::InvalidWorkProjection(ref reason)
        if reason == &format!("detach root execution {:?} crosses the work project or root boundary", run.root_execution_id)),
        "{error:?}"
    );
    assert_eq!(
        before,
        test_database_shape_snapshot(&store.connection).expect("after")
    );
}

#[test]
fn detach_catalog_is_projection_only_but_mutation_checks_canonical_ancestry() {
    let (mut store, root, child) = fixture(false);
    let query = crate::WorkCatalogQuery {
        blocked_only: true,
        limit: 100,
        ..crate::WorkCatalogQuery::default()
    };
    crate::canonical::reset_canonical_decode_count();
    let page = store
        .query_work_catalog(&child.project_id, at(5004), &query)
        .expect("catalog");
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].work.work_id, child.work_id);
    assert!(
        page.items[0]
            .reason_codes
            .contains(&WorkReadinessReason::DetachAvailable)
    );
    assert_eq!(crate::canonical::canonical_decode_count(), 0);
    let hash: String = store.connection.query_row(
        "SELECT object_hash FROM work_feed_entries WHERE feed_kind = 'project' AND work_id = ?1 AND object_kind = 'work_event' ORDER BY position DESC LIMIT 1",
        [root.work_id.0.to_string()], |row| row.get(0)).expect("parent latest event");
    assert_eq!(
        store
            .connection
            .execute(
                "UPDATE objects SET canonical_json = CAST('{}' AS BLOB) WHERE object_hash = ?1",
                [&hash]
            )
            .expect("damage parent event"),
        1
    );
    assert!(store.connection.is_autocommit());
    assert_eq!(
        store
            .query_work_catalog(&child.project_id, at(5004), &query)
            .expect("tolerant catalog"),
        page
    );
    assert!(matches!(
        store.inspect_work(root.work_id, at(5004)),
        Err(StoreError::HashMismatch { .. })
    ));
    let before = test_database_shape_snapshot(&store.connection).expect("before");
    let error = store
        .detach_work(&request(&child), &DevelopmentNoopRedactor)
        .expect_err("write verifies ancestry");
    let StoreError::HashMismatch { expected, actual } = error else {
        panic!("wrong refusal: {error:?}")
    };
    assert_eq!(expected.as_str(), hash);
    assert_eq!(actual, crate::ObjectHash::from_canonical_bytes(b"{}"));
    assert_eq!(
        test_database_shape_snapshot(&store.connection).expect("after"),
        before
    );
}

#[test]
fn detach_admission_is_leaf_first_and_does_not_take_over_live_claims() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(
            &root_request("detach-admission", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let mut parent = root.clone();
    let mut chain = vec![root];
    for depth in 1..=3 {
        let plan = store
            .decompose_work(
                &DecomposeWorkRequest {
                    parent_id: parent.work_id,
                    expected_parent_revision: parent.revision,
                    children: vec![child(
                        "optional",
                        ChildRequirement::Optional,
                        "Optional descendant",
                    )],
                    prerequisites: vec![],
                    authority: WorkPlanningAuthority::Project,
                    actor: actor("planner"),
                    idempotency_key: format!("plan-{depth}"),
                    created_at: at(depth),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("plan");
        *chain.last_mut().expect("parent") = plan.parent;
        parent = plan.children[0].clone();
        chain.push(parent.clone());
    }
    for item in &chain {
        let error = store
            .detach_work(&request(item), &DevelopmentNoopRedactor)
            .expect_err("not stranded");
        assert!(matches!(error, StoreError::WorkDetachRefused { .. }));
    }
    let leaf = &chain[3];
    let held = claim(&mut store, leaf, "other", "leaf-claim", 4, 100);
    let parent = &chain[1];
    let owner = claim(&mut store, parent, "owner", "parent-claim", 5, 100);
    let proof = evidence(&mut store, parent, &owner, "owner", "parent-proof", 6);
    checkpoint(
        &mut store,
        parent,
        &owner,
        "owner",
        "parent-checkpoint",
        7,
        std::slice::from_ref(&proof),
    );
    complete(
        &mut store,
        parent,
        &owner,
        "owner",
        &proof,
        "parent-completed",
        200,
    )
    .expect("nonroot parent completion");
    let execution_id = load_work_run(&store.connection, held.run_id)
        .expect("old run")
        .root_execution_id;
    let before_execution =
        load_root_execution(&store.connection, execution_id).expect("old execution");
    assert!(
        before_execution
            .expected_contributors
            .contains(&held.holder)
    );
    assert!(!super::super::root_participant_is_accounted(
        &before_execution,
        &held.holder
    ));
    let mut live = request(leaf);
    // Completion drains descendant authority. A backwards host clock can make
    // the retained expired claim appear live again; detach must still refuse.
    live.detached_at = at(9);
    let before = test_database_shape_snapshot(&store.connection).expect("before");
    let refusal = store
        .detach_work(&live, &DevelopmentNoopRedactor)
        .expect_err("live claim");
    assert!(
        matches!(refusal, StoreError::WorkDetachRefused { ref reason, .. } if reason.contains("live child claim")),
        "{refusal:?}"
    );
    assert_eq!(
        before,
        test_database_shape_snapshot(&store.connection).expect("after")
    );
    assert!(
        !store
            .inspect_work(leaf.work_id, live.detached_at)
            .expect("live claim advisory")
            .reason_codes
            .contains(&WorkReadinessReason::DetachAvailable)
    );
    let refusal = store
        .detach_work(&request(&chain[2]), &DevelopmentNoopRedactor)
        .expect_err("descendants");
    assert!(
        matches!(refusal, StoreError::WorkDetachRefused { ref reason, .. } if reason.contains("open descendants")),
        "{refusal:?}"
    );
    assert!(
        !store
            .inspect_work(chain[2].work_id, request(&chain[2]).detached_at)
            .expect("open descendants advisory")
            .reason_codes
            .contains(&WorkReadinessReason::DetachAvailable)
    );
    // This failure happens after both successor creation and live-root waiver.
    store.connection.execute_batch("CREATE TEMP TRIGGER refuse_live_detach BEFORE UPDATE ON work_items WHEN NEW.lifecycle = 'superseded' BEGIN SELECT RAISE(ABORT, 'injected live detach failure'); END").expect("trigger");
    let before = test_database_shape_snapshot(&store.connection).expect("before live rollback");
    assert!(matches!(
        store.detach_work(&request(leaf), &DevelopmentNoopRedactor),
        Err(StoreError::Sqlite(_))
    ));
    assert_eq!(
        test_database_shape_snapshot(&store.connection).expect("after live rollback"),
        before
    );
    store
        .connection
        .execute_batch("DROP TRIGGER refuse_live_detach")
        .expect("remove trigger");
    store
        .detach_work(&request(leaf), &DevelopmentNoopRedactor)
        .expect("expired claim can detach");
    let after_execution =
        load_root_execution(&store.connection, execution_id).expect("reconciled execution");
    let mut expected_execution = before_execution;
    assert!(waive_root_contributor(
        &mut expected_execution,
        &held.holder,
        &request(leaf).actor.actor_id,
        &request(leaf).reason
    ));
    expected_execution.revision += 1;
    expected_execution.updated_at = request(leaf).detached_at;
    assert_eq!(after_execution, expected_execution);
    assert_eq!(
        Some(held.clone()),
        load_work_claim_optional(&store.connection, leaf.active_run_id.expect("run"))
            .expect("claim unchanged")
    );
    let mut request = request(&chain[2]);
    request.idempotency_key = "detach-parent".into();
    store
        .detach_work(&request, &DevelopmentNoopRedactor)
        .expect("now leaf");
    let verified = store.verify_all().expect("doctor");
    assert!(verified.is_healthy(), "{verified:?}");
    let root = &chain[0];
    assert_eq!(
        *root,
        store
            .get_work_item(root.work_id)
            .expect("root unchanged before its completion")
    );
    let owner = claim(&mut store, root, "root-owner", "root-claim", 5005, 100);
    let proof = evidence(&mut store, root, &owner, "root-owner", "root-proof", 5006);
    checkpoint(
        &mut store,
        root,
        &owner,
        "root-owner",
        "root-checkpoint",
        5007,
        std::slice::from_ref(&proof),
    );
    let seal = complete(
        &mut store,
        root,
        &owner,
        "root-owner",
        &proof,
        "root-completed",
        5008,
    )
    .expect("root completes without hidden barrier");
    assert!(
        seal.waivers
            .iter()
            .any(|waiver| waiver.participant == held.holder)
    );
    assert!(store.verify_all().expect("completed doctor").is_healthy());
}

#[test]
fn detach_live_handoff_is_an_independent_refusal() {
    let (mut store, _, child) = fixture_with_handoff(true, true);
    let mut request = request(&child);
    // Forward-time completion drained this offer by expiry. A backward clock
    // exposes the still-offered record and its claim as live; prefer the offer.
    request.detached_at = at(6);
    let before = test_database_shape_snapshot(&store.connection).expect("before");
    let error = store
        .detach_work(&request, &DevelopmentNoopRedactor)
        .expect_err("live handoff");
    assert!(
        matches!(error, StoreError::WorkDetachRefused { ref reason, .. } if reason == "wait for the live child handoff to expire before detaching"),
        "{error:?}"
    );
    assert_eq!(
        test_database_shape_snapshot(&store.connection).expect("after"),
        before
    );
    assert!(
        !store
            .inspect_work(child.work_id, request.detached_at)
            .expect("status")
            .reason_codes
            .contains(&WorkReadinessReason::DetachAvailable)
    );
    request.detached_at = at(5004);
    store
        .detach_work(&request, &DevelopmentNoopRedactor)
        .expect("expired offer admits detach");
    assert!(store.verify_all().expect("doctor").is_healthy());
}

#[test]
fn detach_is_atomic_and_replays_without_changing_old_authority() {
    for with_claim in [false, true] {
        let (mut store, root, child) = fixture(with_claim);
        let old_parent = store.get_work_item(root.work_id).expect("parent");
        let old_run = store
            .get_work_run(root.active_run_id.expect("run"))
            .expect("run");
        let execution =
            load_root_execution(&store.connection, old_run.root_execution_id).expect("execution");
        let seal_hash = old_run.completion_seal.as_ref().expect("sealed parent");
        let seal_bytes: Vec<u8> = store
            .connection
            .query_row(
                "SELECT canonical_json FROM objects WHERE object_hash = ?1",
                [seal_hash.as_str()],
                |row| row.get(0),
            )
            .expect("seal bytes");
        let old_claim = store.current_work_claim_for_item(&child).expect("claim");
        let old_fence: i64 = store
            .connection
            .query_row(
                "SELECT claim_fence_head FROM work_runs WHERE run_id = ?1",
                [child.active_run_id.expect("run").0.to_string()],
                |row| row.get(0),
            )
            .expect("fence");
        let old_history =
            canonical_work_events_for_item(&store.connection, root.work_id).expect("history");
        let request = request(&child);
        // Force failure after successor creation. The entire coupled graph write must roll back.
        store.connection.execute_batch("CREATE TEMP TRIGGER refuse_detach BEFORE UPDATE ON work_items WHEN NEW.lifecycle = 'superseded' BEGIN SELECT RAISE(ABORT, 'injected detach failure'); END").expect("trigger");
        let before = test_database_shape_snapshot(&store.connection).expect("snapshot");
        let error = store
            .detach_work(&request, &DevelopmentNoopRedactor)
            .expect_err("rollback");
        assert!(matches!(error, StoreError::Sqlite(_)), "{error:?}");
        assert_eq!(
            before,
            test_database_shape_snapshot(&store.connection).expect("after")
        );
        store
            .connection
            .execute_batch("DROP TRIGGER refuse_detach")
            .expect("remove trigger");
        let successor = store
            .detach_work(&request, &DevelopmentNoopRedactor)
            .expect("detach");
        assert_eq!(successor.root_id, successor.work_id);
        assert_eq!(successor.parent_id, None);
        assert_eq!(
            (
                &successor.title,
                &successor.outcome,
                &successor.acceptance,
                successor.kind,
                successor.priority,
                &successor.labels
            ),
            (
                &child.title,
                &child.outcome,
                &child.acceptance,
                child.kind,
                child.priority,
                &child.labels
            )
        );
        assert_eq!(successor.assigned_to, None);
        assert!(
            successor
                .created_by
                .provenance_chain
                .iter()
                .any(|link| link.relation == ProvenanceRelation::DerivedFrom
                    && link.source == "work_detach"
                    && link.reference == Some(child.work_id.0.to_string()))
        );
        let disposed = store.get_work_item(child.work_id).expect("source");
        assert_eq!(disposed.lifecycle, WorkLifecycle::Superseded);
        assert_eq!(disposed.superseded_by, Some(successor.work_id));
        assert_eq!(
            old_parent,
            store.get_work_item(root.work_id).expect("parent")
        );
        assert_eq!(old_run, store.get_work_run(old_run.run_id).expect("run"));
        assert_eq!(
            seal_bytes,
            store
                .connection
                .query_row(
                    "SELECT canonical_json FROM objects WHERE object_hash = ?1",
                    [seal_hash.as_str()],
                    |row| row.get::<_, Vec<u8>>(0)
                )
                .expect("unchanged seal bytes")
        );
        assert_eq!(
            execution,
            load_root_execution(&store.connection, execution.root_execution_id).expect("execution")
        );
        assert_eq!(
            old_claim,
            load_work_claim_optional(&store.connection, child.active_run_id.expect("run"))
                .expect("old claim")
        );
        assert_eq!(
            old_fence,
            store
                .connection
                .query_row(
                    "SELECT claim_fence_head FROM work_runs WHERE run_id = ?1",
                    [child.active_run_id.expect("run").0.to_string()],
                    |row| row.get::<_, i64>(0)
                )
                .expect("fence")
        );
        assert_eq!(
            old_history,
            canonical_work_events_for_item(&store.connection, root.work_id).expect("history")
        );
        let after = test_database_shape_snapshot(&store.connection).expect("snapshot");
        assert_eq!(
            successor,
            store
                .detach_work(&request, &DevelopmentNoopRedactor)
                .expect("exact replay")
        );
        assert_eq!(
            after,
            test_database_shape_snapshot(&store.connection).expect("replay snapshot")
        );
        let mut conflict = request.clone();
        conflict.reason.push('!');
        assert!(matches!(
            store.detach_work(&conflict, &DevelopmentNoopRedactor),
            Err(StoreError::WorkOperationIdempotencyConflict { .. })
        ));
        assert_eq!(
            after,
            test_database_shape_snapshot(&store.connection).expect("conflict snapshot")
        );
        assert!(
            store
                .claim_work(
                    &ClaimWorkRequest {
                        work_id: child.work_id,
                        expected_work_revision: disposed.revision,
                        expected_run_id: child.active_run_id,
                        holder: SessionId("child".into()),
                        ttl_seconds: 60,
                        recovery_reason: None,
                        actor: actor("child"),
                        idempotency_key: "stale".into(),
                        claimed_at: at(5005)
                    },
                    &DevelopmentNoopRedactor
                )
                .is_err()
        );
        assert_eq!(
            inspect_work_on(&store.connection, successor.work_id, at(5005))
                .expect("ready")
                .availability,
            WorkAvailability::Ready
        );
        claim(
            &mut store,
            &successor,
            "new-owner",
            "claim-successor",
            5006,
            100,
        );
        let integrity = store.verify_all().expect("doctor");
        assert!(integrity.is_healthy(), "{integrity:?}");
    }
}

#[test]
fn detach_refuses_independent_constraints_and_stale_or_cross_project_requests() {
    for case in [
        "blocker",
        "multiple blockers",
        "prerequisite",
        "defer",
        "revision",
        "project",
        "blank",
    ] {
        let (mut store, _, child) = fixture(false);
        match case {
            "blocker" | "multiple blockers" => {
                for index in 0..if case == "blocker" { 1 } else { 2 } {
                    let current = store
                        .get_work_item(child.work_id)
                        .expect("current blocker basis");
                    store
                        .add_work_blocker(
                            &AddWorkBlockerRequest {
                                work_id: child.work_id,
                                expected_work_revision: current.revision,
                                kind: WorkBlockerKind::Manual,
                                detail: format!("external input {index}"),
                                authority: WorkPlanningAuthority::Project,
                                actor: actor("planner"),
                                idempotency_key: format!("block-{index}"),
                                blocked_at: at(5004),
                            },
                            &DevelopmentNoopRedactor,
                        )
                        .expect("block");
                }
            }
            "prerequisite" => {
                let other = store
                    .create_work(
                        &root_request("detach", "prerequisite", 5004),
                        &DevelopmentNoopRedactor,
                    )
                    .expect("other");
                store
                    .add_work_prerequisite(
                        &ChangeWorkPrerequisiteRequest {
                            work_id: child.work_id,
                            prerequisite_id: other.work_id,
                            expected_revision: child.revision,
                            authority: WorkPlanningAuthority::Project,
                            actor: actor("planner"),
                            idempotency_key: "edge".into(),
                            changed_at: at(5004),
                        },
                        &DevelopmentNoopRedactor,
                    )
                    .expect("edge");
            }
            "defer" => {
                store
                    .revise_work(
                        &ReviseWorkRequest {
                            work_id: child.work_id,
                            expected_revision: child.revision,
                            patch: WorkRevisionPatch {
                                deferred_until: Some(at(9999)),
                                ..WorkRevisionPatch::default()
                            },
                            authority: WorkPlanningAuthority::Project,
                            actor: actor("planner"),
                            idempotency_key: "defer".into(),
                            updated_at: at(5004),
                        },
                        &DevelopmentNoopRedactor,
                    )
                    .expect("defer");
            }
            _ => {}
        }
        let current = store.get_work_item(child.work_id).expect("child");
        let mut request = request(&current);
        match case {
            "revision" => request.expected_work_revision += 1,
            "project" => request.project_id = ProjectId("foreign".into()),
            "blank" => request.reason = " ".into(),
            _ => {}
        }
        let before = test_database_shape_snapshot(&store.connection).expect("before");
        let error = store
            .detach_work(&request, &DevelopmentNoopRedactor)
            .expect_err(case);
        match case {
            "blocker" | "multiple blockers" | "prerequisite" | "defer" => {
                let StoreError::WorkDetachRefused { remedy, reason, .. } = error else {
                    panic!("wrong refusal: {error:?}")
                };
                assert!(remedy.contains(match case {
                    "blocker" => "--unblock",
                    "prerequisite" => "--drop-after",
                    _ => "engram work show",
                }));
                if matches!(case, "multiple blockers" | "defer") {
                    assert_eq!(remedy, format!("engram work show {}", child.short_ref));
                    assert!(reason.contains(if case == "defer" {
                        "deferred wake time"
                    } else {
                        "2 independent active blocker(s)"
                    }));
                }
                assert!(
                    !inspect_work_on(&store.connection, child.work_id, at(5004))
                        .expect("status")
                        .reason_codes
                        .contains(&WorkReadinessReason::DetachAvailable)
                );
            }
            "revision" => assert!(matches!(error, StoreError::WorkRevisionConflict { .. })),
            _ => assert!(matches!(error, StoreError::InvalidWork(_))),
        }
        assert_eq!(
            before,
            test_database_shape_snapshot(&store.connection).expect("after")
        );
    }
}

#[test]
fn detach_restored_child_does_not_bootstrap_old_execution() {
    let (mut source, root, child) = fixture(false);
    let saved = source
        .save_work_graph_snapshot(
            &child.project_id,
            &actor("exporter"),
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(5010),
            &DevelopmentNoopRedactor,
        )
        .expect("save");
    let bytes = serde_json::to_vec_pretty(&saved.document).expect("snapshot JSON");
    let mut restored = SqliteStore::open_in_memory().expect("destination");
    restored
        .load_work_graph_snapshot(
            &child.project_id,
            &actor("loader"),
            &bytes,
            false,
            at(5011),
            &DevelopmentNoopRedactor,
        )
        .expect("load");
    let old_parent = restored.get_work_item(root.work_id).expect("parent");
    let child = restored.get_work_item(child.work_id).expect("child");
    assert!(child.restored && child.active_run_id.is_none());
    let mut request = request(&child);
    request.detached_at = at(5012);
    let successor = restored
        .detach_work(&request, &DevelopmentNoopRedactor)
        .expect("detach restored");
    assert_eq!(
        old_parent,
        restored.get_work_item(root.work_id).expect("parent")
    );
    assert!(
        restored
            .latest_work_run(child.work_id)
            .expect("old runs")
            .is_none()
    );
    assert!(
        restored
            .latest_work_run(root.work_id)
            .expect("parent runs")
            .is_none()
    );
    assert_eq!(
        inspect_work_on(&restored.connection, successor.work_id, at(5013))
            .expect("status")
            .availability,
        WorkAvailability::Ready
    );
    let verified = restored.verify_all().expect("verify");
    assert!(verified.is_healthy(), "{verified:?}");
}
