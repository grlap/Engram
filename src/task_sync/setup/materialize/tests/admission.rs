use super::*;

#[test]
fn task_materialize_requires_new_opt_in_and_existing_lock_losers_write_nothing() {
    let fixture = Fixture::new(false, 1);
    fixture.disable();
    let before = snapshot(fixture.owned.path());
    let error = fixture
        .service
        .materialize_task_sync_setup(&fixture.marker, &fixture.selection)
        .unwrap_err();
    assert!(error.to_string().contains("not enabled"), "{error:#}");
    assert_eq!(snapshot(fixture.owned.path()), before);
    let fixture = Fixture::new(false, 1);
    let lock = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(fixture.admin.join("engram-task-sync-setup.lock"))
        .unwrap();
    let before = snapshot(fixture.owned.path());
    lock.try_lock().unwrap();
    let error = fixture
        .service
        .materialize_task_sync_setup(&fixture.marker, &fixture.selection)
        .unwrap_err();
    assert!(error.to_string().contains("busy"), "{error:#}");
    lock.unlock().unwrap();
    assert_eq!(snapshot(fixture.owned.path()), before);
}

#[test]
fn task_materialize_disclosure_refuses_whole_protected_tasks_without_quoting_values() {
    for label in ["restricted", "SeCrEt", "SECRET-REF"] {
        let mut tree = tree(2);
        tree.tasks[0].labels.push(label.into());
        tree.tasks[0].outcome = "sensitive-field-sentinel".into();
        let id = tree.tasks[0].id.0;
        let fixture = Fixture::with_tree(false, &tree);
        let before = snapshot(fixture.owned.path());
        let error = fixture
            .service
            .materialize_task_sync_setup(&fixture.marker, &fixture.selection)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("count=1") && error.contains(&id.to_string()),
            "{error}"
        );
        assert!(!error.contains("sensitive-field-sentinel"));
        assert_eq!(snapshot(fixture.owned.path()), before);
        assert!(!fixture.private().exists());
    }
}

#[test]
fn task_materialize_retained_legacy_note_and_activity_disclosure_are_rechecked() {
    for kind in ["note", "activity"] {
        let mut retained = tree(1);
        if kind == "note" {
            retained.project.notes_included = true;
            retained.tasks[0]
                .notes
                .push(crate::task_sync::notes::SharedNote {
                    id: crate::ObjectId::mint(),
                    kind: crate::task_sync::notes::SharedNoteKind::Generic,
                    body: "root=/private/sensitive-field-sentinel".into(),
                    author: crate::task_sync::SharedActor {
                        actor_id: "fixture-author".into(),
                        actor_kind: "agent".into(),
                    },
                    recorded_at: chrono::Utc::now(),
                    origin_machine: "fixture-origin".into(),
                });
        } else {
            let id = crate::ObjectId::mint();
            let task_id = retained.tasks[0].id;
            retained.tasks[0].activity.insert(
                "fixture-origin".into(),
                BTreeMap::from([(
                    id.clone(),
                    crate::task_sync::activity::ActivityNotice {
                        activity_id: id.clone(),
                        task_id,
                        origin_machine: "fixture-origin".into(),
                        actor: crate::task_sync::SharedActor {
                            actor_id: "C:\\private\\sensitive-field-sentinel".into(),
                            actor_kind: "agent".into(),
                        },
                        state: crate::task_sync::activity::ActivityState::Started,
                        state_revision: 1,
                        event_id: id,
                    },
                )]),
            );
        }
        let fixture = Fixture::with_tree(false, &retained);
        let before = snapshot(fixture.owned.path());
        let error = fixture
            .service
            .materialize_task_sync_setup(&fixture.marker, &fixture.selection)
            .unwrap_err()
            .to_string();
        assert!(error.contains("disclosure refused"), "{kind}: {error}");
        assert!(!error.contains("sensitive-field-sentinel"));
        assert_eq!(snapshot(fixture.owned.path()), before);
    }
}

#[test]
fn task_materialize_refuses_actual_filename_attribute_overrides_before_task_publication() {
    let fixture = Fixture::new(false, 1);
    fixture.interrupt("journal-published");
    let path = fixture
        .documents
        .keys()
        .find(|path| path.starts_with("tasks/"))
        .unwrap();
    fs::create_dir_all(fixture.admin.join("info")).unwrap();
    fs::write(
        fixture.admin.join("info/attributes"),
        format!(".engram/{path} filter=unsafe-driver\n"),
    )
    .unwrap();
    let error = fixture
        .service
        .materialize_task_sync_setup(&fixture.marker, &fixture.selection)
        .unwrap_err();
    assert!(
        error.to_string().contains("attribute policy refused"),
        "{error:#}"
    );
    assert!(!fixture.root.join(".engram").exists());
    assert!(fixture.private().join("journal.json").exists());
}

#[test]
fn task_materialize_refuses_repository_and_selection_drift_without_new_state() {
    for change in ["marker", "branch", "index", "operation"] {
        let mut fixture = Fixture::new(false, 1);
        match change {
            "marker" => fs::write(&fixture.marker, b"another-project\n").unwrap(),
            "branch" => {
                git(&fixture.root, &["checkout", "-b", "other"]);
            }
            "index" => {
                fs::write(fixture.root.join(".gitignore"), b"changed\n").unwrap();
                git(&fixture.root, &["add", ".gitignore"]);
            }
            "operation" => fixture.selection.prepared_operation = uuid::Uuid::now_v7(),
            _ => unreachable!(),
        }
        let before = snapshot(fixture.owned.path());
        assert!(
            fixture
                .service
                .materialize_task_sync_setup(&fixture.marker, &fixture.selection)
                .is_err(),
            "{change}"
        );
        assert_eq!(snapshot(fixture.owned.path()), before);
        assert!(!fixture.private().exists());
    }
}

#[test]
fn task_materialize_reports_linked_worktree_omission_without_enrollment_or_edits() {
    let fixture = Fixture::new(false, 1);
    let linked = fixture.owned.path().join("linked-worktree");
    git(
        &fixture.root,
        &[
            "worktree",
            "add",
            "-b",
            "linked-fixture",
            linked.to_str().unwrap(),
            "HEAD",
        ],
    );
    let before = snapshot(&linked);
    let receipt = fixture.run();
    assert_eq!(receipt.omitted_linked_worktrees, 1);
    assert_eq!(snapshot(&linked), before);
    assert!(!linked.join(".engram").exists());
    assert!(
        !fixture
            .admin
            .join("worktrees/linked-worktree/info/sparse-checkout")
            .exists()
    );
    fixture.assert_output();
}

#[test]
fn task_materialize_refuses_an_ignored_actual_task_filename_before_publication() {
    let fixture = Fixture::new(false, 1);
    let path = fixture
        .documents
        .keys()
        .find(|path| path.starts_with("tasks/"))
        .unwrap();
    let error = super::super::run(
        &fixture.marker,
        &ProjectId("materialize/fixture".into()),
        &fixture.selection,
        true,
        attribution(),
        &mut |point| {
            if point == "setup-linked-1" {
                // A concurrent rule appended after the managed block takes precedence.
                // info/exclude alone would lose to the root block's unignore rules.
                let ignore = fixture.root.join(".gitignore");
                let mut bytes = fs::read(&ignore)?;
                bytes.extend_from_slice(format!("/.engram/{path}\n").as_bytes());
                fs::write(ignore, bytes)?;
            }
            Ok(())
        },
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("shared task path is ignored"),
        "{error:#}"
    );
    assert!(!fixture.root.join(".engram").exists());
    assert_eq!(
        candidate::read(&Directory::open(&fixture.private().join("raw")).unwrap()).unwrap(),
        fixture.documents
    );
}
