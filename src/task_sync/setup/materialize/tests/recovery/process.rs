//! Real process exit and restart; injection exists only in this test executable.

use super::*;
use crate::task_sync::setup::files::file_identity;
use serde::{Deserialize, Serialize};

mod boundaries;
mod limits;

const REQUEST_ENV: &str = "ENGRAM_TEST_TASK_MATERIALIZE_PROCESS";

#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Request {
    marker: PathBuf,
    selection: MaterializeSelection,
    enabled: bool,
    interrupt: Option<String>,
    response: PathBuf,
    git_query: Option<Vec<u8>>,
    move_race: bool,
}

#[test]
#[ignore = "owned subprocess entrypoint; exercised by process recovery driver"]
fn task_materialize_process_entrypoint() {
    let request_path = PathBuf::from(std::env::var_os(REQUEST_ENV).expect("owned process request"));
    let owned = fs::canonicalize(std::env::current_dir().unwrap()).unwrap();
    assert_eq!(
        fs::canonicalize(request_path.parent().unwrap()).unwrap(),
        owned
    );
    let request: Request = serde_json::from_slice(&fs::read(&request_path).unwrap()).unwrap();
    assert_eq!(
        fs::canonicalize(request.response.parent().unwrap()).unwrap(),
        owned
    );
    for path in [
        &request.marker,
        &request.selection.repository,
        &request.selection.progress_directory,
        &request.selection.candidate_directory,
    ] {
        assert!(fs::canonicalize(path).unwrap().starts_with(&owned));
    }
    if let Some(input) = &request.git_query {
        let result = super::super::super::repository::git_input_allow_missing(
            &request.selection.repository,
            &["check-ignore", "--no-index", "-z", "--stdin"],
            input,
        );
        let body = match result {
            Ok(bytes) => serde_json::json!({"bytes": bytes}),
            Err(error) => serde_json::json!({"error": format!("{error:#}")}),
        };
        fs::write(request.response, serde_json::to_vec(&body).unwrap()).unwrap();
        return;
    }
    let result = super::super::super::run(
        &request.marker,
        &ProjectId("materialize/fixture".into()),
        &request.selection,
        request.enabled,
        attribution(),
        &mut |point| {
            if request.move_race && point == "before-move-0" {
                fs::write(
                    request.selection.repository.join(".gitignore"),
                    b"concurrent ordinary edit\n",
                )?;
            }
            if request.interrupt.as_deref() == Some(point) {
                // No Rust cleanup is run. OS exit releases locks and handles;
                // every operation source/marker already written stays on disk.
                std::process::exit(77);
            }
            Ok(())
        },
    );
    let body = match result {
        Ok(receipt) => serde_json::json!({"receipt": receipt}),
        Err(error) => serde_json::json!({"error": format!("{error:#}")}),
    };
    fs::write(request.response, serde_json::to_vec(&body).unwrap()).unwrap();
}

fn child(fixture: &Fixture, boundary: Option<&str>, enabled: bool) -> std::process::Output {
    child_with_race(fixture, boundary, enabled, false)
}

fn child_with_race(
    fixture: &Fixture,
    boundary: Option<&str>,
    enabled: bool,
    move_race: bool,
) -> std::process::Output {
    let suffix = boundary.unwrap_or("finish");
    let request_path = fixture.owned.path().join(format!("process-{suffix}.json"));
    let response = fixture
        .owned
        .path()
        .join(format!("process-{suffix}-response.json"));
    let request = Request {
        marker: fixture.marker.clone(),
        selection: fixture.selection.clone(),
        enabled,
        interrupt: boundary.map(str::to_owned),
        response,
        git_query: None,
        move_race,
    };
    fs::write(&request_path, serde_json::to_vec(&request).unwrap()).unwrap();
    entry_command(fixture, &request_path).output().unwrap()
}

fn entry_command(fixture: &Fixture, request_path: &Path) -> Command {
    // Rust's harness lists module paths without the crate-name component.
    let module = module_path!().split_once("::").unwrap().1;
    let mut command = Command::new(std::env::current_exe().unwrap());
    command
        .current_dir(fixture.owned.path())
        .env("HOME", fixture.owned.path())
        .env("XDG_CONFIG_HOME", fixture.owned.path())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env_remove("GIT_CONFIG")
        .env_remove("GIT_ATTR_SOURCE")
        .env(REQUEST_ENV, request_path)
        .env(
            "GIT_CONFIG_GLOBAL",
            fixture.owned.path().join("global.config"),
        )
        .args([
            "--ignored",
            "--exact",
            &format!("{module}::task_materialize_process_entrypoint"),
            "--nocapture",
        ]);
    command
}

#[test]
fn task_materialize_ignore_query_treats_glob_characters_as_literal_pathnames() {
    let fixture = Fixture::new(true, 0);
    let tasks = fixture.root.join(".engram/tasks");
    fs::create_dir(&tasks).unwrap();
    fs::write(tasks.join("[ab].json"), b"ordinary synthetic pathname").unwrap();
    fs::write(tasks.join("a.json"), b"separately ignored pathname").unwrap();
    fs::write(fixture.root.join(".gitignore"), b"/.engram/tasks/a.json\n").unwrap();
    // These are synthetic helper inputs. Real exported task names are UUID.json
    // and cannot contain glob characters; their format admission stays strict.
    let response = fixture.owned.path().join("query-response.json");
    let request_path = fixture.owned.path().join("query-request.json");
    let request = Request {
        marker: fixture.marker.clone(),
        selection: fixture.selection.clone(),
        enabled: false,
        interrupt: None,
        response: response.clone(),
        git_query: Some(b".engram/tasks/[ab].json\0.engram/tasks/a.json\0".to_vec()),
        move_race: false,
    };
    fs::write(&request_path, serde_json::to_vec(&request).unwrap()).unwrap();
    let output = entry_command(&fixture, &request_path)
        .env("GIT_LITERAL_PATHSPECS", "1")
        .env("GIT_GLOB_PATHSPECS", "1")
        .env("GIT_NOGLOB_PATHSPECS", "1")
        .env("GIT_ICASE_PATHSPECS", "1")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    let body: serde_json::Value = serde_json::from_slice(&fs::read(response).unwrap()).unwrap();
    assert!(body.get("error").is_none(), "{body}");
    let bytes: Vec<u8> = serde_json::from_value(body["bytes"].clone()).unwrap();
    assert_eq!(bytes, b".engram/tasks/a.json\0");
}

#[test]
fn task_materialize_process_exit_recovers_every_publication_phase_from_retained_bytes() {
    for existing in [false, true] {
        let mut boundaries = vec![
            "journal-published",
            "replacement-ready-0",
            "before-move-0",
            "moved-0",
            "before-setup-link-0",
            "setup-linked-0",
            "replacement-ready-1",
            "before-move-1",
            "moved-1",
            "before-setup-link-1",
            "setup-linked-1",
            "setup-checked",
            "public-checked",
            "complete-pending-created",
            "complete-pending-prefix",
            "complete-pending-synced",
            "complete-published",
        ];
        if existing {
            boundaries.extend([
                "before-tasks-rename",
                "tasks-renamed",
                "before-project-link",
                "project-linked",
            ]);
        } else {
            boundaries.extend(["before-engram-rename", "engram-renamed"]);
        }
        for boundary in boundaries {
            let fixture = Fixture::new(existing, 1);
            let first = child(&fixture, Some(boundary), true);
            assert_eq!(
                first.status.code(),
                Some(77),
                "{existing}/{boundary}: stdout={} stderr={}",
                String::from_utf8_lossy(&first.stdout),
                String::from_utf8_lossy(&first.stderr)
            );
            fixture.disable();
            let second = child(&fixture, None, false);
            assert!(
                second.status.success(),
                "{}",
                String::from_utf8_lossy(&second.stderr)
            );
            let response: serde_json::Value = serde_json::from_slice(
                &fs::read(fixture.owned.path().join("process-finish-response.json")).unwrap(),
            )
            .unwrap();
            assert!(
                response.get("error").is_none(),
                "{existing}/{boundary}: {response}"
            );
            let completion: serde_json::Value =
                serde_json::from_slice(&fs::read(fixture.private().join("complete")).unwrap())
                    .unwrap();
            assert_eq!(completion["operation"], response["receipt"]["operation"]);
            fixture.assert_output();
        }
    }
}

#[test]
fn task_materialize_process_recovers_partial_completion_and_interrupted_repair_after_opt_out() {
    for partial_length in [0, 1] {
        for repair_boundary in [
            "complete-repair-started",
            "complete-repair-prefix",
            "complete-repair-synced",
        ] {
            let fixture = Fixture::new(true, 1);
            assert_eq!(
                child(&fixture, Some("public-checked"), true).status.code(),
                Some(77)
            );
            let private = fixture.private();
            let admitted: serde_json::Value =
                serde_json::from_slice(&fs::read(private.join("journal.json")).unwrap()).unwrap();
            let operation = serde_json::from_value(admitted["operation"].clone()).unwrap();
            let public_identity = super::super::super::journal::Completion::observe(
                &Directory::open(&fixture.root).unwrap(),
                operation,
                false,
            )
            .unwrap();
            let mut expected = serde_json::to_vec(&public_identity).unwrap();
            expected.push(b'\n');
            let length = if partial_length == 0 {
                0
            } else {
                expected.len() / 2
            };
            fs::write(private.join(".complete.pending"), &expected[..length]).unwrap();
            let original = snapshot(&private.join(".complete.pending"));
            let partial = fs::read(private.join(".complete.pending")).unwrap();
            let original_id =
                file_identity(&fs::File::open(private.join(".complete.pending")).unwrap()).unwrap();
            let public = snapshot(&fixture.root.join(".engram"));
            let setup = [".gitignore", ".gitattributes"].map(|name| {
                (
                    fs::read(fixture.root.join(name)).unwrap(),
                    file_identity(&fs::File::open(fixture.root.join(name)).unwrap()).unwrap(),
                )
            });
            fixture.disable();
            assert_eq!(
                child(&fixture, Some(repair_boundary), false).status.code(),
                Some(77)
            );
            assert_eq!(
                fs::read(private.join(".complete.pending")).unwrap(),
                partial
            );
            assert_eq!(
                file_identity(&fs::File::open(private.join(".complete.pending")).unwrap()).unwrap(),
                original_id
            );
            assert_eq!(snapshot(&private.join(".complete.pending")), original);
            assert!(child(&fixture, None, false).status.success());
            let response: serde_json::Value = serde_json::from_slice(
                &fs::read(fixture.owned.path().join("process-finish-response.json")).unwrap(),
            )
            .unwrap();
            assert!(
                response.get("error").is_none(),
                "{partial_length}/{repair_boundary}: {response}"
            );
            assert_eq!(response["receipt"]["operation"], admitted["operation"]);
            assert_eq!(snapshot(&fixture.root.join(".engram")), public);
            assert_eq!(
                super::super::super::journal::Completion::observe(
                    &Directory::open(&fixture.root).unwrap(),
                    operation,
                    false
                )
                .unwrap(),
                public_identity
            );
            for (index, name) in [".gitignore", ".gitattributes"].iter().enumerate() {
                assert_eq!(fs::read(fixture.root.join(name)).unwrap(), setup[index].0);
                assert_eq!(
                    file_identity(&fs::File::open(fixture.root.join(name)).unwrap()).unwrap(),
                    setup[index].1
                );
            }
            assert_eq!(
                fs::read(private.join(".complete.pending")).unwrap(),
                partial
            );
            assert_eq!(
                file_identity(&fs::File::open(private.join("complete")).unwrap()).unwrap(),
                file_identity(&fs::File::open(private.join("completion-recovery/source")).unwrap())
                    .unwrap()
            );
            fixture.assert_output();
            let before = snapshot(&private);
            assert!(fixture.run().repeated);
            assert_eq!(snapshot(&private), before);
        }
    }
}
