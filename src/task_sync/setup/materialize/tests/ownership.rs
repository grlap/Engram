use super::*;

#[test]
fn task_materialize_refuses_equal_independent_and_edited_public_files() {
    for path in [
        ".gitignore",
        ".gitattributes",
        ".engram/project.json",
        ".engram/tasks",
    ] {
        let fixture = Fixture::new(false, 1);
        fixture.run();
        let path = if path.ends_with("tasks") {
            fixture.root.join(
                fixture
                    .documents
                    .keys()
                    .find(|name| name.starts_with("tasks/"))
                    .map(|name| format!(".engram/{name}"))
                    .unwrap(),
            )
        } else {
            fixture.root.join(path)
        };
        let bytes = fs::read(&path).unwrap();
        // Move the linked file aside; recreate the same bytes as a different inode.
        fs::rename(&path, fixture.owned.path().join("retained-public-file")).unwrap();
        fs::write(&path, bytes).unwrap();
        let before = snapshot(fixture.owned.path());
        assert!(
            fixture
                .service
                .materialize_task_sync_setup(&fixture.marker, &fixture.selection)
                .is_err()
        );
        assert_eq!(snapshot(fixture.owned.path()), before);
    }
    let fixture = Fixture::new(false, 1);
    fixture.run();
    fs::write(
        fixture.root.join(".engram/project.json"),
        b"ordinary public edit",
    )
    .unwrap();
    let before = snapshot(fixture.owned.path());
    assert!(
        fixture
            .service
            .materialize_task_sync_setup(&fixture.marker, &fixture.selection)
            .is_err()
    );
    assert_eq!(snapshot(fixture.owned.path()), before);
    assert_eq!(
        candidate::read(&Directory::open(&fixture.private().join("raw")).unwrap()).unwrap(),
        fixture.documents
    );
}

#[test]
fn task_materialize_preserves_equal_unowned_empty_tasks_and_refuses_nonempty_expectations() {
    for count in [0, 1] {
        let fixture = Fixture::new(true, count);
        fixture.interrupt("journal-published");
        fs::create_dir(fixture.root.join(".engram/tasks")).unwrap();
        let id = Directory::open(&fixture.root.join(".engram/tasks"))
            .unwrap()
            .identity()
            .unwrap();
        let result = fixture
            .service
            .materialize_task_sync_setup(&fixture.marker, &fixture.selection);
        if count == 0 {
            assert!(result.unwrap().equal_unowned_empty_tasks);
            assert_shared(&fixture);
            assert!(fixture.run().equal_unowned_empty_tasks);
        } else {
            assert!(result.is_err());
        }
        assert_eq!(
            Directory::open(&fixture.root.join(".engram/tasks"))
                .unwrap()
                .identity()
                .unwrap(),
            id
        );
        assert!(
            fs::read_dir(fixture.root.join(".engram/tasks"))
                .unwrap()
                .next()
                .is_none()
        );
    }
}

#[test]
fn task_materialize_completed_empty_directory_observation_refuses_recreation() {
    let fixture = Fixture::new(true, 0);
    fixture.interrupt("journal-published");
    let public = fixture.root.join(".engram/tasks");
    fs::create_dir(&public).unwrap();
    assert!(fixture.run().equal_unowned_empty_tasks);
    fs::rename(&public, fixture.owned.path().join("observed-empty-tasks")).unwrap();
    fs::create_dir(&public).unwrap();
    let before = snapshot(fixture.owned.path());
    let error = fixture
        .service
        .materialize_task_sync_setup(&fixture.marker, &fixture.selection)
        .unwrap_err();
    assert!(error.to_string().contains("identity changed"), "{error:#}");
    assert_eq!(snapshot(fixture.owned.path()), before);
}

#[test]
fn task_materialize_repeat_requires_the_retained_displaced_before_file() {
    let fixture = Fixture::new(false, 1);
    fixture.run();
    fs::rename(
        fixture.private().join("replace-0/displaced"),
        fixture.owned.path().join("retained-before-file"),
    )
    .unwrap();
    let before = snapshot(fixture.owned.path());
    let error = fixture
        .service
        .materialize_task_sync_setup(&fixture.marker, &fixture.selection)
        .unwrap_err();
    assert!(
        error.to_string().contains("retained before file"),
        "{error:#}"
    );
    assert_eq!(snapshot(fixture.owned.path()), before);
}

#[cfg(unix)]
#[test]
fn task_materialize_refuses_symlink_public_and_retained_inputs_with_preservation() {
    use std::os::unix::fs::symlink;
    for retained in [false, true] {
        let fixture = Fixture::new(true, 1);
        fixture.interrupt("journal-published");
        let outside = fixture.owned.path().join("outside");
        if retained {
            let name = fixture
                .documents
                .keys()
                .find(|path| path.starts_with("tasks/"))
                .unwrap();
            let path = fixture.private().join("raw").join(name);
            fs::rename(&path, &outside).unwrap();
            symlink(&outside, &path).unwrap();
        } else {
            fs::create_dir(&outside).unwrap();
            fs::write(outside.join("sentinel"), b"preserve").unwrap();
            symlink(&outside, fixture.root.join(".engram/tasks")).unwrap();
        }
        let outside_before = if retained {
            fs::read(&outside).unwrap()
        } else {
            fs::read(outside.join("sentinel")).unwrap()
        };
        let result = fixture
            .service
            .materialize_task_sync_setup(&fixture.marker, &fixture.selection);
        assert!(result.is_err());
        let outside_after = if retained {
            fs::read(&outside).unwrap()
        } else {
            fs::read(outside.join("sentinel")).unwrap()
        };
        assert_eq!(outside_after, outside_before);
        assert!(!fixture.root.join(".engram/project.json").exists());
    }
}

#[test]
fn task_materialize_directory_race_preserves_nonempty_target() {
    for existing in [false, true] {
        let fixture = Fixture::new(existing, 1);
        let target = fixture
            .root
            .join(if existing { ".engram/tasks" } else { ".engram" });
        let boundary = if existing {
            "before-tasks-rename"
        } else {
            "before-engram-rename"
        };
        let result = super::super::run(
            &fixture.marker,
            &ProjectId("materialize/fixture".into()),
            &fixture.selection,
            true,
            attribution(),
            &mut |point| {
                if point == boundary {
                    fs::create_dir(&target)?;
                    fs::write(target.join("unowned.txt"), b"concurrent target bytes")?;
                }
                Ok(())
            },
        );
        assert!(result.is_err());
        assert_eq!(
            fs::read(target.join("unowned.txt")).unwrap(),
            b"concurrent target bytes"
        );
        assert!(fixture.private().join("stage").exists());
    }
}

#[cfg(windows)]
#[test]
fn task_materialize_refuses_junction_public_paths_without_following_them() {
    let fixture = Fixture::new(true, 1);
    fixture.interrupt("journal-published");
    let outside = fixture.owned.path().join("outside-tasks");
    fs::create_dir(&outside).unwrap();
    fs::write(outside.join("sentinel.txt"), b"preserve").unwrap();
    junction::create(&outside, fixture.root.join(".engram/tasks")).unwrap();
    let before = snapshot(fixture.owned.path());
    let outside_before = snapshot(&outside);
    assert!(
        fixture
            .service
            .materialize_task_sync_setup(&fixture.marker, &fixture.selection)
            .is_err()
    );
    // Only owned setup edits may have been applied before shared policy refusal.
    assert_eq!(fs::read(outside.join("sentinel.txt")).unwrap(), b"preserve");
    assert_eq!(snapshot(&outside), outside_before);
    assert!(before.contains_key(Path::new("repository/.engram/tasks")));
}
