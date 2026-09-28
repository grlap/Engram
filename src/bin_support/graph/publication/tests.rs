use super::*;
use std::{
    cell::{Cell, RefCell},
    fs,
};

const SNAPSHOT: &[u8] =
    br#"{"body":{},"manifest":{"exported_at":"now","exporting_build":"build"}}"#;

fn fixture(root: &Path) -> (PathBuf, PathBuf) {
    let database = root.join("projects/digest/engram.db");
    fs::create_dir_all(database.parent().unwrap()).unwrap();
    fs::write(&database, b"store-before").unwrap();
    fs::write(database.with_extension("db-wal"), b"wal-before").unwrap();
    (database, root.join("exports/nested/snapshot.json"))
}

enum LinkBehavior {
    Publish,
    Unsupported,
    Competing,
}

struct Faults {
    link: LinkBehavior,
    cleanup_fails: bool,
    sync_fails: bool,
    synced: Cell<bool>,
    warnings: RefCell<Vec<String>>,
}
impl Publication for Faults {
    fn link(&self, parent: &Dir, stage: &Path, name: &Path) -> io::Result<()> {
        if matches!(self.link, LinkBehavior::Unsupported) {
            return Err(io::Error::new(
                io::ErrorKind::Unsupported,
                "filesystem has no hard links",
            ));
        }
        if matches!(self.link, LinkBehavior::Competing) {
            parent.hard_link(stage, parent, name)?;
        }
        parent.hard_link(stage, parent, name)
    }
    fn cleanup(&self, parent: &Dir, stage: &Path) -> io::Result<()> {
        if self.cleanup_fails {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "stage held open",
            ));
        }
        parent.remove_file(stage)
    }
    fn warning(&self, warning: &str) {
        self.warnings.borrow_mut().push(warning.into());
    }
    fn sync(&self, _: &Dir) -> io::Result<()> {
        self.synced.set(true);
        if self.sync_fails {
            Err(io::Error::other("directory sync unavailable"))
        } else {
            Ok(())
        }
    }
}

#[test]
fn cleanup_warning_preserves_saved_and_competing_success_and_still_syncs() {
    for competing in [false, true] {
        let directory = crate::test_support::temp_home().unwrap();
        let (database, output) = fixture(directory.path());
        let faults = Faults {
            link: if competing {
                LinkBehavior::Competing
            } else {
                LinkBehavior::Publish
            },
            cleanup_fails: true,
            sync_fails: false,
            synced: Cell::new(false),
            warnings: RefCell::new(Vec::new()),
        };
        let outcome = write_with(&database, &output, SNAPSHOT, &faults).unwrap();
        assert_eq!(
            outcome,
            if competing {
                GraphSnapshotWriteOutcome::AlreadySaved
            } else {
                GraphSnapshotWriteOutcome::Saved
            }
        );
        assert_eq!(fs::read(&output).unwrap(), SNAPSHOT);
        assert!(faults.synced.get());
        let warnings = faults.warnings.borrow();
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("snapshot saved"));
        assert!(warnings[0].contains("stage held open"));
        let stage = fs::read_dir(output.parent().unwrap())
            .unwrap()
            .map(|entry| entry.unwrap().path())
            .find(|path| {
                path.file_name()
                    .unwrap()
                    .to_string_lossy()
                    .starts_with(".graph-save-")
            })
            .unwrap();
        assert!(
            warnings[0].contains(&std::path::absolute(&stage).unwrap().display().to_string()),
            "{}",
            warnings[0]
        );
    }
}

#[test]
fn unsupported_link_is_named_and_published_sync_failure_is_distinct() {
    for unsupported in [true, false] {
        let directory = crate::test_support::temp_home().unwrap();
        let (database, output) = fixture(directory.path());
        let faults = Faults {
            link: if unsupported {
                LinkBehavior::Unsupported
            } else {
                LinkBehavior::Publish
            },
            cleanup_fails: false,
            sync_fails: true,
            synced: Cell::new(false),
            warnings: RefCell::new(Vec::new()),
        };
        let error = write_with(&database, &output, SNAPSHOT, &faults).unwrap_err();
        let message = format!("{error:#}");
        if unsupported {
            assert!(message.contains("hard-link support"));
            assert!(message.contains("filesystem has no hard links"));
            assert!(!output.exists());
            assert!(!faults.synced.get());
        } else {
            assert!(message.contains("snapshot published"));
            assert!(message.contains("durability is not confirmed"));
            assert!(
                message.contains(&std::path::absolute(&output).unwrap().display().to_string()),
                "{message}"
            );
            assert_eq!(fs::read(&output).unwrap(), SNAPSHOT);
        }
        assert!(
            fs::read_dir(output.parent().unwrap())
                .unwrap()
                .all(|entry| !entry
                    .unwrap()
                    .file_name()
                    .to_string_lossy()
                    .starts_with(".graph-save-"))
        );
    }
}

#[cfg(any(unix, windows))]
fn directory_link(target: &Path, link: &Path) {
    #[cfg(unix)]
    std::os::unix::fs::symlink(target, link).unwrap();
    #[cfg(windows)]
    junction::create(target, link).unwrap();
}

#[test]
fn publication_preserves_permission_errors_and_cleanup_warning_does_not_hide_sync_error() {
    struct Denied;
    impl Publication for Denied {
        fn link(&self, _: &Dir, _: &Path, _: &Path) -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "link permission denied",
            ))
        }
    }
    let directory = crate::test_support::temp_home().unwrap();
    let (database, output) = fixture(directory.path());
    let error = write_with(&database, &output, SNAPSHOT, &Denied).unwrap_err();
    assert_eq!(
        error.downcast_ref::<io::Error>().unwrap().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert!(format!("{error:#}").contains("link permission denied"));
    assert!(!output.exists());
    let faults = Faults {
        link: LinkBehavior::Publish,
        cleanup_fails: true,
        sync_fails: true,
        synced: Cell::new(false),
        warnings: RefCell::new(Vec::new()),
    };
    let error = write_with(&database, &output, SNAPSHOT, &faults).unwrap_err();
    assert!(format!("{error:#}").contains("durability is not confirmed"));
    assert!(faults.synced.get());
    assert_eq!(faults.warnings.borrow().len(), 1);
    assert_eq!(fs::read(output).unwrap(), SNAPSHOT);
}

#[cfg(windows)]
#[test]
fn windows_destination_syntax_is_explicitly_bounded() {
    for path in [
        r"\\server\share\snapshot.json",
        r"\\.\C:\snapshot.json",
        r"C:\export\file:stream",
        r"C:\export\file.",
        r"C:\export\file ",
        r"\\?\C:\export\projects/digest/snapshot.json",
        r"\\?\C:\export\nested/../snapshot.json",
        r"C:\export\CON",
        r"C:\export\nul.json",
        r"C:\export\PrN.json",
        r"C:\export\aux\snapshot.json",
        r"C:\export\COM1.txt",
        r"C:\export\Lpt9",
        r"C:\export\COM¹.json",
        r"C:\export\COM0.json",
        r"C:\export\LPT0",
        r"C:\export\CONIN$",
        r"C:\export\conout$.json",
        r"C:\export\NUL .json",
    ] {
        assert!(destination_components(Path::new(path)).is_err(), "{path}");
    }
    assert_eq!(
        destination_components(Path::new(r"\\?\C:\export\snapshot.json")).unwrap(),
        destination_components(Path::new(r"C:\export\snapshot.json")).unwrap()
    );
}

#[cfg(any(unix, windows))]
#[test]
fn ancestor_swap_after_binding_cannot_publish_into_project_stores() {
    struct Swap {
        ancestor: PathBuf,
        moved: PathBuf,
        projects: PathBuf,
        called: Cell<bool>,
    }
    impl Publication for Swap {
        fn bound(&self) -> Result<()> {
            self.called.set(true);
            match fs::rename(&self.ancestor, &self.moved) {
                Ok(()) => directory_link(&self.projects, &self.ancestor),
                #[cfg(windows)]
                Err(error) if matches!(error.raw_os_error(), Some(5 | 32)) => (),
                Err(error) => return Err(error.into()),
            }
            Ok(())
        }
    }
    let directory = crate::test_support::temp_home().unwrap();
    let (database, output) = fixture(directory.path());
    let swap = Swap {
        ancestor: directory.path().join("exports"),
        moved: directory.path().join("original-exports"),
        projects: directory.path().join("projects"),
        called: Cell::new(false),
    };
    write_with(&database, &output, SNAPSHOT, &swap).unwrap();
    assert!(swap.called.get());
    assert!(!swap.projects.join("nested").exists());
    assert_eq!(fs::read(&database).unwrap(), b"store-before");
    assert_eq!(
        fs::read(database.with_extension("db-wal")).unwrap(),
        b"wal-before"
    );
    let actual = if swap.moved.exists() {
        swap.moved.join("nested/snapshot.json")
    } else {
        output
    };
    assert_eq!(fs::read(actual).unwrap(), SNAPSHOT);
}

#[cfg(any(unix, windows))]
#[test]
fn existing_ancestor_link_is_refused_before_staging() {
    let directory = crate::test_support::temp_home().unwrap();
    let (database, output) = fixture(directory.path());
    directory_link(
        &directory.path().join("projects"),
        &directory.path().join("exports"),
    );
    let error = write_graph_snapshot_file(&database, &output, SNAPSHOT).unwrap_err();
    let message = format!("{error:#}");
    let ancestor = std::path::absolute(directory.path().join("exports")).unwrap();
    assert!(
        message.contains("cannot bind snapshot ancestor"),
        "{message}"
    );
    assert!(
        message.contains(&ancestor.display().to_string()),
        "{message}"
    );
    assert!(
        message.contains("pass the resolved real directory path instead"),
        "{message}"
    );
    assert!(!directory.path().join("projects/nested").exists());
    assert_eq!(fs::read(&database).unwrap(), b"store-before");
}

#[cfg(windows)]
#[test]
fn protected_directory_identity_is_case_independent() {
    let directory = crate::test_support::temp_home().unwrap();
    let (database, _) = fixture(directory.path());
    let output = directory.path().join("PROJECTS/digest/snapshot.json");
    let error = write_graph_snapshot_file(&database, &output, SNAPSHOT).unwrap_err();
    assert!(format!("{error:#}").contains("outside Engram's project stores"));
    assert!(!output.exists());
}

#[cfg(windows)]
#[test]
fn verbatim_embedded_separators_are_refused_before_binding_or_staging() {
    struct MustNotBind;
    impl Publication for MustNotBind {
        fn bound(&self) -> Result<()> {
            panic!("invalid destination reached the staging boundary");
        }
    }
    let directory = crate::test_support::temp_home().unwrap();
    let (database, _) = fixture(directory.path());
    let real_home = fs::canonicalize(directory.path()).unwrap();
    for tail in [
        "projects/digest/snapshot.json",
        "exports/../projects/digest/snapshot.json",
    ] {
        let output = PathBuf::from(format!("{}\\{tail}", real_home.display()));
        let error = write_with(&database, &output, SNAPSHOT, &MustNotBind).unwrap_err();
        assert!(format!("{error:#}").contains("unsupported Windows path component"));
    }
    assert_eq!(fs::read(&database).unwrap(), b"store-before");
    assert_eq!(
        fs::read(database.with_extension("db-wal")).unwrap(),
        b"wal-before"
    );
    assert_eq!(fs::read_dir(database.parent().unwrap()).unwrap().count(), 2);
    assert_eq!(fs::read_dir(directory.path()).unwrap().count(), 1);
}

#[cfg(any(unix, windows))]
#[test]
fn protected_store_identity_is_checked_at_the_filesystem_root() {
    let directory = crate::test_support::temp_home().unwrap();
    let absolute = fs::canonicalize(directory.path()).unwrap();
    let root = absolute.ancestors().last().unwrap();
    directory_link(root, &directory.path().join("projects"));
    let database = directory.path().join("projects/digest/engram.db");
    // Inspect only: even if the guard regresses this test never creates a file
    // or directory outside its fixture. There are no parent names to create.
    let result = Destination::open(&database, &root.join("snapshot.json"));
    let error = result.err().expect("filesystem root must be refused");
    assert!(format!("{error:#}").contains("outside Engram's project stores"));
    // Remove the link itself so fixture cleanup never traverses its target.
    #[cfg(windows)]
    fs::remove_dir(directory.path().join("projects")).unwrap();
    #[cfg(unix)]
    fs::remove_file(directory.path().join("projects")).unwrap();
}

#[test]
fn disappearing_competitor_has_publication_context_and_cleans_stage() {
    struct Disappeared;
    impl Publication for Disappeared {
        fn link(&self, _: &Dir, _: &Path, _: &Path) -> io::Result<()> {
            Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                "competitor won",
            ))
        }
    }
    let directory = crate::test_support::temp_home().unwrap();
    let (database, output) = fixture(directory.path());
    let error = write_with(&database, &output, SNAPSHOT, &Disappeared).unwrap_err();
    assert!(format!("{error:#}").contains("no-replace publication race"));
    assert_eq!(
        error.downcast_ref::<io::Error>().unwrap().kind(),
        io::ErrorKind::AlreadyExists
    );
    assert_eq!(fs::read_dir(output.parent().unwrap()).unwrap().count(), 0);
}

#[cfg(unix)]
#[test]
fn unix_real_filesystem_publication_syncs_and_preserves_private_mode() {
    use std::os::unix::fs::PermissionsExt;
    let directory = crate::test_support::temp_home().unwrap();
    let real_home = fs::canonicalize(directory.path()).unwrap();
    let (database, output) = fixture(&real_home);
    assert_eq!(
        write_graph_snapshot_file(&database, &output, SNAPSHOT).unwrap(),
        GraphSnapshotWriteOutcome::Saved
    );
    assert_eq!(fs::read(&output).unwrap(), SNAPSHOT);
    assert_eq!(
        fs::metadata(&output).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(fs::read_dir(output.parent().unwrap()).unwrap().count(), 1);
    assert_eq!(
        write_graph_snapshot_file(&database, &output, SNAPSHOT).unwrap(),
        GraphSnapshotWriteOutcome::AlreadySaved
    );
}

#[cfg(unix)]
#[test]
fn unix_parent_components_are_refused_instead_of_resolved_lexically() {
    let error = destination_components(Path::new("/parent/link/../snapshot.json")).unwrap_err();
    assert!(error.to_string().contains("parent-directory components"));
}

#[cfg(windows)]
#[test]
fn opened_drive_alias_below_projects_is_not_a_volume_root() {
    let directory = crate::test_support::temp_home().unwrap();
    let (database, _) = fixture(directory.path());
    let alias = Dir::open_ambient_dir(database.parent().unwrap(), ambient_authority()).unwrap();
    let error = validate_volume_root(&alias).unwrap_err();
    assert!(error.to_string().contains("drive alias"));
    assert!(error.to_string().contains("real volume path"));
    let real = fs::canonicalize(directory.path()).unwrap();
    let root =
        Dir::open_ambient_dir(real.ancestors().last().unwrap(), ambient_authority()).unwrap();
    validate_volume_root(&root).unwrap();
    assert_eq!(fs::read(&database).unwrap(), b"store-before");
    assert_eq!(fs::read_dir(database.parent().unwrap()).unwrap().count(), 2);
}

#[cfg(windows)]
#[test]
fn raw_windows_names_are_refused_before_normalization_or_staging() {
    let directory = crate::test_support::temp_home().unwrap();
    let (database, _) = fixture(directory.path());
    let real = fs::canonicalize(directory.path()).unwrap();
    let ordinary = real.to_str().unwrap().strip_prefix(r"\\?\").unwrap();
    for tail in [
        "file.",
        "file ",
        "NUL .json",
        "CONIN$",
        "CONOUT$.txt",
        "COM0",
        "LPT0",
    ] {
        for base in [ordinary, real.to_str().unwrap()] {
            let output = PathBuf::from(format!("{base}\\exports\\{tail}"));
            let error = write_graph_snapshot_file(&database, &output, SNAPSHOT).unwrap_err();
            assert!(
                error
                    .to_string()
                    .contains("unsupported Windows path component"),
                "{error:#}"
            );
        }
    }
    assert!(!directory.path().join("exports").exists());
    assert_eq!(fs::read(&database).unwrap(), b"store-before");
}

#[test]
fn ordinary_ancestor_error_does_not_prescribe_a_link_remedy() {
    let directory = crate::test_support::temp_home().unwrap();
    let (database, output) = fixture(directory.path());
    fs::write(directory.path().join("exports"), b"not a directory").unwrap();
    let error = write_graph_snapshot_file(&database, &output, SNAPSHOT).unwrap_err();
    let message = format!("{error:#}");
    assert!(message.contains("cannot open snapshot ancestor"));
    assert!(!message.contains("pass the resolved real directory"));
    assert!(message.contains(&directory.path().join("exports").display().to_string()));
}

#[test]
fn failed_publication_and_failed_stage_write_disclose_cleanup_residue() {
    struct WriteFailure(Faults);
    impl Publication for WriteFailure {
        fn write_stage(&self, file: &mut cap_std::fs::File, bytes: &[u8]) -> io::Result<()> {
            file.write_all(bytes)?;
            Err(io::Error::other("stage sync failed"))
        }
        fn cleanup(&self, parent: &Dir, stage: &Path) -> io::Result<()> {
            self.0.cleanup(parent, stage)
        }
        fn warning(&self, warning: &str) {
            self.0.warning(warning);
        }
    }
    for fail_write in [false, true] {
        let directory = crate::test_support::temp_home().unwrap();
        let (database, output) = fixture(directory.path());
        let operations = WriteFailure(Faults {
            link: LinkBehavior::Unsupported,
            cleanup_fails: true,
            sync_fails: false,
            synced: Cell::new(false),
            warnings: RefCell::new(Vec::new()),
        });
        let error = if fail_write {
            write_with(&database, &output, SNAPSHOT, &operations)
        } else {
            write_with(&database, &output, SNAPSHOT, &operations.0)
        }
        .unwrap_err();
        assert!(format!("{error:#}").contains(if fail_write {
            "stage sync failed"
        } else {
            "hard-link support"
        }));
        let stage = fs::read_dir(output.parent().unwrap())
            .unwrap()
            .next()
            .unwrap()
            .unwrap()
            .path();
        let warnings = operations.0.warnings.borrow();
        let message = format!("{error:#}");
        assert!(
            message.contains(&std::path::absolute(&stage).unwrap().display().to_string()),
            "{message}"
        );
        assert!(
            message.contains(&std::path::absolute(&output).unwrap().display().to_string()),
            "{message}"
        );
        assert_eq!(warnings.len(), 1);
        assert!(warnings[0].contains("publication failed"));
        assert!(warnings[0].contains("stage held open"));
        assert!(
            warnings[0].contains(&std::path::absolute(&stage).unwrap().display().to_string()),
            "{}",
            warnings[0]
        );
        assert!(warnings[0].contains("disclosure data"));
        assert_eq!(fs::read(stage).unwrap(), SNAPSHOT);
        assert!(!output.exists());
        assert!(!operations.0.synced.get());
    }
}

#[test]
fn existing_directory_and_oversized_file_are_refused_before_staging() {
    for is_directory in [false, true] {
        let directory = crate::test_support::temp_home().unwrap();
        let (database, output) = fixture(directory.path());
        fs::create_dir_all(output.parent().unwrap()).unwrap();
        if is_directory {
            fs::create_dir(&output).unwrap();
        } else {
            fs::File::create(&output)
                .unwrap()
                .set_len(super::super::MAX_GRAPH_SNAPSHOT_BYTES + 1)
                .unwrap();
        }
        let error = write_graph_snapshot_file(&database, &output, SNAPSHOT).unwrap_err();
        let message = format!("{error:#}");
        assert!(
            message.contains(if is_directory {
                "not a regular file"
            } else {
                "comparison limit"
            }),
            "{message}"
        );
        assert!(message.contains(&std::path::absolute(&output).unwrap().display().to_string()));
        assert_eq!(fs::read_dir(output.parent().unwrap()).unwrap().count(), 1);
        assert_eq!(fs::read(&database).unwrap(), b"store-before");
    }
}

#[test]
fn existing_read_bounds_growth_and_contextualizes_read_errors() {
    struct FailedRead;
    impl Read for FailedRead {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "read denied",
            ))
        }
    }
    let path = Path::new("snapshot.json");
    let mut growing = io::Cursor::new(vec![b'x'; 128]);
    let error = read_existing_bounded(&mut growing, 8, path).unwrap_err();
    assert!(error.to_string().contains("8-byte comparison limit"));
    assert_eq!(growing.position(), 9);
    assert_eq!(
        read_existing_bounded(io::Cursor::new(b"12345678"), 8, path).unwrap(),
        b"12345678"
    );
    let error = read_existing_bounded(FailedRead, 8, path).unwrap_err();
    assert!(format!("{error:#}").contains("failed to read snapshot destination snapshot.json"));
    assert!(format!("{error:#}").contains("read denied"));
}

#[cfg(any(unix, windows))]
#[test]
fn final_component_link_into_project_stores_is_refused_before_staging() {
    let directory = crate::test_support::temp_home().unwrap();
    let (database, output) = fixture(directory.path());
    fs::create_dir_all(output.parent().unwrap()).unwrap();
    let target = database.with_file_name("equivalent.json");
    fs::write(&target, SNAPSHOT).unwrap();
    #[cfg(unix)]
    std::os::unix::fs::symlink(&target, &output).unwrap();
    #[cfg(windows)]
    directory_link(database.parent().unwrap(), &output);
    let error = write_graph_snapshot_file(&database, &output, SNAPSHOT).unwrap_err();
    let message = format!("{error:#}");
    assert!(
        message.contains("not a regular file") || message.contains("reparse point"),
        "{message}"
    );
    assert_eq!(fs::read_dir(output.parent().unwrap()).unwrap().count(), 1);
    assert_eq!(fs::read(&target).unwrap(), SNAPSHOT);
    assert_eq!(fs::read(&database).unwrap(), b"store-before");
    assert_eq!(
        fs::read(database.with_extension("db-wal")).unwrap(),
        b"wal-before"
    );
}

#[cfg(unix)]
#[test]
fn fifo_destination_and_fifo_swap_after_inspection_are_refused_without_a_writer() {
    for swap_after_inspection in [false, true] {
        let directory = crate::test_support::temp_home().unwrap();
        let real = fs::canonicalize(directory.path()).unwrap();
        let (database, output) = fixture(&real);
        fs::create_dir_all(output.parent().unwrap()).unwrap();
        let make_fifo = || {
            nix::unistd::mkfifo(
                &output,
                nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
            )
            .unwrap();
        };
        let error = if swap_after_inspection {
            fs::write(&output, SNAPSHOT).unwrap();
            Destination::open(&database, &output)
                .unwrap()
                .existing_after_inspection(SNAPSHOT, || {
                    fs::remove_file(&output)?;
                    make_fifo();
                    Ok(())
                })
                .unwrap_err()
        } else {
            make_fifo();
            write_graph_snapshot_file(&database, &output, SNAPSHOT).unwrap_err()
        };
        assert!(format!("{error:#}").contains("not a regular file"));
        assert_eq!(fs::read_dir(output.parent().unwrap()).unwrap().count(), 1);
        assert_eq!(fs::read(&database).unwrap(), b"store-before");
    }
}

#[test]
fn directory_only_destination_spelling_is_refused_without_file_or_stage() {
    let directory = crate::test_support::temp_home().unwrap();
    let (database, output) = fixture(directory.path());
    fs::create_dir_all(output.parent().unwrap()).unwrap();
    #[cfg(windows)]
    let suffixes = ["/", "\\"];
    #[cfg(not(windows))]
    let suffixes = ["/", "/."];
    for suffix in suffixes {
        let mut spelling = output.as_os_str().to_os_string();
        spelling.push(suffix);
        let error =
            write_graph_snapshot_file(&database, Path::new(&spelling), SNAPSHOT).unwrap_err();
        assert!(
            error.to_string().contains("directory-only spelling"),
            "{error:#}"
        );
        assert!(!output.exists());
        assert_eq!(fs::read_dir(output.parent().unwrap()).unwrap().count(), 0);
    }
    assert_eq!(fs::read(&database).unwrap(), b"store-before");
}

#[cfg(windows)]
#[test]
fn verbatim_parent_components_report_the_walked_publication_and_stage_paths() {
    let directory = crate::test_support::temp_home().unwrap();
    let (database, output) = fixture(directory.path());
    let real = fs::canonicalize(directory.path()).unwrap();
    let verbatim = PathBuf::from(format!(
        "{}\\exports\\unused\\..\\nested\\.\\snapshot.json",
        real.display()
    ));
    let faults = Faults {
        link: LinkBehavior::Publish,
        cleanup_fails: true,
        sync_fails: true,
        synced: Cell::new(false),
        warnings: RefCell::new(Vec::new()),
    };
    let error = write_with(&database, &verbatim, SNAPSHOT, &faults).unwrap_err();
    let published = std::path::absolute(&output).unwrap();
    let message = format!("{error:#}");
    assert!(
        message.contains(&published.display().to_string()),
        "{message}"
    );
    assert!(!message.contains("unused"));
    assert_eq!(fs::read(&published).unwrap(), SNAPSHOT);
    let stage = fs::read_dir(published.parent().unwrap())
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| {
            path.file_name()
                .unwrap()
                .to_string_lossy()
                .starts_with(".graph-save-")
        })
        .unwrap();
    let warnings = faults.warnings.borrow();
    assert_eq!(warnings.len(), 1);
    assert!(
        warnings[0].contains(&stage.display().to_string()),
        "{}",
        warnings[0]
    );
    assert!(!warnings[0].contains("unused"));
    assert_eq!(fs::read(stage).unwrap(), SNAPSHOT);
}
