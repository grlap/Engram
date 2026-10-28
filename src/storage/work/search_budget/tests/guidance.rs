use super::*;
use crate::storage::work::{completion, cost, execution, feeds, query};
use crate::{
    ChildRequirement, DecomposeWorkRequest, OfferWorkHandoffRequest, WorkPlanningAuthority,
};

fn add_children(
    store: &mut SqliteStore,
    parent: &crate::WorkItem,
    count: usize,
    title: &str,
    second: i64,
) -> Vec<crate::WorkItem> {
    store
        .decompose_work(
            &DecomposeWorkRequest {
                parent_id: parent.work_id,
                expected_parent_revision: parent.revision,
                children: (0..count)
                    .map(|index| {
                        child(&format!("child-{index}"), ChildRequirement::Optional, title)
                    })
                    .collect(),
                prerequisites: Vec::new(),
                authority: WorkPlanningAuthority::Project,
                actor: actor("planner"),
                idempotency_key: format!("plan-{}", parent.work_id.0),
                created_at: at(second),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap()
        .children
}

fn search_cost(
    store: &SqliteStore,
    project: &ProjectId,
    query: &WorkCatalogQuery,
) -> serde_json::Value {
    cost::start();
    store
        .query_work_catalog_continuation(project, at(10), query, None)
        .unwrap();
    serde_json::to_value(cost::snapshot()).unwrap()
}

#[test]
fn deep_and_repeated_ancestor_visits_share_record_and_byte_limits() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let root = store
        .create_work(
            &root_request("search-ancestors", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let mut parent = root.clone();
    for second in 1..=3 {
        parent = add_children(&mut store, &parent, 1, "Intermediate", second).remove(0);
    }
    let leaves = add_children(&mut store, &parent, 8, "unique ancestry needle", 4);
    let leaf = &leaves[0];
    let before = test_database_shape_snapshot(&store.connection).unwrap();
    for (dimension, limits) in [
        (
            "record_visits",
            Limits {
                records: 0,
                ..Limits::default()
            },
        ),
        (
            "work_bytes",
            Limits {
                bytes: 512 + 6,
                ..Limits::default()
            },
        ),
    ] {
        let result = with_limits(limits, || {
            run(&store.connection, Some("needle"), || {
                query::projected_blocking_ancestor(&store.connection, leaf)
            })
        });
        assert!(
            matches!(result, Err(StoreError::SearchBudgetExhausted { dimension: actual, .. }) if actual == dimension)
        );
        assert!(active(&store.connection).is_none());
    }
    // Four shared ancestors per leaf, charged again on each traversal rather
    // than relying only on the per-traversal corruption guard.
    let result = with_limits(
        Limits {
            records: 7,
            ..Limits::default()
        },
        || {
            run(&store.connection, Some("needle"), || {
                assert!(query::projected_blocking_ancestor(&store.connection, leaf)?.is_none());
                query::projected_blocking_ancestor(&store.connection, &leaves[1])
            })
        },
    );
    assert!(matches!(
        result,
        Err(StoreError::SearchBudgetExhausted {
            dimension: "record_visits",
            ..
        })
    ));
    let query = WorkCatalogQuery {
        search: Some("unique ancestry needle".into()),
        limit: 8,
        ..WorkCatalogQuery::default()
    };
    let full = search_cost(&store, &root.project_id, &query);
    assert!(full["search_budget"]["record_visits"].as_u64().unwrap() >= 8 * 4);
    let records = full["search_budget"]["record_visits"].as_u64().unwrap();
    let result = with_limits(
        Limits {
            records: records - 1,
            ..Limits::default()
        },
        || store.query_work_catalog_continuation(&root.project_id, at(10), &query, None),
    );
    assert!(matches!(
        result,
        Err(StoreError::SearchBudgetExhausted {
            dimension: "record_visits",
            ..
        })
    ));
    assert_eq!(
        store
            .query_work_catalog(&root.project_id, at(10), &query)
            .unwrap()
            .items
            .len(),
        8
    );
    assert!(completion::ancestors_admit_execution(&store.connection, leaf).unwrap());
    assert_eq!(
        test_database_shape_snapshot(&store.connection).unwrap(),
        before
    );
}

#[test]
fn terminal_ancestor_search_charges_live_handoff_projection_and_event_before_decode() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let root = store
        .create_work(
            &root_request("search-handoff", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let subject = add_children(&mut store, &root, 1, "unique handoff needle", 1).remove(0);
    let held = claim(&mut store, &subject, "holder", "child-claim", 2, 600);
    let offer = store
        .offer_work_handoff(
            &OfferWorkHandoffRequest {
                work_id: subject.work_id,
                run_id: held.run_id,
                expected_work_revision: subject.revision,
                from: held.holder.clone(),
                to: crate::SessionId("recipient".into()),
                claim_id: held.claim_id,
                claim_fence: held.fence,
                ttl_seconds: 600,
                checkpoint_summary: "retained handoff state".into(),
                actor: actor("holder"),
                idempotency_key: "offer".into(),
                offered_at: at(3),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    // Complete after the child's authority expires. A read at the earlier
    // advisory time still sees a live offer below the now-terminal ancestor;
    // all stored projections and canonical events remain mutually valid.
    let root = store.get_work_item(root.work_id).unwrap();
    let root_claim = claim(&mut store, &root, "root-holder", "root-claim", 10000, 600);
    let root = store.get_work_item(root.work_id).unwrap();
    let proof = evidence(
        &mut store,
        &root,
        &root_claim,
        "root-holder",
        "proof",
        10001,
    );
    checkpoint(
        &mut store,
        &root,
        &root_claim,
        "root-holder",
        "checkpoint",
        10001,
        std::slice::from_ref(&proof),
    );
    complete(
        &mut store,
        &root,
        &root_claim,
        "root-holder",
        &proof,
        "complete",
        10002,
    )
    .unwrap();
    let before = test_database_shape_snapshot(&store.connection).unwrap();
    for canonical_event in [false, true] {
        crate::canonical::reset_canonical_decode_count();
        let result = with_limits(
            Limits {
                bytes: 512 + 6,
                ..Limits::default()
            },
            || {
                run(&store.connection, Some("needle"), || {
                    if canonical_event {
                        execution::latest_canonical_handoff_offer(
                            &store.connection,
                            &offer.offer_id.0.to_string(),
                            &subject.work_id.0.to_string(),
                        )
                    } else {
                        execution::live_handoff_on(&store.connection, held.run_id, at(10))
                    }
                })
            },
        );
        assert!(matches!(
            result,
            Err(StoreError::SearchBudgetExhausted {
                dimension: "work_bytes",
                ..
            })
        ));
        assert_eq!(crate::canonical::canonical_decode_count(), 0);
    }
    let query = WorkCatalogQuery {
        search: Some("unique handoff needle".into()),
        limit: 1,
        ..WorkCatalogQuery::default()
    };
    // Note scanning validates the retained checkpoint as well as the offer.
    // Use the complete catalog path here: continuation-only claim enrichment
    // runs after the handoff and would move the final-read boundary under test.
    cost::start();
    store
        .query_work_catalog(&root.project_id, at(10), &query)
        .unwrap();
    let full = serde_json::to_value(cost::snapshot()).unwrap();
    let complete_decodes = full["canonical_decodes"].as_u64().unwrap();
    assert!(complete_decodes >= 2);
    for limits in [
        Limits {
            bytes: full["search_budget"]["work_bytes"].as_u64().unwrap() - 1,
            ..Limits::default()
        },
        Limits {
            records: full["search_budget"]["record_visits"].as_u64().unwrap() - 1,
            ..Limits::default()
        },
    ] {
        crate::canonical::reset_canonical_decode_count();
        let result = with_limits(limits, || {
            store.query_work_catalog(&root.project_id, at(10), &query)
        });
        assert!(matches!(
            result,
            Err(StoreError::SearchBudgetExhausted { .. })
        ));
        assert_eq!(
            u64::try_from(crate::canonical::canonical_decode_count()).unwrap(),
            complete_decodes - 1,
            "canonical offer event must not decode after refusal"
        );
    }
    let page = store
        .query_work_catalog(&root.project_id, at(10), &query)
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert!(
        !page.items[0]
            .reason_codes
            .contains(&crate::WorkReadinessReason::DetachAvailable)
    );
    assert_eq!(
        execution::live_handoff_on(&store.connection, held.run_id, at(10)).unwrap(),
        Some(offer)
    );
    assert_eq!(
        test_database_shape_snapshot(&store.connection).unwrap(),
        before
    );
}

#[test]
fn evidence_binding_projection_refuses_before_canonical_decode_and_is_noop_outside_scope() {
    let (store, _, evidence_id) = fixture();
    let run_id: String = store
        .connection
        .query_row(
            "SELECT run_id FROM work_run_evidence WHERE evidence_id = ?1",
            [evidence_id.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    let run_id = query::parse_work_run_id(&run_id).unwrap();
    crate::canonical::reset_canonical_decode_count();
    let refused = with_limits(
        Limits {
            records: 0,
            ..Limits::default()
        },
        || {
            run(&store.connection, Some("needle"), || {
                execution::work_evidence_kind_on(&store.connection, run_id, &evidence_id)
            })
        },
    );
    assert!(matches!(
        refused,
        Err(StoreError::SearchBudgetExhausted {
            dimension: "record_visits",
            ..
        })
    ));
    assert_eq!(crate::canonical::canonical_decode_count(), 0);
    cost::start();
    with_limits(
        Limits {
            records: 0,
            bytes: 0,
            ..Limits::default()
        },
        || {
            assert_eq!(
                execution::work_evidence_kind_on(&store.connection, run_id, &evidence_id).unwrap(),
                crate::WorkEvidenceKind::Generic
            );
        },
    );
    assert!(serde_json::to_value(cost::snapshot()).unwrap()["search_budget"].is_null());
}

#[test]
fn named_root_and_source_observation_validation_charge_before_decode_and_keep_nonsearch_bytes() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let work = store
        .create_work(
            &root_request("search-validation", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let held = claim(&mut store, &work, "holder", "claim", 1, 600);
    let work_run = store.get_work_run(held.run_id).unwrap();
    let event = crate::domain::NamedRootBindingEvent {
        schema_version: crate::domain::SCHEMA_VERSION,
        project_id: work.project_id.clone(),
        root_execution_id: work_run.root_execution_id,
        work_id: work.work_id,
        run_id: held.run_id,
        claim_id: held.claim_id,
        claim_fence: held.fence,
        session_id: held.holder.clone(),
        workspace_id: "search-validation-workspace".into(),
        generation: 1,
        named_at: at(2),
        kind: crate::domain::NamedRootBindingKind::Bound,
        end_reason: None,
        actor: actor("holder"),
        recorded_at: at(2),
    };
    let transaction = store.connection.transaction().unwrap();
    let (_, position) = feeds::append_named_root_binding_on(&transaction, &event).unwrap();
    let producer = host_check_producer(
        &transaction,
        &work,
        &held,
        "holder",
        "producer",
        3,
        crate::domain::ExecutionSourceBasis {
            git_tree: None,
            workspace_id: event.workspace_id.clone(),
            source_revision: "revision".into(),
            source_root_generation: Some(1),
            source_root_state: Some(crate::domain::SourceRootState::Named),
        },
    );
    transaction.commit().unwrap();
    let binding = feeds::latest_named_root_binding_on(
        &store.connection,
        held.run_id,
        held.claim_id,
        position.position,
    )
    .unwrap();
    let source = feeds::load_source_observation_on(&store.connection, &producer).unwrap();
    let before = test_database_shape_snapshot(&store.connection).unwrap();
    for named_root in [true, false] {
        for limits in [
            Limits {
                records: 0,
                ..Limits::default()
            },
            Limits {
                bytes: 512 + 6,
                ..Limits::default()
            },
        ] {
            crate::canonical::reset_canonical_decode_count();
            let result = with_limits(limits, || {
                run(&store.connection, Some("needle"), || {
                    if named_root {
                        feeds::latest_named_root_binding_on(
                            &store.connection,
                            held.run_id,
                            held.claim_id,
                            position.position,
                        )
                        .map(|_| ())
                    } else {
                        feeds::load_source_observation_on(&store.connection, &producer).map(|_| ())
                    }
                })
            });
            assert!(matches!(
                result,
                Err(StoreError::SearchBudgetExhausted { .. })
            ));
            assert_eq!(crate::canonical::canonical_decode_count(), 0);
        }
    }
    cost::start();
    with_limits(
        Limits {
            records: 0,
            bytes: 0,
            unit_bytes: 0,
            elapsed: Duration::ZERO,
            ..Limits::default()
        },
        || {
            assert_eq!(
                feeds::latest_named_root_binding_on(
                    &store.connection,
                    held.run_id,
                    held.claim_id,
                    position.position
                )
                .unwrap(),
                binding
            );
            assert_eq!(
                feeds::load_source_observation_on(&store.connection, &producer).unwrap(),
                source
            );
        },
    );
    assert!(serde_json::to_value(cost::snapshot()).unwrap()["search_budget"].is_null());
    assert_eq!(
        test_database_shape_snapshot(&store.connection).unwrap(),
        before
    );
}
