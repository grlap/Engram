//! One export or import at a time on a data worktree, and an export
//! interrupted after recording its head: finished by the next export or
//! import over the folder it was planned against, refused over a folder that
//! changed since. Unowned journals are left for explicit operator recovery.

use std::{collections::BTreeMap, fs};

use super::task_sync_folder_counts::{Exchange, counted_queries, profile, run};
use crate::task_sync::directory::{ExportParent, FolderWrite};

fn now() -> chrono::DateTime<chrono::Utc> {
    chrono::Utc::now()
}

fn owned_write(
    service: &super::LocalWorkService,
    data: &crate::task_sync::data_worktree::DataWorktree,
) -> Option<FolderWrite> {
    FolderWrite::pending(data, Some(&service.folder_write_owner().unwrap())).unwrap()
}

fn assert_basis_refusal(error: &anyhow::Error, remedy: &str) {
    assert!(
        matches!(
            error.downcast_ref::<crate::storage::StoreError>(),
            Some(crate::storage::StoreError::TaskSyncRefused {
                refusal: crate::task_sync::TaskSyncRefusal::IncrementalBasisUnavailable,
                ..
            })
        ),
        "{error:#}"
    );
    assert!(format!("{error:#}").contains(remedy), "{error:#}");
}

fn assert_writer_tip_move_keeps_publication<T>(
    exchange: &Exchange,
    data: &crate::task_sync::data_worktree::DataWorktree,
    apply: impl FnOnce() -> anyhow::Result<T>,
) {
    let service = &exchange.a.service;
    let proof = owned_write(service, data).unwrap();
    let head = service.task_sync_folder_head(&profile(), now()).unwrap();
    let journal = proof.retained_bytes(data).unwrap();
    let folder = data.checkout.join(".engram");
    let files = preimages(&folder, proof.files.keys());
    let checkout = data.checkout.clone();
    let mut moved = false;
    let refused = crate::storage::with_writer_admission_test_policy(
        None,
        move || {
            if !moved {
                run(
                    &checkout,
                    &[
                        "commit",
                        "--allow-empty",
                        "-m",
                        "Move during writer admission",
                    ],
                );
                moved = true;
            }
        },
        apply,
    )
    .err()
    .expect("a moved tip refuses the write");
    assert_basis_refusal(&refused, "HEAD changed since observation");
    assert!(refused.to_string().contains("rerun import at the new tip"));
    assert_ne!(exchange.data().head_commit(), data.head_commit());
    assert_eq!(
        service.task_sync_folder_head(&profile(), now()).unwrap(),
        head
    );
    assert_eq!(proof.retained_bytes(data).unwrap(), journal);
    assert_eq!(preimages(&folder, proof.files.keys()), files);
}

fn assert_rebuild_basis_required(error: &anyhow::Error) {
    assert!(
        matches!(
            error.downcast_ref::<crate::storage::StoreError>(),
            Some(crate::storage::StoreError::TaskSyncRefused {
                refusal: crate::task_sync::TaskSyncRefusal::CommitContextRequired,
                ..
            })
        ),
        "{error:#}"
    );
    assert!(
        error
            .to_string()
            .contains("--basis OWN_EXPORT/project.json")
    );
}

fn combined_export() -> super::FolderMemoryExport<'static> {
    static SELECTED: BTreeMap<String, crate::task_sync::memories::KeyHistory> = BTreeMap::new();
    static RESOLUTIONS: BTreeMap<String, crate::task_sync::memories::ImportOptions> =
        BTreeMap::new();
    super::FolderMemoryExport {
        selected: &SELECTED,
        resolutions: &RESOLUTIONS,
        recovery_basis_matches: None,
        admit: Box::new(|_| {
            Ok(super::FolderMemoryAdmission {
                bytes: 0,
                files: BTreeMap::new(),
                clear_capability_staging: None,
                publish: Box::new(|_| Ok(())),
            })
        }),
    }
}

/// A path spelled as Git accepts it in configuration: Git cannot open a
/// Windows verbatim (`\\?\`) path as an exclude file.
fn git_path(path: &std::path::Path) -> String {
    crate::task_sync::git::plain_path(path)
        .and_then(|plain| plain.to_str().map(str::to_owned))
        .expect("test paths have a plain Git spelling")
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
fn an_owned_interrupted_export_refuses_an_edit_and_finishes_at_its_parent() {
    let exchange = Exchange::new(4);
    let ids = exchange.ids();
    let a = &exchange.a;
    let data = exchange.data();
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
    let mut interrupted = owned_write(&a.service, &data).unwrap();
    let exported = interrupted.files[&edited].clone();
    exchange.git(&["restore", "--", ".engram"]);
    interrupted.published = false;
    interrupted.retain(&data).unwrap();

    let head = a.service.task_sync_folder_head(&profile(), now()).unwrap();
    let journal = interrupted.retained_bytes(&data).unwrap();
    let preimages_before = preimages(&folder, interrupted.files.keys());
    let mut combined = combined_export();
    combined.admit = Box::new(|_| panic!("a combined command must refuse before admission"));
    let refused = a
        .service
        .export_task_sync_data_folder(&data, &profile(), false, Some(combined), now())
        .unwrap_err();
    assert_basis_refusal(&refused, "without --include-memories");
    assert_eq!(interrupted.retained_bytes(&data).unwrap(), journal);
    assert_eq!(
        preimages(&folder, interrupted.files.keys()),
        preimages_before
    );
    assert_eq!(
        a.service.task_sync_folder_head(&profile(), now()).unwrap(),
        head
    );

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
        format!("{refusal:#}").contains("changed after this export was planned"),
        "{refusal:#}"
    );
    assert_eq!(fs::read(&target).unwrap(), b"{}");
    assert_eq!(owned_write(&a.service, &data), Some(interrupted));

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
    assert!(owned_write(&a.service, &data).unwrap().published);
    let parent = data.head_commit().unwrap().to_owned();
    exchange.commit("Edit one task on A");
    let data = exchange.data();
    let completed = owned_write(&a.service, &data).unwrap();
    let mut unfinished = completed.clone();
    unfinished.published = false;
    unfinished.retain(&data).unwrap();
    let refused = a
        .service
        .export_task_sync_data_folder(&data, &profile(), false, None, now())
        .unwrap_err();
    assert_basis_refusal(
        &refused,
        &format!("restore the original basis at data commit {parent}"),
    );
    assert_eq!(owned_write(&a.service, &data).as_ref(), Some(&unfinished));
    completed.retain(&data).unwrap();
    let receipt = a
        .service
        .import_task_sync_data_folder(&data, None, false, false, None, now())
        .unwrap();
    assert_eq!(receipt.route, "unchanged");
    assert!(owned_write(&a.service, &data).is_none());
}

#[test]
fn an_existing_envelope_conflict_refuses_replay_before_any_task_effect() {
    let exchange = Exchange::unborn(1);
    let service = &exchange.a.service;
    crate::task_sync::directory::write_data_folder(&exchange.data(), &exchange.tree, &profile())
        .unwrap();
    exchange.commit("Publish another store's planning");
    let added = exchange.a.add("replacement task");
    let data = exchange.data();
    service
        .export_task_sync_data_folder(&data, &profile(), true, None, now())
        .unwrap();

    let folder = exchange.checkout.join(".engram");
    let replacement_path = format!("tasks/{}.json", added.0);
    let removed_path = format!("tasks/{}.json", exchange.ids()[0].0);
    let replacement = folder.join(&replacement_path);
    let removed = folder.join(&removed_path);
    let envelope = folder.join("project.json");
    let mut interrupted = owned_write(service, &data).unwrap();
    assert_eq!(interrupted.files[&removed_path], None);
    assert!(interrupted.files[&replacement_path].is_some());
    // Recreate the state just after recording the write, with a conflicting
    // envelope already on disk when recovery begins.
    exchange.git(&["restore", "--worktree", "--", ".engram"]);
    fs::remove_file(&replacement).unwrap();
    interrupted.published = false;
    interrupted.retain(&data).unwrap();
    let conflict = [fs::read(&envelope).unwrap(), b"\n ".to_vec()].concat();
    fs::write(&envelope, &conflict).unwrap();
    let original = fs::read(&removed).unwrap();
    let head = service.task_sync_folder_head(&profile(), now()).unwrap();

    for export in [true, false] {
        let error = if export {
            service
                .export_task_sync_data_folder(&data, &profile(), false, None, now())
                .unwrap_err()
        } else {
            service
                .import_task_sync_data_folder(
                    &data,
                    Some(&crate::task_sync::directory::DestinationBasis::Empty),
                    false,
                    false,
                    None,
                    now(),
                )
                .unwrap_err()
        };
        assert_basis_refusal(&error, "project.json changed after this export was planned");
        assert_eq!(fs::read(&removed).unwrap(), original);
        assert!(!replacement.exists());
        assert_eq!(fs::read(&envelope).unwrap(), conflict);
        assert_eq!(owned_write(service, &data).as_ref(), Some(&interrupted));
        assert_eq!(
            service.task_sync_folder_head(&profile(), now()).unwrap(),
            head
        );
    }
}

#[test]
fn identical_heads_in_two_stores_do_not_authorize_unowned_journal_replay() {
    let exchange = Exchange::unborn(0);
    let data = exchange.data();
    let a = &exchange.a.service;
    let b = &exchange.b.service;
    a.export_task_sync_data_folder(&data, &profile(), false, None, now())
        .unwrap();
    let (_, head) = a.task_sync_folder_head(&profile(), now()).unwrap().unwrap();
    b.record_task_sync_folder_head(&head, now()).unwrap();
    let before = b.task_sync_folder_head(&profile(), now()).unwrap();
    let mut unowned = owned_write(a, &data).unwrap();
    unowned.owner = None;
    unowned.parent = None;
    unowned.published = false;
    unowned.retain(&data).unwrap();
    let folder = exchange.checkout.join(".engram");
    let envelope = fs::read(folder.join("project.json")).unwrap();

    let export = b
        .export_task_sync_data_folder(&data, &profile(), false, None, now())
        .unwrap_err();
    let import = b
        .import_task_sync_data_folder(&data, None, false, false, None, now())
        .unwrap_err();
    for error in [export, import] {
        assert_basis_refusal(&error, "engram-task-sync-folder-write.json");
        assert!(format!("{error:#}").contains("originating store and build"));
    }
    assert_eq!(b.task_sync_folder_head(&profile(), now()).unwrap(), before);
    assert_eq!(fs::read(folder.join("project.json")).unwrap(), envelope);
    assert_eq!(
        FolderWrite::pending(&data, None).unwrap(),
        Some(unowned.clone())
    );

    exchange.b.add("new planning on the second store");
    b.export_task_sync_data_folder(&data, &profile(), true, None, now())
        .unwrap();
    assert_eq!(FolderWrite::pending(&data, None).unwrap(), Some(unowned));
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
    exchange.git(&["config", "core.excludesFile", &git_path(&excludes)]);
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
    exchange.git(&["config", "core.excludesFile", &git_path(&excludes)]);
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
    exchange.git(&["config", "core.excludesFile", &git_path(&excludes)]);
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
        owner: Some(exchange.a.service.folder_write_owner().unwrap()),
        head_id: None,
        parent: data
            .head_commit()
            .map(|commit| ExportParent::Commit(commit.to_owned())),
        commit_paths: None,
        published: false,
        memory_files: BTreeMap::new(),
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
    assert!(owned_write(&exchange.a.service, &data).is_some());
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
    FolderWrite::discard(&data, Some(&a.service.folder_write_owner().unwrap())).unwrap();
    FolderWrite {
        owner: Some(a.service.folder_write_owner().unwrap()),
        head_id: None,
        parent: data
            .head_commit()
            .map(|commit| ExportParent::Commit(commit.to_owned())),
        commit_paths: None,
        published: false,
        memory_files: BTreeMap::new(),
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
        format!("{refusal:#}").contains("changed after this export was planned"),
        "{refusal:#}"
    );
    let receipt = a
        .service
        .export_task_sync_data_folder(&data, &profile(), true, None, now())
        .unwrap();
    assert_eq!(receipt.route, "rebuild");
    assert!(owned_write(&a.service, &data).unwrap().published);
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
    assert!(owned_write(&a.service, &data).is_none());
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
fn an_edit_after_status_is_never_used_as_the_admitted_parent_preimage() {
    assert_edit_after_status_refuses(false);
}

#[test]
fn a_content_edit_after_status_refuses_on_a_crlf_checkout() {
    assert_edit_after_status_refuses(true);
}

fn checkout_crlf(exchange: &Exchange, paths: &[&str]) {
    exchange.git(&["config", "core.autocrlf", "true"]);
    for relative in paths {
        let path = exchange.checkout.join(relative);
        let lf = run(&exchange.checkout, &["show", &format!("HEAD:{relative}")]);
        let lf = std::str::from_utf8(&lf).unwrap();
        assert!(lf.contains('\n') && !lf.contains('\r'));
        let crlf = lf.replace('\n', "\r\n");
        fs::remove_file(&path).unwrap();
        exchange.git(&["checkout", "--", relative]);
        if fs::read(&path).unwrap() == lf.as_bytes() {
            // Some checkout environments leave LF in place.
            fs::write(&path, &crlf).unwrap();
        }
        assert_eq!(fs::read(path).unwrap(), crlf.as_bytes());
    }
    assert!(
        exchange
            .data()
            .folder_changes_admitted(false)
            .unwrap()
            .is_empty()
    );
}

#[test]
fn incremental_export_and_exact_confirmation_accept_clean_crlf_preimages() {
    let exchange = Exchange::new(2);
    let service = &exchange.a.service;
    let ids = exchange.ids();
    let selected = format!(".engram/tasks/{}.json", ids[0].0);
    let untouched = format!(".engram/tasks/{}.json", ids[1].0);
    let untouched_bytes = fs::metadata(exchange.checkout.join(&untouched))
        .unwrap()
        .len();
    assert!(!exchange.checkout.join(".engram/.gitattributes").exists());
    checkout_crlf(&exchange, &[".engram/project.json", &selected, &untouched]);
    let untouched_preimage = fs::read(exchange.checkout.join(&untouched)).unwrap();
    exchange.a.rename(&ids[..1], "renamed with CRLF checkout");

    let data = exchange.data();
    let exported = service
        .export_task_sync_data_folder(&data, &profile(), false, None, now())
        .unwrap();
    assert_eq!(
        (exported.route, exported.written, exported.removed),
        ("incremental", 1, 0)
    );
    let written = fs::read(exchange.checkout.join(&selected)).unwrap();
    let task: crate::task_sync::TaskDocument = crate::task_sync::parse(&written).unwrap();
    assert!(task.title.contains("renamed with CRLF checkout"));
    assert_eq!(
        exported.task_bytes,
        untouched_bytes + u64::try_from(written.len()).unwrap()
    );
    assert_eq!(
        fs::read(exchange.checkout.join(&untouched)).unwrap(),
        untouched_preimage
    );
    assert!(owned_write(service, &data).unwrap().published);

    exchange.commit("Publish the selected task from a CRLF checkout");
    let committed = exchange.data();
    let confirmed = service
        .export_task_sync_data_folder(&committed, &profile(), false, None, now())
        .unwrap();
    assert_eq!((confirmed.route, confirmed.written), ("unchanged", 0));
    assert_eq!(confirmed.task_bytes, exported.task_bytes);
    assert!(owned_write(service, &committed).is_none());
}

fn assert_edit_after_status_refuses(crlf: bool) {
    let exchange = Exchange::new(2);
    let service = &exchange.a.service;
    let ids = exchange.ids();
    let relative = format!(".engram/tasks/{}.json", ids[0].0);
    if crlf {
        checkout_crlf(&exchange, &[".engram/project.json", &relative]);
    }
    let data = exchange.data();
    let head = service.task_sync_folder_head(&profile(), now()).unwrap();
    let folder = data.checkout.join(".engram");
    let path = data.checkout.join(relative);
    let mut edited: crate::task_sync::TaskDocument =
        crate::task_sync::parse(&fs::read(&path).unwrap()).unwrap();
    edited.title.push_str(" — loose edit after status");
    let bytes = crate::task_sync::pretty(&edited).unwrap();
    let bytes = if crlf {
        String::from_utf8(bytes)
            .unwrap()
            .replace('\n', "\r\n")
            .into_bytes()
    } else {
        bytes
    };
    let envelope = fs::read(folder.join("project.json")).unwrap();
    exchange.a.rename(&ids[..1], "local store edit");
    let (edit_path, edit_bytes) = (path.clone(), bytes.clone());
    super::task_sync_folder_export::BEFORE_EXPORT_PREIMAGES.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            fs::write(edit_path, edit_bytes).unwrap();
        }));
    });

    let refusal = service
        .export_task_sync_data_folder(&data, &profile(), false, None, now())
        .unwrap_err();
    assert_basis_refusal(&refusal, "differs from the admitted commit");
    assert_basis_refusal(&refusal, "Commit and import the edit");
    assert_eq!(fs::read(path).unwrap(), bytes);
    assert_eq!(fs::read(folder.join("project.json")).unwrap(), envelope);
    assert_eq!(
        service.task_sync_folder_head(&profile(), now()).unwrap(),
        head
    );
    assert!(owned_write(service, &data).is_none());
}

#[test]
fn a_tip_move_during_file_publication_keeps_fresh_and_replayed_writes_unfinished() {
    for replay in [false, true] {
        let exchange = Exchange::new(2);
        let service = &exchange.a.service;
        let data = exchange.data();
        let ids = exchange.ids();
        let original = data.head_commit().unwrap();
        let export = || service.export_task_sync_data_folder(&data, &profile(), false, None, now());
        exchange.a.rename(&ids[..1], "exported local task");
        if replay {
            export().unwrap();
            exchange.git(&["restore", "--", ".engram/project.json", ".engram/tasks"]);
            let mut write = owned_write(service, &data).unwrap();
            write.published = false;
            write.retain(&data).unwrap();
        }
        let checkout = exchange.checkout.clone();
        let peer_path = format!(".engram/tasks/{}.json", ids[1].0);
        let owner = service.folder_write_owner().unwrap();
        let hook_data = data.clone();
        crate::task_sync::directory::BEFORE_EXPORT_COMPLETION.with(|hook| {
            *hook.borrow_mut() = Some(Box::new(move || {
                let write = FolderWrite::pending(&hook_data, Some(&owner))
                    .unwrap()
                    .unwrap();
                assert!(!write.published);
                let file = checkout.join(&peer_path);
                let mut task: crate::task_sync::TaskDocument =
                    crate::task_sync::parse(&fs::read(&file).unwrap()).unwrap();
                task.title.push_str(" — committed during publication");
                fs::write(&file, crate::task_sync::pretty(&task).unwrap()).unwrap();
                run(
                    &checkout,
                    &["commit", "-m", "Commit another task", "--", &peer_path],
                );
            }));
        });

        let refusal = export().unwrap_err();
        assert_basis_refusal(
            &refusal,
            &format!("restore the original basis at data commit {original}"),
        );
        let moved = exchange.data();
        assert_ne!(moved.head_commit(), Some(original));
        let unfinished = owned_write(service, &moved).unwrap();
        assert!(!unfinished.published);
        assert_eq!(
            unfinished.parent,
            Some(ExportParent::Commit(original.to_owned()))
        );
        let relative = format!("tasks/{}.json", ids[0].0);
        assert_eq!(
            fs::read_to_string(moved.checkout.join(".engram").join(&relative)).unwrap(),
            *unfinished.files[&relative].as_ref().unwrap()
        );
        let (id, head) = service
            .task_sync_folder_head(&profile(), now())
            .unwrap()
            .unwrap();
        assert_eq!(unfinished.head_id, Some(id));
        assert_eq!(unfinished.head, serde_json::to_value(head).unwrap());
    }
}

#[test]
fn a_git_commit_during_planning_leaves_the_head_and_journal_untouched() {
    let exchange = Exchange::new(2);
    let a = &exchange.a;
    let data = exchange.data();
    let ids = exchange.ids();
    let before = a.service.task_sync_folder_head(&profile(), now()).unwrap();
    let file = exchange
        .checkout
        .join(format!(".engram/tasks/{}.json", ids[0].0));
    let original = fs::read(&file).unwrap();
    a.rename(&ids[..1], "edit planned before HEAD moved");
    let checkout = exchange.checkout.clone();
    super::task_sync_folder_export::AFTER_EXPORT_PLAN.with(|hook| {
        *hook.borrow_mut() = Some(Box::new(move || {
            run(
                &checkout,
                &["commit", "--allow-empty", "-m", "Move the observed parent"],
            );
        }));
    });
    let error = a
        .service
        .export_task_sync_data_folder(&data, &profile(), false, None, now())
        .unwrap_err();
    assert_basis_refusal(&error, "HEAD changed since observation");
    let moved = exchange.data();
    assert_ne!(moved.head_commit(), data.head_commit());
    for expected in [
        data.head_commit().unwrap(),
        moved.head_commit().unwrap(),
        "rerun",
    ] {
        assert!(format!("{error:#}").contains(expected), "{error:#}");
    }
    assert_eq!(
        a.service.task_sync_folder_head(&profile(), now()).unwrap(),
        before
    );
    assert!(owned_write(&a.service, &data).is_none());
    assert_eq!(fs::read(file).unwrap(), original);
}

#[test]
fn a_combined_export_is_unfinished_until_the_memory_writer_succeeds() {
    let exchange = Exchange::new(1);
    let a = &exchange.a;
    let export = |memory| {
        a.service.export_task_sync_data_folder(
            &exchange.data(),
            &profile(),
            false,
            Some(memory),
            now(),
        )
    };
    export(combined_export()).unwrap();
    exchange.commit("Publish the memory-capable parent");
    export(combined_export()).unwrap();
    let excludes = exchange.data().git_directory.join("pending-excludes");
    fs::write(&excludes, "/.engram/memories/*/*.pending\n").unwrap();
    exchange.git(&["config", "core.excludesFile", &git_path(&excludes)]);
    let stage_path = format!(".engram/memories/key/head-{}.pending", uuid::Uuid::new_v4());
    let staging = exchange.checkout.join(&stage_path);
    let mut interrupted = combined_export();
    interrupted.recovery_basis_matches = Some(true);
    let stage = staging.clone();
    interrupted.admit = Box::new(move |_| {
        Ok(super::FolderMemoryAdmission {
            bytes: 0,
            files: BTreeMap::new(),
            clear_capability_staging: None,
            publish: Box::new(move |_| {
                fs::create_dir_all(stage.parent().unwrap())?;
                fs::write(stage, b"{")?;
                anyhow::bail!("stop between the task and memory writers")
            }),
        })
    });
    a.rename(&exchange.ids(), "combined task edit");
    let error = export(interrupted).unwrap_err();
    assert!(
        error
            .to_string()
            .contains("stop between the task and memory writers")
    );
    let data = exchange.data();
    let unfinished = owned_write(&a.service, &data).unwrap();
    assert!(!unfinished.published);
    let mut retry = combined_export();
    retry.recovery_basis_matches = Some(true);
    let error = export(retry).unwrap_err();
    assert_basis_refusal(
        &error,
        "export --rebuild --include-memories --memory-recovery-basis",
    );
    assert_eq!(owned_write(&a.service, &data).unwrap(), unfinished);
    assert_eq!(fs::read(&staging).unwrap(), b"{");
    exchange.git(&["check-ignore", "--", &stage_path]);
    exchange.commit("Commit only the task phase");
    let data = exchange.data();
    let before = a.service.task_sync_folder_head(&profile(), now()).unwrap();
    let journal = owned_write(&a.service, &data).unwrap();
    assert!(!journal.published);

    let error = export(combined_export()).unwrap_err();
    assert!(error.to_string().contains("--memory-recovery-basis"));
    assert_eq!(fs::read(&staging).unwrap(), b"{");
    assert_eq!(
        a.service.task_sync_folder_head(&profile(), now()).unwrap(),
        before
    );
    assert_eq!(owned_write(&a.service, &data).as_ref(), Some(&journal));

    fs::remove_file(staging).unwrap();
    assert_basis_refusal(
        &export(combined_export()).unwrap_err(),
        "restore the original basis",
    );
    assert_eq!(
        a.service.task_sync_folder_head(&profile(), now()).unwrap(),
        before
    );
    assert_eq!(owned_write(&a.service, &data).as_ref(), Some(&journal));
}

#[test]
fn exact_own_confirmation_adopts_a_committed_task_deletion() {
    use crate::storage::{FolderDisk, FolderHeadReason};
    use std::collections::BTreeSet;
    let exchange = Exchange::new(2);
    let service = &exchange.a.service;
    let data = exchange.data();
    let document = &exchange.tree.tasks[0];
    let relative = format!("tasks/{}.json", document.id.0);
    let folder = data.checkout.join(".engram");
    let file = folder.join(&relative);
    // Retain a deletion against an already admitted parent. Confirm through
    // ordinary import after the actual file write and commit.
    let (id, mut head) = service
        .task_sync_folder_head(&profile(), now())
        .unwrap()
        .unwrap();
    let from = head.import_basis().unwrap();
    head.previous = Some(id);
    head.reason = FolderHeadReason::Export;
    head.inventory_root = head.inventory_root.excluding(document).unwrap();
    head.task_count -= 1;
    head.task_bytes -= fs::metadata(&file).unwrap().len();
    head.disk = FolderDisk::Written {
        from,
        files: BTreeMap::from([(document.id, None)]),
    };
    let parent_paths = crate::task_sync::git::ExportParentPaths::read(
        &data.checkout,
        &exchange.tree.project.project_id,
        data.head_commit(),
        BTreeSet::from([relative.clone()]),
        std::time::Duration::from_secs(60),
    )
    .unwrap();
    assert!(!parent_paths.matches(&relative, None).unwrap());
    let paths = BTreeSet::from([relative.clone()]);
    let write = FolderWrite {
        owner: Some(service.folder_write_owner().unwrap()),
        head: serde_json::to_value(&head).unwrap(),
        head_id: None,
        before: preimages(&folder, [&relative].into_iter()),
        files: BTreeMap::from([(relative, None)]),
        parent: Some(ExportParent::Commit(data.head_commit().unwrap().to_owned())),
        commit_paths: Some(paths),
        published: false,
        memory_files: BTreeMap::new(),
    };
    write.retain(&data).unwrap();
    service.record_task_sync_folder_head(&head, now()).unwrap();
    write.publish(&data).unwrap();
    assert!(!file.exists());
    exchange.commit("Commit the exact task deletion");

    let data = exchange.data();
    let receipt = service
        .import_task_sync_data_folder(&data, None, false, false, None, now())
        .unwrap();
    assert_eq!((receipt.route, receipt.result.task_count), ("unchanged", 1));
    let (_, confirmed) = service
        .task_sync_folder_head(&profile(), now())
        .unwrap()
        .unwrap();
    assert_eq!(
        Some(confirmed.import_basis().unwrap().commit.as_str()),
        data.head_commit()
    );
    assert!(owned_write(service, &data).is_none());
}

#[test]
fn a_git_commit_after_memory_planning_refuses_before_creating_the_component() {
    use crate::task_sync::memories::directory as memory_directory;
    let exchange = Exchange::new(1);
    let service = &exchange.a.service;
    let export = service
        .export_task_sync_data_folder(
            &exchange.data(),
            &profile(),
            false,
            Some(combined_export()),
            now(),
        )
        .unwrap();
    exchange.commit("Commit the memory-capable envelope");
    let data = exchange.data();
    let folder = exchange.checkout.join(".engram");
    let envelope = fs::read(folder.join("project.json")).unwrap();
    let fence = memory_directory::publication_lock(&data).unwrap();
    let checkout = exchange.checkout.clone();
    memory_directory::BEFORE_MEMORY_WRITE.with(|hook| {
        hook.set(Some(Box::new(move || {
            run(
                &checkout,
                &[
                    "commit",
                    "--allow-empty",
                    "-m",
                    "Move after memory preflight",
                ],
            );
        })));
    });
    let error = memory_directory::write_admitted(
        &data,
        &BTreeMap::new(),
        &BTreeMap::new(),
        false,
        &fence,
        &exchange.tree.project.project_id,
        usize::try_from(export.task_bytes).unwrap(),
    )
    .unwrap_err();
    assert_basis_refusal(&error, "HEAD changed since observation");
    assert_ne!(exchange.data().head_commit(), data.head_commit());
    assert!(!folder.join("memories").exists());
    assert_eq!(fs::read(folder.join("project.json")).unwrap(), envelope);
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
    assert!(owned_write(&a.service, &data).is_none());
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
    // A confirms its own publication before anyone removes it.
    let receipt = a
        .service
        .import_task_sync_data_folder(&exchange.data(), None, false, false, None, now())
        .unwrap();
    assert_eq!(receipt.route, "unchanged");
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
fn an_export_requires_importing_a_reformatting_commit_first() {
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
    // Equal decoded documents are insufficient proof of an own export.
    exchange.a.rename(&ids[1..2], "renamed here");
    let refusal = exchange
        .a
        .service
        .export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now())
        .unwrap_err();
    assert!(format!("{refusal:#}").contains("engram task-sync import"));
    exchange
        .a
        .service
        .import_task_sync_data_folder(&exchange.data(), None, false, false, None, now())
        .unwrap();
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
fn an_own_commit_must_have_the_exact_exported_postimages() {
    let exchange = Exchange::new(2);
    let a = &exchange.a;
    let ids = exchange.ids();
    a.rename(&ids[..1], "exported here");
    a.service
        .export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now())
        .unwrap();
    let before = a
        .service
        .task_sync_folder_head(&profile(), now())
        .unwrap()
        .unwrap();

    let file = exchange
        .checkout
        .join(format!(".engram/tasks/{}.json", ids[0].0));
    let bytes = fs::read(&file).unwrap();
    let mut document = crate::task_sync::parse::<crate::task_sync::TaskDocument>(&bytes).unwrap();
    document.title.push_str(" modified before commit");
    fs::write(&file, crate::task_sync::pretty(&document).unwrap()).unwrap();
    exchange.commit("Commit an edited export");

    let refusal = a
        .service
        .export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now())
        .unwrap_err();
    let message = format!("{refusal:#}");
    assert!(
        message.contains("engram task-sync import") && message.contains("export --rebuild"),
        "{message}"
    );
    assert_eq!(
        a.service
            .task_sync_folder_head(&profile(), now())
            .unwrap()
            .unwrap(),
        before
    );
    assert!(owned_write(&a.service, &exchange.data()).is_some());
}

#[test]
fn an_explicit_empty_basis_refuses_without_adopting_an_exact_export() {
    let exchange = Exchange::unborn(0);
    let a = &exchange.a;
    let service = &a.service;
    let id = a.add("nonempty local planning");
    service
        .export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now())
        .unwrap();
    exchange.commit("Publish the exact initial export");
    let data = exchange.data();
    let before = service.task_sync_folder_head(&profile(), now()).unwrap();
    let mut journal = owned_write(service, &data).unwrap();
    for published in [false, true] {
        journal.published = published;
        journal.retain(&data).unwrap();
        for dry_run in [true, false] {
            let refusal = service
                .import_task_sync_data_folder(
                    &data,
                    Some(&crate::task_sync::directory::DestinationBasis::Empty),
                    false,
                    dry_run,
                    None,
                    now(),
                )
                .unwrap_err();
            if published {
                assert!(
                    matches!(
                        refusal.downcast_ref::<crate::storage::StoreError>(),
                        Some(crate::storage::StoreError::TaskSyncRefused {
                            refusal: crate::task_sync::TaskSyncRefusal::DivergentPlanning,
                            ..
                        })
                    ),
                    "{refusal:#}"
                );
            } else {
                assert_basis_refusal(&refusal, "import --basis OWN_EXPORT/project.json");
            }
            assert_eq!(
                service.task_sync_folder_head(&profile(), now()).unwrap(),
                before
            );
            assert_eq!(owned_write(service, &data).as_ref(), Some(&journal));
            assert!(a.title(id).ends_with("nonempty local planning"));
        }
    }
    let imported = service
        .import_task_sync_data_folder(&data, None, false, false, None, now())
        .unwrap();
    assert_eq!(imported.route, "unchanged");
    assert!(owned_write(service, &data).is_none());
}

#[test]
fn a_moved_tip_during_an_unfinished_export_names_the_same_remedy() {
    let exchange = Exchange::unborn(0);
    let a = &exchange.a;
    let service = &a.service;
    let id = a.add("initial planning");
    service
        .export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now())
        .unwrap();
    exchange.commit("Commit the initial export");
    exchange.git(&["commit", "--allow-empty", "-m", "Move beyond the export"]);
    let data = exchange.data();
    let mut journal = owned_write(service, &data).unwrap();
    journal.published = false;
    journal.retain(&data).unwrap();

    let export = service
        .export_task_sync_data_folder(&data, &profile(), false, None, now())
        .unwrap_err();
    assert_basis_refusal(&export, "import --basis OWN_EXPORT/project.json");
    let before = service.task_sync_folder_head(&profile(), now()).unwrap();
    for dry_run in [false, true] {
        let import = service
            .import_task_sync_data_folder(&data, None, false, dry_run, None, now())
            .unwrap_err();
        assert_eq!(export.to_string(), import.to_string());
    }
    assert_eq!(owned_write(service, &data).as_ref(), Some(&journal));
    let basis = crate::task_sync::directory::DestinationBasis::Recorded(Box::new(
        crate::task_sync::directory::read_folder_basis(&data.checkout.join(".engram/project.json"))
            .unwrap(),
    ));
    let import = |dry_run| {
        service.import_task_sync_data_folder(&data, Some(&basis), false, dry_run, None, now())
    };
    assert!(import(true).unwrap().result.dry_run);
    assert_eq!(
        service.task_sync_folder_head(&profile(), now()).unwrap(),
        before
    );
    assert_eq!(owned_write(service, &data).as_ref(), Some(&journal));
    assert!(!import(false).unwrap().result.dry_run);
    assert!(owned_write(service, &data).is_none());
    assert_eq!(a.title(id), "initial planning");
    assert_eq!(exchange.data().head_commit(), data.head_commit());
    assert_eq!(
        service
            .export_task_sync_data_folder(&data, &profile(), false, None, now())
            .unwrap()
            .route,
        "unchanged"
    );
}

#[test]
fn a_held_first_import_keeps_its_interrupted_proof_until_retry() {
    use crate::task_sync::{TaskDocument, directory::DestinationBasis};
    let exchange = Exchange::unborn(0);
    let a = &exchange.a;
    let id = a.add("original planning");
    let data = exchange.data();
    a.service
        .export_task_sync_data_folder(&data, &profile(), false, None, now())
        .unwrap();
    let mut proof = owned_write(&a.service, &data).unwrap();
    proof.published = false;
    proof.retain(&data).unwrap();
    let before = proof.retained_bytes(&data).unwrap();
    let head = a.service.task_sync_folder_head(&profile(), now()).unwrap();
    let basis = DestinationBasis::Recorded(Box::new(
        crate::task_sync::directory::read_folder_basis(&data.checkout.join(".engram/project.json"))
            .unwrap(),
    ));
    let claim = a.claim(id);
    let path = data.checkout.join(format!(".engram/tasks/{}.json", id.0));
    let mut edited: TaskDocument = crate::task_sync::parse(&fs::read(&path).unwrap()).unwrap();
    edited.title = "peer planning".into();
    fs::write(&path, crate::task_sync::pretty(&edited).unwrap()).unwrap();
    exchange.commit("Publish planning with a held edit");
    let data = exchange.data();
    let retry = || {
        a.service
            .import_task_sync_data_folder(&data, Some(&basis), false, false, None, now())
            .unwrap()
    };
    let partial = retry();
    assert_eq!(partial.result.tasks.len(), 1);
    assert_eq!(
        partial.result.tasks[0].status,
        crate::storage::TaskSyncTaskStatus::Held
    );
    assert_eq!(a.title(id), "original planning");
    assert_eq!(
        a.service.task_sync_folder_head(&profile(), now()).unwrap(),
        head
    );
    assert_eq!(proof.retained_bytes(&data).unwrap(), before);

    a.release(&claim);
    assert_eq!(
        retry().result.tasks[0].status,
        crate::storage::TaskSyncTaskStatus::Applied
    );
    assert_eq!(a.title(id), "peer planning");
    let (_, head) = a
        .service
        .task_sync_folder_head(&profile(), now())
        .unwrap()
        .unwrap();
    assert_eq!(
        Some(head.import_basis().unwrap().commit.as_str()),
        data.head_commit()
    );
    assert!(owned_write(&a.service, &data).is_none());
}

#[test]
fn an_exact_export_requires_its_recorded_parent_and_no_extra_path() {
    for mismatch in [
        "extra path",
        "wrong parent",
        "two parents",
        "executable task",
    ] {
        let exchange = Exchange::new(2);
        let a = &exchange.a;
        let parent_data = exchange.data();
        let parent = parent_data.head_commit().unwrap();
        a.rename(&exchange.ids()[..1], "local planning");
        a.service
            .export_task_sync_data_folder(&parent_data, &profile(), false, None, now())
            .unwrap();
        match mismatch {
            "extra path" => {
                fs::write(
                    exchange.checkout.join(".engram/extra.txt"),
                    b"separate payload",
                )
                .unwrap();
                exchange.git(&["add", "-f", "--", ".engram/extra.txt"]);
            }
            "wrong parent" => {
                exchange.git(&["commit", "--allow-empty", "-m", "Intervening commit"]);
            }
            _ => {}
        }
        if mismatch == "executable task" {
            let path = format!(".engram/tasks/{}.json", exchange.ids()[0].0);
            exchange.git(&["config", "core.filemode", "false"]);
            exchange.git(&["add", "--", ".engram"]);
            exchange.git(&["update-index", "--chmod=+x", "--", &path]);
            exchange.git(&["commit", "-m", "Commit an executable task file"]);
        } else {
            exchange.commit("Commit the planned bytes");
        }
        if mismatch == "two parents" {
            let child = exchange.data();
            let tree =
                String::from_utf8(run(&exchange.checkout, &["rev-parse", "HEAD^{tree}"])).unwrap();
            let merge = String::from_utf8(run(
                &exchange.checkout,
                &[
                    "commit-tree",
                    tree.trim(),
                    "-p",
                    parent,
                    "-p",
                    child.head_commit().unwrap(),
                    "-m",
                    "A merge with the same task bytes",
                ],
            ))
            .unwrap();
            exchange.git(&["update-ref", "refs/heads/engram-data", merge.trim()]);
        }
        let data = exchange.data();
        let before = a.service.task_sync_folder_head(&profile(), now()).unwrap();
        let journal = owned_write(&a.service, &data);
        let error = a
            .service
            .export_task_sync_data_folder(&data, &profile(), false, None, now())
            .unwrap_err();
        assert_basis_refusal(&error, "engram task-sync import");
        assert!(
            format!("{error:#}").contains("export --rebuild"),
            "{mismatch}: {error:#}"
        );
        assert_eq!(
            a.service.task_sync_folder_head(&profile(), now()).unwrap(),
            before,
            "{mismatch}"
        );
        assert_eq!(owned_write(&a.service, &data), journal, "{mismatch}");
    }
}

#[test]
fn export_names_the_basis_and_remedy_when_data_head_is_rewound() {
    let exchange = Exchange::new(2);
    let service = &exchange.a.service;
    let older = exchange.data();
    let older_commit = older.head_commit().unwrap();
    exchange
        .a
        .rename(&exchange.ids()[..1], "publish a later edit");
    service
        .export_task_sync_data_folder(&older, &profile(), false, None, now())
        .unwrap();
    exchange.commit("Commit the later edit");
    let latest = exchange.data();
    let expected_commit = latest.head_commit().unwrap();
    service
        .export_task_sync_data_folder(&latest, &profile(), false, None, now())
        .unwrap();
    assert!(owned_write(service, &latest).is_none());
    let basis = service.task_sync_folder_head(&profile(), now()).unwrap();

    exchange.git(&["reset", "--hard", older_commit]);
    let rewound = exchange.data();
    assert_eq!(rewound.head_commit(), Some(older_commit));
    let error = service
        .export_task_sync_data_folder(&rewound, &profile(), false, None, now())
        .unwrap_err();
    assert_basis_refusal(
        &error,
        &format!(
            "data HEAD {older_commit} does not match the admitted export basis {expected_commit}"
        ),
    );
    let message = error.to_string();
    assert!(message.contains(&format!("restore the data branch to {expected_commit}")));
    assert!(message.contains("`engram task-sync export --rebuild`"));
    assert!(!message.contains("ahead"));
    assert!(!message.contains("`engram task-sync import`"));
    assert_eq!(
        service.task_sync_folder_head(&profile(), now()).unwrap(),
        basis
    );
    assert!(owned_write(service, &rewound).is_none());
}

#[test]
fn a_byte_identical_rebuild_keeps_the_admitted_basis_without_a_new_write() {
    let exchange = Exchange::new(2);
    let a = &exchange.a.service;
    a.export_task_sync_data_folder(&exchange.data(), &profile(), true, None, now())
        .unwrap();
    let mut orphan = owned_write(a, &exchange.data()).unwrap();
    exchange.commit("Commit the rebuilt envelope");
    let data = exchange.data();
    a.export_task_sync_data_folder(&data, &profile(), false, None, now())
        .unwrap();
    let before = a.task_sync_folder_head(&profile(), now()).unwrap();
    let export = |rebuild| a.export_task_sync_data_folder(&data, &profile(), rebuild, None, now());
    for slot in ["empty", "unmatched", "malformed"] {
        match slot {
            "unmatched" => {
                orphan.head["reason"] = "export".into();
                orphan.retain(&data).unwrap();
            }
            "malformed" => orphan.restore_retained(&data, Some(b"{")).unwrap(),
            _ => {}
        }
        if slot != "empty" {
            assert_basis_refusal(&export(false).unwrap_err(), "export --rebuild");
        }
        let result = export(true).unwrap();
        assert_eq!(
            (result.route, result.written, result.removed),
            ("unchanged", 0, 0)
        );
        assert_eq!(a.task_sync_folder_head(&profile(), now()).unwrap(), before);
        assert!(owned_write(a, &data).is_none());
        assert_eq!(export(false).unwrap().route, "unchanged");
    }
    assert!(
        !crate::task_sync::git::matches_export_commit(
            &data.checkout,
            &exchange.tree.project.project_id,
            data.head_commit(),
            data.head_commit().unwrap(),
            &BTreeMap::new(),
            std::time::Duration::from_secs(60),
        )
        .unwrap()
    );
}

#[test]
fn an_invalid_proof_before_confirmation_recovers_with_the_saved_basis() {
    use crate::task_sync::directory::{DestinationBasis, read_folder_basis};
    let exchange = Exchange::new(2);
    let service = &exchange.a.service;
    service
        .export_task_sync_data_folder(&exchange.data(), &profile(), true, None, now())
        .unwrap();
    exchange.commit("Publish the bound rebuild");
    let data = exchange.data();
    let proof = owned_write(service, &data).unwrap();
    let original_head = service.task_sync_folder_head(&profile(), now()).unwrap();
    let envelope = data.checkout.join(".engram/project.json");
    let basis = read_folder_basis(&envelope).unwrap();
    let invalid = b"{";
    proof.restore_retained(&data, Some(invalid)).unwrap();

    let refused = service
        .export_task_sync_data_folder(&data, &profile(), true, None, now())
        .unwrap_err();
    assert_basis_refusal(&refused, "import --basis OWN_EXPORT/project.json");
    assert_eq!(
        proof.retained_bytes(&data).unwrap().as_deref(),
        Some(invalid.as_slice())
    );
    let retry = |basis, dry_run| {
        service.import_task_sync_data_folder(
            &data,
            Some(&DestinationBasis::Recorded(Box::new(basis))),
            false,
            dry_run,
            None,
            now(),
        )
    };
    let mut invalid_basis = basis.clone();
    invalid_basis.project_id.0 = "different-project".into();
    assert!(retry(invalid_basis, false).is_err());
    assert!(retry(basis.clone(), true).unwrap().result.dry_run);
    assert_eq!(
        service.task_sync_folder_head(&profile(), now()).unwrap(),
        original_head
    );
    assert_eq!(
        proof.retained_bytes(&data).unwrap().as_deref(),
        Some(invalid.as_slice())
    );
    assert!(!retry(basis, false).unwrap().result.dry_run);
    let (_, head) = service
        .task_sync_folder_head(&profile(), now())
        .unwrap()
        .unwrap();
    assert_eq!(
        Some(head.import_basis().unwrap().commit.as_str()),
        data.head_commit()
    );
    assert!(owned_write(service, &data).is_none());
    assert_eq!(
        service
            .export_task_sync_data_folder(&data, &profile(), false, None, now())
            .unwrap()
            .route,
        "unchanged"
    );
}

#[test]
fn first_import_after_own_bootstrap_confirms_the_exact_commit() {
    let exchange = Exchange::unborn(0);
    let a = &exchange.a;
    a.add("authored before the first commit");
    let publication = a
        .service
        .export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now())
        .unwrap();
    assert_eq!((publication.route, publication.written), ("bootstrap", 1));
    exchange.commit("Publish the first planning state");

    let data = exchange.data();
    let before = a.service.task_sync_folder_head(&profile(), now()).unwrap();
    let mut journal = owned_write(&a.service, &data).unwrap();
    assert!(journal.published);
    journal.published = false;
    journal.retain(&data).unwrap();
    for dry_run in [true, false] {
        let refused = a
            .service
            .import_task_sync_data_folder(&data, None, false, dry_run, None, now())
            .unwrap_err();
        assert_basis_refusal(&refused, "import --basis OWN_EXPORT/project.json");
        assert_eq!(
            a.service.task_sync_folder_head(&profile(), now()).unwrap(),
            before
        );
        assert_eq!(owned_write(&a.service, &data).as_ref(), Some(&journal));
    }
    journal.published = true;
    journal.retain(&data).unwrap();
    let preview = a
        .service
        .import_task_sync_data_folder(&data, None, false, true, None, now())
        .unwrap();
    assert_eq!((preview.route, preview.result.task_count), ("unchanged", 1));
    assert!(preview.result.dry_run);
    assert_eq!(
        a.service.task_sync_folder_head(&profile(), now()).unwrap(),
        before
    );
    assert_eq!(owned_write(&a.service, &data).as_ref(), Some(&journal));

    let receipt = a
        .service
        .import_task_sync_data_folder(&data, None, false, false, None, now())
        .unwrap();
    assert_eq!((receipt.route, receipt.result.task_count), ("unchanged", 1));
    assert_eq!(
        receipt.result.tasks,
        [] as [crate::storage::TaskSyncTaskResult; 0]
    );
    let (_, head) = a
        .service
        .task_sync_folder_head(&profile(), now())
        .unwrap()
        .unwrap();
    assert_eq!(head.source, None);
    assert_eq!(
        Some(head.import_basis().unwrap().commit.as_str()),
        data.head_commit()
    );
    assert!(owned_write(&a.service, &data).is_none());
}

#[test]
fn a_discarded_export_can_import_from_its_confirmed_bootstrap() {
    let exchange = Exchange::unborn(0);
    let a = &exchange.a;
    let task = a.add("authored here");
    let export = || {
        a.service
            .export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now())
            .unwrap()
    };
    export();
    exchange.commit("Publish the bootstrap");
    let import = || {
        a.service
            .import_task_sync_data_folder(&exchange.data(), None, false, false, None, now())
            .unwrap()
    };
    assert_eq!(import().route, "unchanged");
    assert!(
        a.service
            .task_sync_folder_head(&profile(), now())
            .unwrap()
            .unwrap()
            .1
            .source
            .is_none()
    );
    a.rename(&[task], "first edit");
    assert_eq!(export().written, 1);
    a.rename(&[task], "second edit");
    exchange.git(&["restore", "--worktree", "--", ".engram"]);
    let receipt = import();
    assert_eq!(receipt.route, "incremental");
    assert!(a.title(task).ends_with("second edit"));
    assert!(owned_write(&a.service, &exchange.data()).is_none());
    assert_eq!(export().written, 1);
}

#[test]
fn rebuild_confirmation_checks_git_when_replay_bytes_differ_from_its_parent() {
    for discard_edit in [false, true] {
        let exchange = Exchange::new(2);
        let a = &exchange.a;
        let ids = exchange.ids();
        let untouched = format!(".engram/tasks/{}.json", ids[0].0);
        let edited = format!(".engram/tasks/{}.json", ids[1].0);
        let original = fs::read(exchange.checkout.join(&untouched)).unwrap();
        a.rename(&ids[1..], "local edit");
        a.add("new task already in the working folder");
        let export = |rebuild| {
            a.service.export_task_sync_data_folder(
                &exchange.data(),
                &profile(),
                rebuild,
                None,
                now(),
            )
        };
        assert_eq!(export(false).unwrap().written, 2);
        fs::write(exchange.checkout.join(&untouched), b"discarded loose bytes").unwrap();
        assert_eq!(export(true).unwrap().route, "rebuild");
        assert_eq!(
            fs::read(exchange.checkout.join(&untouched)).unwrap(),
            original
        );
        if discard_edit {
            exchange.git(&["restore", "--worktree", "--", &edited]);
        }
        exchange.commit("Commit the rebuilt planning");
        let data = exchange.data();
        let before = a.service.task_sync_folder_head(&profile(), now()).unwrap();
        let journal = owned_write(&a.service, &data);
        if discard_edit {
            let refusal = export(false).unwrap_err();
            assert!(format!("{refusal:#}").contains("engram task-sync import"));
            assert_eq!(
                a.service.task_sync_folder_head(&profile(), now()).unwrap(),
                before
            );
            assert_eq!(owned_write(&a.service, &data), journal);
        } else {
            assert_eq!(export(false).unwrap().route, "unchanged");
            assert!(owned_write(&a.service, &data).is_none());
        }
    }
}

#[test]
fn another_store_preserves_a_retained_write_before_and_after_head_recording() {
    for recorded in [false, true] {
        let exchange = Exchange::new(2);
        let a = &exchange.a;
        let b = &exchange.b;
        let ids = exchange.ids();
        let data = exchange.data();
        a.rename(&ids[..1], "author edit");
        a.service
            .export_task_sync_data_folder(&data, &profile(), false, None, now())
            .unwrap();
        let mut write = owned_write(&a.service, &data).unwrap();
        write.published = false;
        if !recorded {
            write.head["rewritten"] = serde_json::Value::Bool(true);
        }
        write.retain(&data).unwrap();
        exchange.git(&["restore", "--worktree", "--", ".engram"]);

        assert_eq!(
            b.service
                .import_task_sync_data_folder(&data, None, false, false, None, now())
                .unwrap()
                .route,
            "unchanged"
        );
        assert_eq!(owned_write(&a.service, &data), Some(write.clone()));
        b.rename(&ids[1..], "second store edit");
        assert_eq!(
            b.service
                .export_task_sync_data_folder(&data, &profile(), false, None, now())
                .unwrap()
                .written,
            1
        );
        assert!(owned_write(&b.service, &data).is_some());
        assert_eq!(owned_write(&a.service, &data), Some(write));
    }
}

#[test]
fn an_unbound_rebuild_refuses_ordinary_export_before_commit() {
    let exchange = Exchange::new(2);
    let service = &exchange.a.service;
    exchange.git(&[
        "commit",
        "--allow-empty",
        "-m",
        "Advance the unimported tip",
    ]);
    let id = exchange.ids()[0];
    let path = exchange
        .checkout
        .join(format!(".engram/tasks/{}.json", id.0));
    let data = exchange.data();
    exchange.a.rename(&[id], "first local edit");
    let rebuilt = service
        .export_task_sync_data_folder(&data, &profile(), true, None, now())
        .unwrap();
    assert_eq!(rebuilt.route, "rebuild");

    let before = service.task_sync_folder_head(&profile(), now()).unwrap();
    assert!(before.as_ref().unwrap().1.rebuild_needs_complete_import());
    let journal = owned_write(service, &data).unwrap();
    let bytes = fs::read(&path).unwrap();
    exchange.a.rename(&[id], "second local edit");
    let refused = service
        .export_task_sync_data_folder(&data, &profile(), false, None, now())
        .unwrap_err();
    assert_rebuild_basis_required(&refused);
    assert_eq!(
        service.task_sync_folder_head(&profile(), now()).unwrap(),
        before
    );
    assert_eq!(owned_write(service, &data), Some(journal));
    assert_eq!(fs::read(path).unwrap(), bytes);
}

#[test]
fn first_store_rebuild_of_an_existing_commit_requires_its_saved_basis() {
    let exchange = Exchange::unborn(1);
    let service = &exchange.a.service;
    crate::task_sync::directory::write_data_folder(&exchange.data(), &exchange.tree, &profile())
        .unwrap();
    exchange.commit("Publish another store's planning");
    exchange.a.add("local replacement planning");
    let export = |rebuild| {
        service.export_task_sync_data_folder(&exchange.data(), &profile(), rebuild, None, now())
    };
    assert_eq!(export(true).unwrap().route, "rebuild");
    let before = service.task_sync_folder_head(&profile(), now()).unwrap();
    assert!(before.as_ref().unwrap().1.rebuild_needs_complete_import());
    let journal = owned_write(service, &exchange.data()).unwrap();

    let data = exchange.data();
    let mut unfinished = journal.clone();
    unfinished.published = false;
    unfinished.retain(&data).unwrap();
    let unfinished_bytes = unfinished.retained_bytes(&data).unwrap();
    let envelope_path = data.checkout.join(".engram/project.json");
    let envelope = fs::read(&envelope_path).unwrap();
    for dry_run in [true, false] {
        let refused = service
            .import_task_sync_data_folder(&data, None, false, dry_run, None, now())
            .unwrap_err();
        assert!(
            refused
                .to_string()
                .contains("export` to finish the interrupted write")
        );
        assert_eq!(unfinished.retained_bytes(&data).unwrap(), unfinished_bytes);
        assert_eq!(
            service.task_sync_folder_head(&profile(), now()).unwrap(),
            before
        );
        assert_eq!(fs::read(&envelope_path).unwrap(), envelope);
    }
    assert_eq!(export(false).unwrap().route, "unchanged");
    assert_eq!(owned_write(service, &data).as_ref(), Some(&journal));

    let basis = crate::task_sync::directory::DestinationBasis::Recorded(Box::new(
        crate::task_sync::directory::read_folder_basis(
            &exchange.checkout.join(".engram/project.json"),
        )
        .unwrap(),
    ));
    for committed in [false, true] {
        if committed {
            exchange.commit("Publish the replacement planning");
        }
        assert_rebuild_basis_required(&export(false).unwrap_err());
        for dry_run in [false, true] {
            assert_rebuild_basis_required(
                &service
                    .import_task_sync_data_folder(
                        &exchange.data(),
                        None,
                        false,
                        dry_run,
                        None,
                        now(),
                    )
                    .unwrap_err(),
            );
        }
        assert_eq!(
            service.task_sync_folder_head(&profile(), now()).unwrap(),
            before
        );
        assert_eq!(
            owned_write(service, &exchange.data()).as_ref(),
            Some(&journal)
        );
    }
    let imported = service
        .import_task_sync_data_folder(&exchange.data(), Some(&basis), false, false, None, now())
        .unwrap();
    assert_eq!(imported.route, "complete");
    assert!(owned_write(service, &exchange.data()).is_none());
    assert_eq!(export(false).unwrap().route, "unchanged");
}

#[test]
fn first_export_refuses_a_deleted_parent_task_missing_from_its_proof() {
    let exchange = Exchange::unborn(1);
    let service = &exchange.a.service;
    crate::task_sync::directory::write_data_folder(&exchange.data(), &exchange.tree, &profile())
        .unwrap();
    exchange.commit("Publish a parent task");
    let path = exchange
        .checkout
        .join(format!(".engram/tasks/{}.json", exchange.ids()[0].0));
    fs::remove_file(&path).unwrap();
    let data = exchange.data();
    let envelope = exchange.checkout.join(".engram/project.json");
    let original = fs::read(&envelope).unwrap();

    let refused = service
        .export_task_sync_data_folder(&data, &profile(), false, None, now())
        .unwrap_err();
    assert_basis_refusal(&refused, "uncommitted task deletion");
    assert!(
        service
            .task_sync_folder_head(&profile(), now())
            .unwrap()
            .is_none()
    );
    assert!(owned_write(service, &data).is_none());
    assert_eq!(fs::read(envelope).unwrap(), original);
    assert!(!path.exists());
}

#[test]
fn staged_evidence_stays_outside_export_and_confirmation_status() {
    let exchange = Exchange::new(2);
    let a = &exchange.a;
    let id = exchange.ids()[0];
    let evidence =
        crate::task_sync::evidence::path::pack(&crate::ObjectId::mint(), &crate::ObjectId::mint());
    let path = exchange.checkout.join(&evidence);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    fs::write(&path, b"opaque evidence staged independently").unwrap();
    exchange.git(&["add", "--", &evidence]);
    a.rename(&[id], "planning edit");
    let export = || {
        a.service
            .export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now())
            .unwrap()
    };
    let receipt = export();
    assert_eq!((receipt.route, receipt.written), ("incremental", 1));
    let task = format!(".engram/tasks/{}.json", id.0);
    exchange.git(&[
        "commit",
        "-m",
        "Publish planning only",
        "--",
        ".engram/project.json",
        &task,
    ]);
    assert_eq!(export().route, "unchanged");
    assert!(owned_write(&a.service, &exchange.data()).is_none());
    assert_eq!(
        fs::read(&path).unwrap(),
        b"opaque evidence staged independently"
    );
}

#[test]
fn ordinary_commands_leave_pending_capability_staging_alone() {
    let exchange = Exchange::new(1);
    let service = &exchange.a.service;
    let data = exchange.data();
    let folder = data.checkout.join(".engram");
    let staging = folder.join(format!("project-{}.pending", uuid::Uuid::new_v4()));
    fs::write(&staging, b"retained staging").unwrap();
    let envelope = fs::read(folder.join("project.json")).unwrap();
    let head = service.task_sync_folder_head(&profile(), now()).unwrap();

    let export = service
        .export_task_sync_data_folder(&data, &profile(), false, None, now())
        .unwrap_err();
    let import = service
        .import_task_sync_data_folder(&data, None, false, false, None, now())
        .unwrap_err();
    assert_basis_refusal(&export, "--memory-recovery-basis");
    assert_eq!(export.to_string(), import.to_string());
    assert_eq!(fs::read(staging).unwrap(), b"retained staging");
    assert_eq!(fs::read(folder.join("project.json")).unwrap(), envelope);
    assert_eq!(
        service.task_sync_folder_head(&profile(), now()).unwrap(),
        head
    );
    assert!(owned_write(service, &data).is_none());
}

#[test]
fn task_only_export_and_import_preserve_an_interrupted_combined_proof() {
    let exchange = Exchange::unborn(0);
    let a = &exchange.a;
    a.add("combined planning");
    a.service
        .export_task_sync_data_folder(
            &exchange.data(),
            &profile(),
            false,
            Some(combined_export()),
            now(),
        )
        .unwrap();
    exchange.commit("Commit a combined export");
    let data = exchange.data();
    let before = a.service.task_sync_folder_head(&profile(), now()).unwrap();
    let mut journal = owned_write(&a.service, &data).unwrap();
    journal.published = false;
    journal.retain(&data).unwrap();
    let export = a
        .service
        .export_task_sync_data_folder(&data, &profile(), false, None, now())
        .unwrap_err();
    let import = a
        .service
        .import_task_sync_data_folder(&data, None, false, false, None, now())
        .unwrap_err();
    for result in [export, import] {
        assert!(
            matches!(
                result.downcast_ref::<crate::storage::StoreError>(),
                Some(crate::storage::StoreError::TaskSyncRefused {
                    refusal: crate::task_sync::TaskSyncRefusal::MemoryProfileRequiresOptIn,
                    ..
                })
            ),
            "{result:#}"
        );
    }
    assert_eq!(
        a.service.task_sync_folder_head(&profile(), now()).unwrap(),
        before
    );
    assert_eq!(owned_write(&a.service, &data), Some(journal));
}

#[test]
fn an_identical_combined_rebuild_needs_its_original_parent_to_finish() {
    let exchange = Exchange::new(2);
    let a = &exchange.a;
    let export = |rebuild, memory| {
        a.service.export_task_sync_data_folder(
            &exchange.data(),
            &profile(),
            rebuild,
            Some(memory),
            now(),
        )
    };
    export(false, combined_export()).unwrap();
    exchange.commit("Publish a combined parent");
    export(false, combined_export()).unwrap();
    a.rename(&exchange.ids()[..1], "edited during a combined rebuild");
    let mut interrupted = combined_export();
    interrupted.admit = Box::new(|_| {
        Ok(super::FolderMemoryAdmission {
            bytes: 0,
            files: BTreeMap::new(),
            clear_capability_staging: None,
            publish: Box::new(|_| anyhow::bail!("stop before publishing memories")),
        })
    });
    assert!(
        export(true, interrupted)
            .unwrap_err()
            .to_string()
            .contains("stop before publishing memories")
    );
    let data = exchange.data();
    let proof = owned_write(&a.service, &data).unwrap();
    assert!(!proof.published);
    let before = proof.retained_bytes(&data).unwrap();
    let head = a.service.task_sync_folder_head(&profile(), now()).unwrap();
    assert!(!head.as_ref().unwrap().1.rebuild_needs_complete_import());
    let file = data.checkout.join(".engram/project.json");
    let envelope = fs::read(&file).unwrap();
    let payload = || {
        proof
            .files
            .keys()
            .map(|path| fs::read(data.checkout.join(".engram").join(path)).unwrap())
            .collect::<Vec<_>>()
    };
    let original = payload();
    let pending = a
        .service
        .prepare_folder_import(&data, None, false, false, now())
        .err()
        .unwrap();
    assert_basis_refusal(
        &pending,
        "export --rebuild --include-memories --memory-recovery-basis",
    );
    assert_eq!(proof.retained_bytes(&data).unwrap(), before);
    assert_eq!(payload(), original);

    exchange.commit("Commit only the task phase of the rebuild");
    let mut recovery = combined_export();
    recovery.recovery_basis_matches = Some(true);
    let refused = export(true, recovery).unwrap_err();
    assert_basis_refusal(&refused, "restore the original basis");
    assert!(refused.to_string().contains("--memory-recovery-basis"));
    assert_eq!(proof.retained_bytes(&data).unwrap(), before);
    assert_eq!(
        a.service.task_sync_folder_head(&profile(), now()).unwrap(),
        head
    );
    assert_eq!(payload(), original);

    exchange.git(&["branch", "preserve-task-phase"]);
    exchange.git(&["reset", "--mixed", "HEAD^"]);
    assert_eq!(fs::read(file).unwrap(), envelope);
    assert_eq!(payload(), original);
    let mut recovery = combined_export();
    recovery.recovery_basis_matches = Some(true);
    assert_eq!(export(true, recovery).unwrap().route, "rebuild");
    let finished = owned_write(&a.service, &exchange.data()).unwrap();
    assert!(finished.published);
    assert_ne!(finished.retained_bytes(&data).unwrap(), before);
}

#[test]
fn an_exact_commit_is_not_adopted_before_export_planning_refuses() {
    let exchange = Exchange::new(2);
    let a = &exchange.a;
    let data = exchange.data();
    let mut original_profile = profile();
    original_profile.origin_machine = Some("original".into());
    a.rename(&exchange.ids()[..1], "publish under the original alias");
    a.service
        .export_task_sync_data_folder(&data, &original_profile, false, None, now())
        .unwrap();
    exchange.commit("Commit the exact task export");
    let data = exchange.data();
    let proof = owned_write(&a.service, &data).unwrap();
    let before = proof.retained_bytes(&data).unwrap();
    let head = a.service.task_sync_folder_head(&profile(), now()).unwrap();
    let path = data.checkout.join(".engram/project.json");
    let envelope = fs::read(&path).unwrap();
    let mut different = original_profile.clone();
    different.origin_machine = Some("different".into());
    let refused = a
        .service
        .export_task_sync_data_folder(&data, &different, false, None, now())
        .unwrap_err();
    assert!(refused.to_string().contains("--origin-machine original"));
    assert_eq!(proof.retained_bytes(&data).unwrap(), before);
    assert_eq!(
        a.service.task_sync_folder_head(&profile(), now()).unwrap(),
        head
    );
    assert_eq!(fs::read(path).unwrap(), envelope);
    assert_eq!(
        a.service
            .export_task_sync_data_folder(&data, &original_profile, false, None, now())
            .unwrap()
            .route,
        "unchanged"
    );
    assert!(owned_write(&a.service, &data).is_none());
}

#[test]
fn a_slot_with_no_recorded_head_requires_explicit_rebuild() {
    let exchange = Exchange::new(2);
    let a = &exchange.a;
    let data = exchange.data();
    a.rename(&exchange.ids()[..1], "the next local edit");
    a.service
        .export_task_sync_data_folder(&data, &profile(), false, None, now())
        .unwrap();
    let mut orphan = owned_write(&a.service, &data).unwrap();
    orphan.head["reason"] = serde_json::Value::String("rebuild".into());
    orphan.published = false;
    orphan.retain(&data).unwrap();
    let before = orphan.retained_bytes(&data).unwrap();
    let saved_head = a.service.task_sync_folder_head(&profile(), now()).unwrap();
    let envelope = data.checkout.join(".engram/project.json");
    let bytes = fs::read(&envelope).unwrap();
    let export = |rebuild| {
        a.service
            .export_task_sync_data_folder(&data, &profile(), rebuild, None, now())
    };
    assert_basis_refusal(
        &export(false).unwrap_err(),
        "engram task-sync export --rebuild",
    );
    for dry_run in [false, true] {
        let refused = a
            .service
            .import_task_sync_data_folder(&data, None, false, dry_run, None, now())
            .unwrap_err();
        assert_basis_refusal(&refused, "engram task-sync export --rebuild");
    }
    assert_eq!(orphan.retained_bytes(&data).unwrap(), before);
    assert_eq!(fs::read(envelope).unwrap(), bytes);
    assert_eq!(
        a.service.task_sync_folder_head(&profile(), now()).unwrap(),
        saved_head
    );
    assert_eq!(export(true).unwrap().route, "rebuild");
    let recorded = owned_write(&a.service, &data).unwrap();
    assert!(recorded.published);
    assert_ne!(recorded.retained_bytes(&data).unwrap(), before);
}

#[test]
fn a_refused_rebuild_keeps_the_only_retained_proof() {
    let exchange = Exchange::new(2);
    let service = &exchange.a.service;
    let data = exchange.data();
    exchange.a.rename(&exchange.ids()[..1], "first task edit");
    let export =
        |rebuild| service.export_task_sync_data_folder(&data, &profile(), rebuild, None, now());
    export(false).unwrap();
    let proof = owned_write(service, &data).unwrap();
    let before = proof.retained_bytes(&data).unwrap();
    let head = service.task_sync_folder_head(&profile(), now()).unwrap();
    let payload = || {
        proof
            .files
            .keys()
            .map(|path| fs::read(data.checkout.join(".engram").join(path)).unwrap())
            .collect::<Vec<_>>()
    };
    let original = payload();
    exchange.a.rename(&exchange.ids()[..1], "later task edit");
    super::task_sync_folder_export::AFTER_EXPORT_RETAIN.with(|hook| {
        hook.set(Some(|| {
            anyhow::bail!("simulate a refused head transaction")
        }));
    });
    assert!(
        export(true)
            .unwrap_err()
            .to_string()
            .contains("simulate a refused head transaction")
    );
    assert_eq!(proof.retained_bytes(&data).unwrap(), before);
    assert_eq!(
        service.task_sync_folder_head(&profile(), now()).unwrap(),
        head
    );
    assert_eq!(payload(), original);
    assert_eq!(export(true).unwrap().route, "rebuild");
    let finished = owned_write(service, &data).unwrap();
    assert!(finished.published);
    assert_ne!(finished.retained_bytes(&data).unwrap(), before);
    assert_eq!(
        finished.head,
        serde_json::to_value(
            service
                .task_sync_folder_head(&profile(), now())
                .unwrap()
                .unwrap()
                .1
        )
        .unwrap()
    );
}

#[test]
fn a_tip_move_during_writer_admission_keeps_the_head_proof_and_folder() {
    let exchange = Exchange::new(2);
    let a = &exchange.a;
    let data = exchange.data();
    a.rename(&exchange.ids()[..1], "first edit");
    a.service
        .export_task_sync_data_folder(&data, &profile(), false, None, now())
        .unwrap();
    a.rename(&exchange.ids()[..1], "later edit");
    assert_writer_tip_move_keeps_publication(&exchange, &data, || {
        a.service
            .export_task_sync_data_folder(&data, &profile(), true, None, now())
    });
}

#[test]
fn a_tip_move_during_quiet_export_confirmation_keeps_the_proof() {
    let exchange = Exchange::new(2);
    let service = &exchange.a.service;
    exchange.a.rename(&exchange.ids()[..1], "own edit");
    service
        .export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now())
        .unwrap();
    exchange.commit("Publish the exact own edit");
    let data = exchange.data();
    assert_writer_tip_move_keeps_publication(&exchange, &data, || {
        service.export_task_sync_data_folder(&data, &profile(), false, None, now())
    });
}

#[test]
fn a_tip_move_during_quiet_import_confirmation_keeps_the_proof() {
    let exchange = Exchange::new(2);
    let service = &exchange.a.service;
    exchange.a.rename(&exchange.ids()[..1], "own edit");
    service
        .export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now())
        .unwrap();
    exchange.commit("Publish the exact own edit");
    let data = exchange.data();
    let preview = service
        .import_task_sync_data_folder(&data, None, false, true, None, now())
        .unwrap();
    assert_eq!(preview.route, "unchanged");
    assert_writer_tip_move_keeps_publication(&exchange, &data, || {
        service.import_task_sync_data_folder(&data, None, false, false, None, now())
    });
}

#[test]
fn a_tip_move_during_delta_import_confirmation_keeps_the_proof() {
    let exchange = Exchange::new(2);
    let ids = exchange.ids();
    let a = &exchange.a;
    let b = &exchange.b;
    let _claim = a.claim(ids[0]);
    b.rename(&ids[..1], "peer edit of held task");
    b.service
        .export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now())
        .unwrap();
    exchange.commit("Publish the peer edit");
    let imported = a
        .service
        .import_task_sync_data_folder(&exchange.data(), None, false, false, None, now())
        .unwrap();
    assert_eq!(
        imported.result.tasks[0].status,
        crate::storage::TaskSyncTaskStatus::Held
    );
    a.rename(&ids[1..], "own edit");
    a.service
        .export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now())
        .unwrap();
    exchange.commit("Publish the exact own edit");
    let data = exchange.data();
    let preview = a
        .service
        .import_task_sync_data_folder(&data, None, false, true, None, now())
        .unwrap();
    assert_eq!(preview.route, "incremental");
    assert_eq!(
        preview.result.tasks[0].status,
        crate::storage::TaskSyncTaskStatus::Held
    );
    assert!(
        a.service
            .prepare_folder_import(&data, None, false, true, now())
            .unwrap()
            .confirmation
            .is_some()
    );
    assert_writer_tip_move_keeps_publication(&exchange, &data, || {
        a.service
            .import_task_sync_data_folder(&data, None, false, false, None, now())
    });
}

#[test]
fn a_tip_move_during_saved_basis_recovery_keeps_the_unfinished_proof() {
    use crate::task_sync::{
        TaskDocument,
        directory::{DestinationBasis, read_folder_basis},
    };
    for (dry_run, held) in [(false, false), (true, false), (false, true)] {
        let exchange = Exchange::unborn(0);
        let a = &exchange.a;
        let id = a.add("own planning");
        let data = exchange.data();
        a.service
            .export_task_sync_data_folder(&data, &profile(), false, None, now())
            .unwrap();
        let mut proof = owned_write(&a.service, &data).unwrap();
        proof.published = false;
        proof.retain(&data).unwrap();
        let basis = DestinationBasis::Recorded(Box::new(
            read_folder_basis(&data.checkout.join(".engram/project.json")).unwrap(),
        ));
        if held {
            a.claim(id);
            let task = data.checkout.join(format!(".engram/tasks/{}.json", id.0));
            let mut edited: TaskDocument =
                crate::task_sync::parse(&fs::read(&task).unwrap()).unwrap();
            edited.title = "peer edit of held task".into();
            fs::write(task, crate::task_sync::pretty(&edited).unwrap()).unwrap();
        }
        exchange.commit("Commit the interrupted first publication");
        let data = exchange.data();
        assert_writer_tip_move_keeps_publication(&exchange, &data, || {
            a.service
                .import_task_sync_data_folder(&data, Some(&basis), false, dry_run, None, now())
        });
    }
}

#[test]
fn a_tip_move_during_complete_import_keeps_the_existing_proof() {
    use crate::task_sync::directory::{DestinationBasis, read_folder_basis};
    for committed in [false, true] {
        let exchange = Exchange::unborn(0);
        let service = &exchange.a.service;
        exchange.a.add("own planning");
        service
            .export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now())
            .unwrap();
        if committed {
            exchange.commit("Commit the first publication");
        }
        let data = exchange.data();
        let basis = DestinationBasis::Recorded(Box::new(
            read_folder_basis(&data.checkout.join(".engram/project.json")).unwrap(),
        ));
        assert_writer_tip_move_keeps_publication(&exchange, &data, || {
            service.import_task_sync_data_folder(&data, Some(&basis), false, false, None, now())
        });
    }
}

#[test]
fn a_refused_combined_rebuild_keeps_valid_capability_staging() {
    use crate::task_sync::{directory, memories::directory::publication_lock};
    let exchange = Exchange::new(2);
    let a = &exchange.a;
    let data = exchange.data();
    let folder = data.checkout.join(".engram");
    let envelope = folder.join("project.json");
    let retained = crate::test_support::temp_home().unwrap();
    fs::copy(&envelope, retained.path().join("project.json")).unwrap();
    a.rename(&exchange.ids()[..1], "first combined edit");
    a.service
        .export_task_sync_data_folder(&data, &profile(), false, Some(combined_export()), now())
        .unwrap();
    let proof = owned_write(&a.service, &data).unwrap();
    let before = proof.retained_bytes(&data).unwrap();
    let head = a.service.task_sync_folder_head(&profile(), now()).unwrap();
    let published_envelope = fs::read(&envelope).unwrap();
    let staging = folder.join(format!("project-{}.pending", uuid::Uuid::new_v4()));
    let staged = &published_envelope[..published_envelope.len() / 2];
    fs::write(&staging, staged).unwrap();
    a.rename(&exchange.ids()[..1], "later combined edit");
    let fence = publication_lock(&data).unwrap();
    let retry = || {
        let (data, retained, fence) = (&data, retained.path(), &fence);
        let memories = super::FolderMemoryExport {
            recovery_basis_matches: Some(true),
            admit: Box::new(move |_| {
                directory::validate_memory_profile_staging_locked(data, retained, fence)?;
                Ok(super::FolderMemoryAdmission {
                    bytes: 0,
                    files: BTreeMap::new(),
                    clear_capability_staging: Some(Box::new(move || {
                        directory::clear_memory_profile_staging_locked(data, retained, fence)?;
                        Ok(())
                    })),
                    publish: Box::new(|_| Ok(())),
                })
            }),
            ..combined_export()
        };
        a.service
            .export_task_sync_data_folder(data, &profile(), true, Some(memories), now())
    };
    super::task_sync_folder_export::AFTER_EXPORT_RETAIN.with(|hook| {
        hook.set(Some(|| {
            anyhow::bail!("simulate a refused head transaction")
        }));
    });
    let refused = retry().unwrap_err();
    assert!(
        refused
            .to_string()
            .contains("simulate a refused head transaction")
    );
    assert_eq!(proof.retained_bytes(&data).unwrap(), before);
    assert_eq!(
        a.service.task_sync_folder_head(&profile(), now()).unwrap(),
        head
    );
    assert_eq!(fs::read(&envelope).unwrap(), published_envelope);
    assert_eq!(fs::read(&staging).unwrap(), staged);
    assert_eq!(retry().unwrap().route, "rebuild");
    assert!(!staging.exists());
}

#[test]
fn a_displaced_proof_keeps_export_decodes_constant_across_head_history() {
    use crate::task_sync::directory::{
        DestinationBasis, read_folder_basis, take_folder_write_payload_reads,
    };
    let exchange = Exchange::new(2_000);
    let service = &exchange.a.service;
    let quiet_export =
        || service.export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now());
    let (quiet, without_proof) = counted_queries(quiet_export);
    assert_eq!(quiet.unwrap().route, "unchanged");

    exchange
        .a
        .rename(&exchange.ids()[..1], "an edit to rebuild");
    let rebuild = service
        .export_task_sync_data_folder(&exchange.data(), &profile(), true, None, now())
        .unwrap();
    assert_eq!(rebuild.route, "rebuild");
    exchange.commit("Publish the rebuilt planning");
    // Keep the rebuilt bytes but make the tip inexact so importing the
    // saved basis displaces the retained proof instead of confirming it.
    exchange.git(&[
        "commit",
        "--allow-empty",
        "-m",
        "Advance beyond the exact rebuild commit",
    ]);
    let data = exchange.data();
    let proof = owned_write(service, &data).unwrap();
    assert_eq!(
        proof
            .files
            .keys()
            .filter(|path| path.starts_with("tasks/"))
            .count(),
        2_000
    );
    let before = proof.retained_bytes(&data).unwrap();
    let basis = DestinationBasis::Recorded(Box::new(
        read_folder_basis(&data.checkout.join(".engram/project.json")).unwrap(),
    ));
    service
        .import_task_sync_data_folder(&data, Some(&basis), false, false, None, now())
        .unwrap();
    let measure = || {
        crate::canonical::reset_canonical_decode_count();
        take_folder_write_payload_reads();
        let (receipt, queries) = counted_queries(quiet_export);
        let receipt = receipt.unwrap();
        assert_eq!(
            (receipt.route, receipt.written, receipt.removed),
            ("unchanged", 0, 0)
        );
        assert_eq!(take_folder_write_payload_reads(), 0);
        assert_eq!(queries, without_proof);
        assert_eq!(proof.retained_bytes(&data).unwrap(), before);
        crate::canonical::canonical_decode_count()
    };
    let (mut id, mut head) = service
        .task_sync_folder_head(&profile(), now())
        .unwrap()
        .unwrap();
    assert_ne!(Some(&id), proof.head_id.as_ref());
    let one = measure();
    for _ in 0..7 {
        head.previous = Some(id);
        id = service.record_task_sync_folder_head(&head, now()).unwrap();
    }
    let eight = measure();
    println!(
        "unchanged export with a displaced proof: one head={one}, eight heads={eight} canonical decodes"
    );
    println!("quiet export: zero displaced-journal payload reads, Git starts={without_proof:?}");
    assert_eq!(one, eight);
}

#[test]
fn an_uncommitted_export_refuses_replacement_and_keeps_its_proof() {
    let exchange = Exchange::new(2);
    let a = &exchange.a;
    let ids = exchange.ids();
    a.rename(&ids[..1], "first edit");
    let first = a
        .service
        .export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now())
        .unwrap();
    assert_eq!((first.route, first.written), ("incremental", 1));
    let data = exchange.data();
    let before = a.service.task_sync_folder_head(&profile(), now()).unwrap();
    let retained = owned_write(&a.service, &data);
    let envelope = data.checkout.join(".engram/project.json");
    let bytes = fs::read(&envelope).unwrap();
    a.rename(&ids, "later edit");
    super::task_sync_folder_export::AFTER_EXPORT_RETAIN.with(|hook| {
        hook.set(Some(|| {
            anyhow::bail!("stop between retention and head recording")
        }));
    });
    let second = a
        .service
        .export_task_sync_data_folder(&data, &profile(), false, None, now())
        .unwrap_err();
    assert!(super::task_sync_folder_export::AFTER_EXPORT_RETAIN.with(|hook| hook.take().is_some()));
    assert_basis_refusal(&second, "commit the exported folder");
    assert_eq!(owned_write(&a.service, &data), retained);
    assert_eq!(
        a.service.task_sync_folder_head(&profile(), now()).unwrap(),
        before
    );
    assert_eq!(fs::read(envelope).unwrap(), bytes);
    exchange.commit("Publish the first export");

    let data = exchange.data();
    let confirmed = a
        .service
        .import_task_sync_data_folder(&data, None, false, false, None, now())
        .unwrap();
    assert_eq!(confirmed.route, "unchanged");
    assert!(owned_write(&a.service, &data).is_none());
    assert_eq!(
        a.service
            .export_task_sync_data_folder(&data, &profile(), false, None, now())
            .unwrap()
            .written,
        2
    );
}

#[test]
fn confirming_an_own_commit_keeps_later_local_edits_owed() {
    let exchange = Exchange::new(2);
    let a = &exchange.a;
    let ids = exchange.ids();
    let (_, imported) = a
        .service
        .task_sync_folder_head(&profile(), now())
        .unwrap()
        .unwrap();
    a.rename(&ids[..1], "first edit");
    a.service
        .export_task_sync_data_folder(&exchange.data(), &profile(), false, None, now())
        .unwrap();
    a.rename(&ids[1..], "second edit");
    exchange.commit("Publish the first edit");

    let data = exchange.data();
    let next = a
        .service
        .export_task_sync_data_folder(&data, &profile(), false, None, now())
        .unwrap();
    assert_eq!(
        (next.route, next.written, next.removed),
        ("incremental", 1, 0)
    );
    let (_, written) = a
        .service
        .task_sync_folder_head(&profile(), now())
        .unwrap()
        .unwrap();
    assert_eq!(written.source, imported.source);
    let crate::storage::FolderDisk::Written { from, files } = written.disk else {
        panic!("the second edit was not retained as a write");
    };
    assert_eq!(Some(from.commit.as_str()), data.head_commit());
    assert!(files.keys().eq(ids[1..].iter()));
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
