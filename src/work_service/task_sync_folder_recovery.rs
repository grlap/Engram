//! One export or import at a time on a data worktree, and an export
//! interrupted after recording its head: finished by the next export or
//! import over the folder it was planned against, refused over a folder that
//! changed since, and dropped when its head is no longer the current one.

use std::{collections::BTreeMap, fs};

use super::task_sync_folder_counts::{Exchange, profile};
use crate::task_sync::directory::FolderWrite;

fn now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

/// Each path's content fingerprint as the folder holds it now.
fn preimages<'a>(
    folder: &std::path::Path,
    paths: impl Iterator<Item = &'a String>,
) -> BTreeMap<String, Option<String>> {
    paths
        .map(|path| {
            let bytes = fs::read(folder.join(path)).ok();
            (
                path.clone(),
                bytes.map(|bytes| crate::ObjectId::from_canonical_bytes(&bytes).to_string()),
            )
        })
        .collect()
}

#[test]
fn a_running_export_or_import_refuses_another_on_the_same_data_worktree() {
    let exchange = Exchange::new(3);
    let data = exchange.data();
    let held = super::task_sync_folder_export::lock_folder(&data).unwrap();
    let export = exchange
        .a
        .service
        .export_task_sync_data_folder(&data, &profile(), false, None, now())
        .unwrap_err();
    let import = exchange
        .b
        .service
        .import_task_sync_data_folder(&data, None, false, false, None, now())
        .unwrap_err();
    for refusal in [export, import] {
        assert!(
            format!("{refusal:#}").contains("another task-sync export or import is running"),
            "{refusal:#}"
        );
    }
    drop(held);
    let receipt = exchange
        .a
        .service
        .export_task_sync_data_folder(&data, &profile(), false, None, now())
        .unwrap();
    assert_eq!(receipt.route, "unchanged");
}

#[test]
fn an_export_interrupted_after_its_head_is_finished_refused_or_dropped() {
    let exchange = Exchange::new(4);
    let ids = exchange.ids();
    let a = &exchange.a;
    let data = exchange.data();
    let (_, enrolled) = a
        .service
        .task_sync_folder_head(&profile(), now())
        .unwrap()
        .unwrap();
    a.rename(&ids[..1], "edited on A");
    let receipt = a
        .service
        .export_task_sync_data_folder(&data, &profile(), false, None, now())
        .unwrap();
    assert_eq!((receipt.route, receipt.written), ("incremental", 1));

    // The publication as it stood once its head was recorded: the journal
    // retained, the folder still the commit it was planned against.
    let folder = exchange.checkout.join(".engram");
    let edited = format!("tasks/{}.json", ids[0].0);
    let files = [edited.clone(), "project.json".to_owned()]
        .into_iter()
        .map(|path| {
            let bytes = fs::read_to_string(folder.join(&path)).unwrap();
            (path, Some(bytes))
        })
        .collect::<BTreeMap<_, _>>();
    let exported = files[&edited].clone();
    let (_, head) = a
        .service
        .task_sync_folder_head(&profile(), now())
        .unwrap()
        .unwrap();
    exchange.git(&["restore", "--", ".engram"]);
    let interrupted = FolderWrite {
        head: serde_json::to_value(&head).unwrap(),
        before: preimages(&folder, files.keys()),
        files,
    };
    interrupted.retain(&data).unwrap();

    // A later edit to a file the journal writes is never overwritten: the
    // import refuses and keeps the journal.
    let target = folder.join(&edited);
    let original = fs::read(&target).unwrap();
    fs::write(&target, b"{}").unwrap();
    let refusal = a
        .service
        .import_task_sync_data_folder(&data, None, false, false, None, now())
        .unwrap_err();
    assert!(
        format!("{refusal:#}").contains("interrupted export"),
        "{refusal:#}"
    );
    assert_eq!(fs::read(&target).unwrap(), b"{}");
    assert!(FolderWrite::pending(&data).unwrap().is_some());

    // Back at its preimage, the next export finishes it.
    fs::write(&target, original).unwrap();
    let receipt = a
        .service
        .export_task_sync_data_folder(&data, &profile(), false, None, now())
        .unwrap();
    assert_eq!(receipt.route, "unchanged");
    assert_eq!(
        fs::read_to_string(folder.join(&edited)).ok(),
        exported,
        "the interrupted export's file is written"
    );
    assert!(FolderWrite::pending(&data).unwrap().is_none());
    exchange.commit("Edit one task on A");
    // An observation records HEAD, as each command makes its own.
    let data = exchange.data();

    // A journal whose head is not the current one never touches the folder.
    let untouched = folder.join(format!("tasks/{}.json", ids[2].0));
    let before = fs::read(&untouched).unwrap();
    let files = BTreeMap::from([(format!("tasks/{}.json", ids[2].0), Some("{}".to_owned()))]);
    FolderWrite {
        head: serde_json::to_value(&enrolled).unwrap(),
        before: preimages(&folder, files.keys()),
        files,
    }
    .retain(&data)
    .unwrap();
    let receipt = a
        .service
        .import_task_sync_data_folder(&data, None, false, false, None, now())
        .unwrap();
    assert_eq!(receipt.route, "incremental");
    assert_eq!(fs::read(&untouched).unwrap(), before);
    assert!(FolderWrite::pending(&data).unwrap().is_none());
}

#[test]
fn a_task_file_hidden_from_status_is_refused_rather_than_read_as_committed() {
    let exchange = Exchange::new(3);
    let ids = exchange.ids();
    let path = format!(".engram/tasks/{}.json", ids[0].0);
    exchange.git(&["update-index", "--assume-unchanged", "--", &path]);
    let target = exchange.checkout.join(&path);
    let mut edited = fs::read_to_string(&target).unwrap();
    edited.push('\n');
    fs::write(&target, &edited).unwrap();
    let data = exchange.data();
    let export = exchange
        .a
        .service
        .export_task_sync_data_folder(&data, &profile(), false, None, now())
        .unwrap_err();
    let import = exchange
        .b
        .service
        .import_task_sync_data_folder(&data, None, false, false, None, now())
        .unwrap_err();
    for refusal in [export, import] {
        assert!(
            format!("{refusal:#}").contains("index inspection is suppressed"),
            "{refusal:#}"
        );
    }
    // The hidden edit is still there: nothing overwrote it.
    assert_eq!(fs::read_to_string(&target).unwrap(), edited);
}

#[test]
fn an_ignored_task_file_counts_as_a_folder_change() {
    let exchange = Exchange::new(3);
    // A rule outside the folder ignores a loose task file at a canonical path.
    let path = format!(".engram/tasks/{}.json", crate::WorkId::new().0);
    let excludes = exchange.data().git_directory.join("folder-excludes");
    fs::write(&excludes, format!("/{path}\n")).unwrap();
    exchange.git(&["config", "core.excludesFile", excludes.to_str().unwrap()]);
    let first = format!(".engram/tasks/{}.json", exchange.ids()[0].0);
    fs::copy(exchange.checkout.join(first), exchange.checkout.join(&path)).unwrap();
    let data = exchange.data();
    assert!(data.folder_changes().unwrap().contains(&path));
    // So no import or export takes the folder on disk for its commit.
    assert!(data.committed_folder().unwrap().is_none());
}

#[test]
fn an_ignored_entry_that_is_no_task_file_leaves_the_folder_committed() {
    let exchange = Exchange::new(3);
    let committed = exchange.data().committed_folder().unwrap();
    // A desktop's metadata file beside the task files, ignored as the
    // folder's own policy ignores every entry that is not a task file.
    let path = ".engram/tasks/.DS_Store";
    let excludes = exchange.data().git_directory.join("folder-excludes");
    fs::write(&excludes, format!("/{path}\n")).unwrap();
    exchange.git(&["config", "core.excludesFile", excludes.to_str().unwrap()]);
    fs::write(exchange.checkout.join(path), b"metadata").unwrap();
    let data = exchange.data();
    assert!(!data.folder_changes().unwrap().contains(path));
    assert_eq!(data.committed_folder().unwrap(), committed);
}

#[test]
fn a_loose_task_file_in_an_ignored_tasks_directory_counts_as_a_folder_change() {
    let exchange = Exchange::new(0);
    // A rule outside the folder ignores the whole tasks directory, which no
    // committed task file holds open.
    let excludes = exchange.data().git_directory.join("folder-excludes");
    fs::write(&excludes, "/.engram/tasks/\n").unwrap();
    exchange.git(&["config", "core.excludesFile", excludes.to_str().unwrap()]);
    let tasks = exchange.checkout.join(".engram").join("tasks");
    fs::create_dir_all(&tasks).unwrap();
    let path = format!(".engram/tasks/{}.json", crate::WorkId::new().0);
    fs::write(exchange.checkout.join(&path), b"{}").unwrap();
    fs::write(tasks.join(".DS_Store"), b"metadata").unwrap();
    let data = exchange.data();
    let changes = data.folder_changes().unwrap();
    assert!(changes.contains(&path), "{changes:?}");
    assert!(!changes.contains(".engram/tasks/.DS_Store"), "{changes:?}");
    assert!(data.committed_folder().unwrap().is_none());
}

#[test]
fn an_addition_whose_id_another_project_holds_refuses_the_incremental_import_and_names_the_rebuild()
{
    use super::task_sync_folder_counts::document;
    use crate::storage::TaskSyncTaskStatus::{Applied, Held, Refused};
    let exchange = Exchange::new(1);
    let a = &exchange.a;
    // This host publishes an export of its own, whose basis the rebuild
    // takes later.
    let _own = a.add("Own on A");
    a.service
        .export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now())
        .unwrap();
    let basis = crate::task_sync::directory::read_folder_basis(
        &exchange.checkout.join(".engram/project.json"),
    )
    .unwrap();
    exchange.commit("Publish a task from A");
    // Another host's commit adds a task whose id another project holds in
    // this store, a child of it, and an independent task.
    let foreign = a.add_to_project("another-project", "held by another project");
    let mut colliding = document(1);
    colliding.id = foreign;
    let mut child = document(2);
    child.parent_id = Some(foreign);
    let independent = document(3);
    let folder = exchange.checkout.join(".engram").join("tasks");
    for task in [&colliding, &child, &independent] {
        fs::write(
            folder.join(format!("{}.json", task.id.0)),
            crate::task_sync::pretty(task).unwrap(),
        )
        .unwrap();
    }
    exchange.commit("Another host adds a colliding task");
    // The incremental import refuses as a whole, in a dry run and for real,
    // naming the id, its project and the rebuild; nothing applies.
    for dry_run in [true, false] {
        let refusal = a
            .service
            .import_task_sync_data_folder(&exchange.data(), None, false, dry_run, None, now())
            .unwrap_err();
        let text = format!("{refusal:#}");
        assert!(
            text.contains(&foreign.0.to_string())
                && text.contains("another-project")
                && text.contains("--rebuild"),
            "{text}"
        );
    }
    assert!(!a.holds(independent.id));
    assert!(!a.holds(child.id));
    // The rebuild it names refuses the colliding addition, holds its child
    // and applies the rest.
    let receipt = a
        .service
        .import_task_sync_data_folder(
            &exchange.data(),
            Some(&crate::task_sync::directory::DestinationBasis::Recorded(
                Box::new(basis),
            )),
            true,
            false,
            None,
            now(),
        )
        .unwrap();
    assert_eq!(receipt.route, "rebuild");
    let decided = receipt
        .result
        .tasks
        .iter()
        .map(|task| (task.task, (task.status, task.reason.clone())))
        .collect::<BTreeMap<_, _>>();
    for (id, expected) in [
        (foreign, (Refused, Some("id_collision".to_owned()))),
        (child.id, (Held, Some("ancestor_binding".to_owned()))),
        (independent.id, (Applied, None)),
    ] {
        assert_eq!(decided.get(&id), Some(&expected), "{decided:?}");
    }
    assert_eq!(a.title(independent.id), "task 3");
    assert_eq!(a.title(foreign), "held by another project");
    let pending = |a: &super::task_sync_folder_counts::Host| {
        let (_, head) = a
            .service
            .task_sync_folder_head(&profile(), now())
            .unwrap()
            .unwrap();
        head.pending.keys().copied().collect::<Vec<_>>()
    };
    let mut held = vec![foreign, child.id];
    held.sort();
    assert_eq!(pending(a), held);
    // The rebuild holds both, so each plain import reads them again and
    // refuses while the folder carries them, naming both exits; so it does
    // once the other project's task is gone and only a task naming it stays.
    let plain = |a: &super::task_sync_folder_counts::Host| {
        a.service
            .import_task_sync_data_folder(&exchange.data(), None, false, false, None, now())
    };
    let refusal = format!("{:#}", plain(a).unwrap_err());
    assert!(
        refusal.contains(&foreign.0.to_string())
            && refusal.contains("another-project")
            && refusal.contains("Remove those tasks")
            && refusal.contains("--rebuild"),
        "{refusal}"
    );
    fs::remove_file(folder.join(format!("{}.json", foreign.0))).unwrap();
    exchange.commit("Remove another project's task from the folder");
    let refusal = format!("{:#}", plain(a).unwrap_err());
    assert!(
        refusal.contains(&format!("{}, which names {}", child.id.0, foreign.0))
            && refusal.contains("Remove those tasks")
            && refusal.contains("--rebuild"),
        "{refusal}"
    );
    assert!(!a.holds(child.id));
    // With every such task out of the folder, the incremental route imports
    // again and holds nothing.
    fs::remove_file(folder.join(format!("{}.json", child.id.0))).unwrap();
    exchange.commit("Remove the task naming it");
    assert_eq!(plain(a).unwrap().route, "incremental");
    assert_eq!(pending(a), Vec::<crate::WorkId>::new());
    assert!(!a.holds(child.id));
    assert_eq!(a.title(foreign), "held by another project");
}

#[test]
fn a_retired_task_whose_refused_return_leaves_the_folder_again_never_wedges_the_import() {
    use crate::storage::TaskSyncTaskStatus::Refused;
    let exchange = Exchange::new(3);
    let a = &exchange.a;
    let ids = exchange.ids();
    let (retired, other) = (ids[0], ids[1]);
    let folder = exchange.checkout.join(".engram").join("tasks");
    let file = folder.join(format!("{}.json", retired.0));
    let original = fs::read(&file).unwrap();
    let import = |dry_run: bool| {
        a.service
            .import_task_sync_data_folder(&exchange.data(), None, false, dry_run, None, now())
    };
    let pending = || {
        let (_, head) = a
            .service
            .task_sync_folder_head(&profile(), now())
            .unwrap()
            .unwrap();
        head.pending.keys().copied().collect::<Vec<_>>()
    };
    // Another host removes the task, and this host imports the removal.
    fs::remove_file(&file).unwrap();
    exchange.commit("Remove a task elsewhere");
    assert_eq!(import(false).unwrap().route, "incremental");
    let retired_here = a.holds(retired);
    // Its file returns; this host refuses the return of a retired id and
    // holds it.
    fs::write(&file, &original).unwrap();
    exchange.commit("Return the removed task");
    let receipt = import(false).unwrap();
    let decided = receipt
        .result
        .tasks
        .iter()
        .find(|task| task.task == retired)
        .map(|task| (task.status, task.reason.clone()));
    assert_eq!(decided, Some((Refused, Some("id_collision".to_owned()))));
    assert_eq!(pending(), vec![retired]);
    // The file leaves again beside an unrelated edit: the removal of a task
    // already removed changes nothing, so the edit applies and nothing stays
    // held, in a dry run, the import and a retry.
    fs::remove_file(&file).unwrap();
    let mut edited = exchange
        .tree
        .tasks
        .iter()
        .find(|task| task.id == other)
        .unwrap()
        .clone();
    edited.title = "edited elsewhere".into();
    fs::write(
        folder.join(format!("{}.json", other.0)),
        crate::task_sync::pretty(&edited).unwrap(),
    )
    .unwrap();
    exchange.commit("Remove the returned task again and edit another");
    assert_eq!(import(true).unwrap().route, "incremental");
    assert_eq!(import(false).unwrap().route, "incremental");
    assert_eq!(a.title(other), "edited elsewhere");
    assert_eq!(pending(), Vec::<crate::WorkId>::new());
    assert_eq!(a.holds(retired), retired_here);
    assert_eq!(import(false).unwrap().route, "unchanged");
}

#[test]
fn projection_repair_derives_the_folder_heads_feed_coverage_and_custody_index_a_store_never_held() {
    use crate::storage::{SqliteStore, StoreError};
    use rusqlite::Connection;
    let exchange = Exchange::new(3);
    exchange.a.rename(&exchange.ids()[..1], "edited on A");
    exchange
        .a
        .service
        .export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now())
        .unwrap();
    // A consistent copy of a store that recorded folder heads and feed
    // coverage, then without those structures, as a store written before
    // they existed holds none of them.
    let database = exchange.a.database().with_file_name("never-held.db");
    Connection::open(exchange.a.database())
        .unwrap()
        .execute("VACUUM INTO ?1", [database.to_str().unwrap()])
        .unwrap();
    let rows = |connection: &Connection| {
        let heads = connection
            .prepare(
                "SELECT project_id, profile_key, record_id FROM work_task_sync_folder_heads
                 ORDER BY project_id, profile_key",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<Result<Vec<(String, String, String)>, _>>()
            .unwrap();
        let coverage = connection
            .prepare(
                "SELECT project_id, position FROM work_project_feed_coverage
                 ORDER BY project_id, position",
            )
            .unwrap()
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
            .unwrap()
            .collect::<Result<Vec<(String, i64)>, _>>()
            .unwrap();
        (heads, coverage)
    };
    let index = |connection: &Connection| {
        connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_schema
                 WHERE type = 'index' AND name = 'objects_task_sync_custody_checkpoint'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .unwrap()
    };
    let before = {
        let connection = Connection::open(&database).unwrap();
        let before = rows(&connection);
        assert!(!before.0.is_empty() && !before.1.is_empty(), "{before:?}");
        assert_eq!(index(&connection), 1);
        connection
            .execute_batch(
                "DROP TABLE work_task_sync_folder_heads;
                 DROP TABLE work_project_feed_coverage;
                 DROP INDEX objects_task_sync_custody_checkpoint;",
            )
            .unwrap();
        before
    };
    // Such a store is one projection repair away from this build, never a
    // store of a different build.
    let Err(error) = SqliteStore::open(&database) else {
        panic!("a store without the projections opened without repair");
    };
    assert!(
        matches!(&error, StoreError::InvalidWorkProjection(message) if message.contains("--repair-projections")),
        "{error}"
    );
    let report = SqliteStore::repair_rebuildable_projections(&database).unwrap();
    assert!(report.is_healthy(), "{report:?}");
    drop(SqliteStore::open(&database).unwrap());
    let connection = Connection::open(&database).unwrap();
    assert_eq!(rows(&connection), before);
    assert_eq!(index(&connection), 1);
}

#[test]
fn an_incremental_export_under_another_alias_refuses_and_names_the_rebuild() {
    let exchange = Exchange::new(3);
    let ids = exchange.ids();
    let aliased = |alias: Option<&str>| crate::task_sync::progress::ExportProfile {
        origin_machine: alias.map(str::to_owned),
        ..profile()
    };
    exchange.a.rename(&ids[..1], "first");
    exchange
        .a
        .service
        .export_task_sync_data_folder(
            &exchange.data(),
            &aliased(Some("host-a")),
            false,
            None,
            now(),
        )
        .unwrap();
    exchange.commit("Export under host-a");
    exchange.a.rename(&ids[1..2], "second");
    for other in [None, Some("host-b")] {
        let refusal = exchange
            .a
            .service
            .export_task_sync_data_folder(&exchange.data(), &aliased(other), false, None, now())
            .unwrap_err();
        assert!(format!("{refusal:#}").contains("--rebuild"), "{refusal:#}");
    }
    let receipt = exchange
        .a
        .service
        .export_task_sync_data_folder(
            &exchange.data(),
            &aliased(Some("host-a")),
            false,
            None,
            now(),
        )
        .unwrap();
    assert_eq!(receipt.route, "incremental");
}

#[test]
fn a_folder_edit_made_after_an_export_was_planned_is_never_overwritten() {
    let exchange = Exchange::new(2);
    let ids = exchange.ids();
    let data = exchange.data();
    let folder = exchange.checkout.join(".engram");
    let relative = format!("tasks/{}.json", ids[0].0);
    let target = folder.join(&relative);
    let planned = fs::read(&target).unwrap();
    let write = FolderWrite {
        head: serde_json::json!({}),
        files: BTreeMap::from([(relative.clone(), Some("{}\n".to_owned()))]),
        before: preimages(&folder, std::iter::once(&relative)),
    };
    write.retain(&data).unwrap();
    // An editor saves the file between the plan and its publication.
    let edited = [planned.as_slice(), b"\n"].concat();
    fs::write(&target, &edited).unwrap();
    let refusal = write.publish(&data).unwrap_err();
    assert!(
        format!("{refusal:#}").contains("changed after this export was planned"),
        "{refusal:#}"
    );
    assert_eq!(fs::read(&target).unwrap(), edited);
    assert!(FolderWrite::pending(&data).unwrap().is_some());
}

#[test]
fn an_interrupted_export_whose_starting_point_was_discarded_is_superseded_by_a_rebuild() {
    let exchange = Exchange::new(3);
    let ids = exchange.ids();
    let a = &exchange.a;
    let data = exchange.data();
    a.rename(&ids[..1], "edited on A");
    a.service
        .export_task_sync_data_folder(&data, &profile(), false, None, now())
        .unwrap();
    let folder = exchange.checkout.join(".engram");
    let edited = format!("tasks/{}.json", ids[0].0);
    let files = [edited.clone(), "project.json".to_owned()]
        .into_iter()
        .map(|path| {
            let bytes = fs::read_to_string(folder.join(&path)).unwrap();
            (path, Some(bytes))
        })
        .collect::<BTreeMap<_, _>>();
    let (_, head) = a
        .service
        .task_sync_folder_head(&profile(), now())
        .unwrap()
        .unwrap();
    // The export started from an earlier uncommitted export's bytes, which
    // were discarded since: the folder holds neither its preimage nor its
    // output, and nothing can bring that preimage back.
    exchange.git(&["restore", "--", ".engram"]);
    let mut before = preimages(&folder, files.keys());
    before.insert(
        edited.clone(),
        Some("an earlier export's discarded bytes".into()),
    );
    FolderWrite {
        head: serde_json::to_value(&head).unwrap(),
        files,
        before,
    }
    .retain(&data)
    .unwrap();
    let refusal = a
        .service
        .export_task_sync_data_folder(&data, &profile(), false, None, now())
        .unwrap_err();
    assert!(
        format!("{refusal:#}").contains("export --rebuild"),
        "{refusal:#}"
    );
    let receipt = a
        .service
        .export_task_sync_data_folder(&data, &profile(), true, None, now())
        .unwrap();
    assert_eq!(receipt.route, "rebuild");
    assert!(FolderWrite::pending(&data).unwrap().is_none());
    assert!(
        fs::read_to_string(folder.join(&edited))
            .unwrap()
            .contains("edited on A")
    );
}

#[test]
fn a_task_file_changed_after_planning_read_it_refuses_the_export_before_any_write() {
    let exchange = Exchange::new(2);
    let ids = exchange.ids();
    let a = &exchange.a;
    let data = exchange.data();
    let (_, enrolled) = a
        .service
        .task_sync_folder_head(&profile(), now())
        .unwrap()
        .unwrap();
    a.rename(&ids[..1], "edited on A");
    let target = exchange
        .checkout
        .join(".engram")
        .join(format!("tasks/{}.json", ids[0].0));
    let merged = [fs::read(&target).unwrap().as_slice(), b"\n"].concat();
    // A merge into the folder lands after planning read this task's file.
    let (hook_target, hook_bytes) = (target.clone(), merged.clone());
    super::task_sync_folder_export::AFTER_EXPORT_PLAN.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            fs::write(hook_target, hook_bytes).unwrap();
        }));
    });
    let refusal = a
        .service
        .export_task_sync_data_folder(&data, &profile(), false, None, now())
        .unwrap_err();
    assert!(
        format!("{refusal:#}").contains("changed after this export was planned"),
        "{refusal:#}"
    );
    assert_eq!(fs::read(&target).unwrap(), merged);
    assert!(FolderWrite::pending(&data).unwrap().is_none());
    let (_, head) = a
        .service
        .task_sync_folder_head(&profile(), now())
        .unwrap()
        .unwrap();
    assert_eq!(
        head, enrolled,
        "no head is recorded for an export not written"
    );
}

#[test]
fn a_task_file_changed_after_a_complete_export_was_planned_is_never_overwritten() {
    let exchange = Exchange::new(3);
    let ids = exchange.ids();
    let a = &exchange.a;
    let data = exchange.data();
    let (_, enrolled) = a
        .service
        .task_sync_folder_head(&profile(), now())
        .unwrap()
        .unwrap();
    a.rename(&ids[..1], "edited on A");
    // A task the rebuild rewrites unchanged is saved by an editor after the
    // complete folder was planned.
    let target = exchange
        .checkout
        .join(".engram")
        .join(format!("tasks/{}.json", ids[1].0));
    let edited = [fs::read(&target).unwrap().as_slice(), b"\n"].concat();
    let (hook_target, hook_bytes) = (target.clone(), edited.clone());
    super::task_sync_folder_export::AFTER_EXPORT_PLAN.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            fs::write(hook_target, hook_bytes).unwrap();
        }));
    });
    let refusal = a
        .service
        .export_task_sync_data_folder(&data, &profile(), true, None, now())
        .unwrap_err();
    assert!(
        format!("{refusal:#}").contains("changed after this export was planned"),
        "{refusal:#}"
    );
    assert_eq!(fs::read(&target).unwrap(), edited);
    assert!(FolderWrite::pending(&data).unwrap().is_none());
    let (_, head) = a
        .service
        .task_sync_folder_head(&profile(), now())
        .unwrap()
        .unwrap();
    assert_eq!(
        head, enrolled,
        "no head is recorded for an export not written"
    );
}

#[test]
fn a_task_published_here_and_removed_elsewhere_is_held_by_its_return_import_not_republished() {
    let exchange = Exchange::new(2);
    let a = &exchange.a;
    let added = a.add("added on A");
    let receipt = a
        .service
        .export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now())
        .unwrap();
    assert_eq!((receipt.route, receipt.written), ("incremental", 1));
    exchange.commit("Add a task on A");
    // Another host removes it in a merge resolution before A imports again,
    // so the task never reached A's import baseline.
    fs::remove_file(
        exchange
            .checkout
            .join(".engram/tasks")
            .join(format!("{}.json", added.0)),
    )
    .unwrap();
    exchange.commit("Remove the task A added");
    let (_, head) = a
        .service
        .task_sync_folder_head(&profile(), now())
        .unwrap()
        .unwrap();
    assert!(
        head.exported.contains_key(&added)
            && matches!(head.disk, crate::storage::FolderDisk::Written { .. }),
        "the export recorded the file it published: {:?} {:?}",
        head.disk,
        head.exported
    );
    let receipt = a
        .service
        .import_task_sync_data_folder(&exchange.data(), None, false, false, None, now())
        .unwrap();
    assert_eq!(receipt.route, "incremental");
    assert_removal_held(a, &exchange, added, &receipt.result);
}

/// The other host's removal of `task`, a task created here and so with a run
/// here, is refused by name and held: the task stays open here, the next
/// export does not publish it again over the removal, and the next import
/// decides it again rather than refusing the whole folder.
fn assert_removal_held(
    host: &super::task_sync_folder_counts::Host,
    exchange: &Exchange,
    task: crate::WorkId,
    result: &crate::storage::TaskSyncImportResult,
) {
    assert_refused_and_held(host, task, result);
    host.service
        .export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now())
        .unwrap();
    assert!(
        !exchange
            .checkout
            .join(".engram/tasks")
            .join(format!("{}.json", task.0))
            .exists(),
        "no export publishes the task again over the other host's removal"
    );
    let again = host
        .service
        .import_task_sync_data_folder(&exchange.data(), None, false, false, None, now())
        .unwrap();
    assert_eq!(again.route, "incremental");
    assert_refused_and_held(host, task, &again.result);
}

/// The import refused the other host's removal of `task` by name, and the
/// task stays open and held here.
fn assert_refused_and_held(
    host: &super::task_sync_folder_counts::Host,
    task: crate::WorkId,
    result: &crate::storage::TaskSyncImportResult,
) {
    assert!(
        result.tasks.iter().any(|entry| entry.task == task
            && entry.status == crate::storage::TaskSyncTaskStatus::Refused
            && entry.reason.as_deref() == Some("execution_history")),
        "the other host's removal is refused by name here: {result:?}"
    );
    assert_eq!(host.lifecycle(task), crate::WorkLifecycle::Open);
    let (_, head) = host
        .service
        .task_sync_folder_head(&profile(), now())
        .unwrap()
        .unwrap();
    assert!(head.pending.contains_key(&task), "{:?}", head.pending);
}

#[test]
fn a_task_read_back_unchanged_is_shared_so_a_later_removal_is_held() {
    let exchange = Exchange::new(2);
    let a = &exchange.a;
    let added = a.add("added on A");
    let receipt = a
        .service
        .export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now())
        .unwrap();
    assert_eq!((receipt.route, receipt.written), ("incremental", 1));
    exchange.commit("Add a task on A");
    // A imports its own publication back unchanged before anyone removes it.
    let receipt = a
        .service
        .import_task_sync_data_folder(&exchange.data(), None, false, false, None, now())
        .unwrap();
    assert_eq!(receipt.route, "incremental");
    // Another host removes it afterwards; A's next import decides the
    // removal as a member's, rather than refusing the whole folder.
    fs::remove_file(
        exchange
            .checkout
            .join(".engram/tasks")
            .join(format!("{}.json", added.0)),
    )
    .unwrap();
    exchange.commit("Remove the task A shared");
    let receipt = a
        .service
        .import_task_sync_data_folder(&exchange.data(), None, false, false, None, now())
        .unwrap();
    assert_eq!(receipt.route, "incremental");
    assert_removal_held(a, &exchange, added, &receipt.result);
}

#[test]
fn a_held_edit_of_a_task_never_published_is_not_taken_for_a_removal_once_discarded() {
    let exchange = Exchange::new(2);
    let a = &exchange.a;
    let added = a.add("added on A");
    a.claim(added);
    let receipt = a
        .service
        .export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now())
        .unwrap();
    assert_eq!((receipt.route, receipt.written), ("incremental", 1));
    // A's own export, never committed, kept as the basis of complete imports.
    let basis = crate::task_sync::directory::read_folder_basis(
        &exchange.checkout.join(".engram/project.json"),
    )
    .unwrap();
    let recorded = || {
        Some(crate::task_sync::directory::DestinationBasis::Recorded(
            Box::new(basis.clone()),
        ))
    };
    // An edit of that loose file is held: the task is claimed here.
    let file = exchange
        .checkout
        .join(".engram/tasks")
        .join(format!("{}.json", added.0));
    let mut edited =
        crate::task_sync::parse::<crate::task_sync::TaskDocument>(&fs::read(&file).unwrap())
            .unwrap();
    edited.title.push_str(" edited in the loose folder");
    fs::write(&file, crate::task_sync::pretty(&edited).unwrap()).unwrap();
    let receipt = a
        .service
        .import_task_sync_data_folder(
            &exchange.data(),
            recorded().as_ref(),
            false,
            false,
            None,
            now(),
        )
        .unwrap();
    assert_eq!(receipt.route, "complete");
    assert!(
        receipt
            .result
            .tasks
            .iter()
            .any(|task| task.task == added
                && task.status == crate::storage::TaskSyncTaskStatus::Held),
        "{:?}",
        receipt.result
    );
    // The loose folder is discarded: no commit ever held the task.
    fs::remove_file(&file).unwrap();
    exchange.git(&["checkout", "--", ".engram"]);
    let receipt = a
        .service
        .import_task_sync_data_folder(
            &exchange.data(),
            recorded().as_ref(),
            false,
            false,
            None,
            now(),
        )
        .unwrap();
    assert!(
        !receipt.result.tasks.iter().any(|task| task.task == added),
        "a task never published is no removal: {:?}",
        receipt.result
    );
    assert_eq!(a.lifecycle(added), crate::WorkLifecycle::Open);
    // It stays this host's own, owed to the next export.
    a.service
        .export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now())
        .unwrap();
    assert!(file.exists(), "the next export publishes the task");
}

#[test]
fn a_published_task_whose_incoming_edit_was_held_is_not_republished_over_a_later_removal() {
    let exchange = Exchange::new(2);
    let a = &exchange.a;
    let added = a.add("added on A");
    a.claim(added);
    let receipt = a
        .service
        .export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now())
        .unwrap();
    assert_eq!((receipt.route, receipt.written), ("incremental", 1));
    exchange.commit("Add a task on A");
    // Another host edits it while A holds a claim on it, so A's first import
    // after publishing it holds that edit instead of applying it.
    let file = exchange
        .checkout
        .join(".engram/tasks")
        .join(format!("{}.json", added.0));
    let mut edited =
        crate::task_sync::parse::<crate::task_sync::TaskDocument>(&fs::read(&file).unwrap())
            .unwrap();
    edited.title.push_str(" edited elsewhere");
    fs::write(&file, crate::task_sync::pretty(&edited).unwrap()).unwrap();
    exchange.commit("Edit the task A added");
    let receipt = a
        .service
        .import_task_sync_data_folder(&exchange.data(), None, false, false, None, now())
        .unwrap();
    assert_eq!(receipt.route, "incremental");
    assert!(
        receipt
            .result
            .tasks
            .iter()
            .any(|task| task.task == added
                && task.status == crate::storage::TaskSyncTaskStatus::Held),
        "{:?}",
        receipt.result
    );
    // Another host then removes it.
    fs::remove_file(&file).unwrap();
    exchange.commit("Remove the task A added");
    for _ in 0..2 {
        let receipt = a
            .service
            .import_task_sync_data_folder(&exchange.data(), None, false, false, None, now())
            .unwrap();
        assert_eq!(receipt.route, "incremental");
        assert!(
            receipt.result.tasks.iter().any(|task| task.task == added
                && task.status != crate::storage::TaskSyncTaskStatus::Applied),
            "the removal is decided as a published task's, not dropped: {:?}",
            receipt.result
        );
        assert_eq!(a.lifecycle(added), crate::WorkLifecycle::Open);
        a.service
            .export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now())
            .unwrap();
        assert!(
            !file.exists(),
            "no export publishes the task again over the other host's removal"
        );
    }
}

#[test]
fn a_removal_meeting_a_discarded_export_is_refused_and_recovers_without_a_rebuild() {
    let exchange = Exchange::new(2);
    let a = &exchange.a;
    let added = a.add("added on A");
    let receipt = a
        .service
        .export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now())
        .unwrap();
    assert_eq!((receipt.route, receipt.written), ("incremental", 1));
    exchange.commit("Add a task on A");
    let published = exchange.data().head_commit().unwrap().to_owned();
    // A edits it and exports again, but that export is discarded before any
    // commit holds it.
    a.rename(&[added], "edited on A");
    let receipt = a
        .service
        .export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now())
        .unwrap();
    assert_eq!((receipt.route, receipt.written), ("incremental", 1));
    exchange.git(&["checkout", "--", ".engram"]);
    let (_, written) = a
        .service
        .task_sync_folder_head(&profile(), now())
        .unwrap()
        .unwrap();
    // Another host removes it.
    let file = exchange
        .checkout
        .join(".engram/tasks")
        .join(format!("{}.json", added.0));
    fs::remove_file(&file).unwrap();
    exchange.commit("Remove the task A added");
    // The task was published, so the removal meets A's discarded edit, which
    // is still owed to the next export: the import names the conflict and the
    // commit that export was written onto, dry run or not, and records nothing.
    for dry_run in [true, false, false] {
        let refusal = a
            .service
            .import_task_sync_data_folder(&exchange.data(), None, false, dry_run, None, now())
            .unwrap_err();
        let message = format!("{refusal:#}");
        assert!(
            message.contains("nothing is overwritten")
                && message.contains(&added.0.to_string())
                && message.contains(&published),
            "{message}"
        );
        let (_, head) = a
            .service
            .task_sync_folder_head(&profile(), now())
            .unwrap()
            .unwrap();
        assert_eq!(head, written, "a refused import records no head");
    }
    assert_eq!(a.lifecycle(added), crate::WorkLifecycle::Open);
    // The exit the refusal names, with no rebuild: set the other host's change
    // aside at that commit, import, which owes the discarded edit again, and
    // export it.
    exchange.git(&[
        "restore",
        &format!("--source={published}"),
        "--staged",
        "--worktree",
        "--",
        ".engram/tasks",
    ]);
    exchange.commit("Set the other host's task changes aside");
    let receipt = a
        .service
        .import_task_sync_data_folder(&exchange.data(), None, false, false, None, now())
        .unwrap();
    assert_eq!(receipt.route, "incremental");
    let receipt = a
        .service
        .export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now())
        .unwrap();
    assert_eq!((receipt.route, receipt.written), ("incremental", 1));
    let edited =
        crate::task_sync::parse::<crate::task_sync::TaskDocument>(&fs::read(&file).unwrap())
            .unwrap();
    assert!(edited.title.contains("edited on A"), "{}", edited.title);
    exchange.commit("Publish the discarded edit again");
    // Reverting the set-aside conflicts on the task; the agent merge keeps the
    // other host's removal, which A's next import decides as a member's.
    fs::remove_file(&file).unwrap();
    exchange.commit("Revert the set-aside, keeping the other host's removal");
    let receipt = a
        .service
        .import_task_sync_data_folder(&exchange.data(), None, false, false, None, now())
        .unwrap();
    assert_eq!(receipt.route, "incremental");
    assert_removal_held(a, &exchange, added, &receipt.result);
}

#[test]
fn an_export_discarded_after_an_unimported_commit_is_owed_again_by_the_next_import() {
    let exchange = Exchange::new(2);
    let a = &exchange.a;
    let added = a.add("added on A");
    let receipt = a
        .service
        .export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now())
        .unwrap();
    assert_eq!((receipt.route, receipt.written), ("incremental", 1));
    exchange.commit("Add a task on A");
    // A edits it and exports again onto that commit, before importing it,
    // and the second export is discarded.
    a.rename(&[added], "edited on A");
    let receipt = a
        .service
        .export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now())
        .unwrap();
    assert_eq!((receipt.route, receipt.written), ("incremental", 1));
    exchange.git(&["checkout", "--", ".engram"]);
    // The fact the import's rule rests on: the commit the discarded export
    // was written onto holds exactly the inventory that export recorded.
    let (_, head) = a
        .service
        .task_sync_folder_head(&profile(), now())
        .unwrap()
        .unwrap();
    let crate::storage::FolderDisk::Written { from, .. } = &head.disk else {
        panic!("the export recorded its write: {:?}", head.disk);
    };
    let on_disk = crate::task_sync::directory::read_folder(&exchange.checkout.join(".engram"))
        .unwrap()
        .tree
        .tasks
        .iter()
        .try_fold(
            crate::task_sync::inventory::InventoryRoot::ZERO,
            crate::task_sync::inventory::InventoryRoot::including,
        )
        .unwrap();
    assert_eq!(from.inventory_root, on_disk);
    assert_eq!(Some(from.commit.as_str()), exchange.data().head_commit());
    // The export names the import as the way on; the import reads A's own
    // published version back, owes the discarded edit, and the export writes it.
    let refusal = a
        .service
        .export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now())
        .unwrap_err();
    assert!(
        format!("{refusal:#}").contains("engram task-sync import"),
        "{refusal:#}"
    );
    for dry_run in [true, false] {
        let receipt = a
            .service
            .import_task_sync_data_folder(&exchange.data(), None, false, dry_run, None, now())
            .unwrap();
        assert_eq!(receipt.route, "incremental");
        assert!(
            !receipt.result.tasks.iter().any(|task| task.task == added),
            "A's own version is no change: {:?}",
            receipt.result
        );
    }
    let receipt = a
        .service
        .export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now())
        .unwrap();
    assert_eq!((receipt.route, receipt.written), ("incremental", 1));
    let file = exchange
        .checkout
        .join(".engram/tasks")
        .join(format!("{}.json", added.0));
    let edited =
        crate::task_sync::parse::<crate::task_sync::TaskDocument>(&fs::read(&file).unwrap())
            .unwrap();
    assert!(edited.title.contains("edited on A"), "{}", edited.title);
}

#[test]
fn another_hosts_revert_of_a_task_whose_export_was_discarded_is_never_overwritten() {
    let exchange = Exchange::new(2);
    let ids = exchange.ids();
    let a = &exchange.a;
    let file = exchange
        .checkout
        .join(".engram/tasks")
        .join(format!("{}.json", ids[0].0));
    let original = fs::read(&file).unwrap();
    // A edits a shared task, exports and commits it, then edits it again and
    // discards that export.
    a.rename(&ids[..1], "edited on A");
    let receipt = a
        .service
        .export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now())
        .unwrap();
    assert_eq!((receipt.route, receipt.written), ("incremental", 1));
    exchange.commit("Edit a task on A");
    let published = exchange.data().head_commit().unwrap().to_owned();
    a.rename(&ids[..1], "again");
    let receipt = a
        .service
        .export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now())
        .unwrap();
    assert_eq!((receipt.route, receipt.written), ("incremental", 1));
    exchange.git(&["checkout", "--", ".engram"]);
    // Another host returns the task to exactly its bytes before A's edit,
    // which the diff from A's last import cannot show.
    fs::write(&file, &original).unwrap();
    exchange.commit("Revert A's edit");
    let refusal = a
        .service
        .import_task_sync_data_folder(&exchange.data(), None, false, false, None, now())
        .unwrap_err();
    let message = format!("{refusal:#}");
    assert!(
        message.contains("nothing is overwritten")
            && message.contains(&ids[0].0.to_string())
            && message.contains(&published),
        "{message}"
    );
    // Nor does an export write the discarded edit over the revert.
    assert!(
        a.service
            .export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now())
            .is_err()
    );
    assert_eq!(fs::read(&file).unwrap(), original);
}

#[test]
fn a_folder_whose_path_listings_outgrow_a_general_query_is_still_read() {
    use std::io::Write;
    let exchange = Exchange::new(1);
    // Index 20,000 task paths without writing their files: both the index
    // listing and the status listing then name each of them, together well
    // over the 1 MiB a general Git query may return.
    let git = |args: &[&str], input: Option<&[u8]>| {
        let mut child = std::process::Command::new("git")
            .arg("-C")
            .arg(&exchange.checkout)
            .args(args)
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.unwrap_or_default())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "git {args:?}");
        String::from_utf8(output.stdout).unwrap()
    };
    let empty = git(&["hash-object", "-w", "--stdin"], None);
    let paths = (0..20_000)
        .map(|_| format!(".engram/tasks/{}.json", uuid::Uuid::new_v4()))
        .collect::<Vec<_>>();
    let entries = paths.iter().fold(String::new(), |mut entries, path| {
        entries.push_str("100644 ");
        entries.push_str(empty.trim());
        entries.push('\t');
        entries.push_str(path);
        entries.push('\n');
        entries
    });
    git(&["update-index", "--index-info"], Some(entries.as_bytes()));
    let changes = exchange.data().folder_changes().unwrap();
    assert!(paths.iter().all(|path| changes.contains(path)));
}

#[test]
fn an_export_over_a_reformatting_commit_records_the_size_its_files_hold() {
    let exchange = Exchange::new(4);
    let ids = exchange.ids();
    let folder = exchange.checkout.join(".engram");
    // Another host's commit reformats one task file without changing it.
    let file = folder.join(format!("tasks/{}.json", ids[0].0));
    let text = fs::read_to_string(&file).unwrap();
    fs::write(
        &file,
        text.replacen('{', &format!("{{{}", " ".repeat(64 * 1024)), 1),
    )
    .unwrap();
    exchange.commit("Reformat one task file");
    // This host exports another task over that commit without importing it.
    exchange.a.rename(&ids[1..2], "renamed here");
    let receipt = exchange
        .a
        .service
        .export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now())
        .unwrap();
    assert_eq!(receipt.route, "incremental");
    let (_, head) = exchange
        .a
        .service
        .task_sync_folder_head(&profile(), now())
        .unwrap()
        .unwrap();
    assert_eq!(
        head.task_bytes,
        super::task_sync_folder_export::stored_task_bytes(&folder).unwrap()
    );
}

#[test]
fn the_recorded_folder_size_is_what_its_files_hold_however_they_are_formatted() {
    let exchange = Exchange::new(4);
    let ids = exchange.ids();
    let folder = exchange.checkout.join(".engram");
    // An agent merge reformats one task file: the same document, valid JSON,
    // many more bytes than an export would write.
    let file = folder.join(format!("tasks/{}.json", ids[0].0));
    let text = fs::read_to_string(&file).unwrap();
    fs::write(
        &file,
        text.replacen('{', &format!("{{{}", " ".repeat(64 * 1024)), 1),
    )
    .unwrap();
    exchange.commit("Reformat one task file");
    let data = exchange.data();
    exchange
        .b
        .service
        .import_task_sync_data_folder(&data, None, false, false, None, now())
        .unwrap();
    let (_, head) = exchange
        .b
        .service
        .task_sync_folder_head(&profile(), now())
        .unwrap()
        .unwrap();
    let stored = super::task_sync_folder_export::stored_task_bytes(&folder).unwrap();
    assert_eq!(head.task_bytes, stored);
    assert_eq!(
        head.source.as_ref().map(|source| source.task_bytes),
        Some(stored)
    );
}
