use super::*;
use crate::{AddWorkBlockerRequest, ChangeWorkPrerequisiteRequest, WorkPlanningAuthority};
use rusqlite::OptionalExtension;

fn fixture(import: bool) -> (SqliteStore, crate::WorkItem) {
    let mut source = SqliteStore::open_in_memory().unwrap();
    let mut request = root_request("restored-search-budget", "subject", 0);
    request.title = "Restored subject".into();
    request.notes = vec![format!(
        "restored budget needle {}",
        "retained prose ".repeat(1024)
    )];
    let subject = source
        .create_work(&request, &DevelopmentNoopRedactor)
        .unwrap();
    let provider = source
        .create_work(
            &root_request("restored-search-budget", "provider", 0),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    source
        .add_work_blocker(
            &AddWorkBlockerRequest {
                work_id: subject.work_id,
                expected_work_revision: subject.revision,
                kind: crate::WorkBlockerKind::Manual,
                detail: "retained blocker payload".into(),
                authority: WorkPlanningAuthority::Project,
                actor: actor("planner"),
                idempotency_key: "blocker".into(),
                blocked_at: at(1),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let current = source.get_work_item(subject.work_id).unwrap();
    source
        .add_work_prerequisite(
            &ChangeWorkPrerequisiteRequest {
                work_id: subject.work_id,
                prerequisite_id: provider.work_id,
                expected_revision: current.revision,
                authority: WorkPlanningAuthority::Project,
                actor: actor("planner"),
                idempotency_key: "edge".into(),
                changed_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let mut destination = SqliteStore::open_in_memory().unwrap();
    if import {
        let exported = source
            .export_task_tree_with_notes(&subject.project_id, "fixture-host")
            .unwrap();
        destination
            .import_task_tree(
                &subject.project_id,
                &actor("importer"),
                &exported.tree,
                false,
                at(4),
            )
            .unwrap();
    } else {
        let saved = source
            .save_work_graph_snapshot(
                &subject.project_id,
                &actor("saver"),
                None,
                crate::WorkGraphSnapshotDestinationKind::Stdout,
                at(3),
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        destination
            .load_work_graph_snapshot(
                &subject.project_id,
                &actor("loader"),
                &serde_json::to_vec(&saved.document).unwrap(),
                false,
                at(4),
                &DevelopmentNoopRedactor,
            )
            .unwrap();
    }
    let restored = destination.get_work_item(subject.work_id).unwrap();
    assert!(restored.restored);
    (destination, restored)
}

#[test]
fn restored_search_refuses_tight_byte_and_record_limits_then_recovers() {
    for import in [true, false] {
        let (store, item) = fixture(import);
        let query = WorkCatalogQuery {
            search: Some("restored budget needle".into()),
            limit: 1,
            ..WorkCatalogQuery::default()
        };
        for (dimension, limits) in [
            (
                "work_bytes",
                Limits {
                    bytes: 1024,
                    ..Limits::default()
                },
            ),
            (
                "record_visits",
                Limits {
                    records: 2,
                    ..Limits::default()
                },
            ),
        ] {
            let result = with_limits(limits, || {
                store.query_work_catalog_continuation(&item.project_id, at(5), &query, None)
            });
            assert!(
                matches!(result, Err(StoreError::SearchBudgetExhausted {dimension: actual, ..})
                if actual == dimension)
            );
            assert!(active(&store.connection).is_none());
        }
        let (page, count, _, _, _, matches) = store
            .query_work_catalog_continuation(&item.project_id, at(5), &query, None)
            .unwrap();
        assert_eq!(count, 1);
        assert_eq!(page.items[0].work.work_id, item.work_id);
        assert_eq!(page.items[0].blockers.len(), 1);
        assert_eq!(page.items[0].blocked_by.len(), 1);
        assert!(matches.contains_key(&item.work_id.0));
    }
}

#[test]
fn restored_origin_and_snapshot_audit_refuse_before_canonical_decode() {
    for import in [true, false] {
        let (store, item) = fixture(import);
        let origin: String = store.connection.query_row(
            "SELECT object_id FROM objects WHERE object_kind = 'task_sync_import_origin' ORDER BY length(canonical_json) DESC LIMIT 1",
            [], |row| row.get(0),
        ).optional().unwrap().unwrap_or_default();
        let kind = if import {
            "task_sync_import_origin"
        } else {
            "work_graph_snapshot_loaded"
        };
        let body_bytes: i64 = store
            .connection
            .query_row(
                "SELECT max(length(canonical_json)) FROM objects WHERE object_kind = ?1",
                [kind],
                |row| row.get(0),
            )
            .unwrap();
        let body_bytes = u64::try_from(body_bytes).unwrap();
        crate::canonical::reset_canonical_decode_count();
        let result = with_limits(
            Limits {
                bytes: 512 + u64::try_from("needle".len()).unwrap() + body_bytes - 1,
                ..Limits::default()
            },
            || {
                run(&store.connection, Some("needle"), || {
                    if import {
                        let id = crate::ObjectId::from_stored(origin.clone()).unwrap();
                        let object = SqliteStore::get_canonical_object_on(
                            &store.connection,
                            &id,
                            "task_sync_import_origin",
                        )?
                        .unwrap();
                        let _: serde_json::Value = object.decode()?;
                    } else {
                        crate::storage::graph_snapshot::work_graph_snapshot_load_origin_on(
                            &store.connection,
                            &item.project_id,
                        )?;
                    }
                    Ok(())
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
        assert!(store.get_work_item(item.work_id).unwrap().restored);
    }
}

#[test]
fn restored_relation_readers_charge_before_retaining_rows_or_decoding_bindings() {
    for import in [true, false] {
        let (store, item) = fixture(import);
        for blockers in [true, false] {
            crate::canonical::reset_canonical_decode_count();
            let result = with_limits(
                Limits {
                    records: 0,
                    ..Limits::default()
                },
                || {
                    run(&store.connection, Some("needle"), || {
                        if blockers {
                            super::super::super::query::load_active_blocker_projections(
                                &store.connection,
                                item.work_id,
                            )
                            .map(|rows| rows.len())
                        } else {
                            super::super::super::query::load_prerequisite_projection_ids(
                                &store.connection,
                                item.work_id,
                            )
                            .map(|rows| rows.len())
                        }
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
            assert_eq!(crate::canonical::canonical_decode_count(), 0);
        }
        assert!(store.get_work_item(item.work_id).unwrap().restored);
    }
}

#[test]
fn central_restored_readers_are_unchanged_without_a_search_scope() {
    for import in [true, false] {
        let (store, item) = fixture(import);
        let kind = if import {
            "task_sync_import_origin"
        } else {
            "work_graph_snapshot_loaded"
        };
        let (id, bytes): (String, Vec<u8>) = store.connection.query_row(
            "SELECT object_id, canonical_json FROM objects WHERE object_kind = ?1 ORDER BY length(canonical_json) DESC LIMIT 1",
            [kind], |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        let id = crate::ObjectId::from_stored(id).unwrap();
        let before = test_database_shape_snapshot(&store.connection).unwrap();
        super::super::super::cost::start();
        with_limits(
            Limits {
                vm_steps: 0,
                records: 0,
                bytes: 0,
                unit_bytes: 0,
                elapsed: Duration::ZERO,
            },
            || {
                // Test limits alone do not install a search scope. These same
                // central readers must return their original bytes without charging.
                let object = SqliteStore::get_canonical_object_on(&store.connection, &id, kind)
                    .unwrap()
                    .unwrap();
                assert_eq!(object.bytes(), bytes);
                if !import {
                    let audit = crate::storage::graph_snapshot::work_graph_snapshot_load_origin_on(
                        &store.connection,
                        &item.project_id,
                    )
                    .unwrap();
                    assert_eq!(crate::CanonicalObject::mint(&audit).unwrap().bytes(), bytes);
                }
            },
        );
        assert!(active(&store.connection).is_none());
        let cost = serde_json::to_value(super::super::super::cost::snapshot()).unwrap();
        assert!(cost["search_budget"].is_null());
        assert_eq!(
            test_database_shape_snapshot(&store.connection).unwrap(),
            before
        );
    }
}
