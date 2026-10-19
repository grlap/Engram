use super::*;

#[test]
fn task_sync_directory_parent_lookup_only_checks_the_lexical_component() {
    // Both writers use this helper after resolving and inspecting the destination.
    // This is helper coverage; existing-root inspection may refuse or fail with I/O.
    let root = Path::new(std::path::MAIN_SEPARATOR_STR);
    let error = destination_parent(root).unwrap_err();
    assert_eq!(refusal_code(&error), "task_sync_destination_no_parent");
    assert_eq!(
        destination_parent(Path::new("parent/candidate")).unwrap(),
        Path::new("parent")
    );
}

#[test]
fn task_sync_directory_appeared_refusal_preserves_destination_and_cleans_staging() {
    let fixture = crate::test_support::temp_home().unwrap();
    let path = fixture.path().join("candidate");
    let tree = candidate();
    let mut checkpoints = 0;
    let error = write_with(&path, &tree, LIMITS, &mut |point, destination| {
        assert_eq!(point, "before-publish");
        checkpoints += 1;
        fs::create_dir(destination)?;
        fs::write(destination.join("sentinel"), b"owned")?;
        Ok(())
    })
    .unwrap_err();
    assert_eq!(checkpoints, 1);
    assert_eq!(refusal_code(&error), "task_sync_destination_appeared");
    assert_eq!(fs::read(path.join("sentinel")).unwrap(), b"owned");
    assert_eq!(fs::read_dir(fixture.path()).unwrap().count(), 1);
    let positive = fixture.path().join("positive");
    write_with(&positive, &tree, LIMITS, &mut |_, _| Ok(())).unwrap();
    assert_eq!(read(&positive).unwrap(), tree);
}

#[test]
fn task_sync_directory_file_growth_is_distinct_from_an_initially_oversized_file() {
    let fixture = crate::test_support::temp_home().unwrap();
    let path = fixture.path().join("file");
    let limits = Limits {
        file_bytes: 4,
        ..LIMITS
    };
    fs::write(&path, b"1234").unwrap();
    assert_eq!(
        read_file_with(&path, limits, &mut |_, _| Ok(())).unwrap(),
        b"1234"
    );
    let error = read_file_with(&path, limits, &mut |point, file| {
        use std::io::Write;
        assert_eq!(point, "before-file-read");
        fs::OpenOptions::new()
            .append(true)
            .open(file)?
            .write_all(b"5")?;
        Ok(())
    })
    .unwrap_err();
    assert_eq!(refusal_code(&error), "task_sync_destination_file_grew");
    let error = read_file_with(&path, limits, &mut |_, _| {
        panic!("metadata admission must refuse first")
    })
    .unwrap_err();
    assert_eq!(refusal_code(&error), "task_sync_destination_file_limit");
    assert_eq!(fs::read(&path).unwrap(), b"12345");
}

#[test]
fn task_sync_directory_read_and_export_enforce_task_and_tree_limits_at_the_boundary() {
    let fixture = crate::test_support::temp_home().unwrap();
    let path = fixture.path().join("candidate");
    let mut tree = candidate();
    let mut second = tree.tasks[0].clone();
    second.id = WorkId(uuid::Uuid::new_v4());
    tree.tasks.push(second);
    tree.tasks.sort_by_key(|task| task.id.0);
    let documents = tree.documents().unwrap();
    let total = documents.values().map(Vec::len).sum::<usize>();
    let limits = Limits {
        tasks: 2,
        tree_bytes: total,
        ..LIMITS
    };
    write_with(&path, &tree, limits, &mut |_, _| Ok(())).unwrap();
    assert_eq!(
        read_documents_with(&path, limits, true, &mut |_, _| Ok(())).unwrap(),
        documents
    );
    for (limits, code) in [
        (
            Limits { tasks: 1, ..limits },
            "task_sync_destination_task_limit",
        ),
        (
            Limits {
                tree_bytes: total - 1,
                ..limits
            },
            "task_sync_destination_tree_limit",
        ),
    ] {
        let error = read_documents_with(&path, limits, true, &mut |_, _| Ok(())).unwrap_err();
        assert_eq!(refusal_code(&error), code);
        let destination = fixture.path().join("refused");
        let error = write_with(&destination, &tree, limits, &mut |_, _| Ok(())).unwrap_err();
        assert_eq!(refusal_code(&error), code);
        assert!(!destination.exists());
        assert_eq!(fs::read_dir(fixture.path()).unwrap().count(), 1);
    }
    assert_eq!(read(&path).unwrap(), tree);
}

// APFS cannot create the invalid UTF-8 name this producer fixture needs.
#[cfg(any(windows, all(unix, not(target_vendor = "apple"))))]
#[test]
fn task_sync_directory_real_non_unicode_filename_has_its_own_refusal() {
    #[cfg(windows)]
    let name = {
        use std::os::windows::ffi::OsStringExt;
        std::ffi::OsString::from_wide(&[0xd800])
    };
    #[cfg(unix)]
    let name = {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(vec![0xff])
    };
    let fixture = crate::test_support::temp_home().unwrap();
    let path = fixture.path().join("candidate");
    let tree = candidate();
    write(&path, &tree).unwrap();
    let invalid = path.join("tasks").join(&name);
    fs::write(&invalid, b"not parsed")
        .expect("filesystem must support the non-Unicode filename fixture");
    assert!(
        fs::read_dir(path.join("tasks")).unwrap().any(|entry| entry
            .unwrap()
            .file_name()
            .into_string()
            .is_err())
    );
    let error = read(&path).unwrap_err();
    assert_eq!(refusal_code(&error), "task_sync_destination_non_utf8_name");
    fs::remove_file(invalid).unwrap();
    assert_eq!(read(&path).unwrap(), tree);
}

#[cfg(any(unix, windows))]
#[test]
fn task_sync_directory_real_file_symlink_is_refused_without_following_it() {
    let fixture = crate::test_support::temp_home().unwrap();
    let path = fixture.path().join("candidate");
    let tree = candidate();
    write(&path, &tree).unwrap();
    let task = path.join(format!("tasks/{}.json", tree.tasks[0].id.0));
    let target = fixture.path().join("retained.json");
    fs::rename(&task, &target).unwrap();
    let bytes = fs::read(&target).unwrap();
    #[cfg(windows)]
    let linked = std::os::windows::fs::symlink_file(&target, &task);
    #[cfg(unix)]
    let linked = std::os::unix::fs::symlink(&target, &task);
    linked.unwrap_or_else(|error| panic!("real file-symlink fixture unavailable (Windows requires Developer Mode or symlink privilege): {error}"));
    assert!(
        fs::symlink_metadata(&task)
            .unwrap()
            .file_type()
            .is_symlink()
    );
    let error = read(&path).unwrap_err();
    assert_eq!(refusal_code(&error), "task_sync_destination_link");
    assert_eq!(fs::read(&target).unwrap(), bytes);
    fs::remove_file(&task).unwrap();
    fs::copy(&target, &task).unwrap();
    assert_eq!(read(&path).unwrap(), tree);
}
