use super::*;
use std::time::Duration;

#[test]
fn folder_basis_rechecks_another_connections_committed_edit() {
    let home = crate::test_support::temp_home().unwrap();
    let database = home.path().join("freshness.db");
    let mut importer = SqliteStore::open(&database).unwrap();
    let fixture = fixture();
    exchange(&mut importer, &fixture.tree, "enrol", 5).unwrap();
    let retained_basis = basis(&importer);
    let mut writer = SqliteStore::open(&database).unwrap();
    revise_title(&mut writer, fixture.first, "committed after export", 6);
    let selected = edited(&fixture.tree, fixture.second, |task| task.priority = 4);
    let before = state(&importer);
    let error =
        apply_basis(&mut importer, &selected, &retained_basis, "stale-folder", 7).unwrap_err();
    assert_eq!(refusal_code(&error), "task_sync_divergent_planning");
    assert_eq!(state(&importer), before);
    assert_eq!(state(&writer), before);
    assert!(importer.verify_all().unwrap().is_healthy());
}

#[test]
fn folder_write_lock_serializes_a_competing_revision_without_overwrite() {
    let home = crate::test_support::temp_home().unwrap();
    let database = home.path().join("serialization.db");
    let mut importer = SqliteStore::open(&database).unwrap();
    let fixture = fixture();
    exchange(&mut importer, &fixture.tree, "enrol", 5).unwrap();
    let mut writer = SqliteStore::open(&database).unwrap();
    writer.connection.busy_timeout(Duration::ZERO).unwrap();
    let original = query::load_work_item(&writer.connection, fixture.first).unwrap();
    let request = crate::ReviseWorkRequest {
        work_id: fixture.first,
        expected_revision: original.revision,
        patch: crate::WorkRevisionPatch {
            title: Some("competing writer".into()),
            ..Default::default()
        },
        authority: WorkPlanningAuthority::Project,
        actor: actor("planner"),
        idempotency_key: "competing-revision".into(),
        updated_at: at(7),
    };
    let retained_basis = basis(&importer);
    let selected = edited(&fixture.tree, fixture.first, |task| {
        task.title = "committed import".into();
    });
    let before = state(&writer);
    let transaction = importer.begin_work_mutation().unwrap();
    crate::storage::work::task_sync::folder::apply_task_sync_folder_on(
        &transaction,
        TaskSyncFolderApply {
            project: &project(),
            actor: &actor("folder-importer"),
            tree: &selected,
            export_basis: &retained_basis,
            profile: &profile(),
            complete: true,
            now: at(6),
        },
        "serialized-folder",
    )
    .unwrap();
    let error = writer
        .revise_work(&request, &crate::DevelopmentNoopRedactor)
        .unwrap_err();
    assert!(
        matches!(
            error,
            StoreError::WorkWriterAdmissionRefused {
                reason: crate::storage::WorkWriterAdmissionReason::BusyBudgetExhausted,
                ..
            }
        ),
        "{error:#}"
    );
    assert_eq!(state(&writer), before);
    transaction.commit().unwrap();
    let committed = state(&writer);
    let error = writer
        .revise_work(&request, &crate::DevelopmentNoopRedactor)
        .unwrap_err();
    assert!(
        matches!(error, StoreError::WorkRevisionConflict { .. }),
        "{error:#}"
    );
    assert_eq!(state(&writer), committed);
    assert_eq!(exported(&writer), selected);
    assert!(writer.verify_all().unwrap().is_healthy());
}

#[test]
fn folder_original_basis_allows_planning_aba_but_refuses_execution_aba() {
    for case in ["scalar", "relation", "execution"] {
        let (mut store, fixture) = enrolled();
        let retained_basis = basis(&store);
        match case {
            "scalar" => {
                let original = exported(&store)
                    .tasks
                    .into_iter()
                    .find(|task| task.id == fixture.first)
                    .unwrap()
                    .title;
                revise_title(&mut store, fixture.first, "away", 6);
                revise_title(&mut store, fixture.first, &original, 7);
            }
            "relation" => {
                for (second, add) in [(6, true), (7, false)] {
                    let item = query::load_work_item(&store.connection, fixture.first).unwrap();
                    let request = crate::domain::ChangeWorkPrerequisiteRequest {
                        work_id: fixture.first,
                        prerequisite_id: fixture.second,
                        expected_revision: item.revision,
                        authority: WorkPlanningAuthority::Project,
                        actor: actor("planner"),
                        idempotency_key: format!("relation-{second}"),
                        changed_at: at(second),
                    };
                    if add {
                        store
                            .add_work_prerequisite(&request, &crate::DevelopmentNoopRedactor)
                            .unwrap();
                    } else {
                        store
                            .remove_work_prerequisite(&request, &crate::DevelopmentNoopRedactor)
                            .unwrap();
                    }
                }
            }
            "execution" => {
                let claim = claim_restored(&mut store, fixture.first, "worker", 6);
                let item = query::load_work_item(&store.connection, fixture.first).unwrap();
                store
                    .release_work(
                        &ReleaseWorkRequest {
                            work_id: fixture.first,
                            run_id: claim.run_id,
                            expected_work_revision: item.revision,
                            holder: SessionId("worker".into()),
                            claim_id: claim.claim_id,
                            claim_fence: claim.fence,
                            reason: "hand back".into(),
                            waiver_reason: Some("no contribution".into()),
                            actor: actor("worker"),
                            idempotency_key: "release".into(),
                            released_at: at(7),
                        },
                        &crate::DevelopmentNoopRedactor,
                    )
                    .unwrap();
            }
            _ => unreachable!(),
        }
        assert_eq!(basis(&store), retained_basis, "{case}");
        assert_eq!(exported(&store), fixture.tree, "{case}");
        let selected = edited(&fixture.tree, fixture.first, |task| {
            task.title = "incoming change".into();
        });
        let before = state(&store);
        let result = apply_basis(&mut store, &selected, &retained_basis, "original-basis", 8);
        if case == "execution" {
            assert!(conflict(&result.unwrap_err()).contains("active_run"));
            assert_eq!(state(&store), before);
        } else {
            assert!(result.unwrap().changed);
            assert_eq!(exported(&store), selected);
            assert_eq!(events(&store, fixture.first).len(), 3);
        }
        assert!(store.verify_all().unwrap().is_healthy());
    }
}

#[test]
fn folder_old_basis_detects_feedless_restored_planning() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let retained_basis = basis(&store);
    let feed_cut = |store: &SqliteStore| -> i64 {
        store
            .connection
            .query_row(
                "SELECT COALESCE(MAX(position), 0) FROM work_feed_entries
             WHERE feed_kind = 'project' AND feed_id = ?1",
                [&project().0],
                |row| row.get(0),
            )
            .unwrap()
    };
    let before_cut = feed_cut(&store);
    assert_eq!(before_cut, 0);
    let fixture = fixture();
    store
        .import_task_tree(&project(), &actor("restorer"), &fixture.tree, false, at(5))
        .unwrap();
    assert_eq!(feed_cut(&store), before_cut);
    assert_ne!(basis(&store), retained_basis);
    // Keep the restored payload identical. Without the primary basis guard,
    // the fallback plain import would otherwise also refuse a changed payload,
    // hiding the missing freshness check. This is a new folder intent, not a
    // replay of the plain import's receipt.
    let selected = fixture.tree.clone();
    let before = state(&store);
    let error =
        apply_basis(&mut store, &selected, &retained_basis, "feedless-stale", 6).unwrap_err();
    assert_eq!(refusal_code(&error), "task_sync_divergent_planning");
    assert_eq!(state(&store), before);
    assert!(store.verify_all().unwrap().is_healthy());
}
