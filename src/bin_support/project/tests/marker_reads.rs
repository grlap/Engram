use super::*;

#[test]
fn regular_marker_byte_limit_is_checked_before_decoding() {
    let directory = crate::test_support::temp_home().unwrap();
    let marker = directory.path().join(".engram-project");
    let limit = usize::try_from(MAX_PROJECT_FILE_BYTES).unwrap();
    for length in [1, limit] {
        let bytes = vec![b'x'; length];
        fs::write(&marker, &bytes).unwrap();
        assert_eq!(read_entry(&marker).unwrap(), Some(bytes));
    }
    fs::write(&marker, vec![b'x'; limit + 1]).unwrap();
    let error = read_entry(&marker).unwrap_err();
    assert!(error.to_string().contains("too large"));
    // A large logical file must be refused from metadata, before opening it.
    fs::File::create(&marker)
        .unwrap()
        .set_len(1024 * 1024 * 1024)
        .unwrap();
    let error = read_entry_after_inspection(&marker, || {
        panic!("oversized marker must refuse before open")
    })
    .unwrap_err();
    assert!(error.to_string().contains("4096 bytes"));
}

#[test]
fn bounded_reader_consumes_only_limit_plus_one_even_without_an_end() {
    struct Endless {
        consumed: u64,
    }
    impl io::Read for Endless {
        fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
            bytes.fill(b'x');
            self.consumed += u64::try_from(bytes.len()).unwrap();
            Ok(bytes.len())
        }
    }
    let mut reader = Endless { consumed: 0 };
    let error = read_project_file_bounded(&mut reader).unwrap_err();
    assert!(error.to_string().contains("too large"));
    assert_eq!(reader.consumed, MAX_PROJECT_FILE_BYTES + 1);
}

#[test]
fn opened_file_size_is_checked_again_after_inspection() {
    let directory = crate::test_support::temp_home().unwrap();
    let marker = directory.path().join(".engram-project");
    fs::write(&marker, b"project").unwrap();
    let error =
        read_entry_after_inspection(&marker, || fs::write(&marker, vec![b'x'; 4097])).unwrap_err();
    assert!(error.to_string().contains("too large"));
}

#[test]
fn directory_marker_is_refused_before_open() {
    let directory = crate::test_support::temp_home().unwrap();
    let marker = directory.path().join(".engram-project");
    fs::create_dir(&marker).unwrap();
    let error = read_entry_after_inspection(&marker, || {
        panic!("directory marker must refuse before open")
    })
    .unwrap_err();
    assert!(error.to_string().contains("not a regular file"));
}

#[cfg(windows)]
#[test]
fn directory_junction_marker_is_refused() {
    let directory = crate::test_support::temp_home().unwrap();
    let target = directory.path().join("target-directory");
    fs::create_dir(&target).unwrap();
    let marker = directory.path().join(".engram-project");
    junction::create(&target, &marker).unwrap();
    let error = read_entry(&marker).unwrap_err();
    assert!(error.to_string().contains("not a regular file"));
    junction::delete(&marker).unwrap();
}

#[cfg(unix)]
#[test]
fn links_keep_the_marker_pathname_and_special_targets_refuse() {
    use std::os::unix::fs::symlink;

    let directory = crate::test_support::temp_home().unwrap();
    let target = directory.path().join("identity");
    fs::write(&target, b"linked-project").unwrap();
    let marker = directory.path().join(".engram-project");
    symlink(&target, &marker).unwrap();
    let selected = select_project_file(Some(&marker)).unwrap();
    assert_eq!(selected, (marker.clone(), "linked-project".into()));
    fs::remove_file(&target).unwrap();
    assert_eq!(
        read_entry(&marker).unwrap_err().kind(),
        io::ErrorKind::NotFound
    );
    fs::remove_file(&marker).unwrap();
    symlink("/dev/zero", &marker).unwrap();
    assert!(
        read_entry(&marker)
            .unwrap_err()
            .to_string()
            .contains("not a regular file")
    );
}

#[cfg(unix)]
#[test]
fn fifo_and_fifo_replacement_refuse_without_a_writer() {
    for replaced in [false, true] {
        let directory = crate::test_support::temp_home().unwrap();
        let marker = directory.path().join(".engram-project");
        let make_fifo = || {
            nix::unistd::mkfifo(
                &marker,
                nix::sys::stat::Mode::S_IRUSR | nix::sys::stat::Mode::S_IWUSR,
            )
            .unwrap();
        };
        let error = if replaced {
            fs::write(&marker, b"project").unwrap();
            read_entry_after_inspection(&marker, || {
                fs::remove_file(&marker)?;
                make_fifo();
                Ok(())
            })
            .unwrap_err()
        } else {
            make_fifo();
            read_entry(&marker).unwrap_err()
        };
        assert!(error.to_string().contains("not a regular file"));
    }
}
