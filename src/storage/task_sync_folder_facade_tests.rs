use super::{SqliteStore, TaskSyncFolderApply};
use crate::{
    ProjectId,
    task_sync::{
        FORMAT, ProjectDocument, TaskTree,
        progress::{ExportProfile, documents_fingerprint},
    },
};

#[test]
fn storage_facade_exposes_folder_sync_to_a_non_work_caller() {
    let project = ProjectId("folder-facade".into());
    let tree = TaskTree {
        project: ProjectDocument {
            format: FORMAT.into(),
            project_id: project.clone(),
            notes_included: true,
        },
        tasks: Vec::new(),
    };
    let profile = ExportProfile {
        include_notes: true,
        origin_machine: Some("folder-facade".into()),
    };
    let export_basis = documents_fingerprint(&tree.folder_documents().unwrap());
    let mut store = SqliteStore::open_in_memory().unwrap();
    let actor = super::test_support::actor("folder-facade");

    let result = store
        .apply_task_sync_folder(
            TaskSyncFolderApply {
                project: &project,
                actor: &actor,
                tree: &tree,
                export_basis: &export_basis,
                profile: &profile,
                complete: true,
                now: chrono::DateTime::from_timestamp(1, 0).unwrap(),
            },
            "folder-facade-apply",
        )
        .unwrap();

    assert!(result.changed);
    assert_eq!(
        store
            .export_task_sync_folder(&project, &profile)
            .unwrap()
            .tree,
        tree
    );
}
