use std::{
    fs, io,
    path::{Path, PathBuf},
    process::Command,
    time::{Duration, Instant},
};

use chrono::{TimeZone, Utc};
use engram::{
    ObjectId, ProjectId, WorkGraphSnapshotCut,
    backup::{CaptureManifest, CopyKind},
};
use flate2::{Compression, write::GzEncoder};
use sha2::{Digest, Sha256};

use super::{
    adapter::{AdapterError, BackupAdapter, Confirmation, Reconciled},
    directory::{Decode, DirectoryAdapter, decode_bounded},
};
use crate::test_support::{TempHome, make_dir_link, remove_dir_link, temp_home};

fn project() -> ProjectId {
    ProjectId("adapter-project".into())
}

fn identity() -> ObjectId {
    ObjectId::from_canonical_bytes(b"directory target identity")
}

#[allow(
    clippy::unnecessary_wraps,
    reason = "it stands in for a free-space reading, which can fail"
)]
fn plenty(_: &Path) -> io::Result<u64> {
    Ok(u64::MAX)
}

fn sha256(bytes: &[u8]) -> String {
    format!("{:x}", Sha256::digest(bytes))
}

/// Bytes that gzip cannot shrink.
fn incompressible(length: usize) -> Vec<u8> {
    let mut bytes = Vec::with_capacity(length + 32);
    let mut block = Sha256::digest(b"seed").to_vec();
    while bytes.len() < length {
        block = Sha256::digest(&block).to_vec();
        bytes.extend_from_slice(&block);
    }
    bytes.truncate(length);
    bytes
}

/// A staged artifact under the fixture home and the capture manifest that
/// describes it.
fn artifact(home: &Path, name: &str, bytes: &[u8], second: u32) -> (PathBuf, CaptureManifest) {
    let stage = home.join("stage").join(name);
    fs::create_dir_all(&stage).unwrap();
    let path = stage.join("store.db");
    fs::write(&path, bytes).unwrap();
    let manifest = CaptureManifest {
        project_digest: engram::project_digest(&project()),
        kind: CopyKind::Store,
        cut: WorkGraphSnapshotCut {
            work_feed: 7,
            project_memory: 3,
        },
        capture_started_at: Utc.with_ymd_and_hms(2026, 10, 2, 12, 0, second).unwrap(),
        bytes: bytes.len() as u64,
        sha256: sha256(bytes),
        format_identity: ObjectId::from_canonical_bytes(b"schema"),
        build_fingerprint: None,
        source_revision: None,
        host_name: Some("test-host".into()),
    };
    (path, manifest)
}

struct Fixture {
    home: TempHome,
    root: PathBuf,
}

fn fixture() -> Fixture {
    let home = temp_home().unwrap();
    let root = home.path().join("targets");
    fs::create_dir_all(&root).unwrap();
    Fixture { home, root }
}

fn adapter<'a>(
    root: &Path,
    free_space: &'a dyn Fn(&Path) -> io::Result<u64>,
) -> DirectoryAdapter<'a> {
    DirectoryAdapter::new(
        root.to_path_buf(),
        identity(),
        Duration::from_secs(120),
        free_space,
    )
}

fn project_dir(root: &Path) -> PathBuf {
    root.join(engram::project_digest(&project()))
}

fn names(directory: &Path) -> Vec<String> {
    let mut names: Vec<_> = match fs::read_dir(directory) {
        Ok(entries) => entries
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect(),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Vec::new(),
        Err(error) => panic!("{error}"),
    };
    names.sort();
    names
}

#[test]
fn put_confirm_list_and_get_a_compressible_copy() {
    let fixture = fixture();
    let adapter = adapter(&fixture.root, &plenty);
    let bytes = b"engram backup ".repeat(80_000);
    let (path, capture) = artifact(fixture.home.path(), "one", &bytes, 0);
    let (attempt, stored) = adapter
        .prepare(&path, &capture, uuid::Uuid::now_v7())
        .unwrap();
    let receipt = adapter.put(&project(), &attempt, &stored).unwrap();
    assert_eq!(receipt.sha256, capture.sha256);
    assert_eq!(receipt.target_identity, identity());
    assert_eq!(
        serde_json::to_value(receipt.acknowledgement).unwrap(),
        "read_back"
    );
    assert_eq!(serde_json::to_value(receipt.off_host).unwrap(), "asserted");
    assert!(receipt.at <= Utc::now());

    let data = project_dir(&fixture.root).join(format!("{}.db.gz", attempt.manifest.copy));
    let stored_size = fs::metadata(&data).unwrap().len();
    assert!(
        stored_size < bytes.len() as u64 / 10,
        "{stored_size} of {}",
        bytes.len()
    );
    assert_eq!(stored_size, attempt.manifest.stored_bytes);
    // Nothing else is left at the target: no temporary file of the attempt.
    assert_eq!(
        names(&project_dir(&fixture.root)),
        [
            format!("{}.db.gz", attempt.manifest.copy),
            format!("{}.manifest.json", attempt.manifest.copy)
        ]
    );

    assert_eq!(
        adapter.confirm(&project(), &attempt.manifest),
        Confirmation::Confirmed
    );
    let page = adapter.list(&project(), None).unwrap();
    assert_eq!(page.manifests, std::slice::from_ref(&attempt.manifest));
    assert_eq!(page.unreadable, Vec::<String>::new());
    assert_eq!(page.next, None);

    let restored = fixture.home.path().join("restored.db");
    adapter
        .get(&project(), &attempt.manifest, &restored)
        .unwrap();
    assert_eq!(fs::read(&restored).unwrap(), bytes);
}

#[test]
fn an_incompressible_copy_round_trips_and_any_gzip_reads_it() {
    let fixture = fixture();
    let adapter = adapter(&fixture.root, &plenty);
    let bytes = incompressible(300_000);
    let (path, capture) = artifact(fixture.home.path(), "random", &bytes, 1);
    let (attempt, stored) = adapter
        .prepare(&path, &capture, uuid::Uuid::now_v7())
        .unwrap();
    adapter.put(&project(), &attempt, &stored).unwrap();
    assert_eq!(
        adapter.confirm(&project(), &attempt.manifest),
        Confirmation::Confirmed
    );
    let restored = fixture.home.path().join("restored.db");
    adapter
        .get(&project(), &attempt.manifest, &restored)
        .unwrap();
    assert_eq!(fs::read(&restored).unwrap(), bytes);

    // Another gzip implementation reads the stored file to the same bytes.
    let data = project_dir(&fixture.root).join(format!("{}.db.gz", attempt.manifest.copy));
    let decoded = fixture.home.path().join("decoded-elsewhere.db");
    let output = if cfg!(windows) {
        Command::new("pwsh")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "$in = [System.IO.File]::OpenRead($env:ENGRAM_GZIP_IN); \
                 $zip = [System.IO.Compression.GZipStream]::new($in, [System.IO.Compression.CompressionMode]::Decompress); \
                 $out = [System.IO.File]::Create($env:ENGRAM_GZIP_OUT); \
                 $zip.CopyTo($out); $out.Close(); $zip.Close()",
            ])
            .env("ENGRAM_GZIP_IN", &data)
            .env("ENGRAM_GZIP_OUT", &decoded)
            .output()
            .unwrap()
    } else {
        let out = fs::File::create(&decoded).unwrap();
        Command::new("gzip")
            .arg("-dc")
            .arg(&data)
            .stdout(out)
            .output()
            .unwrap()
    };
    assert!(output.status.success(), "{output:?}");
    assert_eq!(sha256(&fs::read(&decoded).unwrap()), capture.sha256);
}

#[test]
fn put_never_replaces_a_file_already_under_the_copy_s_name() {
    let fixture = fixture();
    let adapter = adapter(&fixture.root, &plenty);
    let (path, capture) = artifact(fixture.home.path(), "taken", b"the copy", 2);
    let (attempt, stored) = adapter
        .prepare(&path, &capture, uuid::Uuid::now_v7())
        .unwrap();
    let directory = project_dir(&fixture.root);
    fs::create_dir_all(&directory).unwrap();
    let data = directory.join(format!("{}.db.gz", attempt.manifest.copy));
    fs::write(&data, b"someone else's bytes").unwrap();
    let error = adapter.put(&project(), &attempt, &stored).unwrap_err();
    assert_eq!(error.code(), "backup_copy_exists", "{error}");
    assert_eq!(fs::read(&data).unwrap(), b"someone else's bytes");
    assert_eq!(
        names(&directory),
        [format!("{}.db.gz", attempt.manifest.copy)]
    );

    // The same holds for the manifest: a second put of a finished copy.
    fs::remove_file(&data).unwrap();
    adapter.put(&project(), &attempt, &stored).unwrap();
    let error = adapter.put(&project(), &attempt, &stored).unwrap_err();
    assert_eq!(error.code(), "backup_copy_exists", "{error}");
    assert_eq!(
        adapter.confirm(&project(), &attempt.manifest),
        Confirmation::Confirmed
    );
}

#[test]
fn confirm_answers_missing_for_a_gone_or_changed_copy_and_unknown_when_unreachable() {
    let fixture = fixture();
    let adapter = adapter(&fixture.root, &plenty);
    let bytes = b"confirm me ".repeat(1000);
    let (path, capture) = artifact(fixture.home.path(), "confirm", &bytes, 3);
    let (attempt, stored) = adapter
        .prepare(&path, &capture, uuid::Uuid::now_v7())
        .unwrap();
    adapter.put(&project(), &attempt, &stored).unwrap();
    let data = project_dir(&fixture.root).join(format!("{}.db.gz", attempt.manifest.copy));
    let original = fs::read(&data).unwrap();

    // Same size, other content: the ordinary confirm reads it and does not
    // confirm it.
    let mut other = original.clone();
    let middle = other.len() / 2;
    other[middle] ^= 0xff;
    fs::write(&data, &other).unwrap();
    assert_eq!(fs::metadata(&data).unwrap().len(), original.len() as u64);
    assert!(matches!(
        adapter.confirm(&project(), &attempt.manifest),
        Confirmation::Missing { .. }
    ));

    fs::remove_file(&data).unwrap();
    assert!(matches!(
        adapter.confirm(&project(), &attempt.manifest),
        Confirmation::Missing { .. }
    ));

    let unreachable = adapter_at(&fixture.home.path().join("not-mounted"));
    assert!(matches!(
        unreachable.confirm(&project(), &attempt.manifest),
        Confirmation::Unreachable { .. }
    ));

    // A read past the deadline, at a reachable target, cannot say.
    fs::write(&data, &original).unwrap();
    let hurried = DirectoryAdapter::new(fixture.root.clone(), identity(), Duration::ZERO, &plenty);
    assert!(matches!(
        hurried.confirm(&project(), &attempt.manifest),
        Confirmation::TimedOut { .. }
    ));
    assert_eq!(
        adapter.confirm(&project(), &attempt.manifest),
        Confirmation::Confirmed
    );
}

fn adapter_at(root: &Path) -> DirectoryAdapter<'static> {
    DirectoryAdapter::new(
        root.to_path_buf(),
        identity(),
        Duration::from_secs(120),
        &plenty,
    )
}

fn gzip(bytes: &[u8]) -> Vec<u8> {
    let mut encoder = GzEncoder::new(Vec::new(), Compression::fast());
    io::Write::write_all(&mut encoder, bytes).unwrap();
    encoder.finish().unwrap()
}

#[test]
fn decoding_writes_no_more_than_the_declared_length() {
    let declared = b"exactly these bytes".to_vec();
    let mut longer = declared.clone();
    longer.extend_from_slice(&[7_u8; 4096]);

    let mut output = Vec::new();
    let error = decode_bounded(
        gzip(&longer).as_slice(),
        &mut output,
        declared.len() as u64,
        Instant::now(),
        Duration::from_secs(60),
    )
    .unwrap_err();
    assert!(matches!(error, Decode::Invalid(_)), "{error:?}");
    assert_eq!(output, declared);

    let mut output = Vec::new();
    let sha = decode_bounded(
        gzip(&declared).as_slice(),
        &mut output,
        declared.len() as u64,
        Instant::now(),
        Duration::from_secs(60),
    )
    .unwrap();
    assert_eq!(sha, sha256(&declared));

    // Through get: a stored file that decodes to more is refused, and the
    // destination it would have written is gone.
    let fixture = fixture();
    let adapter = adapter(&fixture.root, &plenty);
    let (path, capture) = artifact(fixture.home.path(), "excess", &declared, 4);
    let (attempt, stored) = adapter
        .prepare(&path, &capture, uuid::Uuid::now_v7())
        .unwrap();
    adapter.put(&project(), &attempt, &stored).unwrap();
    let data = project_dir(&fixture.root).join(format!("{}.db.gz", attempt.manifest.copy));
    fs::write(&data, gzip(&longer)).unwrap();
    // The manifest declares the stored file's true length, so the refusal
    // comes from the bounded decode, not from the length check before it.
    let mut manifest = attempt.manifest.clone();
    manifest.stored_bytes = fs::metadata(&data).unwrap().len();
    let restored = fixture.home.path().join("restored.db");
    let error = adapter.get(&project(), &manifest, &restored).unwrap_err();
    assert_eq!(error.code(), "backup_copy_invalid", "{error}");
    assert!(error.to_string().contains("more than"), "{error}");
    assert!(!restored.exists());
}

#[test]
fn put_checks_target_space_first_and_cleans_only_its_own_files_when_space_runs_out() {
    let fixture = fixture();
    let (path, capture) = artifact(fixture.home.path(), "space", &incompressible(50_000), 5);
    let probe = DirectoryAdapter::new(
        fixture.root.clone(),
        identity(),
        Duration::from_secs(60),
        &plenty,
    );
    let (attempt, stored) = probe
        .prepare(&path, &capture, uuid::Uuid::now_v7())
        .unwrap();
    let manifest_size = serde_json::to_vec_pretty(&attempt.manifest).unwrap().len() as u64 + 1;
    let required = attempt.manifest.stored_bytes + manifest_size;

    let short = move |_: &Path| Ok(required - 1);
    let adapter = adapter(&fixture.root, &short);
    let error = adapter.put(&project(), &attempt, &stored).unwrap_err();
    assert_eq!(error.code(), "backup_target_no_space", "{error}");
    assert!(matches!(
        error,
        AdapterError::TargetNoSpace { required: needed, .. } if needed == required
    ));
    assert_eq!(names(&fixture.root), Vec::<String>::new());

    // Space runs out after the check passed: only this attempt's files go.
    let directory = project_dir(&fixture.root);
    fs::create_dir_all(&directory).unwrap();
    fs::write(directory.join("unrelated.txt"), b"keep me").unwrap();
    let exact = move |_: &Path| Ok(required);
    let mut filling = adapter_with(&fixture.root, &exact);
    filling.fail_after = Some(attempt.manifest.stored_bytes / 2);
    let error = filling.put(&project(), &attempt, &stored).unwrap_err();
    assert_eq!(error.code(), "backup_target_no_space", "{error}");
    assert_eq!(names(&directory), ["unrelated.txt"]);
}

fn adapter_with<'a>(
    root: &Path,
    free_space: &'a dyn Fn(&Path) -> io::Result<u64>,
) -> DirectoryAdapter<'a> {
    adapter(root, free_space)
}

#[test]
fn reconcile_completes_or_removes_only_the_attempt_s_own_files() {
    let fixture = fixture();
    let adapter = adapter(&fixture.root, &plenty);
    let directory = project_dir(&fixture.root);
    fs::create_dir_all(&directory).unwrap();
    // Files that are not this home's attempts.
    let unrelated = directory.join("unrelated.txt");
    fs::write(&unrelated, b"keep me").unwrap();
    let foreign = directory.join(format!("20260101T000000Z-{}.db.gz", uuid::Uuid::now_v7()));
    fs::write(&foreign, gzip(b"another origin")).unwrap();

    // Cut off after the rename, before the manifest: content matches.
    let bytes = b"reconcile me ".repeat(500);
    let (path, capture) = artifact(fixture.home.path(), "matching", &bytes, 6);
    let (matching, stored) = adapter
        .prepare(&path, &capture, uuid::Uuid::now_v7())
        .unwrap();
    let data = directory.join(format!("{}.db.gz", matching.manifest.copy));
    fs::copy(&stored, &data).unwrap();
    assert!(matches!(
        adapter.confirm(&project(), &matching.manifest),
        Confirmation::Missing { .. }
    ));
    assert_eq!(
        adapter.reconcile(&project(), &matching),
        Reconciled::Completed
    );
    assert_eq!(
        adapter.confirm(&project(), &matching.manifest),
        Confirmation::Confirmed
    );
    assert_eq!(
        adapter.reconcile(&project(), &matching),
        Reconciled::Complete
    );

    // Cut off with other content under the attempt's name: it is removed.
    let (path, capture) = artifact(fixture.home.path(), "garbled", b"garbled copy", 7);
    let (garbled, _) = adapter
        .prepare(&path, &capture, uuid::Uuid::now_v7())
        .unwrap();
    let garbled_data = directory.join(format!("{}.db.gz", garbled.manifest.copy));
    fs::write(&garbled_data, b"not the copy").unwrap();
    assert_eq!(adapter.reconcile(&project(), &garbled), Reconciled::Removed);
    assert!(!garbled_data.exists());
    assert_eq!(adapter.reconcile(&project(), &garbled), Reconciled::Absent);
    adapter.remove_attempt(&project(), &garbled).unwrap();

    assert_eq!(fs::read(&unrelated).unwrap(), b"keep me");
    assert_eq!(fs::read(&foreign).unwrap(), gzip(b"another origin"));
    let page = adapter.list(&project(), None).unwrap();
    assert_eq!(page.manifests, std::slice::from_ref(&matching.manifest));
}

#[test]
fn an_attempt_records_its_file_names_and_one_naming_other_files_is_never_acted_on() {
    let fixture = fixture();
    let adapter = adapter(&fixture.root, &plenty);
    let directory = project_dir(&fixture.root);
    let bytes = b"named files ".repeat(300);
    let (path, capture) = artifact(fixture.home.path(), "named", &bytes, 8);
    let (attempt, stored) = adapter
        .prepare(&path, &capture, uuid::Uuid::now_v7())
        .unwrap();
    // The attempt records the final and temporary names of its data file.
    assert_eq!(
        attempt.data_file,
        format!("{}.db.gz", attempt.manifest.copy)
    );
    assert_eq!(
        attempt.temporary_data_file,
        format!(".{}.db.gz.tmp", attempt.manifest.copy)
    );
    adapter.put(&project(), &attempt, &stored).unwrap();
    let before = names(&directory);

    // A recorded name that is not its copy's own is refused, and nothing at
    // the target is touched for it.
    for (data_file, temporary_data_file) in [
        (
            "unrelated.txt".to_owned(),
            attempt.temporary_data_file.clone(),
        ),
        (attempt.data_file.clone(), "..\\escape.tmp".to_owned()),
    ] {
        let tampered = engram::backup::record::Attempt {
            data_file,
            temporary_data_file,
            ..attempt.clone()
        };
        assert!(matches!(
            adapter.reconcile(&project(), &tampered),
            Reconciled::Unknown { .. }
        ));
        let error = adapter.remove_attempt(&project(), &tampered).unwrap_err();
        assert_eq!(error.code(), "backup_copy_invalid", "{error}");
        assert_eq!(names(&directory), before);
    }
    // Nor does put store a copy for such an attempt.
    let (path, capture) = artifact(fixture.home.path(), "named-second", &bytes, 9);
    let (second, stored) = adapter
        .prepare(&path, &capture, uuid::Uuid::now_v7())
        .unwrap();
    let tampered = engram::backup::record::Attempt {
        data_file: "unrelated.txt".into(),
        ..second
    };
    let error = adapter.put(&project(), &tampered, &stored).unwrap_err();
    assert_eq!(error.code(), "backup_copy_invalid", "{error}");
    assert_eq!(names(&directory), before);
    assert_eq!(
        adapter.reconcile(&project(), &attempt),
        Reconciled::Complete
    );
}

#[test]
fn get_checks_local_space_first() {
    let fixture = fixture();
    let adapter = adapter(&fixture.root, &plenty);
    let bytes = b"restore me ".repeat(100);
    let (path, capture) = artifact(fixture.home.path(), "local", &bytes, 8);
    let (attempt, stored) = adapter
        .prepare(&path, &capture, uuid::Uuid::now_v7())
        .unwrap();
    adapter.put(&project(), &attempt, &stored).unwrap();
    // Room for the uncompressed copy alone is not enough: a fetch needs room
    // for the stored file and the uncompressed copy together.
    let needed = attempt.manifest.stored_bytes + bytes.len() as u64;
    let short = move |_: &Path| Ok(needed - 1);
    let local = adapter_with(&fixture.root, &short);
    let restored = fixture.home.path().join("restored.db");
    let error = local
        .get(&project(), &attempt.manifest, &restored)
        .unwrap_err();
    assert_eq!(error.code(), "backup_local_no_space", "{error}");
    assert!(error.to_string().contains(&needed.to_string()), "{error}");
    assert!(!restored.exists());

    let unknown = |_: &Path| Err(io::Error::other("no answer from the file system"));
    let error = adapter_with(&fixture.root, &unknown)
        .get(&project(), &attempt.manifest, &restored)
        .unwrap_err();
    assert_eq!(error.code(), "backup_local_space_unknown", "{error}");
    assert!(!restored.exists());

    let exact = move |_: &Path| Ok(needed);
    adapter_with(&fixture.root, &exact)
        .get(&project(), &attempt.manifest, &restored)
        .unwrap();
    assert_eq!(fs::read(&restored).unwrap(), bytes);
}

#[test]
fn get_refuses_sizes_that_do_not_add_up_or_a_stored_file_of_another_length() {
    let fixture = fixture();
    let adapter = adapter(&fixture.root, &plenty);
    let bytes = b"sized copy ".repeat(120);
    let (path, capture) = artifact(fixture.home.path(), "sized", &bytes, 11);
    let (attempt, stored) = adapter
        .prepare(&path, &capture, uuid::Uuid::now_v7())
        .unwrap();
    adapter.put(&project(), &attempt, &stored).unwrap();
    let restored = fixture.home.path().join("restored.db");

    let mut overflowing = attempt.manifest.clone();
    overflowing.stored_bytes = u64::MAX;
    let error = adapter
        .get(&project(), &overflowing, &restored)
        .unwrap_err();
    assert_eq!(error.code(), "backup_copy_invalid", "{error}");
    assert!(error.to_string().contains("add up"), "{error}");
    assert!(!restored.exists());

    let mut longer = attempt.manifest.clone();
    longer.stored_bytes += 1;
    let error = adapter.get(&project(), &longer, &restored).unwrap_err();
    assert_eq!(error.code(), "backup_copy_invalid", "{error}");
    assert!(
        error.to_string().contains("its manifest declares"),
        "{error}"
    );
    assert!(!restored.exists());
}

#[test]
fn a_linked_ancestor_is_an_ordinary_target_path() {
    let home = temp_home().unwrap();
    let real = home.path().join("real-targets");
    fs::create_dir_all(real.join("targets")).unwrap();
    let link = home.path().join("linked");
    make_dir_link(&real, &link);
    let root = link.join("targets");
    let adapter = adapter(&root, &plenty);
    let bytes = b"through a link ".repeat(200);
    let (path, capture) = artifact(home.path(), "linked", &bytes, 9);
    let (attempt, stored) = adapter
        .prepare(&path, &capture, uuid::Uuid::now_v7())
        .unwrap();
    adapter.put(&project(), &attempt, &stored).unwrap();
    assert_eq!(
        adapter.confirm(&project(), &attempt.manifest),
        Confirmation::Confirmed
    );
    let restored = home.path().join("restored.db");
    adapter
        .get(&project(), &attempt.manifest, &restored)
        .unwrap();
    assert_eq!(fs::read(&restored).unwrap(), bytes);
    assert!(
        real.join("targets")
            .join(engram::project_digest(&project()))
            .join(format!("{}.manifest.json", attempt.manifest.copy))
            .is_file()
    );
    remove_dir_link(&link);
}

#[test]
fn reconcile_finishes_a_copy_whose_put_was_cut_off_while_writing_its_manifest() {
    let fixture = fixture();
    let adapter = adapter(&fixture.root, &plenty);
    let bytes = b"cut off at the manifest ".repeat(300);
    let (path, capture) = artifact(fixture.home.path(), "cut", &bytes, 10);
    let (attempt, stored) = adapter
        .prepare(&path, &capture, uuid::Uuid::now_v7())
        .unwrap();
    let directory = project_dir(&fixture.root);
    fs::create_dir_all(&directory).unwrap();
    fs::copy(
        &stored,
        directory.join(format!("{}.db.gz", attempt.manifest.copy)),
    )
    .unwrap();
    // The put died after creating its temporary manifest, partly written.
    fs::write(
        directory.join(format!(".{}.manifest.json.tmp", attempt.manifest.copy)),
        b"{\"format_ver",
    )
    .unwrap();
    assert_eq!(
        adapter.reconcile(&project(), &attempt),
        Reconciled::Completed
    );
    assert_eq!(
        adapter.confirm(&project(), &attempt.manifest),
        Confirmation::Confirmed
    );
    assert_eq!(
        names(&directory),
        [
            format!("{}.db.gz", attempt.manifest.copy),
            format!("{}.manifest.json", attempt.manifest.copy)
        ]
    );
}

#[test]
fn decoding_refuses_anything_after_the_gzip_stream() {
    let declared = b"the declared copy".to_vec();
    let mut two_members = gzip(&declared);
    two_members.extend_from_slice(&gzip(b"a second member"));
    let mut trailing = gzip(&declared);
    trailing.extend_from_slice(b"stray bytes");
    for stored in [two_members, trailing] {
        let mut output = Vec::new();
        let error = decode_bounded(
            stored.as_slice(),
            &mut output,
            declared.len() as u64,
            Instant::now(),
            Duration::from_secs(60),
        )
        .unwrap_err();
        assert!(matches!(error, Decode::Invalid(_)), "{error:?}");
        assert_eq!(output, declared);
    }
}

/// A reader that stalls once, just before the last bytes of its input.
struct StallsAtTheEnd<'a> {
    bytes: &'a [u8],
    position: usize,
    stall: Duration,
    stalled: bool,
}

impl io::Read for StallsAtTheEnd<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        let tail = self.bytes.len().saturating_sub(8);
        if self.position >= tail && !self.stalled {
            self.stalled = true;
            std::thread::sleep(self.stall);
        }
        // One byte at a time, so the trailer arrives only after the data.
        let read = buffer.len().min(1).min(self.bytes.len() - self.position);
        buffer[..read].copy_from_slice(&self.bytes[self.position..self.position + read]);
        self.position += read;
        Ok(read)
    }
}

#[test]
fn a_read_that_ends_after_the_deadline_does_not_confirm() {
    let declared = b"slow at the very end".to_vec();
    let stored = gzip(&declared);
    let deadline = Duration::from_millis(300);
    let reader = io::BufReader::with_capacity(
        1,
        StallsAtTheEnd {
            bytes: &stored,
            position: 0,
            stall: deadline + Duration::from_millis(200),
            stalled: false,
        },
    );
    let mut output = Vec::new();
    let error = decode_bounded(
        reader,
        &mut output,
        declared.len() as u64,
        Instant::now(),
        deadline,
    )
    .unwrap_err();
    assert!(matches!(error, Decode::Deadline), "{error:?}");
}

#[test]
fn put_and_remove_never_recreate_or_pass_over_a_target_that_is_gone() {
    let home = temp_home().unwrap();
    let gone = home.path().join("removable").join("engram");
    let adapter = adapter(&gone, &plenty);
    let (path, capture) = artifact(home.path(), "gone", b"nowhere to go", 11);
    let (attempt, stored) = adapter
        .prepare(&path, &capture, uuid::Uuid::now_v7())
        .unwrap();
    let error = adapter.put(&project(), &attempt, &stored).unwrap_err();
    assert_eq!(error.code(), "backup_target_unreachable", "{error}");
    assert!(!home.path().join("removable").exists());
    let error = adapter.remove_attempt(&project(), &attempt).unwrap_err();
    assert_eq!(error.code(), "backup_target_unreachable", "{error}");
}

#[test]
fn put_never_replaces_a_manifest_already_under_the_copy_s_name() {
    let fixture = fixture();
    let adapter = adapter(&fixture.root, &plenty);
    let (path, capture) = artifact(fixture.home.path(), "manifest", b"the copy", 12);
    let (attempt, stored) = adapter
        .prepare(&path, &capture, uuid::Uuid::now_v7())
        .unwrap();
    let directory = project_dir(&fixture.root);
    fs::create_dir_all(&directory).unwrap();
    let manifest = directory.join(format!("{}.manifest.json", attempt.manifest.copy));
    fs::write(&manifest, b"someone else's manifest").unwrap();
    let error = adapter.put(&project(), &attempt, &stored).unwrap_err();
    assert_eq!(error.code(), "backup_copy_exists", "{error}");
    assert_eq!(fs::read(&manifest).unwrap(), b"someone else's manifest");
    // The data this put moved into place stays for the push's reconcile,
    // which only touches the attempt's own names.
    assert!(
        directory
            .join(format!("{}.db.gz", attempt.manifest.copy))
            .is_file()
    );
}

#[test]
fn a_target_path_longer_than_the_classic_limit_works() {
    let fixture = fixture();
    let root = fixture.root.join("t".repeat(200));
    fs::create_dir_all(&root).unwrap();
    let adapter = adapter(&root, &plenty);
    let bytes = b"deep ".repeat(100);
    let (path, capture) = artifact(fixture.home.path(), "deep", &bytes, 13);
    let (attempt, stored) = adapter
        .prepare(&path, &capture, uuid::Uuid::now_v7())
        .unwrap();
    let data = project_dir(&root).join(format!(".{}.db.gz.tmp", attempt.manifest.copy));
    assert!(data.as_os_str().len() > 260, "{}", data.display());
    adapter.put(&project(), &attempt, &stored).unwrap();
    assert_eq!(
        adapter.confirm(&project(), &attempt.manifest),
        Confirmation::Confirmed
    );
}

#[cfg(windows)]
#[test]
fn moves_take_the_long_path_forms() {
    use super::directory::verbatim;
    for (path, expected) in [
        (r"C:\a\b", r"\\?\C:\a\b"),
        (r"\\server\share\a", r"\\?\UNC\server\share\a"),
        (r"\\.\C:\a", r"\\?\C:\a"),
        (r"\\?\C:\a", r"\\?\C:\a"),
    ] {
        assert_eq!(
            verbatim(Path::new(path)).unwrap(),
            PathBuf::from(expected),
            "{path}"
        );
    }
}

/// A reader that hands over all its bytes promptly and stalls only on the
/// read that finds nothing more.
struct StallsAtEof<'a> {
    bytes: &'a [u8],
    position: usize,
    stall: Duration,
}

impl io::Read for StallsAtEof<'_> {
    fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
        if self.position == self.bytes.len() {
            std::thread::sleep(self.stall);
            return Ok(0);
        }
        let read = buffer.len().min(self.bytes.len() - self.position);
        buffer[..read].copy_from_slice(&self.bytes[self.position..self.position + read]);
        self.position += read;
        Ok(read)
    }
}

#[test]
fn a_read_that_finds_the_end_after_the_deadline_does_not_confirm() {
    let declared = b"slow only at end of file".to_vec();
    let stored = gzip(&declared);
    let deadline = Duration::from_millis(300);
    let reader = io::BufReader::new(StallsAtEof {
        bytes: &stored,
        position: 0,
        stall: deadline + Duration::from_millis(200),
    });
    let mut output = Vec::new();
    let error = decode_bounded(
        reader,
        &mut output,
        declared.len() as u64,
        Instant::now(),
        deadline,
    )
    .unwrap_err();
    assert!(matches!(error, Decode::Deadline), "{error:?}");
}

#[test]
fn a_copy_of_one_project_is_never_taken_for_another_s() {
    let fixture = fixture();
    let adapter = adapter(&fixture.root, &plenty);
    let (path, capture) = artifact(fixture.home.path(), "mine", b"project A's store", 14);
    let (attempt, stored) = adapter
        .prepare(&path, &capture, uuid::Uuid::now_v7())
        .unwrap();
    let other = ProjectId("another-project".into());
    let error = adapter.put(&other, &attempt, &stored).unwrap_err();
    assert_eq!(error.code(), "backup_copy_invalid", "{error}");
    assert_eq!(names(&fixture.root), Vec::<String>::new());

    // Stored for its own project, it is still not the other project's.
    adapter.put(&project(), &attempt, &stored).unwrap();
    let other_dir = fixture.root.join(engram::project_digest(&other));
    fs::create_dir_all(&other_dir).unwrap();
    for name in [
        format!("{}.db.gz", attempt.manifest.copy),
        format!("{}.manifest.json", attempt.manifest.copy),
    ] {
        fs::copy(
            project_dir(&fixture.root).join(&name),
            other_dir.join(&name),
        )
        .unwrap();
    }
    assert!(matches!(
        adapter.confirm(&other, &attempt.manifest),
        Confirmation::Missing { .. }
    ));
    let page = adapter.list(&other, None).unwrap();
    assert_eq!(page.manifests, Vec::new());
    assert_eq!(
        page.unreadable,
        [format!("{}.manifest.json", attempt.manifest.copy)]
    );
    let restored = fixture.home.path().join("restored.db");
    let error = adapter
        .get(&other, &attempt.manifest, &restored)
        .unwrap_err();
    assert_eq!(error.code(), "backup_copy_invalid", "{error}");
    assert!(!restored.exists());
    let error = adapter.remove_attempt(&other, &attempt).unwrap_err();
    assert_eq!(error.code(), "backup_copy_invalid", "{error}");
    assert_eq!(names(&other_dir).len(), 2);
}

#[test]
fn list_pages_through_copies_and_reports_unusable_manifests() {
    let fixture = fixture();
    let adapter = adapter(&fixture.root, &plenty).with_page_size(2);
    let mut copies = Vec::new();
    for second in 20..23 {
        let bytes = format!("copy {second}").repeat(50);
        let (path, capture) = artifact(
            fixture.home.path(),
            &format!("page-{second}"),
            bytes.as_bytes(),
            second,
        );
        let (attempt, stored) = adapter
            .prepare(&path, &capture, uuid::Uuid::now_v7())
            .unwrap();
        adapter.put(&project(), &attempt, &stored).unwrap();
        copies.push(attempt.manifest);
    }
    let directory = project_dir(&fixture.root);
    // A manifest that does not parse, and one whose name is not its copy's.
    let corrupt = format!("20261002T120030Z-{}", uuid::Uuid::now_v7());
    fs::write(
        directory.join(format!("{corrupt}.manifest.json")),
        b"{ not json",
    )
    .unwrap();
    let renamed = format!("20261002T120031Z-{}", uuid::Uuid::now_v7());
    fs::copy(
        directory.join(format!("{}.manifest.json", copies[0].copy)),
        directory.join(format!("{renamed}.manifest.json")),
    )
    .unwrap();

    let mut seen = Vec::new();
    let mut unreadable = Vec::new();
    let mut cursor = None;
    let mut pages = 0;
    loop {
        let page = adapter.list(&project(), cursor.as_deref()).unwrap();
        pages += 1;
        assert!(page.manifests.len() + page.unreadable.len() <= 2);
        seen.extend(page.manifests);
        unreadable.extend(page.unreadable);
        match page.next {
            Some(next) => cursor = Some(next),
            None => break,
        }
    }
    assert_eq!(pages, 3);
    assert_eq!(seen, copies);
    unreadable.sort();
    let mut expected = vec![
        format!("{corrupt}.manifest.json"),
        format!("{renamed}.manifest.json"),
    ];
    expected.sort();
    assert_eq!(unreadable, expected);

    // `backup list` and `backup fetch` join those pages into one listing.
    let listing = super::fetch::list_all(&adapter, &project()).unwrap();
    assert_eq!(listing.manifests, copies);
    let mut joined = listing.unreadable;
    joined.sort();
    assert_eq!(joined, expected);
}

#[test]
fn list_and_get_report_a_target_that_is_gone() {
    let home = temp_home().unwrap();
    let gone = adapter(&home.path().join("unmounted"), &plenty);
    let error = gone.list(&project(), None).unwrap_err();
    assert_eq!(error.code(), "backup_target_unreachable", "{error}");
    let (path, capture) = artifact(home.path(), "gone-get", b"gone", 15);
    let (attempt, _) = gone.prepare(&path, &capture, uuid::Uuid::now_v7()).unwrap();
    let restored = home.path().join("restored.db");
    let error = gone
        .get(&project(), &attempt.manifest, &restored)
        .unwrap_err();
    assert_eq!(error.code(), "backup_target_unreachable", "{error}");
    assert!(!restored.exists());
}

#[test]
fn reconcile_leaves_a_pair_whose_manifest_is_not_the_attempt_s() {
    let fixture = fixture();
    let adapter = adapter(&fixture.root, &plenty);
    let (path, capture) = artifact(fixture.home.path(), "foreign", b"the attempt", 16);
    let (attempt, stored) = adapter
        .prepare(&path, &capture, uuid::Uuid::now_v7())
        .unwrap();
    let directory = project_dir(&fixture.root);
    fs::create_dir_all(&directory).unwrap();
    let data = directory.join(format!("{}.db.gz", attempt.manifest.copy));
    let manifest = directory.join(format!("{}.manifest.json", attempt.manifest.copy));
    fs::copy(&stored, &data).unwrap();
    fs::write(&manifest, b"{\"someone\": \"else\"}").unwrap();
    assert!(matches!(
        adapter.reconcile(&project(), &attempt),
        Reconciled::Unknown { .. }
    ));
    assert_eq!(fs::read(&manifest).unwrap(), b"{\"someone\": \"else\"}");
    assert!(data.is_file());
}

#[test]
fn a_copy_made_for_an_earlier_identity_is_restored_but_never_confirmed_or_touched() {
    let fixture = fixture();
    let earlier = adapter(&fixture.root, &plenty);
    let bytes = b"made before the target was set again ".repeat(100);
    let (path, capture) = artifact(fixture.home.path(), "earlier", &bytes, 17);
    let (attempt, stored) = earlier
        .prepare(&path, &capture, uuid::Uuid::now_v7())
        .unwrap();
    earlier.put(&project(), &attempt, &stored).unwrap();
    let before = names(&project_dir(&fixture.root));

    // The same directory, configured again: a new identity.
    let again = DirectoryAdapter::new(
        fixture.root.clone(),
        ObjectId::from_canonical_bytes(b"the target set again"),
        Duration::from_secs(120),
        &plenty,
    );
    let page = again.list(&project(), None).unwrap();
    assert_eq!(page.manifests, std::slice::from_ref(&attempt.manifest));
    let restored = fixture.home.path().join("restored.db");
    again.get(&project(), &attempt.manifest, &restored).unwrap();
    assert_eq!(fs::read(&restored).unwrap(), bytes);

    // Push-side requests keep to the current identity.
    assert!(matches!(
        again.confirm(&project(), &attempt.manifest),
        Confirmation::Missing { .. }
    ));
    assert!(matches!(
        again.reconcile(&project(), &attempt),
        Reconciled::Unknown { .. }
    ));
    let error = again.remove_attempt(&project(), &attempt).unwrap_err();
    assert_eq!(error.code(), "backup_copy_invalid", "{error}");
    let error = again.put(&project(), &attempt, &stored).unwrap_err();
    assert_eq!(error.code(), "backup_copy_invalid", "{error}");
    assert_eq!(names(&project_dir(&fixture.root)), before);
}

#[test]
fn get_reports_a_copy_whose_data_is_gone_as_missing() {
    let fixture = fixture();
    let adapter = adapter(&fixture.root, &plenty);
    let (path, capture) = artifact(fixture.home.path(), "gone-data", b"gone data", 18);
    let (attempt, stored) = adapter
        .prepare(&path, &capture, uuid::Uuid::now_v7())
        .unwrap();
    adapter.put(&project(), &attempt, &stored).unwrap();
    fs::remove_file(project_dir(&fixture.root).join(format!("{}.db.gz", attempt.manifest.copy)))
        .unwrap();
    let restored = fixture.home.path().join("restored.db");
    let error = adapter
        .get(&project(), &attempt.manifest, &restored)
        .unwrap_err();
    assert_eq!(error.code(), "backup_copy_missing", "{error}");
    assert!(!restored.exists());
}

#[test]
fn a_cleanup_that_fails_is_reported_beside_the_failure() {
    let home = temp_home().unwrap();
    // A directory cannot be removed as a file: the cleanup fails.
    let stuck = home.path().join("stuck");
    fs::create_dir(&stuck).unwrap();
    let error = super::directory::with_cleanup(
        AdapterError::CopyInvalid {
            path: stuck.clone(),
            reason: "the original failure".into(),
        },
        &stuck,
    );
    assert_eq!(error.code(), "backup_copy_invalid", "{error}");
    let text = error.to_string();
    assert!(text.contains("the original failure"), "{text}");
    assert!(text.contains("could not be removed"), "{text}");
    assert!(stuck.is_dir());
}

#[test]
fn preparing_stops_at_its_deadline_and_leaves_no_stored_file() {
    let fixture = fixture();
    let adapter = adapter(&fixture.root, &plenty);
    let bytes = incompressible(1 << 20);
    let (path, capture) = artifact(fixture.home.path(), "late", &bytes, 10);
    let error = adapter
        .prepare_until(&path, &capture, uuid::Uuid::now_v7(), Instant::now())
        .unwrap_err();
    assert_eq!(error.code(), "backup_capture_deadline", "{error}");
    // Only the staged copy is left in its stage.
    assert_eq!(names(path.parent().unwrap()), ["store.db"]);
}

#[test]
fn the_move_to_a_fetched_file_never_replaces_one_and_keeps_its_source_on_refusal() {
    let home = temp_home().unwrap();
    let staging = home.path().join(".out.db.1.fetching");
    let out = home.path().join("out.db");
    fs::write(&staging, b"checked copy").unwrap();
    fs::write(&out, b"already here").unwrap();
    let error = super::directory::move_without_replacing(&staging, &out).unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::AlreadyExists, "{error}");
    assert_eq!(fs::read(&out).unwrap(), b"already here");
    assert_eq!(fs::read(&staging).unwrap(), b"checked copy");

    fs::remove_file(&out).unwrap();
    super::directory::move_without_replacing(&staging, &out).unwrap();
    assert_eq!(fs::read(&out).unwrap(), b"checked copy");
    assert!(!staging.exists());
}

#[test]
fn a_staging_file_that_stays_is_named_in_the_fetch_refusal() {
    let home = temp_home().unwrap();
    // A directory cannot be removed as a file: the removal fails.
    let stuck = home.path().join(".out.db.1.fetching");
    fs::create_dir(&stuck).unwrap();
    let failure = super::fetch::removing(
        super::fetch::ReadFailure::new("backup_copy_exists", "the original failure"),
        &stuck,
    );
    assert_eq!(failure.code, "backup_copy_exists");
    assert!(
        failure.message.starts_with("the original failure; "),
        "{}",
        failure.message
    );
    assert!(
        failure
            .message
            .contains(&format!("{} could not be removed", stuck.display())),
        "{}",
        failure.message
    );

    let gone = home.path().join(".gone.db.1.fetching");
    let failure = super::fetch::removing(
        super::fetch::ReadFailure::new("backup_io", "the original failure"),
        &gone,
    );
    assert_eq!(failure.message, "the original failure");
}
