use super::ring::{Ring, Sink};
use super::*;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::{
    fs,
    io::{self, Write},
    path::Path,
    time::Duration,
};

// Record assertions need completion, not the best-effort production exit
// budget. This finite watchdog bounds a broken test under host or disk load.
pub(crate) const RECORD_DRAIN: Duration = Duration::from_secs(30);

pub(crate) fn drain(sink: &Diagnostics) {
    assert!(
        sink.flush(RECORD_DRAIN),
        "diagnostic records did not drain within {RECORD_DRAIN:?}: {sink:#?}"
    );
}

pub(crate) fn records(home: &Path) -> Vec<Value> {
    (0..4)
        .flat_map(|index| {
            fs::read_to_string(home.join(format!("diagnostics/engram-{index}.jsonl")))
                .unwrap_or_default()
                .lines()
                .map(|line| serde_json::from_str(line).unwrap())
                .collect::<Vec<_>>()
        })
        .collect()
}

#[test]
fn diagnostics_writer_admission_keeps_native_fields_without_source_text() {
    let home = crate::test_support::temp_home().unwrap();
    let sentinel = "PRIVATE SQL path and argument";
    let sink = Diagnostics::start(home.path(), "session-11296", Surface::Mcp);
    let error = crate::StoreError::WorkWriterAdmissionRefused {
        reason: crate::storage::WorkWriterAdmissionReason::BusyBudgetExhausted,
        elapsed_ms: 10_007,
        budget_ms: 10_000,
        sqlite_primary_code: Some(rusqlite::ffi::SQLITE_BUSY),
        sqlite_extended_code: Some(rusqlite::ffi::SQLITE_BUSY),
        source: Box::new(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            Some(sentinel.into()),
        )),
    };
    sink.emit(Event::failure(
        "note",
        Correlation::default(),
        Duration::from_millis(10_011),
        Fault::store(&error),
    ));
    drain(&sink);
    let rows = records(home.path());
    let event = &rows
        .iter()
        .find(|row| row["event"]["code"] == "writer_admission_refused")
        .unwrap()["event"];
    assert_eq!(event["writer_admission"]["reason"], "busy_budget_exhausted");
    assert_eq!(event["writer_admission"]["elapsed_ms"], 10_007);
    assert_eq!(event["writer_admission"]["budget_ms"], 10_000);
    assert_eq!(
        event["writer_admission"]["sqlite_primary_code"],
        rusqlite::ffi::SQLITE_BUSY
    );
    assert_eq!(
        event["writer_admission"]["sqlite_extended_code"],
        rusqlite::ffi::SQLITE_BUSY
    );
    assert_eq!(event["elapsed_ms"], 10_011);
    assert!(!serde_json::to_string(&rows).unwrap().contains(sentinel));
}

#[test]
fn exact_fingerprints_and_privacy_survive_normal_exit() {
    let home = crate::test_support::temp_home().unwrap();
    let sentinel = "PRIVATE actor SQL SELECT token path C:/secret arbitrary identity";
    let sink = Diagnostics::start(home.path(), sentinel, Surface::Mcp);
    let error = crate::StoreError::Sqlite(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
        Some(sentinel.into()),
    ));
    sink.emit(Event::failure(
        sentinel,
        Correlation {
            attempt_fingerprint: Some(Fingerprint::attempt(sentinel)),
            ..Correlation::default()
        },
        Duration::from_secs(6),
        Fault::store(&error),
    ));
    drain(&sink);
    let rows = records(home.path());
    assert_eq!(rows.len(), 2);
    let row = rows
        .iter()
        .find(|row| row["event"]["code"] == "sqlite_busy")
        .unwrap();
    assert_eq!(row["event"]["sqlite_code"], rusqlite::ffi::SQLITE_BUSY);
    assert_eq!(row["event"]["elapsed_ms"], 6000);
    assert_eq!(row["event"]["operation"], "unknown");
    assert!(row["producer"]["session_id"].is_null());
    let mut reference = Sha256::new();
    reference.update(b"engram-diagnostic-session-v1");
    reference.update([0]);
    reference.update(sentinel.as_bytes());
    assert_eq!(
        row["producer"]["session_fingerprint"],
        format!("{:x}", reference.finalize())
    );
    assert_ne!(
        Fingerprint::session(sentinel),
        Fingerprint::attempt(sentinel)
    );
    assert_ne!(Fingerprint::attempt(" key"), Fingerprint::attempt("key"));
    assert!(!serde_json::to_string(&rows).unwrap().contains(sentinel));
    assert_eq!(
        row["producer"]["build_fingerprint"],
        crate::build_identity::current()
            .build_fingerprint
            .as_ref()
            .unwrap()
            .as_str()
    );
}

#[test]
fn ring_repairs_only_a_bounded_torn_tail_and_refuses_bad_layouts() {
    let home = crate::test_support::temp_home().unwrap();
    let ring = Ring::new(home.path());
    ring.append(b"{\"ok\":1}\n").unwrap();
    fs::OpenOptions::new()
        .append(true)
        .open(home.path().join("diagnostics/engram-0.jsonl"))
        .unwrap()
        .write_all(b"{\"torn\":")
        .unwrap();
    ring.append(b"{\"ok\":2}\n").unwrap();
    assert_eq!(records(home.path()).len(), 2);
    fs::write(home.path().join("diagnostics/engram.lock"), b"not empty").unwrap();
    assert_eq!(
        ring.append(b"{}\n").unwrap_err().kind(),
        io::ErrorKind::InvalidData
    );
    fs::write(home.path().join("diagnostics/engram.lock"), b"").unwrap();
    fs::create_dir(home.path().join("diagnostics/engram-1.jsonl")).unwrap();
    assert_eq!(
        ring.append(b"{}\n").unwrap_err().kind(),
        io::ErrorKind::InvalidInput
    );
    assert_eq!(
        ring.append(&vec![0; MAX_RECORD_BYTES + 1])
            .unwrap_err()
            .kind(),
        io::ErrorKind::InvalidData
    );
}

#[test]
fn concurrent_writers_rotate_without_interleaving_or_exceeding_bounds() {
    let home = crate::test_support::temp_home().unwrap();
    let seed = format!("{{\"padding\":\"{}\"}}\n", "x".repeat(3900));
    for _ in 0..1100 {
        Ring::new(home.path()).append(seed.as_bytes()).unwrap();
    }
    let successes = std::thread::scope(|scope| {
        let writers = (0..2).map(|producer| { let root = home.path(); scope.spawn(move || {
            let ring = Ring::new(root); let mut successes = 0;
            for sequence in 0..800 {
                let line = format!("{{\"producer\":{producer},\"sequence\":{sequence},\"padding\":\"{}\"}}\n", "x".repeat(3900));
                match ring.append(line.as_bytes()) { Ok(()) => successes += 1, Err(error) => assert_eq!(error.kind(), io::ErrorKind::WouldBlock) }
            }
            successes
        }) }).collect::<Vec<_>>();
        writers
            .into_iter()
            .map(|writer| writer.join().unwrap())
            .sum::<usize>()
    });
    assert!(successes > 0);
    let rows = records(home.path());
    assert_ne!(rows.len(), 0);
    let mut total = 0;
    for entry in fs::read_dir(home.path().join("diagnostics")).unwrap() {
        let entry = entry.unwrap();
        let size = entry.metadata().unwrap().len();
        assert!(size <= ring::FILE_BYTES);
        total += size;
    }
    assert!(total <= 4 * ring::FILE_BYTES);
    assert_eq!(
        fs::metadata(home.path().join("diagnostics/engram.lock"))
            .unwrap()
            .len(),
        0
    );
}

#[test]
fn record_write_handles_partial_writes_full_disk_and_flush_failure() {
    struct Broken {
        writes: usize,
        fail_flush: bool,
    }
    impl Write for Broken {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.writes += 1;
            if self.writes == 1 {
                Ok(bytes.len().min(2))
            } else if self.fail_flush {
                Ok(bytes.len())
            } else {
                Err(io::ErrorKind::StorageFull.into())
            }
        }
        fn flush(&mut self) -> io::Result<()> {
            Err(io::ErrorKind::PermissionDenied.into())
        }
    }
    assert_eq!(
        ring::write_record(
            &mut Broken {
                writes: 0,
                fail_flush: false
            },
            b"record\n"
        )
        .unwrap_err()
        .kind(),
        io::ErrorKind::StorageFull
    );
    assert_eq!(
        ring::write_record(
            &mut Broken {
                writes: 0,
                fail_flush: true
            },
            b"record\n"
        )
        .unwrap_err()
        .kind(),
        io::ErrorKind::PermissionDenied
    );
}

#[test]
fn real_sqlite_contention_preserves_typed_codes_without_logging_sql() {
    let home = crate::test_support::temp_home().unwrap();
    let database = home.path().join("contention.sqlite3");
    let first = rusqlite::Connection::open(&database).unwrap();
    first
        .execute_batch("CREATE TABLE observed (value TEXT); BEGIN IMMEDIATE;")
        .unwrap();
    let second = rusqlite::Connection::open(&database).unwrap();
    second.busy_timeout(Duration::ZERO).unwrap();
    let error = second
        .execute_batch("INSERT INTO observed VALUES ('PRIVATE-SQL-text')")
        .unwrap_err();
    let sink = Diagnostics::start(home.path(), "session-1", Surface::Mcp);
    sink.emit(Event::failure(
        "note",
        Correlation::default(),
        Duration::ZERO,
        Fault::store(&crate::StoreError::Sqlite(error)),
    ));
    drain(&sink);
    let rows = records(home.path());
    let error = rows
        .iter()
        .find(|row| row["event"]["code"] == "sqlite_busy")
        .unwrap();
    assert_eq!(error["event"]["sqlite_code"], rusqlite::ffi::SQLITE_BUSY);
    assert!(!serde_json::to_string(&rows).unwrap().contains("PRIVATE"));
}

#[test]
fn diagnostic_ring_process_worker() {
    let Some(root) = std::env::var_os("ENGRAM_DIAGNOSTICS_TEST_HOME") else {
        return;
    };
    let root = std::path::PathBuf::from(root).canonicalize().unwrap();
    let anchor = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("target")
        .canonicalize()
        .unwrap();
    assert!(root.starts_with(anchor));
    let ring = Ring::new(&root);
    let mut written = 0;
    for sequence in 0..800 {
        let line = format!(
            "{{\"pid\":{},\"sequence\":{sequence},\"padding\":\"{}\"}}\n",
            std::process::id(),
            "x".repeat(3900)
        );
        match ring.append(line.as_bytes()) {
            Ok(()) => written += 1,
            Err(error) => assert_eq!(error.kind(), io::ErrorKind::WouldBlock),
        }
    }
    assert!(written > 0);
}

#[test]
fn separate_processes_share_one_bounded_ring_and_never_interleave() {
    let home = crate::test_support::temp_home().unwrap();
    let seed = format!("{{\"padding\":\"{}\"}}\n", "x".repeat(3900));
    for _ in 0..1100 {
        Ring::new(home.path()).append(seed.as_bytes()).unwrap();
    }
    let mut children = Vec::new();
    for _ in 0..2 {
        children.push(
            std::process::Command::new(std::env::current_exe().unwrap())
                .args([
                    "--exact",
                    "diagnostics::tests::diagnostic_ring_process_worker",
                    "--nocapture",
                ])
                .env("ENGRAM_DIAGNOSTICS_TEST_HOME", home.path())
                .stdout(std::process::Stdio::piped())
                .stderr(std::process::Stdio::piped())
                .spawn()
                .unwrap(),
        );
    }
    for child in children {
        let output = child.wait_with_output().unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
    }
    let rows = records(home.path());
    assert_ne!(rows.len(), 0);
    let mut total = 0;
    for index in 0..4 {
        let path = home
            .path()
            .join(format!("diagnostics/engram-{index}.jsonl"));
        let size = fs::metadata(&path).unwrap().len();
        assert!(size <= ring::FILE_BYTES);
        total += size;
        for line in fs::read(&path)
            .unwrap()
            .split_inclusive(|byte| *byte == b'\n')
        {
            assert!(line.len() <= MAX_RECORD_BYTES);
            let _: Value = serde_json::from_slice(line).unwrap();
        }
    }
    assert!(total <= 4 * ring::FILE_BYTES);
}
