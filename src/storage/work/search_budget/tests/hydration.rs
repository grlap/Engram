use super::*;
use crate::storage::work::cost;
use crate::{
    AddWorkBlockerRequest, ChangeWorkPrerequisiteRequest, ChildRequirement, DecomposeWorkRequest,
    DisposeWorkRequest, WorkDisposition, WorkLifecycle, WorkPlanningAuthority,
};

fn budget_cost() -> serde_json::Value {
    serde_json::to_value(cost::snapshot()).unwrap()["search_budget"].clone()
}

fn catalog_cost(
    store: &SqliteStore,
    project: &ProjectId,
    query: &WorkCatalogQuery,
) -> serde_json::Value {
    cost::start();
    store
        .query_work_catalog_continuation(project, at(10), query, None)
        .unwrap();
    budget_cost()
}

#[test]
fn metadata_search_charges_blocker_bytes_and_prerequisite_records() {
    for blockers in [true, false] {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let mut request = root_request("search-relations", "subject", 0);
        request.title = "unique metadata needle".into();
        let subject = store
            .create_work(&request, &DevelopmentNoopRedactor)
            .unwrap();
        let query = WorkCatalogQuery {
            search: Some("unique metadata needle".into()),
            limit: 1,
            ..WorkCatalogQuery::default()
        };
        let mut before = catalog_cost(&store, &subject.project_id, &query);
        for index in 0..12 {
            let current = store.get_work_item(subject.work_id).unwrap();
            if blockers {
                store
                    .add_work_blocker(
                        &AddWorkBlockerRequest {
                            work_id: subject.work_id,
                            expected_work_revision: current.revision,
                            kind: crate::WorkBlockerKind::Manual,
                            detail: format!("blocker {index} {}", "retained detail ".repeat(512)),
                            authority: WorkPlanningAuthority::Project,
                            actor: actor("planner"),
                            idempotency_key: format!("block-{index}"),
                            blocked_at: at(1),
                        },
                        &DevelopmentNoopRedactor,
                    )
                    .unwrap();
            } else {
                let provider = store
                    .create_work(
                        &root_request("search-relations", &format!("provider-{index}"), 1),
                        &DevelopmentNoopRedactor,
                    )
                    .unwrap();
                store
                    .add_work_prerequisite(
                        &ChangeWorkPrerequisiteRequest {
                            work_id: subject.work_id,
                            prerequisite_id: provider.work_id,
                            expected_revision: current.revision,
                            authority: WorkPlanningAuthority::Project,
                            actor: actor("planner"),
                            idempotency_key: format!("edge-{index}"),
                            changed_at: at(2),
                        },
                        &DevelopmentNoopRedactor,
                    )
                    .unwrap();
            }
            if index == 0 {
                // Keep availability blocked in both measurements: an unblocked
                // item also reads claim availability, which blocked items skip.
                before = catalog_cost(&store, &subject.project_id, &query);
            }
        }
        let complete = catalog_cost(&store, &subject.project_id, &query);
        assert!(
            complete["record_visits"].as_u64().unwrap()
                >= before["record_visits"].as_u64().unwrap() + 11
        );
        let limits = if blockers {
            let baseline = before["work_bytes"].as_u64().unwrap();
            assert!(complete["work_bytes"].as_u64().unwrap() > baseline + 32_768);
            Limits {
                bytes: baseline + 32_768,
                ..Limits::default()
            }
        } else {
            Limits {
                records: before["record_visits"].as_u64().unwrap() + 6,
                ..Limits::default()
            }
        };
        let result = with_limits(limits, || {
            store.query_work_catalog_continuation(&subject.project_id, at(10), &query, None)
        });
        assert!(
            matches!(result, Err(StoreError::SearchBudgetExhausted { dimension, .. })
            if dimension == if blockers { "work_bytes" } else { "record_visits" })
        );
        let page = store
            .query_work_catalog_continuation(&subject.project_id, at(10), &query, None)
            .unwrap()
            .0;
        assert_eq!(page.items.len(), 1);
        assert_eq!(page.items[0].blockers.len(), if blockers { 12 } else { 0 });
    }
}

fn successor_fixture(
    database: &std::path::Path,
) -> (SqliteStore, crate::WorkItem, WorkCatalogQuery) {
    let mut store = SqliteStore::open(database).unwrap();
    let root = store
        .create_work(
            &root_request("search-enrichment", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let plan = store
        .decompose_work(
            &DecomposeWorkRequest {
                parent_id: root.work_id,
                expected_parent_revision: root.revision,
                children: (0..16)
                    .map(|index| {
                        child(
                            &format!("child-{index}"),
                            ChildRequirement::Required,
                            if index == 0 {
                                "unique successor needle"
                            } else {
                                "Other required outcome"
                            },
                        )
                    })
                    .collect(),
                prerequisites: Vec::new(),
                authority: WorkPlanningAuthority::Project,
                actor: actor("planner"),
                idempotency_key: "plan".into(),
                created_at: at(1),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let target = &plan.children[0];
    let target = store
        .dispose_work(
            &DisposeWorkRequest {
                work_id: target.work_id,
                expected_work_revision: target.revision,
                disposition: WorkDisposition::Superseded,
                replacement_id: Some(plan.children[1].work_id),
                reason: "successor owns this outcome".into(),
                actor: actor("planner"),
                idempotency_key: "supersede".into(),
                disposed_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let query = WorkCatalogQuery {
        search: Some("unique successor needle".into()),
        lifecycles: vec![WorkLifecycle::Superseded],
        limit: 1,
        ..WorkCatalogQuery::default()
    };
    (store, target, query)
}

#[test]
fn service_search_keeps_one_budget_through_successor_root_materialization() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("enrichment.db");
    let (store, target, query) = successor_fixture(&database);
    let catalog = catalog_cost(&store, &target.project_id, &query);
    let service = LocalWorkService::new(
        database,
        target.project_id.clone(),
        "reader".into(),
        crate::SessionId("search-reader".into()),
        None,
    );
    cost::start();
    let page = service
        .work_catalog_page(&query, None, None, at(10))
        .unwrap();
    assert_eq!(page.items.len(), 1);
    assert!(page.items[0].work.required_child_successor.is_some());
    let full = budget_cost();
    assert!(
        full["record_visits"].as_u64().unwrap() > catalog["record_visits"].as_u64().unwrap() + 12
    );
    cost::start();
    let result = with_limits(
        Limits {
            records: catalog["record_visits"].as_u64().unwrap() + 12,
            ..Limits::default()
        },
        || service.work_catalog_page(&query, None, None, at(10)),
    );
    assert!(matches!(
        result,
        Err(StoreError::SearchBudgetExhausted {
            dimension: "record_visits",
            ..
        })
    ));
    let partial = serde_json::to_value(cost::snapshot()).unwrap();
    assert!(partial["root"]["materialized_rows"].as_u64().unwrap() > 0);
    assert!(partial["root"]["materialized_rows"].as_u64().unwrap() < 16);
    assert_eq!(
        service
            .work_catalog_page(&query, None, None, at(10))
            .unwrap()
            .items
            .len(),
        1
    );
}

#[test]
fn cancellation_between_catalog_and_successor_is_seen_inside_enrichment() {
    let directory = crate::test_support::temp_home().unwrap();
    let (store, target, query) = successor_fixture(&directory.path().join("cancel.db"));
    let cancel = CancellationToken::new();
    let result = Request::new(cancel.clone()).run(|| {
        Ok(store.with_search_budget(query.search.as_deref(), || {
            store.query_work_catalog_continuation(&target.project_id, at(10), &query, None)?;
            cost::start();
            cancel.cancel();
            let enriched = store.required_child_successor(&target);
            // SQLite row mappers carry the refusal as a rusqlite error;
            // the enclosing scope restores its typed cancellation below.
            assert!(
                enriched.is_err(),
                "enrichment must stop before returning data"
            );
            let stopped = serde_json::to_value(cost::snapshot()).unwrap();
            assert_eq!(stopped["root"]["materialized_rows"], 0);
            assert_eq!(stopped["canonical_decodes"], 0);
            enriched
        })?)
    });
    assert!(matches!(
        result.unwrap_err().error,
        StoreError::SearchCancelled
    ));
    assert!(active(&store.connection).is_none());
    assert!(store.required_child_successor(&target).unwrap().is_some());
}
