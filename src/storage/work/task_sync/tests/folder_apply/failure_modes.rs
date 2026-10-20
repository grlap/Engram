//! Recovery and freshness of the complete folder transaction, not replication.

use super::*;
use crate::storage::work::test_support::{checkpoint, complete, evidence};
use std::collections::BTreeMap;

mod freshness;
mod inputs;
mod process;
mod sealed;

fn apply_basis(
    store: &mut SqliteStore,
    tree: &TaskTree,
    retained_basis: &str,
    key: &str,
    second: i64,
) -> Result<TaskSyncImportResult, StoreError> {
    store.apply_task_sync_folder(
        TaskSyncFolderApply {
            project: &project(),
            actor: &actor("folder-importer"),
            tree,
            export_basis: retained_basis,
            profile: &profile(),
            complete: true,
            now: at(second),
        },
        key,
    )
}

fn revise_title(store: &mut SqliteStore, id: WorkId, title: &str, second: i64) {
    let item = query::load_work_item(&store.connection, id).unwrap();
    store
        .revise_work(
            &crate::ReviseWorkRequest {
                work_id: id,
                expected_revision: item.revision,
                patch: crate::WorkRevisionPatch {
                    title: Some(title.into()),
                    ..Default::default()
                },
                authority: WorkPlanningAuthority::Project,
                actor: actor("planner"),
                idempotency_key: format!("revision-{second}"),
                updated_at: at(second),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
}

fn all_durable_rows(store: &SqliteStore) -> Vec<(String, Vec<Vec<u8>>)> {
    state(store)
        .rows
        .into_iter()
        .filter(|(name, _)| {
            !crate::storage::work::is_rebuilt_projection_table(name)
                && !name.starts_with("work_catalog_fts")
        })
        .collect()
}

fn native_rows(store: &SqliteStore) -> Vec<(String, Vec<Vec<u8>>)> {
    state(store)
        .rows
        .into_iter()
        .filter(|(name, _)| {
            name == "work_completion_seals"
                || name == "work_root_members"
                || name.starts_with("work_run")
                || name == "work_claims"
                || name == "work_root_executions"
                || name.starts_with("control_")
        })
        .collect()
}

fn objects(store: &SqliteStore) -> BTreeMap<String, Vec<u8>> {
    store
        .connection
        .prepare("SELECT object_id, canonical_json FROM objects ORDER BY object_id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn count(store: &SqliteStore, table: &str) -> i64 {
    store
        .connection
        .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
            row.get(0)
        })
        .unwrap()
}

fn baselines(store: &SqliteStore) -> i64 {
    store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM work_feed_entries
         WHERE feed_kind = 'project' AND feed_id = ?1
         AND object_kind = 'task_sync_import_planning'",
            [&project().0],
            |row| row.get(0),
        )
        .unwrap()
}

/// Every imported mutation kind beside a real, unchanged local completion.
struct Mixed {
    selected: TaskTree,
    retained_basis: String,
    scalar: WorkId,
    relation: WorkId,
    removed: WorkId,
    added: WorkId,
    sealed: WorkId,
}

fn mixed(store: &mut SqliteStore) -> Mixed {
    let scalar = document("scalar", None);
    let removed = document("removed prerequisite", None);
    let mut relation = document("relation owner", None);
    relation.prerequisites.push(removed.id);
    let sealed = document("completed here", None);
    let mut initial = fixture().tree;
    initial.tasks = vec![
        scalar.clone(),
        relation.clone(),
        removed.clone(),
        sealed.clone(),
    ];
    initial.tasks.sort_by_key(|task| task.id.0);
    exchange(store, &initial, "enrol", 5).unwrap();
    let claim = claim_restored(store, sealed.id, "native-worker", 6);
    let item = query::load_work_item(&store.connection, sealed.id).unwrap();
    let proof = evidence(store, &item, &claim, "native-worker", "proof", 7);
    checkpoint(
        store,
        &item,
        &claim,
        "native-worker",
        "checkpoint",
        8,
        std::slice::from_ref(&proof),
    );
    let seal = complete(store, &item, &claim, "native-worker", &proof, "complete", 9).unwrap();
    assert_eq!(seal.work_id, sealed.id);

    let current = exported(store);
    let retained_basis = basis(store);
    let added = document("new independent task", None);
    let mut selected = without(&current, &[removed.id]);
    selected = edited(&selected, scalar.id, |task| {
        task.title = "imported scalar".into();
    });
    selected = edited(&selected, relation.id, |task| {
        task.prerequisites = vec![added.id];
    });
    selected.tasks.push(added.clone());
    selected.tasks.sort_by_key(|task| task.id.0);
    assert!(selected.validate_folder().is_ok());
    assert!(store.verify_all().unwrap().is_healthy());
    Mixed {
        selected,
        retained_basis,
        scalar: scalar.id,
        relation: relation.id,
        removed: removed.id,
        added: added.id,
        sealed: sealed.id,
    }
}

fn assert_applied(store: &SqliteStore, fixture: &Mixed) {
    assert_eq!(exported(store), fixture.selected);
    let scalar = events(store, fixture.scalar);
    assert_eq!(scalar.len(), 1);
    assert!(matches!(
        scalar[0].transition,
        WorkTransition::Revised {
            authority: WorkPlanningAuthority::Project
        }
    ));
    let relations = events(store, fixture.relation);
    assert_eq!(relations.len(), 2);
    assert!(matches!(relations[0].transition,
        WorkTransition::PrerequisiteRemoved { prerequisite_id, .. } if prerequisite_id == fixture.removed));
    assert!(matches!(relations[1].transition,
        WorkTransition::PrerequisiteAdded { prerequisite_id, .. } if prerequisite_id == fixture.added));
    let removal = events(store, fixture.removed);
    assert_eq!(removal.len(), 1);
    assert!(matches!(
        removal[0].transition,
        WorkTransition::Disposed { .. }
    ));
    for event in scalar.iter().chain(&relations).chain(&removal) {
        assert_eq!(event.actor, actor("folder-importer"));
        assert!(
            event.run_id.is_none()
                && event.run.is_none()
                && event.root_execution.is_none()
                && event.claim.is_none()
                && event.handoff_offer.is_none()
        );
    }
    assert_eq!(events(store, fixture.added), []);
    let added = query::load_work_item(&store.connection, fixture.added).unwrap();
    assert!(added.restored && added.active_run_id.is_none());
    assert!(store.verify_all().unwrap().is_healthy());
}
