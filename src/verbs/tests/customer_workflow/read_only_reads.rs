//! The read words open the existing store read-only: they succeed where the
//! process cannot write the database or its WAL file, change neither, and
//! never create or initialize a store.

use super::*;
use std::path::{Path, PathBuf};

/// The database and WAL files of a live store made unwritable for this
/// process, as a reader without write access sees them. The shared-memory
/// file stays as the live store has it. Writability is restored on drop so
/// that the fixture directory can be removed.
pub(in crate::verbs::tests) struct UnwritableStoreFiles {
    paths: Vec<PathBuf>,
}

impl UnwritableStoreFiles {
    pub(in crate::verbs::tests) fn deny(database: &Path) -> Self {
        let wal = PathBuf::from(format!("{}-wal", database.display()));
        let mut denied = Self { paths: Vec::new() };
        for path in [database.to_path_buf(), wal] {
            if !path.exists() {
                continue;
            }
            set_writable(&path, false);
            // Restored on drop even if the check below fails.
            denied.paths.push(path.clone());
            // The precondition is shown, not assumed. A process that can
            // write a read-only file anyway (root on POSIX) cannot model a
            // reader without write access, and says so.
            assert!(
                std::fs::OpenOptions::new().write(true).open(&path).is_err(),
                "{} must not be writable by this process; a process that bypasses file permissions (root on POSIX) cannot run this test",
                path.display()
            );
        }
        denied
    }
}

impl Drop for UnwritableStoreFiles {
    fn drop(&mut self) {
        for path in &self.paths {
            set_writable(path, true);
        }
    }
}

#[allow(
    clippy::permissions_set_readonly_false,
    reason = "a fixture file inside the test's own temporary directory is made writable again for cleanup"
)]
fn set_writable(path: &Path, writable: bool) {
    let mut permissions = std::fs::metadata(path).expect("fixture file").permissions();
    permissions.set_readonly(!writable);
    std::fs::set_permissions(path, permissions).expect("change the fixture file's permissions");
}

type ReadForm = (
    &'static str,
    Box<dyn Fn(&AgentVerbs) -> Result<Receipt, VerbError>>,
);

/// Every form of the read words that records nothing: `ls` and `search`,
/// `show` in every form, `memories` in every form but the recording one, and
/// the non-advancing `next --peek`.
fn read_forms(item: &str, locator: &str) -> Vec<ReadForm> {
    let item = item.to_owned();
    let show = move |input: ShowInput| {
        let item = item.clone();
        Box::new(move |verbs: &AgentVerbs| verbs.show_records(&item, &input, at(100)))
            as Box<dyn Fn(&AgentVerbs) -> Result<Receipt, VerbError>>
    };
    let memories = |input: MemoriesInput| {
        Box::new(move |verbs: &AgentVerbs| verbs.memories(&input, at(100)))
            as Box<dyn Fn(&AgentVerbs) -> Result<Receipt, VerbError>>
    };
    let ls = |input: LsInput| {
        Box::new(move |verbs: &AgentVerbs| verbs.ls(&input, at(100)))
            as Box<dyn Fn(&AgentVerbs) -> Result<Receipt, VerbError>>
    };
    vec![
        ("ls", ls(LsInput::default())),
        (
            "ls --all --limit 1",
            ls(LsInput {
                all: true,
                limit: Some(1),
                ..LsInput::default()
            }),
        ),
        (
            "ls --ready",
            ls(LsInput {
                ready: true,
                ..LsInput::default()
            }),
        ),
        (
            "search",
            Box::new(|verbs: &AgentVerbs| verbs.search("Readable", None, at(100))),
        ),
        ("show", show(ShowInput::default())),
        (
            "show --full",
            show(ShowInput {
                full: true,
                ..ShowInput::default()
            }),
        ),
        (
            "show --notes",
            show(ShowInput {
                notes: true,
                ..ShowInput::default()
            }),
        ),
        (
            "show --notes --gates",
            show(ShowInput {
                notes: true,
                gates: true,
                ..ShowInput::default()
            }),
        ),
        (
            "show --history",
            show(ShowInput {
                history: true,
                ..ShowInput::default()
            }),
        ),
        (
            "show --note",
            show(ShowInput {
                note: Some(locator.to_owned()),
                ..ShowInput::default()
            }),
        ),
        (
            "show --evaluations",
            show(ShowInput {
                evaluations: true,
                ..ShowInput::default()
            }),
        ),
        ("memories", memories(MemoriesInput::default())),
        (
            "memories QUERY",
            memories(MemoriesInput {
                query: Some("rule".into()),
                ..MemoriesInput::default()
            }),
        ),
        (
            "memories --after",
            memories(MemoriesInput {
                after: Some("alpha".into()),
                ..MemoriesInput::default()
            }),
        ),
        (
            "memories KEY --full",
            memories(MemoriesInput {
                query: Some("alpha".into()),
                full: true,
                ..MemoriesInput::default()
            }),
        ),
        (
            "memories KEY --full --revision",
            memories(MemoriesInput {
                query: Some("alpha".into()),
                full: true,
                revision: Some(1),
                ..MemoriesInput::default()
            }),
        ),
        (
            "next --peek",
            Box::new(|verbs: &AgentVerbs| {
                verbs.next(
                    &NextInput {
                        peek: true,
                        ..NextInput::default()
                    },
                    at(100),
                )
            }),
        ),
    ]
}

/// A store with one item, a note, a gate and two memories, one of them
/// revised, written by another session that keeps its connection open, as a
/// live store is.
fn established_store() -> (
    crate::test_support::TempHome,
    AgentVerbs,
    PathBuf,
    ProjectId,
    String,
    String,
) {
    let (home, writer, path, project) = fixture();
    let item = add(&writer, "Readable item", None, false, 0);
    writer
        .claim(
            ClaimInput {
                work_ref: item.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(1),
        )
        .expect("claim");
    note(&writer, &item, "A note to read", 2);
    writer
        .gate(
            GateInput {
                work_ref: Some(item.clone()),
                name: "fmt".into(),
                failed: Vec::new(),
                evidence_ref: None,
            },
            at(3),
        )
        .expect("gate");
    for (key, now) in [("alpha", 4), ("beta", 5)] {
        writer
            .service
            .remember_project_memory(
                format!("A rule named {key}"),
                Some(key.into()),
                false,
                None,
                at(now),
            )
            .expect("remember");
    }
    writer
        .service
        .remember_project_memory(
            "A rule named alpha, revised".into(),
            Some("alpha".into()),
            true,
            Some(1),
            at(6),
        )
        .expect("revise");
    let locator = writer
        .show_with_notes(&item, true, at(7))
        .expect("notes")
        .value["notes"][0]["locator"]
        .as_str()
        .expect("note locator")
        .to_owned();
    (home, writer, path, project, item, locator)
}

#[test]
fn every_read_word_succeeds_where_the_database_and_wal_files_cannot_be_written() {
    let (_home, _writer, path, project, item, locator) = established_store();
    let wal = PathBuf::from(format!("{}-wal", path.display()));
    assert!(wal.exists(), "a live store has its WAL file");
    let reader = AgentVerbs::new(
        path.clone(),
        project,
        "reader".into(),
        SessionId("reader".into()),
        None,
    );
    let _unwritable = UnwritableStoreFiles::deny(&path);
    let database_before = std::fs::read(&path).unwrap();
    let wal_before = std::fs::read(&wal).unwrap();
    for (form, read) in read_forms(&item, &locator) {
        let receipt = read(&reader).unwrap_or_else(|error| panic!("{form}: {error}"));
        assert!(!receipt.text().is_empty(), "{form}");
        assert_eq!(std::fs::read(&path).unwrap(), database_before, "{form}");
        assert_eq!(std::fs::read(&wal).unwrap(), wal_before, "{form}");
    }
    // The detail of one evaluation record reaches the store and finds no such
    // record; it is refused for that, not for want of write access.
    let missing = reader
        .show_records(
            &item,
            &ShowInput {
                evaluation: Some("0".repeat(64)),
                ..ShowInput::default()
            },
            at(100),
        )
        .expect_err("no such evaluation record");
    assert!(
        !matches!(
            missing.error,
            StoreError::Sqlite(_) | StoreError::StoreNotInitialized
        ),
        "{}",
        missing.error
    );
}

#[test]
fn every_read_word_refuses_a_path_without_a_store_and_creates_none() {
    let home = crate::test_support::temp_home().expect("temp");
    let project = ProjectId("read-only-reads".into());
    let absent = home.path().join("absent.db");
    let absent_directory = home.path().join("absent-directory").join("work.db");
    let empty = home.path().join("empty.db");
    std::fs::write(&empty, b"").unwrap();
    let schemaless = home.path().join("schemaless.db");
    rusqlite::Connection::open(&schemaless)
        .unwrap()
        .execute_batch("PRAGMA user_version = 7;")
        .unwrap();
    for path in [&absent, &absent_directory, &empty, &schemaless] {
        let before = std::fs::read(path).ok();
        let reader = AgentVerbs::new(
            path.clone(),
            project.clone(),
            "reader".into(),
            SessionId("reader".into()),
            None,
        );
        let mut forms = read_forms("w-000000000001", &"0".repeat(32));
        forms.push((
            "show --evaluation",
            Box::new(|verbs: &AgentVerbs| {
                verbs.show_records(
                    "w-000000000001",
                    &ShowInput {
                        evaluation: Some("0".repeat(64)),
                        ..ShowInput::default()
                    },
                    at(100),
                )
            }),
        ));
        for (form, read) in forms {
            let error = read(&reader).expect_err(form);
            assert!(
                matches!(error.error, StoreError::StoreNotInitialized),
                "{form} on {}: {}",
                path.display(),
                error.error
            );
        }
        drop(reader);
        assert_eq!(std::fs::read(path).ok(), before, "{}", path.display());
        let sidecars = ["-wal", "-shm", "-journal"]
            .iter()
            .filter(|suffix| PathBuf::from(format!("{}{suffix}", path.display())).exists())
            .count();
        assert_eq!(sidecars, 0, "{}", path.display());
    }
    assert!(!absent_directory.parent().unwrap().exists());
}

#[test]
fn a_read_connection_cannot_write_even_where_the_files_can_be() {
    let (_home, _writer, path, project, item, _) = established_store();
    let reader = AgentVerbs::new(
        path.clone(),
        project,
        "reader".into(),
        SessionId("reader".into()),
        None,
    );
    let inspect = rusqlite::Connection::open(&path).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&inspect).unwrap();
    for (form, read) in read_forms(&item, &"0".repeat(32)) {
        // The note locator here is not a note's; that form refuses and every
        // other form succeeds. Either way nothing is written.
        let _ = read(&reader);
        assert_eq!(
            crate::storage::test_database_shape_snapshot(&inspect).unwrap(),
            before,
            "{form}"
        );
    }
}
