use super::super::folder::TaskSyncFolderApply;
use super::*;
use crate::task_sync::progress::ExportProfile;
use std::collections::BTreeMap;

fn profile() -> ExportProfile {
    ExportProfile {
        include_notes: false,
        origin_machine: None,
    }
}

#[test]
fn memory_sync_combined_dry_run_sees_new_task_targets_and_rolls_back_everything() {
    use crate::task_sync::memories::{self, Head, ImportProvenance, KeyHistory, Revision, Source};
    let tree = candidate();
    let recorder = actor("combined-importer");
    let target = &tree.tasks[0];
    let body = "review this rule when the incoming task retires";
    let id = ObjectId::mint();
    let tag = ImportProvenance {
        format: memories::FORMAT.into(),
        project_id: tree.project.project_id.clone(),
        key: "rule".into(),
        revision_id: id.clone(),
        previous: None,
        body_sha256: memories::body_sha256(body),
        retiring_target: Some(crate::domain::ProjectMemoryRetiringTarget::Local {
            work_id: target.id,
            work_ref: format!("w-{}", &target.id.0.simple().to_string()[20..]),
        }),
        source_clear: false,
        source: Source::from_actor(&recorder, at(1)),
    };
    let selected = KeyHistory {
        format: memories::FORMAT.into(),
        project_id: tree.project.project_id.clone(),
        key: "rule".into(),
        head: Head::Active {
            chain: vec![id.clone()],
        },
        revisions: BTreeMap::from([(
            id,
            Revision {
                provenance: tag,
                body: body.into(),
            },
        )]),
    };
    let mut held = selected.clone();
    held.key = "held".into();
    for revision in held.revisions.values_mut() {
        revision.provenance.key = held.key.clone();
        if let Some(crate::domain::ProjectMemoryRetiringTarget::Local { work_id, .. }) =
            &mut revision.provenance.retiring_target
        {
            *work_id = WorkId::new();
        }
    }
    let memories = BTreeMap::from([("rule".into(), selected), ("held".into(), held)]);
    let mut store = SqliteStore::open_in_memory().unwrap();
    let profile = profile();
    let basis = store
        .export_task_sync_folder(&tree.project.project_id, &profile)
        .unwrap()
        .export_basis;
    let before = state(&store);
    let options = BTreeMap::new();
    let dry = store
        .apply_task_sync_folder_with_memories(
            request(&tree, &recorder, &basis, &profile, at(5)),
            "combined",
            &memories,
            &options,
            true,
        )
        .unwrap();
    assert!(dry.memory_partial && dry.tasks.dry_run);
    assert!(
        dry.memories
            .iter()
            .any(|r| matches!(r, memories::KeyImportResult::Applied { key, .. } if key == "rule"))
    );
    assert!(
        dry.memories
            .iter()
            .any(|r| matches!(r, memories::KeyImportResult::Refused { key, .. } if key == "held"))
    );
    assert_eq!(state(&store), before);
    let actual = store
        .apply_task_sync_folder_with_memories(
            request(&tree, &recorder, &basis, &profile, at(5)),
            "combined",
            &memories,
            &options,
            false,
        )
        .unwrap();
    assert!(actual.memory_partial);
    assert_eq!(actual.memories.len(), dry.memories.len());
    assert!(
        store
            .project_memory_sync_head(&tree.project.project_id, "rule")
            .unwrap()
            .is_some()
    );
    assert!(
        store
            .project_memory_sync_head(&tree.project.project_id, "held")
            .unwrap()
            .is_none()
    );
    let after = state(&store);
    let repeated = store
        .apply_task_sync_folder_with_memories(
            request(&tree, &recorder, &basis, &profile, at(6)),
            "combined",
            &memories,
            &options,
            false,
        )
        .unwrap();
    assert!(repeated.memories.iter().any(|r| matches!(r, memories::KeyImportResult::Applied { key, outcome } if key == "rule" && outcome.duplicate)));
    assert_eq!(state(&store), after);
    let (_, exported) = store
        .export_task_sync_folder_with_memories(
            &tree.project.project_id,
            &profile,
            &memories,
            &BTreeMap::new(),
        )
        .unwrap();
    assert!(exported.withheld.is_empty());
    assert_eq!(exported.keys.len(), 1);
    assert!(exported.keys.contains_key("rule"));
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
