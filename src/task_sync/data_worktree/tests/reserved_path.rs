use super::*;
use crate::task_sync::{TaskTree, directory, progress::ExportProfile};

fn export() -> (TaskTree, ExportProfile) {
    (
        TaskTree {
            project: crate::task_sync::ProjectDocument {
                format: crate::task_sync::FORMAT.into(),
                project_id: crate::ProjectId("data/fixture".into()),
                notes_included: false,
            },
            tasks: Vec::new(),
        },
        ExportProfile {
            include_notes: false,
            origin_machine: None,
        },
    )
}

fn initialized() -> (Fixture, DataWorktree, StagedCommitRequest) {
    let fixture = Fixture::code_only(true);
    initialize(
        &fixture.code.join(".engram-project"),
        &crate::ProjectId("data/fixture".into()),
        "engram-data",
    )
    .unwrap();
    let data = DataWorktree::observe(&fixture.code, &fixture.data, "engram-data").unwrap();
    let (tree, profile) = export();
    directory::write_data_folder(&data, &tree, &profile).unwrap();
    run(&fixture.data, &["add", ".engram"]);
    let request = StagedCommitRequest {
        operation: uuid::Uuid::new_v4(),
        tree: oid_line(&run(&fixture.data, &["write-tree"])).unwrap(),
        expected_head: None,
        parents: Vec::new(),
        author_name: "Data fixture".into(),
        author_email: "data@example.invalid".into(),
        authored_at: chrono::DateTime::from_timestamp(0, 0).unwrap(),
        message: "Retain original task data\n".into(),
    };
    (fixture, data, request)
}

fn assert_refused_after_tracking(committed: bool, gitlink: bool) {
    let (fixture, data, request) = initialized();
    let retained = data.create_retained(&request).unwrap();
    let (mode, oid, path) = if gitlink {
        ("160000", retained.oid.clone(), DIRECTORY_NAME.to_owned())
    } else {
        (
            "100644",
            oid_line(&run(&fixture.code, &["rev-parse", "HEAD:code.rs"])).unwrap(),
            format!("{DIRECTORY_NAME}/foreign.rs"),
        )
    };
    run(
        &fixture.code,
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("{mode},{oid},{path}"),
        ],
    );
    if committed {
        run(&fixture.code, &["commit", "-m", "Track reserved path"]);
        run(&fixture.code, &["update-index", "--force-remove", &path]);
        assert_eq!(
            run(&fixture.code, &["ls-files", "--", DIRECTORY_NAME]),
            [] as [u8; 0]
        );
    }
    let code_before = fixture.code_snapshot();
    let data_index = fs::read(data.git_directory.join("index")).unwrap();
    let data_head = fs::read(data.git_directory.join("HEAD")).unwrap();
    let refs = run(&fixture.data, &["for-each-ref"]);
    let objects = run(
        &fixture.data,
        &[
            "cat-file",
            "--batch-all-objects",
            "--batch-check=%(objectname)",
        ],
    );
    let (tree, profile) = export();
    let project_bytes = fs::read(fixture.data.join(".engram/project.json")).unwrap();
    let expected = if committed {
        "code HEAD tracks"
    } else {
        "code index tracks"
    };
    assert!(
        DataWorktree::observe(&fixture.code, &fixture.data, "engram-data")
            .unwrap_err()
            .to_string()
            .contains(expected)
    );
    assert!(
        data.revalidate()
            .unwrap_err()
            .to_string()
            .contains(expected)
    );
    assert!(
        initialize(
            &fixture.code.join(".engram-project"),
            &crate::ProjectId("data/fixture".into()),
            "engram-data",
        )
        .unwrap_err()
        .to_string()
        .contains(expected)
    );
    assert!(
        directory::read_resolved_data_folder(&data)
            .unwrap_err()
            .to_string()
            .contains(expected)
    );
    assert!(
        directory::write_data_folder(&data, &tree, &profile)
            .unwrap_err()
            .to_string()
            .contains(expected)
    );
    assert!(
        data.create_retained(&request)
            .unwrap_err()
            .to_string()
            .contains(expected)
    );
    assert!(
        data.advance_retained(&retained, None)
            .unwrap_err()
            .to_string()
            .contains(expected)
    );
    assert_eq!(fixture.code_snapshot(), code_before);
    assert_eq!(
        fs::read(data.git_directory.join("index")).unwrap(),
        data_index
    );
    assert_eq!(
        fs::read(data.git_directory.join("HEAD")).unwrap(),
        data_head
    );
    assert_eq!(run(&fixture.data, &["for-each-ref"]), refs);
    assert_eq!(
        run(
            &fixture.data,
            &[
                "cat-file",
                "--batch-all-objects",
                "--batch-check=%(objectname)"
            ]
        ),
        objects
    );
    assert_eq!(
        fs::read(fixture.data.join(".engram/project.json")).unwrap(),
        project_bytes
    );
}

#[test]
fn indexed_reserved_gitlinks_and_descendants_refuse_data_operations() {
    for gitlink in [true, false] {
        assert_refused_after_tracking(false, gitlink);
    }
}

#[test]
fn committed_reserved_paths_refuse_even_after_staged_index_removal() {
    for gitlink in [true, false] {
        assert_refused_after_tracking(true, gitlink);
    }
}
