use super::*;
use engram::{
    ObjectId, ProjectId, project_database_path,
    task_sync::{
        FORMAT, ProjectDocument, TaskTree,
        notes::NoteOmissions,
        progress::{ExportCapture, ExportCut, ExportProfile, FeedAnchor, prepare},
    },
};

const DISABLED_PREPARATION: &str = concat!(
    "task sync is not enabled and no admitted pending preparation is available; ",
    "to start a new preparation, enable task sync with an explicit user instruction ",
    "(`engram task-sync enable --user-instruction \"<user's explicit request>\"`)"
);

#[test]
fn task_prepare_cli_pending_profile_guidance_preserves_operation_and_cut() {
    for enabled in [false, true] {
        let fixture = test_support::temp_home().unwrap();
        let project = ProjectId("pending-profile-cli".into());
        let marker = fixture.path().join(".engram-project");
        fs::write(&marker, format!("{}\n", project.0)).unwrap();
        let home = fixture.path().join("home");
        if enabled {
            task_sync_opt_in::enable(&home, &project.0);
        }
        let store = project_database_path(&home, &project);
        fs::create_dir_all(store.parent().unwrap()).unwrap();
        fs::write(&store, b"not SQLite; recovery must not capture").unwrap();
        let destination = fixture.path().join("candidate");
        let progress = fixture.path().join("progress");
        let tree = TaskTree {
            project: ProjectDocument {
                format: FORMAT.into(),
                project_id: project.clone(),
                notes_included: false,
            },
            tasks: Vec::new(),
        };
        let original = ExportProfile {
            include_notes: false,
            origin_machine: Some("host-a".into()),
        };
        let receipt = prepare(
            &destination,
            &progress,
            &store,
            &project,
            &original,
            false,
            |_| {
                Ok(ExportCapture {
                    tree,
                    omissions: NoteOmissions::default(),
                    activity_omissions: NoteOmissions::default(),
                    cut: ExportCut {
                        position: 9,
                        anchor: Some(FeedAnchor {
                            object_kind: "work_event".into(),
                            object_id: ObjectId::from_canonical_bytes(b"CLI pending cut"),
                        }),
                    },
                    full_reason: None,
                })
            },
        )
        .unwrap();
        let generation = fs::read_dir(&progress)
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| path.is_dir())
            .unwrap();
        fs::rename(
            generation.join("complete"),
            fixture.path().join("original-complete"),
        )
        .unwrap();
        let before = fixture_snapshot(fixture.path());
        let invoke = |flags: &[&str], rebaseline| {
            let mut invocation = command(&home, &marker);
            invocation
                .current_dir(fixture.path())
                .args([
                    "task-sync",
                    "--actor-id",
                    "operator",
                    "--session-id",
                    "operator-session",
                    "prepare",
                    "candidate",
                    "--progress-directory",
                    "progress",
                ])
                .args(flags);
            if rebaseline {
                invocation.arg("--rebaseline");
            }
            invocation.output().unwrap()
        };
        for flags in [
            &["--include-notes", "--origin-machine", "host-a"][..],
            &["--origin-machine", "host-b"][..],
            &[][..],
        ] {
            for rebaseline in [false, true] {
                let refused = invoke(flags, rebaseline);
                assert!(!refused.status.success());
                assert_eq!(refused.stdout, [] as [u8; 0]);
                let stderr = String::from_utf8_lossy(&refused.stderr);
                if !enabled && rebaseline {
                    assert!(
                        stderr.contains("task sync is not enabled; a new baseline cannot start"),
                        "{stderr}"
                    );
                    assert!(
                        stderr.contains(
                            "engram task-sync enable --user-instruction \"<user's explicit request>\""
                        ),
                        "{stderr}"
                    );
                    assert!(
                        stderr.contains(
                            "or rerun without --rebaseline to finish a pending preparation"
                        ),
                        "{stderr}"
                    );
                    assert!(!stderr.contains("original destination"), "{stderr}");
                } else {
                    for guidance in [
                        "original destination",
                        "original include-notes setting",
                        "origin-machine selection (including no origin-machine)",
                        "without --rebaseline",
                    ] {
                        assert!(stderr.contains(guidance), "{stderr}");
                    }
                    assert!(!stderr.contains("use --rebaseline to change it"));
                }
                assert_eq!(fixture_snapshot(fixture.path()), before);
            }
        }
        let recovered = invoke(&["--origin-machine", "host-a"], false);
        assert!(
            recovered.status.success(),
            "{}",
            String::from_utf8_lossy(&recovered.stderr)
        );
        let value: Value = serde_json::from_slice(&recovered.stdout).unwrap();
        assert_eq!(value["operation"], receipt.operation.to_string());
        assert_eq!(value["project_position"], receipt.project_position);
        assert_eq!(value["recovered"], true);
        assert_eq!(
            fs::read(generation.join("complete")).unwrap(),
            receipt.operation.to_string().as_bytes()
        );
        assert_eq!(
            fs::read(&store).unwrap(),
            b"not SQLite; recovery must not capture"
        );
    }
}

#[test]
fn task_prepare_cli_disabled_completed_history_without_rebaseline_guides_enable_without_mutation() {
    let fixture = test_support::temp_home().unwrap();
    let project = ProjectId("disabled-completed-history-cli".into());
    let marker = fixture.path().join(".engram-project");
    fs::write(&marker, format!("{}\n", project.0)).unwrap();
    let home = fixture.path().join("home");
    task_sync_opt_in::enable(&home, &project.0);
    let store = project_database_path(&home, &project);
    fs::create_dir_all(store.parent().unwrap()).unwrap();
    fs::write(&store, b"not SQLite; disabled recovery must not capture").unwrap();
    let destination = fixture.path().join("candidate");
    let progress = fixture.path().join("progress");
    let tree = TaskTree {
        project: ProjectDocument {
            format: FORMAT.into(),
            project_id: project.clone(),
            notes_included: false,
        },
        tasks: Vec::new(),
    };
    let profile = ExportProfile {
        include_notes: false,
        origin_machine: Some("host-a".into()),
    };
    prepare(
        &destination,
        &progress,
        &store,
        &project,
        &profile,
        false,
        |_| {
            Ok(ExportCapture {
                tree,
                omissions: NoteOmissions::default(),
                activity_omissions: NoteOmissions::default(),
                cut: ExportCut {
                    position: 9,
                    anchor: Some(FeedAnchor {
                        object_kind: "work_event".into(),
                        object_id: ObjectId::from_canonical_bytes(
                            b"CLI disabled completed history cut",
                        ),
                    }),
                },
                full_reason: None,
            })
        },
    )
    .unwrap();
    let disabled = command(&home, &marker)
        .args([
            "task-sync",
            "--actor-id",
            "operator",
            "--session-id",
            "operator-session",
            "disable",
        ])
        .output()
        .unwrap();
    assert!(
        disabled.status.success(),
        "{}",
        String::from_utf8_lossy(&disabled.stderr)
    );

    let before = fixture_snapshot(fixture.path());
    let refused = command(&home, &marker)
        .current_dir(fixture.path())
        .args([
            "task-sync",
            "--actor-id",
            "operator",
            "--session-id",
            "operator-session",
            "prepare",
            "candidate",
            "--progress-directory",
            "progress",
        ])
        .output()
        .unwrap();
    assert!(!refused.status.success());
    assert_eq!(refused.stdout, [] as [u8; 0]);
    let stderr = String::from_utf8_lossy(&refused.stderr);
    assert_eq!(stderr, format!("error: {DISABLED_PREPARATION}\n"));
    assert!(
        !stderr.contains("only the latest admitted pending preparation"),
        "{stderr}"
    );
    assert!(!stderr.contains("rerun without --rebaseline"), "{stderr}");
    assert_eq!(fixture_snapshot(fixture.path()), before);
    assert_eq!(
        fs::read(&store).unwrap(),
        b"not SQLite; disabled recovery must not capture"
    );
}
