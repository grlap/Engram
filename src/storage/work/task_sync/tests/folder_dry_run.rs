use super::super::folder::TaskSyncFolderApply;
use super::*;
use crate::task_sync::progress::ExportProfile;

fn profile() -> ExportProfile {
    ExportProfile {
        include_notes: false,
        origin_machine: None,
    }
}

fn request<'a>(
    tree: &'a TaskTree,
    actor: &'a crate::ActorContext,
    basis: &'a str,
    profile: &'a ExportProfile,
    now: chrono::DateTime<chrono::Utc>,
) -> TaskSyncFolderApply<'a> {
    TaskSyncFolderApply {
        project: &tree.project.project_id,
        actor,
        tree,
        export_basis: basis,
        profile,
        complete: true,
        now,
    }
}

#[test]
fn task_sync_folder_dry_run_rolls_back_on_the_same_connection_and_predicts_the_real_run() {
    let tree = candidate();
    let mut store = SqliteStore::open_in_memory().unwrap();
    let profile = profile();
    let actor = actor("folder-importer");
    let basis = store
        .export_task_sync_folder(&tree.project.project_id, &profile)
        .unwrap()
        .export_basis;

    // A dry run applies everything and leaves the connection outside any
    // transaction, with no task, object, feed row or receipt behind.
    let empty = state(&store);
    let dry = store
        .dry_run_task_sync_folder(request(&tree, &actor, &basis, &profile, at(5)), "key")
        .unwrap();
    assert!(dry.changed && dry.dry_run);
    assert!(store.connection.is_autocommit());
    assert_eq!(state(&store), empty);

    // The real run under the same key reports what the dry run predicted.
    let real = store
        .apply_task_sync_folder(request(&tree, &actor, &basis, &profile, at(5)), "key")
        .unwrap();
    assert_eq!(
        real,
        TaskSyncImportResult {
            dry_run: false,
            ..dry
        }
    );

    // A dry run of the committed intent replays its receipt, as the real
    // repeat would, and still writes nothing.
    let committed = state(&store);
    let replayed = store
        .dry_run_task_sync_folder(request(&tree, &actor, &basis, &profile, at(6)), "key")
        .unwrap();
    assert_eq!(
        replayed,
        TaskSyncImportResult {
            dry_run: true,
            ..real
        }
    );
    assert!(store.connection.is_autocommit());
    assert_eq!(state(&store), committed);

    // A new intent against a stale basis refuses exactly as the real run does.
    let mut edited = tree.clone();
    edited.tasks[0].title = "Edited after the baseline".into();
    let stale = request(&edited, &actor, &basis, &profile, at(7));
    let dry_refusal = store.dry_run_task_sync_folder(stale, "stale").unwrap_err();
    assert!(store.connection.is_autocommit());
    let real_refusal = store.apply_task_sync_folder(stale, "stale").unwrap_err();
    assert_eq!(dry_refusal.to_string(), real_refusal.to_string());
    assert!(matches!(
        dry_refusal,
        StoreError::TaskSyncRefused {
            refusal: TaskSyncRefusal::DivergentPlanning,
            ..
        }
    ));
    assert!(store.connection.is_autocommit());
    assert_eq!(state(&store), committed);
}
