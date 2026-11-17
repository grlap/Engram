use super::*;

const PROJECT: &str = "catalog-label-cost";
const PENDING_START: u128 = 1_000_000;

// These are SQL projection fixtures, not imported canonical work. The test
// exercises candidate selection before Rust decodes selected work records.
fn seed_projection_rows(
    store: &SqliteStore,
    ids: std::ops::Range<u128>,
    lifecycle: &str,
    labelled: bool,
) {
    let transaction = store.connection.unchecked_transaction().unwrap();
    {
        let mut item = transaction
            .prepare(
                "INSERT INTO work_items (
                     work_id, project_id, short_ref, root_id, child_requirement,
                     lifecycle, priority, revision, created_at_ms, updated_at_ms, item_json
                 ) VALUES (?1, ?2, ?1, ?1, 'required', ?3, 2, 1, 0, 0, X'7B7D')",
            )
            .unwrap();
        let mut label = transaction
            .prepare("INSERT INTO work_item_labels VALUES (?1, 'ready-to-land')")
            .unwrap();
        for id in ids {
            let id = uuid::Uuid::from_u128(id).to_string();
            item.execute(params![id, PROJECT, lifecycle]).unwrap();
            if labelled {
                label.execute([id]).unwrap();
            }
        }
    }
    transaction.commit().unwrap();
}

fn selected_ids_and_steps(
    store: &SqliteStore,
    query: &WorkCatalogQuery,
    page: bool,
) -> (Vec<String>, i32) {
    let (sql, parameters) =
        work_catalog_sql(&ProjectId(PROJECT.into()), at(1), query, page, None).unwrap();
    // Match catalog_membership's ordered selected-id pass without hashing or
    // decoding; returned-row counters alone miss rejected candidate scans.
    let sql = if page {
        sql
    } else {
        assert!(sql.contains("SELECT COUNT(*) FROM classified"));
        format!(
            "{} ORDER BY work_id",
            sql.replace(
                "SELECT COUNT(*) FROM classified",
                "SELECT work_id, priority FROM classified"
            )
        )
    };
    let mut statement = store.connection.prepare(&sql).unwrap();
    let ids = statement
        .query_map(rusqlite::params_from_iter(parameters.iter()), |row| {
            row.get(0)
        })
        .unwrap()
        .collect::<Result<Vec<String>, _>>()
        .unwrap();
    let steps = statement.get_status(rusqlite::StatementStatus::VmStep);
    (ids, steps)
}

fn assert_cost_stays_flat(completed_history: bool) {
    let store = SqliteStore::open_in_memory().unwrap();
    seed_projection_rows(&store, PENDING_START..PENDING_START + 3, "open", true);
    let expected = (PENDING_START..PENDING_START + 3)
        .map(|id| uuid::Uuid::from_u128(id).to_string())
        .collect::<Vec<_>>();
    let queries = if completed_history {
        vec![
            WorkCatalogQuery {
                lifecycles: vec![WorkLifecycle::Open],
                limit: 20,
                ..WorkCatalogQuery::default()
            },
            WorkCatalogQuery {
                lifecycles: vec![WorkLifecycle::Open],
                label: Some("ready-to-land".into()),
                limit: 20,
                ..WorkCatalogQuery::default()
            },
        ]
    } else {
        vec![WorkCatalogQuery {
            label: Some("ready-to-land".into()),
            limit: 20,
            ..WorkCatalogQuery::default()
        }]
    };
    let mut observations = Vec::new();
    let mut previous = 0;
    for count in [100, 10_000, 100_000] {
        seed_projection_rows(
            &store,
            previous + 1..count + 1,
            if completed_history {
                "completed"
            } else {
                "open"
            },
            completed_history,
        );
        let mut costs = Vec::new();
        for query in &queries {
            for page in [false, true] {
                let (ids, steps) = selected_ids_and_steps(&store, query, page);
                assert_eq!(ids, expected);
                costs.push(steps);
            }
        }
        println!(
            "completed_history={completed_history} nonpending={count} selected=3 vm_steps={costs:?}"
        );
        observations.push(costs);
        previous = count;
    }
    assert!(
        observations.windows(2).all(|pair| pair[0] == pair[1]),
        "selection must not walk growing nonmatching work: {observations:?}"
    );
}

#[test]
fn open_catalog_and_label_queue_do_not_walk_completed_history() {
    assert_cost_stays_flat(true);
}

#[test]
fn label_catalog_does_not_walk_unlabelled_work() {
    assert_cost_stays_flat(false);
}

fn assert_filtered_cost_stays_flat(
    query: &WorkCatalogQuery,
    lifecycle: &str,
    configure: impl Fn(&SqliteStore),
) {
    let store = SqliteStore::open_in_memory().unwrap();
    seed_projection_rows(&store, PENDING_START..PENDING_START + 3, "open", false);
    let expected = (PENDING_START..PENDING_START + 3)
        .map(|id| uuid::Uuid::from_u128(id).to_string())
        .collect::<Vec<_>>();
    let mut observations = Vec::new();
    let mut previous = 0;
    for count in [100, 10_000, 100_000] {
        seed_projection_rows(&store, previous + 1..count + 1, lifecycle, false);
        configure(&store);
        let mut costs = Vec::new();
        for page in [false, true] {
            let (ids, steps) = selected_ids_and_steps(&store, query, page);
            assert_eq!(ids, expected);
            costs.push(steps);
        }
        println!("filtered nonpending={count} selected=3 vm_steps={costs:?}");
        observations.push(costs);
        previous = count;
    }
    assert!(
        observations.windows(2).all(|pair| pair[0] == pair[1]),
        "filtered selection must not walk unrelated work: {observations:?}"
    );
}

#[test]
fn short_text_lifecycle_search_does_not_walk_completed_assignments() {
    let query = WorkCatalogQuery {
        lifecycles: vec![WorkLifecycle::Open],
        search: Some("zz".into()),
        limit: 20,
        ..WorkCatalogQuery::default()
    };
    assert_filtered_cost_stays_flat(&query, "completed", |store| {
        store
            .connection
            .execute(
                "UPDATE work_items SET assigned_to_key = 'zz-owner',
                                       search_text_key = 'unrelated'",
                [],
            )
            .unwrap();
    });
}

#[test]
fn parent_scoped_catalog_does_not_walk_other_open_items() {
    let parent = WorkId(uuid::Uuid::from_u128(1));
    let query = WorkCatalogQuery {
        lifecycles: vec![WorkLifecycle::Open],
        parent_id: Some(parent),
        limit: 20,
        ..WorkCatalogQuery::default()
    };
    assert_filtered_cost_stays_flat(&query, "open", |store| {
        store
            .connection
            .execute(
                "UPDATE work_items SET parent_id = ?1 WHERE work_id >= ?2",
                params![
                    parent.0.to_string(),
                    uuid::Uuid::from_u128(PENDING_START).to_string()
                ],
            )
            .unwrap();
    });
}
