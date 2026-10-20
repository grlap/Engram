//! The owned test child exits without Rust destructors at two exact cuts.

use super::*;
use serde::{Deserialize, Serialize};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    time::{Duration, Instant},
};

const REQUEST_ENV: &str = "ENGRAM_TEST_FOLDER_PROCESS_REQUEST";
const KEY: &str = "mixed-folder-intent";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Request {
    database: PathBuf,
    selected: TaskTree,
    retained_basis: String,
    before_commit: bool,
}

#[test]
#[ignore = "subprocess entry; exercised by folder_process_exit_recovers_atomic_state_and_receipt"]
fn folder_apply_process_entry() {
    let owned = fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
    let request_path = PathBuf::from(std::env::var_os(REQUEST_ENV).expect("owned process request"));
    assert_eq!(
        fs::canonicalize(request_path.parent().unwrap()).unwrap(),
        owned
    );
    let request: Request = serde_json::from_slice(&fs::read(&request_path).unwrap()).unwrap();
    assert_eq!(
        fs::canonicalize(request.database.parent().unwrap()).unwrap(),
        owned
    );
    let mut store = SqliteStore::open(&request.database).unwrap();
    if request.before_commit {
        let transaction = store.begin_work_mutation().unwrap();
        let result = crate::storage::work::task_sync::folder::apply_task_sync_folder_on(
            &transaction,
            TaskSyncFolderApply {
                project: &project(),
                actor: &actor("folder-importer"),
                tree: &request.selected,
                export_basis: &request.retained_basis,
                profile: &profile(),
                complete: true,
                now: at(10),
            },
            KEY,
        )
        .unwrap();
        assert!(result.changed);
        let receipts: i64 = transaction
            .query_row(
                "SELECT COUNT(*) FROM work_operation_results
             WHERE operation = 'apply_task_sync_folder' AND idempotency_key = ?1",
                [KEY],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(receipts, 1);
        // The whole body, including baseline and receipt, ran. Commit did not.
        std::process::exit(77);
    }
    assert!(
        apply_basis(
            &mut store,
            &request.selected,
            &request.retained_basis,
            KEY,
            10
        )
        .unwrap()
        .changed
    );
    // Public apply committed, but no receipt is delivered to the caller.
    std::process::exit(78);
}

fn stored_receipt(store: &SqliteStore) -> TaskSyncImportResult {
    let bytes: Vec<u8> = store
        .connection
        .query_row(
            "SELECT result_json FROM work_operation_results
         WHERE operation = 'apply_task_sync_folder' AND idempotency_key = ?1",
            [KEY],
            |row| row.get(0),
        )
        .unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn bounded_output(command: &mut Command, directory: &Path) -> Output {
    let stdout = directory.join("child.stdout");
    let stderr = directory.join("child.stderr");
    let mut child = command
        .stdout(fs::File::create(&stdout).unwrap())
        .stderr(fs::File::create(&stderr).unwrap())
        .spawn()
        .unwrap();
    let started = Instant::now();
    // This generous deadline is a hang watchdog, not a timing assertion.
    let (status, expired) = loop {
        if let Some(status) = child.try_wait().unwrap() {
            break (status, false);
        }
        if started.elapsed() >= Duration::from_secs(120) {
            child.kill().unwrap();
            break (child.wait().unwrap(), true);
        }
        std::thread::sleep(Duration::from_millis(10));
    };
    let output = Output {
        status,
        stdout: fs::read(stdout).unwrap(),
        stderr: fs::read(stderr).unwrap(),
    };
    assert!(
        !expired,
        "owned child exceeded its 120-second watchdog\nstdout:\n{}\nstderr:\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    output
}

#[test]
fn folder_process_exit_recovers_atomic_state_and_receipt() {
    for before_commit in [true, false] {
        let home = crate::test_support::temp_home().unwrap();
        let database = home.path().join("folder.db");
        let mut store = SqliteStore::open(&database).unwrap();
        let fixture = mixed(&mut store);
        let sealed = query::load_work_item(&store.connection, fixture.sealed).unwrap();
        let before = state(&store);
        let old_objects = objects(&store);
        let native = native_rows(&store);
        let old_baselines = baselines(&store);
        let old_results = count(&store, "work_operation_results");
        drop(store);
        let request_path = home.path().join("request.json");
        fs::write(
            &request_path,
            serde_json::to_vec(&Request {
                database: database.clone(),
                selected: fixture.selected.clone(),
                retained_basis: fixture.retained_basis.clone(),
                before_commit,
            })
            .unwrap(),
        )
        .unwrap();
        let module = module_path!().split_once("::").unwrap().1;
        let output = bounded_output(
            Command::new(std::env::current_exe().unwrap())
                .current_dir(home.path())
                .env(REQUEST_ENV, &request_path)
                .args([
                    "--ignored",
                    "--exact",
                    &format!("{module}::folder_apply_process_entry"),
                    "--nocapture",
                ]),
            home.path(),
        );
        assert_eq!(
            output.status.code(),
            Some(if before_commit { 77 } else { 78 }),
            "{}\n{}",
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        let mut store = SqliteStore::open(&database).unwrap();
        if before_commit {
            assert_eq!(state(&store), before);
            assert!(
                apply_basis(
                    &mut store,
                    &fixture.selected,
                    &fixture.retained_basis,
                    KEY,
                    10
                )
                .unwrap()
                .changed
            );
        }
        assert_applied(&store, &fixture);
        assert_eq!(
            query::load_work_item(&store.connection, fixture.sealed).unwrap(),
            sealed
        );
        assert_eq!(native_rows(&store), native);
        let after_objects = objects(&store);
        for (id, bytes) in &old_objects {
            assert_eq!(after_objects.get(id), Some(bytes), "old object {id}");
        }
        assert_eq!(count(&store, "work_operation_results"), old_results + 1);
        assert_eq!(baselines(&store), old_baselines + 1);
        let receipt = stored_receipt(&store);
        assert!(receipt.changed && !receipt.dry_run);
        // A later local change is not undone by replay of the lost old reply.
        revise_title(&mut store, fixture.scalar, "later local work", 11);
        drop(store);
        let mut store = SqliteStore::open(&database).unwrap();
        let later = state(&store);
        assert_eq!(
            apply_basis(
                &mut store,
                &fixture.selected,
                &fixture.retained_basis,
                KEY,
                12
            )
            .unwrap(),
            receipt
        );
        assert_eq!(state(&store), later);
        assert!(store.verify_all().unwrap().is_healthy());
    }
}

#[test]
fn folder_commit_hook_refusal_rolls_back_the_complete_body() {
    use std::sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    };
    let home = crate::test_support::temp_home().unwrap();
    let database = home.path().join("commit-refusal.db");
    let mut store = SqliteStore::open(&database).unwrap();
    let fixture = mixed(&mut store);
    let before = state(&store);
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    store
        .connection
        .commit_hook(Some(move || {
            observed.fetch_add(1, Ordering::SeqCst);
            true
        }))
        .unwrap();
    let error = apply_basis(
        &mut store,
        &fixture.selected,
        &fixture.retained_basis,
        KEY,
        10,
    )
    .unwrap_err();
    assert!(
        matches!(error, StoreError::Sqlite(ref error)
        if error.sqlite_error().is_some_and(|sqlite| sqlite.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_COMMITHOOK)),
        "{error:#}"
    );
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    assert!(store.connection.is_autocommit());
    store.connection.commit_hook(None::<fn() -> bool>).unwrap();
    assert_eq!(state(&store), before);
    drop(store);
    let mut store = SqliteStore::open(&database).unwrap();
    assert_eq!(state(&store), before);
    apply_basis(
        &mut store,
        &fixture.selected,
        &fixture.retained_basis,
        KEY,
        10,
    )
    .unwrap();
    assert_applied(&store, &fixture);
}
