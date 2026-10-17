use super::*;

type Writer = fn(&Path, &TaskTree) -> Result<()>;

fn write_retained(path: &Path, tree: &TaskTree) -> Result<()> {
    write_documents(path, &tree.documents()?)
}

fn writers() -> [Writer; 2] {
    [write, write_retained]
}

#[test]
fn task_sync_directory_missing_disk_parent_is_io_for_both_writers() {
    for writer in writers() {
        let fixture = crate::test_support::temp_home().unwrap();
        let path = fixture.path().join("missing/candidate");
        let error = writer(&path, &candidate()).unwrap_err();
        assert!(
            error.downcast_ref::<std::io::Error>().is_some(),
            "{error:#}"
        );
        assert!(error.downcast_ref::<crate::storage::StoreError>().is_none());
        assert!(!path.parent().unwrap().exists());
        assert_eq!(fs::read_dir(fixture.path()).unwrap().count(), 0);
    }
}

#[test]
fn task_sync_directory_both_writers_preserve_ordinary_parent_candidates() {
    for writer in writers() {
        let fixture = crate::test_support::temp_home().unwrap();
        let path = fixture.path().join("candidate");
        let tree = candidate();
        writer(&path, &tree).unwrap();
        assert_eq!(read(&path).unwrap(), tree);
        let before = read_documents(&path).unwrap();
        writer(&path, &tree).unwrap();
        assert_eq!(read_documents(&path).unwrap(), before);

        let mut divergent = tree;
        divergent.tasks[0].title = "different".into();
        let error = writer(&path, &divergent).unwrap_err();
        assert_eq!(refusal_code(&error), "task_sync_destination_occupied");
        assert_eq!(read_documents(&path).unwrap(), before);
        assert_eq!(fs::read_dir(fixture.path()).unwrap().count(), 1);
    }
}

#[cfg(any(unix, windows))]
#[test]
fn task_sync_directory_existing_destination_precedes_linked_parent_admission() {
    for writer in writers() {
        let fixture = crate::test_support::temp_home().unwrap();
        let real = fixture.path().join("real");
        let link = fixture.path().join("link");
        fs::create_dir(&real).unwrap();
        let tree = candidate();
        let existing = real.join("candidate");
        writer(&existing, &tree).unwrap();
        let before = read_documents(&existing).unwrap();
        fs::write(real.join("sentinel"), b"owned file").unwrap();
        crate::test_support::make_dir_link(&real, &link);
        assert!(
            fs::symlink_metadata(&link)
                .unwrap()
                .file_type()
                .is_symlink()
        );

        // These competing refusals distinguish destination-first from parent-first.
        let error = writer(&link.join("sentinel"), &tree).unwrap_err();
        assert_eq!(refusal_code(&error), "task_sync_destination_not_directory");
        assert_eq!(fs::read(real.join("sentinel")).unwrap(), b"owned file");

        writer(&link.join("candidate"), &tree).unwrap();
        assert_eq!(read_documents(&existing).unwrap(), before);
        let mut divergent = tree.clone();
        divergent.tasks[0].title = "different".into();
        let error = writer(&link.join("candidate"), &divergent).unwrap_err();
        assert_eq!(refusal_code(&error), "task_sync_destination_occupied");
        assert_eq!(read_documents(&existing).unwrap(), before);

        let error = writer(&link.join("fresh"), &tree).unwrap_err();
        assert_eq!(refusal_code(&error), "task_sync_destination_link");
        assert!(!real.join("fresh").exists());
        assert_eq!(fs::read_dir(&real).unwrap().count(), 2);
        crate::test_support::remove_dir_link(&link);
        assert_eq!(fs::read_dir(fixture.path()).unwrap().count(), 1);
    }
}
