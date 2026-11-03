use super::*;
use crate::{
    ChildRequirement, WorkItemKind, WorkLifecycle,
    task_sync::{ProjectDocument, TaskTree, progress::ExportProfile},
};
use std::{fs, path::PathBuf};

mod guards;
mod validation;

struct Fixture {
    _owned: crate::test_support::TempHome,
    root: PathBuf,
    tree: TaskTree,
}

fn task() -> TaskDocument {
    TaskDocument {
        id: WorkId::new(),
        external_ref: None,
        title: "Shared task".into(),
        outcome: "Deliver work".into(),
        acceptance: vec!["Implemented".into()],
        acceptance_bindings: Vec::new(),
        evaluation_mode: None,
        kind: WorkItemKind::Task,
        priority: 2,
        labels: Vec::new(),
        assigned_to: None,
        deferred_until: None,
        lifecycle: WorkLifecycle::Open,
        parent_id: None,
        child_requirement: ChildRequirement::Required,
        prerequisites: Vec::new(),
        blockers: Vec::new(),
        superseded_by: None,
        disposal_reason: None,
        completion: None,
        notes: Vec::new(),
        activity: BTreeMap::new(),
    }
}

impl Fixture {
    fn new(count: usize) -> Self {
        Self::with_format(count, "sha1")
    }

    fn with_format(count: usize, object_format: &str) -> Self {
        let owned = crate::test_support::temp_home().unwrap();
        let root = owned.path().join("repository");
        fs::create_dir(&root).unwrap();
        let tree = TaskTree {
            project: ProjectDocument {
                format: task_sync::FORMAT.into(),
                project_id: ProjectId("commit-delta/fixture".into()),
                notes_included: false,
            },
            tasks: (0..count).map(|_| task()).collect(),
        };
        directory::write_folder(
            &root.join(".engram"),
            &tree,
            &ExportProfile {
                include_notes: false,
                origin_machine: None,
            },
        )
        .unwrap();
        let fixture = Self {
            _owned: owned,
            root,
            tree,
        };
        fixture.git(&[
            "init",
            "--initial-branch=main",
            &format!("--object-format={object_format}"),
        ]);
        fixture.git(&["config", "core.autocrlf", "false"]);
        fixture.git(&["config", "core.filemode", "true"]);
        fixture
    }

    fn git(&self, args: &[&str]) -> Vec<u8> {
        let mut command = crate::task_sync::git::command(&self.root, &[]);
        let output = command
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .env("HOME", self.root.parent().unwrap())
            .env("XDG_CONFIG_HOME", self.root.parent().unwrap())
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_ATTR_NOSYSTEM", "1")
            .env(
                "GIT_CONFIG_GLOBAL",
                self.root.parent().unwrap().join("global.config"),
            )
            .args([
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "-c",
                "commit.gpgSign=false",
            ])
            .args(args)
            .output()
            .unwrap();
        assert!(
            output.status.success(),
            "{args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        output.stdout
    }

    fn commit(&self) -> String {
        self.git(&["add", "--all"]);
        self.git(&["commit", "--allow-empty", "-m", "Update fixture tasks"]);
        self.text(&["rev-parse", "HEAD"])
    }

    fn text(&self, args: &[&str]) -> String {
        String::from_utf8(self.git(args)).unwrap().trim().into()
    }
    fn relative(id: WorkId) -> String {
        format!(".engram/tasks/{}.json", id.0)
    }
    fn write(&self, task: &TaskDocument) {
        fs::write(
            self.root.join(Self::relative(task.id)),
            task_sync::pretty(task).unwrap(),
        )
        .unwrap();
    }
    fn read(&self, prior: &str, incoming: &str) -> Result<CommitTaskDelta> {
        read_task_delta(&self.root, &self.tree.project.project_id, prior, incoming)
    }
    fn profile(&self, notes: bool, alias: Option<&str>) {
        let path = self.root.join(".engram/project.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        value["project"]["notes_included"] = notes.into();
        value["profile"]["include_notes"] = notes.into();
        value["profile"]["origin_machine"] = serde_json::to_value(alias).unwrap();
        fs::write(path, serde_json::to_vec(&value).unwrap()).unwrap();
    }
    fn identity(&self) -> Vec<Vec<u8>> {
        [
            vec!["rev-parse", "HEAD"],
            vec!["show-ref"],
            vec!["ls-files", "--stage", "-z"],
            vec!["status", "--porcelain", "--untracked-files=all"],
        ]
        .into_iter()
        .map(|args| self.git(&args))
        .collect()
    }
}

fn reads() -> Vec<String> {
    BLOB_READS.with(|reads| std::mem::take(&mut *reads.borrow_mut()))
}
fn task_reads() -> Vec<String> {
    reads()
        .into_iter()
        .filter(|path| path != ".engram/project.json")
        .collect()
}
fn code(error: &anyhow::Error) -> serde_json::Value {
    crate::store_error_value(error.downcast_ref::<StoreError>().unwrap())["error"]["code"].clone()
}

#[test]
fn two_thousand_tasks_read_only_five_edits_addition_and_no_deleted_or_unchanged_blob() {
    let fixture = Fixture::new(2000);
    // An unrelated unread payload is a discriminator against whole-tree parsing.
    fs::write(
        fixture
            .root
            .join(Fixture::relative(fixture.tree.tasks[1999].id)),
        b"not JSON",
    )
    .unwrap();
    fs::write(
        fixture
            .root
            .join(Fixture::relative(fixture.tree.tasks[5].id)),
        b"removed malformed payload",
    )
    .unwrap();
    let prior = fixture.commit();
    let mut expected = BTreeMap::new();
    for original in &fixture.tree.tasks[..5] {
        let mut changed = original.clone();
        changed.title = "Changed planning text".into();
        fixture.write(&changed);
        expected.insert(changed.id, changed);
    }
    fixture.commit();
    let added = task();
    fixture.write(&added);
    expected.insert(added.id, added);
    let removed = fixture.tree.tasks[5].id;
    fs::remove_file(fixture.root.join(Fixture::relative(removed))).unwrap();
    let incoming = fixture.commit();
    let identity = fixture.identity();
    reads();
    let delta = fixture.read(&prior, &incoming).unwrap();
    let opened = reads();
    assert_eq!(opened.len(), 8);
    assert_eq!(
        opened
            .iter()
            .filter(|path| *path == ".engram/project.json")
            .count(),
        2
    );
    let actual: BTreeSet<_> = opened
        .into_iter()
        .filter(|path| path != ".engram/project.json")
        .collect();
    assert_eq!(
        actual,
        expected.keys().copied().map(Fixture::relative).collect()
    );
    assert_eq!(actual.len(), 6);
    assert_eq!(delta.upserts, expected);
    assert_eq!(delta.removed, BTreeSet::from([removed]));
    assert_eq!(delta.prior_commit, prior);
    assert_eq!(delta.incoming_commit, incoming);
    assert_eq!(fixture.identity(), identity);
    // The same operation is repeatable without retaining a cursor or receipt.
    assert_eq!(fixture.read(&prior, &incoming).unwrap().upserts, expected);
}

#[test]
fn no_op_reverse_and_rename_are_explicit_tree_deltas() {
    let fixture = Fixture::new(1);
    let prior = fixture.commit();
    reads();
    let same = fixture.read(&prior, &prior).unwrap();
    assert!(same.upserts.is_empty());
    assert!(same.removed.is_empty());
    assert_eq!(task_reads(), Vec::<String>::new());
    let mut renamed = fixture.tree.tasks[0].clone();
    let old = renamed.id;
    renamed.id = WorkId::new();
    fixture.write(&renamed);
    fs::remove_file(fixture.root.join(Fixture::relative(old))).unwrap();
    let incoming = fixture.commit();
    let forward = fixture.read(&prior, &incoming).unwrap();
    assert_eq!(
        forward.upserts,
        BTreeMap::from([(renamed.id, renamed.clone())])
    );
    assert_eq!(forward.removed, BTreeSet::from([old]));
    assert_eq!(task_reads(), [Fixture::relative(renamed.id)]);
    let backward = fixture.read(&incoming, &prior).unwrap();
    assert_eq!(
        backward.upserts,
        BTreeMap::from([(old, fixture.tree.tasks[0].clone())])
    );
    assert_eq!(backward.removed, BTreeSet::from([renamed.id]));
    assert_eq!(task_reads(), [Fixture::relative(old)]);
}

#[test]
fn merge_delta_includes_both_branches_and_leaves_graph_admission_to_caller() {
    let fixture = Fixture::new(2);
    let prior = fixture.commit();
    fixture.git(&["checkout", "-b", "side"]);
    let mut side = fixture.tree.tasks[0].clone();
    side.title = "Side branch edit".into();
    fixture.write(&side);
    fixture.commit();
    fixture.git(&["checkout", "main"]);
    let mut main = fixture.tree.tasks[1].clone();
    main.title = "Main branch edit".into();
    // Resolving this reference is intentionally the later graph owner's duty.
    main.parent_id = Some(WorkId::new());
    fixture.write(&main);
    fixture.commit();
    fixture.git(&["merge", "--no-ff", "-m", "Join fixture changes", "side"]);
    let incoming = fixture.text(&["rev-parse", "HEAD"]);
    let delta = fixture.read(&prior, &incoming).unwrap();
    assert_eq!(
        delta.upserts,
        BTreeMap::from([(side.id, side), (main.id, main)])
    );
    assert!(delta.removed.is_empty());
}

#[test]
fn missing_noncommit_and_invalid_bases_refuse_without_mutating_input() {
    let fixture = Fixture::new(1);
    let prior = fixture.commit();
    let missing = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    let blob = fixture.text(&[
        "rev-parse",
        &format!("HEAD:{}", Fixture::relative(fixture.tree.tasks[0].id)),
    ]);
    let identity = fixture.identity();
    for (old, new) in [
        (missing.as_str(), prior.as_str()),
        (prior.as_str(), missing.as_str()),
        (blob.as_str(), prior.as_str()),
    ] {
        let error = fixture.read(old, new).unwrap_err();
        assert_eq!(code(&error), "task_sync_incremental_basis_unavailable");
        assert!(error.to_string().contains("commit"));
    }
    assert_eq!(
        code(&fixture.read("HEAD", &prior).unwrap_err()),
        "task_sync_invalid_graph"
    );
    assert_eq!(fixture.identity(), identity);
}

#[test]
fn malformed_changed_documents_and_wrong_project_refuse_by_name() {
    let fixture = Fixture::new(1);
    let prior = fixture.commit();
    let path = fixture
        .root
        .join(Fixture::relative(fixture.tree.tasks[0].id));
    let mut invalid = fixture.tree.tasks[0].clone();
    invalid.priority = 9;
    let mut mismatch = invalid.clone();
    mismatch.priority = 2;
    mismatch.id = WorkId::new();
    for bytes in [
        b"<<<<<<< conflict".to_vec(),
        b"{\"id\":null,\"id\":null}".to_vec(),
        task_sync::pretty(&invalid).unwrap(),
        task_sync::pretty(&mismatch).unwrap(),
    ] {
        fs::write(&path, bytes).unwrap();
        let incoming = fixture.commit();
        assert_eq!(
            code(&fixture.read(&prior, &incoming).unwrap_err()),
            "task_sync_invalid_graph"
        );
    }
    fixture.write(&fixture.tree.tasks[0]);
    let metadata = fixture.root.join(".engram/project.json");
    let mut value: serde_json::Value =
        serde_json::from_slice(&fs::read(&metadata).unwrap()).unwrap();
    value["project"]["project_id"] = "another/project".into();
    fs::write(&metadata, serde_json::to_vec(&value).unwrap()).unwrap();
    let incoming = fixture.commit();
    assert_eq!(
        code(&fixture.read(&prior, &incoming).unwrap_err()),
        "task_sync_origin_mismatch"
    );
}

#[test]
fn metadata_disclosure_and_changed_unsafe_entries_refuse_even_without_task_reads() {
    let fixture = Fixture::new(1);
    let prior = fixture.commit();
    let path = fixture.root.join(".engram/project.json");
    let original = fs::read(&path).unwrap();
    let mut value: serde_json::Value = serde_json::from_slice(&original).unwrap();
    value["project"]["notes_included"] = true.into();
    value["profile"]["include_notes"] = true.into();
    value["profile"]["origin_machine"] = "fixture".into();
    fs::write(&path, serde_json::to_vec(&value).unwrap()).unwrap();
    directory::read_folder_basis(&path)
        .unwrap()
        .profile
        .options()
        .validate()
        .unwrap();
    let incoming = fixture.commit();
    reads();
    assert_eq!(
        code(&fixture.read(&prior, &incoming).unwrap_err()),
        "task_sync_divergent_planning"
    );
    assert_eq!(task_reads(), Vec::<String>::new());
    assert_eq!(
        code(&fixture.read(&incoming, &prior).unwrap_err()),
        "task_sync_divergent_planning"
    );
    assert_eq!(task_reads(), Vec::<String>::new());
    fs::write(&path, original).unwrap();
    fs::write(fixture.root.join(".engram/unexpected"), b"unexpected").unwrap();
    let incoming = fixture.commit();
    assert_eq!(
        code(&fixture.read(&prior, &incoming).unwrap_err()),
        "task_sync_destination_unknown_entry"
    );
}

#[test]
fn symlink_executable_and_nested_task_entries_refuse_before_blob_reads() {
    let fixture = Fixture::new(1);
    let prior = fixture.commit();
    let relative = Fixture::relative(fixture.tree.tasks[0].id);
    let blob = fixture.text(&["rev-parse", &format!("HEAD:{relative}")]);
    for mode in ["120000", "100755"] {
        fixture.git(&["update-index", "--cacheinfo", mode, &blob, &relative]);
        fixture.git(&["commit", "-m", "Change fixture mode"]);
        let incoming = fixture.text(&["rev-parse", "HEAD"]);
        reads();
        assert_eq!(
            code(&fixture.read(&prior, &incoming).unwrap_err()),
            "task_sync_destination_not_file"
        );
        assert_eq!(task_reads(), Vec::<String>::new());
        fixture.git(&["update-index", "--cacheinfo", "100644", &blob, &relative]);
        fixture.git(&["commit", "-m", "Restore fixture mode"]);
    }
    let nested = fixture.root.join(".engram/tasks/nested");
    fs::create_dir(&nested).unwrap();
    fs::write(nested.join("task.json"), b"{}").unwrap();
    let incoming = fixture.commit();
    assert_eq!(
        code(&fixture.read(&prior, &incoming).unwrap_err()),
        "task_sync_destination_unknown_entry"
    );
}

#[test]
fn raw_records_require_nul_canonical_paths_modes_and_unique_changes() {
    let id = WorkId(uuid::Uuid::from_u128(u128::MAX));
    let oid = format!(
        "{}{}",
        uuid::Uuid::new_v4().simple(),
        &uuid::Uuid::new_v4().simple().to_string()[..8]
    );
    let zero = "0".repeat(oid.len());
    let path = Fixture::relative(id);
    let valid = format!(":000000 100644 {zero} {oid} A\0{path}\0");
    assert_eq!(parse_changes(valid.as_bytes()).unwrap().len(), 1);
    for invalid in [
        valid.trim_end_matches('\0').to_owned(),
        valid.repeat(2),
        valid.replace("100644", "160000"),
        valid.replace(" A\0", " R100\0"),
        valid.replace(&path, ".engram/tasks/../outside.json"),
        valid.replace(&path, ".engram/tasks/UPPER.json"),
        valid.replace(&path, &format!(".engram/tasks/{}.json", id.0.simple())),
        valid.replace(
            &path,
            &format!(".engram/tasks/{}.json", id.0.to_string().to_uppercase()),
        ),
    ] {
        assert!(parse_changes(invalid.as_bytes()).is_err());
    }
    assert!(nul_records(b"invalid\xff\0").is_err());
    let entry = format!("100644 blob {oid}\tproject.json\0");
    assert_eq!(parse_entries(entry.as_bytes()).unwrap().len(), 1);
    assert!(parse_entries(entry.repeat(2).as_bytes()).is_err());
}
