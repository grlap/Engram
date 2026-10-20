use super::*;

fn write_candidate(kind: &str, path: &Path, tree: &TaskTree) -> Result<()> {
    match kind {
        "ordinary" => write(path, tree),
        "retained" => write_documents(path, &tree.documents()?),
        "folder" => write_folder(path, tree, &profile()).map(|_| ()),
        _ => unreachable!(),
    }
}

#[test]
fn task_sync_directory_unsupported_publication_leaves_no_output_or_staging() {
    let fixture = crate::test_support::temp_home().unwrap();
    let path = fixture.path().join("candidate");
    let tree = candidate();
    for kind in ["ordinary", "retained", "folder"] {
        let error = crate::task_sync::publication::with_unsupported_native_move(|| {
            write_candidate(kind, &path, &tree)
        })
        .unwrap_err();
        assert_eq!(refusal_code(&error), "task_sync_publication_unsupported");
        assert!(!path.exists(), "{kind}");
        assert_eq!(fs::read_dir(fixture.path()).unwrap().count(), 0, "{kind}");
    }
    // The simulated capability refusal is local to these calls, not cached.
    write(&path, &tree).unwrap();
    assert_eq!(read(&path).unwrap(), tree);
}

#[test]
fn task_sync_directory_matching_retry_needs_no_native_move() {
    let fixture = crate::test_support::temp_home().unwrap();
    let tree = candidate();
    for kind in ["ordinary", "retained", "folder"] {
        let path = fixture.path().join(kind);
        write_candidate(kind, &path, &tree).unwrap();
        let original = read_raw_documents_with_tasks(&path, false).unwrap();
        crate::task_sync::publication::with_unsupported_native_move(|| {
            write_candidate(kind, &path, &tree)
        })
        .unwrap();
        assert_eq!(
            read_raw_documents_with_tasks(&path, false).unwrap(),
            original
        );
        assert_eq!(fs::read_dir(&path).unwrap().count(), 2);
    }
    assert_eq!(fs::read_dir(fixture.path()).unwrap().count(), 3);
}
