use super::{capture, prepare_with_producer, producer, profile, tree};
use crate::{
    ObjectId, ProjectId,
    task_sync::{
        SharedActor, TaskTree,
        notes::{SharedNote, SharedNoteKind},
        progress::{
            ExportProfile, PreparedCandidate, PreparedSelection, documents_fingerprint, journal,
            read_prepared,
        },
    },
};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::SystemTime,
};

struct Fixture {
    home: crate::test_support::TempHome,
    store: PathBuf,
    progress: PathBuf,
    generation: PathBuf,
    selection: PreparedSelection,
}

impl Fixture {
    fn new() -> Self {
        Self::with_tree(tree(2), profile())
    }

    fn with_tree(tree: TaskTree, profile: ExportProfile) -> Self {
        let home = crate::test_support::temp_home().unwrap();
        let store = home.path().join("store.db");
        fs::write(&store, b"route, not an opened SQLite database").unwrap();
        let progress = home.path().join("progress");
        let destination = home.path().join("candidate");
        let mut captured = capture(tree.clone());
        captured.omissions.counts.insert("excluded-note".into(), 3);
        captured
            .activity_omissions
            .counts
            .insert("private-activity".into(), 5);
        let receipt = prepare_with_producer(
            &destination,
            &progress,
            &store,
            &tree.project.project_id,
            &profile,
            false,
            &producer(),
            |_| Ok(captured),
        )
        .unwrap();
        let generation = journal::load(&progress).unwrap().current.unwrap().directory;
        Self {
            home,
            store,
            progress,
            generation,
            selection: PreparedSelection {
                operation: receipt.operation,
                destination,
                project: tree.project.project_id,
                profile,
            },
        }
    }

    fn read(&self) -> PreparedCandidate {
        let before = snapshot(self.home.path());
        let result = read_prepared(&self.progress, &self.selection).unwrap();
        assert_eq!(
            before,
            snapshot(self.home.path()),
            "read changed an input entry, timestamp or bytes"
        );
        result
    }

    fn refuse(&self) {
        let before = snapshot(self.home.path());
        assert!(read_prepared(&self.progress, &self.selection).is_err());
        assert_eq!(
            before,
            snapshot(self.home.path()),
            "refusal changed an input entry, timestamp or bytes"
        );
    }

    fn rewrite_state(&self, mutate: impl FnOnce(&mut serde_json::Value)) {
        let path = self.generation.join("state.json");
        let mut state: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        mutate(&mut state);
        fs::write(path, serde_json::to_vec(&state).unwrap()).unwrap();
    }
}

type Snapshot = BTreeMap<PathBuf, (Option<Vec<u8>>, SystemTime)>;

fn snapshot(root: &Path) -> Snapshot {
    fn visit(root: &Path, path: &Path, entries: &mut Snapshot) {
        let meta = fs::symlink_metadata(path).unwrap();
        let link = meta.file_type().is_symlink();
        #[cfg(windows)]
        let link = {
            use std::os::windows::fs::MetadataExt;
            link || meta.file_attributes() & 0x400 != 0
        };
        let bytes = if link {
            Some(b"link: not traversed".to_vec())
        } else if meta.is_file() {
            Some(fs::read(path).unwrap())
        } else {
            None
        };
        entries.insert(
            path.strip_prefix(root).unwrap().to_owned(),
            (bytes, meta.modified().unwrap()),
        );
        if meta.is_dir() && !link {
            for entry in fs::read_dir(path).unwrap() {
                visit(root, &entry.unwrap().path(), entries);
            }
        }
    }
    let mut entries = BTreeMap::new();
    visit(root, root, &mut entries);
    entries
}

#[test]
fn task_prepared_reader_returns_selected_bytes_and_provenance_without_mutation() {
    let fixture = Fixture::new();
    let result = fixture.read();
    assert_eq!(result.provenance.operation, fixture.selection.operation);
    assert_eq!(result.provenance.project, fixture.selection.project);
    assert_eq!(result.provenance.profile, fixture.selection.profile);
    assert_eq!(result.provenance.sequence, 1);
    assert_eq!(result.provenance.predecessor, None);
    assert_eq!(result.provenance.producer, producer());
    assert_eq!(
        result.provenance.store,
        fs::canonicalize(&fixture.store).unwrap()
    );
    assert_eq!(
        result.provenance.destination,
        fs::canonicalize(&fixture.selection.destination).unwrap()
    );
    assert_eq!(result.provenance.omissions.counts["excluded-note"], 3);
    assert_eq!(
        result.provenance.activity_omissions.counts["private-activity"],
        5
    );
    for (path, bytes) in &result.documents {
        assert_eq!(
            *bytes,
            fs::read(fixture.selection.destination.join(path)).unwrap()
        );
    }
    fs::write(&fixture.store, b"source changed after preparation").unwrap();
    assert_eq!(
        fixture.read().documents,
        result.documents,
        "reader recaptured the source"
    );
}

#[test]
fn task_prepared_reader_selects_retained_predecessor_and_refuses_pruned_operation() {
    let fixture = Fixture::new();
    let original = fixture.read().documents;
    let mut latest = None;
    for number in 2..=3 {
        let receipt = prepare_with_producer(
            &fixture.home.path().join(format!("candidate-{number}")),
            &fixture.progress,
            &fixture.store,
            &fixture.selection.project,
            &fixture.selection.profile,
            false,
            &producer(),
            |_| Ok(capture(tree(number))),
        )
        .unwrap();
        latest = Some(receipt.operation);
        if number == 2 {
            assert_eq!(fixture.read().documents, original);
        }
    }
    fixture.refuse();
    let selection = PreparedSelection {
        operation: latest.unwrap(),
        destination: fixture.home.path().join("candidate-3"),
        ..fixture.selection.clone()
    };
    let before = snapshot(fixture.home.path());
    assert_eq!(
        read_prepared(&fixture.progress, &selection)
            .unwrap()
            .tree
            .tasks
            .len(),
        3
    );
    assert_eq!(before, snapshot(fixture.home.path()));
}

#[test]
fn task_prepared_reader_preserves_noncanonical_retained_bytes_and_legacy_disclosure() {
    let mut retained = tree(1);
    retained.project.notes_included = true;
    retained.tasks[0].labels.push("SeCrEt".into());
    retained.tasks[0].notes.push(SharedNote {
        id: ObjectId::mint(),
        kind: SharedNoteKind::Generic,
        body: "root=/private".into(),
        author: SharedActor {
            actor_id: "fixture".into(),
            actor_kind: "agent".into(),
        },
        recorded_at: chrono::Utc::now(),
        origin_machine: "fixture-host".into(),
    });
    assert!(retained.validate().is_err());
    let fixture = Fixture::with_tree(
        retained,
        ExportProfile {
            include_notes: true,
            origin_machine: Some("fixture-host".into()),
        },
    );
    let mut documents = fixture.read().documents;
    for (path, bytes) in &mut documents {
        bytes.splice(0..0, b" \r\n\t".iter().copied());
        fs::write(fixture.generation.join("candidate").join(path), &*bytes).unwrap();
        fs::write(fixture.selection.destination.join(path), &*bytes).unwrap();
    }
    fixture.rewrite_state(|state| {
        state["documents_fingerprint"] = documents_fingerprint(&documents).into();
    });
    let read = fixture.read();
    assert_eq!(read.documents, documents);
    assert!(
        read.tree.validate().is_err(),
        "reader silently applied today's disclosure admission"
    );
    assert_ne!(
        read.tree.documents().unwrap(),
        read.documents,
        "test bytes would not distinguish reserialization"
    );
}

#[test]
fn task_prepared_reader_refuses_selection_drift_and_missing_paths_without_creation() {
    for changed in [
        "operation",
        "project",
        "profile",
        "alias",
        "destination",
        "missing output",
        "missing progress",
    ] {
        let mut fixture = Fixture::new();
        match changed {
            "operation" => fixture.selection.operation = uuid::Uuid::now_v7(),
            "project" => fixture.selection.project = ProjectId("other-project".into()),
            "profile" => {
                fixture.selection.profile.include_notes = true;
                fixture.selection.profile.origin_machine = Some("other".into());
            }
            "alias" => fixture.selection.profile.origin_machine = Some("other".into()),
            "destination" => {
                fixture.selection.destination = fixture.home.path().join("other");
                fs::create_dir(&fixture.selection.destination).unwrap();
            }
            "missing output" => {
                fixture.selection.destination = fixture.home.path().join("missing-output");
            }
            "missing progress" => fixture.progress = fixture.home.path().join("missing-progress"),
            _ => unreachable!(),
        }
        fixture.refuse();
    }
}

#[test]
fn task_prepared_reader_preserves_pending_unknown_and_malformed_entries() {
    for changed in [
        "staging",
        "complete temporary",
        "unknown root",
        "unknown generation",
        "incomplete",
        "complete bytes",
        "bad state",
        "duplicate state",
        "unknown candidate",
        "unknown tasks",
        "directory file",
        "oversize",
        "divergent generation",
        "divergent destination",
        "noncanonical filename",
    ] {
        let fixture = Fixture::new();
        match changed {
            "staging" => {
                fs::create_dir(
                    fixture
                        .progress
                        .join(format!(".staging-{}", uuid::Uuid::now_v7())),
                )
                .unwrap();
            }
            "complete temporary" => {
                fs::write(fixture.generation.join(".complete-owned-pending"), b"keep").unwrap();
            }
            "unknown root" => fs::write(fixture.progress.join("unowned"), b"keep").unwrap(),
            "unknown generation" => fs::write(fixture.generation.join("unowned"), b"keep").unwrap(),
            "incomplete" => fs::remove_file(fixture.generation.join("complete")).unwrap(),
            "complete bytes" => {
                fs::write(fixture.generation.join("complete"), b"not this operation").unwrap();
            }
            "bad state" => fs::write(fixture.generation.join("state.json"), b"invalid").unwrap(),
            "duplicate state" => {
                let path = fixture.generation.join("state.json");
                let mut bytes = fs::read(&path).unwrap();
                bytes.splice(1..1, b"\"sequence\":1,".iter().copied());
                fs::write(path, bytes).unwrap();
            }
            "unknown candidate" => {
                fs::write(fixture.generation.join("candidate/unknown"), b"keep").unwrap();
            }
            "unknown tasks" => fs::write(
                fixture.generation.join("candidate/tasks/unowned.txt"),
                b"keep",
            )
            .unwrap(),
            "directory file" => {
                let path = fixture.generation.join("candidate/project.json");
                fs::remove_file(&path).unwrap();
                fs::create_dir(path).unwrap();
            }
            "oversize" => fs::write(
                fixture.generation.join("candidate/project.json"),
                vec![b' '; 1024 * 1024 + 1],
            )
            .unwrap(),
            "divergent generation" => {
                let path = fixture.generation.join("candidate/project.json");
                let mut bytes = fs::read(&path).unwrap();
                bytes.push(b' ');
                fs::write(path, bytes).unwrap();
            }
            "divergent destination" => {
                let path = fixture.selection.destination.join("project.json");
                let mut bytes = fs::read(&path).unwrap();
                bytes.push(b' ');
                fs::write(path, bytes).unwrap();
            }
            "noncanonical filename" => {
                let root = fixture.generation.join("candidate/tasks");
                let old = fs::read_dir(&root).unwrap().next().unwrap().unwrap().path();
                let renamed = root.join(old.file_name().unwrap().to_str().unwrap().to_uppercase());
                fs::rename(old, renamed).unwrap();
            }
            _ => unreachable!(),
        }
        fixture.refuse();
    }
}

#[test]
fn task_prepared_reader_refuses_metadata_and_graph_drift() {
    for changed in [
        "project",
        "profile",
        "sequence",
        "operation",
        "cut",
        "destination",
        "predecessor",
    ] {
        let fixture = Fixture::new();
        fixture.rewrite_state(|state| match changed {
            "project" => state["project"] = "different".into(),
            "profile" => state["profile"]["origin_machine"] = "different".into(),
            "sequence" => state["sequence"] = 2.into(),
            "operation" => state["operation"] = uuid::Uuid::now_v7().to_string().into(),
            "cut" => state["cut"]["position"] = 1.into(),
            "destination" => {
                state["destination"] = fixture.home.path().join("missing").to_str().unwrap().into();
            }
            "predecessor" => state["predecessor"] = uuid::Uuid::now_v7().to_string().into(),
            _ => unreachable!(),
        });
        fixture.refuse();
    }
    let fixture = Fixture::new();
    let path = fs::read_dir(fixture.generation.join("candidate/tasks"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let mut task: serde_json::Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
    task["parent_id"] = uuid::Uuid::now_v7().to_string().into();
    fs::write(path, serde_json::to_vec(&task).unwrap()).unwrap();
    fixture.refuse();
}

#[test]
fn task_prepared_reader_does_not_acquire_or_create_a_lock() {
    let fixture = Fixture::new();
    let expected = fixture.read().documents;
    let lock_path = fixture.progress.join("prepare.lock");
    fs::write(&lock_path, [0xa5; 64]).unwrap();
    let before = snapshot(fixture.home.path());
    let lock = crate::task_sync::progress::lock::ProgressLock::acquire(&fixture.progress).unwrap();
    let result = read_prepared(&fixture.progress, &fixture.selection)
        .expect("reader must inspect the candidate while preparation holds its lock");
    assert_eq!(result.provenance.operation, fixture.selection.operation);
    assert_eq!(result.documents, expected);
    drop(lock);
    assert_eq!(before, snapshot(fixture.home.path()));
    fs::remove_file(&lock_path).unwrap();
    fixture.read();
    assert!(!lock_path.exists());
}

#[test]
fn task_prepared_reader_refuses_nonordinary_or_oversize_lock_without_mutation() {
    for directory in [false, true] {
        let fixture = Fixture::new();
        let lock_path = fixture.progress.join("prepare.lock");
        if directory {
            fs::remove_file(&lock_path).unwrap();
            fs::create_dir(&lock_path).unwrap();
        } else {
            fs::write(&lock_path, [0xa5; 65]).unwrap();
        }
        fixture.refuse();
    }
}

#[cfg(windows)]
#[test]
fn task_prepared_reader_refuses_junction_candidates_generations_and_ancestors() {
    for changed in [
        "candidate",
        "tasks",
        "generation",
        "ancestor",
        "destination",
    ] {
        let fixture = Fixture::new();
        let link = fixture.home.path().join("linked");
        fixture.read();
        let target = match changed {
            "candidate" => fixture.generation.join("candidate"),
            "tasks" => fixture.generation.join("candidate/tasks"),
            "generation" => fixture.generation.clone(),
            "ancestor" => fixture.progress.clone(),
            "destination" => fixture.selection.destination.clone(),
            _ => unreachable!(),
        };
        junction::create(&target, &link).unwrap();
        let mut selection = fixture.selection.clone();
        let progress = match changed {
            "ancestor" => link.clone(),
            "destination" => {
                selection.destination = link.clone();
                fixture.progress.clone()
            }
            _ => {
                let original = fixture.home.path().join("retained-directory");
                junction::delete(&link).unwrap();
                fs::rename(&target, &original).unwrap();
                junction::create(&original, &target).unwrap();
                let before = snapshot(fixture.home.path());
                assert_link_refusal(read_prepared(&fixture.progress, &selection));
                assert_eq!(before, snapshot(fixture.home.path()));
                junction::delete(&target).unwrap();
                continue;
            }
        };
        let before = snapshot(fixture.home.path());
        assert_link_refusal(read_prepared(&progress, &selection));
        assert_eq!(before, snapshot(fixture.home.path()));
        junction::delete(&link).unwrap();
    }
}

#[cfg(any(unix, windows))]
fn assert_link_refusal(result: anyhow::Result<PreparedCandidate>) {
    let error = result.unwrap_err();
    #[cfg(windows)]
    assert!(
        error.chain().any(|cause| cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|cause| cause.raw_os_error() == Some(681))),
        "expected ERROR_STOPPED_ON_SYMLINK (681): {error:#}"
    );
    #[cfg(unix)]
    assert!(
        error.chain().any(|cause| cause
            .downcast_ref::<std::io::Error>()
            .is_some_and(|cause| cause.kind() == std::io::ErrorKind::NotADirectory
                || format!("{:?}", cause.kind()) == "FilesystemLoop")),
        "{error:#}"
    );
}

#[cfg(unix)]
#[test]
fn task_prepared_reader_refuses_symlink_directories_and_files_without_mutation() {
    use std::os::unix::fs::symlink;
    for changed in [
        "candidate",
        "tasks",
        "generation",
        "ancestor",
        "destination",
        "state",
        "complete",
        "project",
        "task",
        "lock",
    ] {
        let fixture = Fixture::new();
        fixture.read();
        let target = match changed {
            "candidate" => fixture.generation.join("candidate"),
            "tasks" => fixture.generation.join("candidate/tasks"),
            "generation" => fixture.generation.clone(),
            "ancestor" => fixture.progress.clone(),
            "destination" => fixture.selection.destination.clone(),
            "state" => fixture.generation.join("state.json"),
            "complete" => fixture.generation.join("complete"),
            "project" => fixture.generation.join("candidate/project.json"),
            "task" => fs::read_dir(fixture.generation.join("candidate/tasks"))
                .unwrap()
                .next()
                .unwrap()
                .unwrap()
                .path(),
            "lock" => fixture.progress.join("prepare.lock"),
            _ => unreachable!(),
        };
        let original = fixture.home.path().join("retained-target");
        fs::rename(&target, &original).unwrap();
        symlink(&original, &target).unwrap();
        let before = snapshot(fixture.home.path());
        assert_link_refusal(read_prepared(&fixture.progress, &fixture.selection));
        assert_eq!(before, snapshot(fixture.home.path()), "case {changed}");
        fs::remove_file(&target).unwrap();
    }
}

#[test]
fn task_prepared_reader_accepts_completed_baseline_after_missing_predecessor_recovery() {
    let mut fixture = Fixture::new();
    prepare_with_producer(
        &fixture.home.path().join("second"),
        &fixture.progress,
        &fixture.store,
        &fixture.selection.project,
        &fixture.selection.profile,
        false,
        &producer(),
        |_| Ok(capture(tree(2))),
    )
    .unwrap();
    // Remove only this fixture's first generation, retaining the orphaned second one.
    let removed = fs::canonicalize(&fixture.generation).unwrap();
    assert!(removed.starts_with(fs::canonicalize(fixture.home.path()).unwrap()));
    fs::remove_dir_all(removed).unwrap();
    let destination = fixture.home.path().join("recovered");
    let receipt = prepare_with_producer(
        &destination,
        &fixture.progress,
        &fixture.store,
        &fixture.selection.project,
        &fixture.selection.profile,
        false,
        &producer(),
        |previous| {
            assert!(previous.is_none());
            Ok(capture(tree(3)))
        },
    )
    .unwrap();
    assert!(receipt.reason.contains("predecessor"));
    fixture.selection.operation = receipt.operation;
    fixture.selection.destination = destination;
    let recovered = fixture.read();
    assert_eq!(recovered.provenance.sequence, 3);
    assert_eq!(recovered.provenance.predecessor, None);
    assert_eq!(recovered.tree.tasks.len(), 3);
}
