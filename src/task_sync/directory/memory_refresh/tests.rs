use super::{FAILURE, MemoryTaskExport, publish_memory_export};
use crate::{
    ObjectId, ProjectId,
    task_sync::{
        self,
        data_worktree::DataWorktree,
        directory,
        memories::{self, Head, ImportProvenance, KeyHistory, Revision, Source},
        progress::ExportProfile,
    },
};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
    time::Duration,
};

mod review;

fn git(root: &Path, arguments: &[&str]) -> Vec<u8> {
    let mut command = task_sync::git::command(root, arguments);
    command
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", root.join("absent-config"))
        .env("GIT_AUTHOR_NAME", "Fixture")
        .env("GIT_COMMITTER_NAME", "Fixture")
        .env("GIT_AUTHOR_EMAIL", "fixture@example.invalid")
        .env("GIT_COMMITTER_EMAIL", "fixture@example.invalid");
    task_sync::git::run_query_command(
        command,
        arguments[0],
        false,
        None,
        Duration::from_secs(30),
        task_sync::git::QueryStderrPolicy::ErrorOnly,
    )
    .unwrap()
}

fn revised(project: &ProjectId, previous: Option<&KeyHistory>) -> KeyHistory {
    let mut chain = previous.map_or_else(Vec::new, |history| match &history.head {
        Head::Active { chain } => chain.clone(),
        Head::Tombstone { .. } => panic!("active fixture"),
    });
    let mut revisions = previous.map_or_else(BTreeMap::new, |history| history.revisions.clone());
    let id = ObjectId::mint();
    let body = format!("rule version {}", chain.len() + 1);
    revisions.insert(
        id.clone(),
        Revision {
            body: body.clone(),
            provenance: ImportProvenance {
                format: memories::FORMAT.into(),
                project_id: project.clone(),
                key: "rule".into(),
                revision_id: id.clone(),
                previous: chain.last().cloned(),
                body_sha256: memories::body_sha256(&body),
                retiring_target: None,
                source_clear: false,
                source: Source {
                    actor_id: "author".into(),
                    actor_kind: "agent".into(),
                    actor_context: Some("fixture-context".into()),
                    reason: "record rule".into(),
                    remembered_at: chrono::Utc::now(),
                },
            },
        },
    );
    chain.push(id);
    KeyHistory {
        format: memories::FORMAT.into(),
        project_id: project.clone(),
        key: "rule".into(),
        head: Head::Active { chain },
        revisions,
    }
}

fn inventory(path: &Path) -> BTreeMap<PathBuf, Vec<u8>> {
    fn collect(root: &Path, path: &Path, output: &mut BTreeMap<PathBuf, Vec<u8>>) {
        for entry in fs::read_dir(path).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_dir() {
                collect(root, &entry.path(), output);
            } else {
                output.insert(
                    entry.path().strip_prefix(root).unwrap().to_path_buf(),
                    fs::read(entry.path()).unwrap(),
                );
            }
        }
    }
    let mut result = BTreeMap::new();
    collect(path, path, &mut result);
    result
}

struct Fixture {
    _owned: crate::test_support::TempHome,
    code: PathBuf,
    data: PathBuf,
    tree: task_sync::TaskTree,
    selected: BTreeMap<String, KeyHistory>,
    retained: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let owned = crate::test_support::temp_home().unwrap();
        let code = owned.path().join("code");
        fs::create_dir(&code).unwrap();
        fs::write(code.join(".gitignore"), "/.engram-data/\n").unwrap();
        fs::write(code.join(".engram-project"), "directory-test\n").unwrap();
        git(&code, &["init", "--initial-branch=code"]);
        git(&code, &["add", "."]);
        git(
            &code,
            &["-c", "commit.gpgSign=false", "commit", "-m", "Bootstrap"],
        );
        let mut tree = directory::tests::candidate();
        let mut removed = tree.tasks[0].clone();
        removed.id = crate::WorkId(uuid::Uuid::new_v4());
        tree.tasks.push(removed);
        tree.tasks.sort_by_key(|task| task.id);
        task_sync::data_worktree::initialize(
            &code.join(".engram-project"),
            &tree.project.project_id,
            "engram-data",
        )
        .unwrap();
        let selected = BTreeMap::from([("rule".into(), revised(&tree.project.project_id, None))]);
        let fixture = Self {
            data: code.join(".engram-data"),
            retained: owned.path().join("retained"),
            _owned: owned,
            code,
            tree,
            selected,
        };
        fixture
            .publish(&fixture.tree, &fixture.selected, false)
            .unwrap();
        git(&fixture.data, &["add", ".engram"]);
        git(
            &fixture.data,
            &[
                "-c",
                "commit.gpgSign=false",
                "commit",
                "-m",
                "Publish initial selection",
            ],
        );
        fs::create_dir(&fixture.retained).unwrap();
        for (path, bytes) in inventory(&fixture.folder()) {
            let target = fixture.retained.join(path);
            fs::create_dir_all(target.parent().unwrap()).unwrap();
            fs::write(target, bytes).unwrap();
        }
        fixture
    }

    fn folder(&self) -> PathBuf {
        self.data.join(".engram")
    }

    fn observe(&self) -> DataWorktree {
        DataWorktree::observe(&self.code, &self.data, "engram-data").unwrap()
    }

    fn changed(&self) -> (task_sync::TaskTree, BTreeMap<String, KeyHistory>) {
        let mut tree = self.tree.clone();
        tree.tasks.truncate(1);
        tree.tasks[0].title = "changed task".into();
        let mut added = tree.tasks[0].clone();
        added.id = crate::WorkId(uuid::Uuid::new_v4());
        tree.tasks.push(added);
        tree.tasks.sort_by_key(|task| task.id);
        (
            tree,
            BTreeMap::from([(
                "rule".into(),
                revised(&self.tree.project.project_id, Some(&self.selected["rule"])),
            )]),
        )
    }

    fn publish(
        &self,
        tree: &task_sync::TaskTree,
        selected: &BTreeMap<String, KeyHistory>,
        recovering: bool,
    ) -> anyhow::Result<directory::FolderExchange> {
        let observed = self.observe();
        let basis = memories::directory::committed(&observed, &tree.project.project_id)?;
        let held = memories::directory::publication_lock(&observed)?;
        publish_memory_export(
            &observed,
            &MemoryTaskExport {
                tree,
                selected,
                basis: &basis,
                profile: &ExportProfile {
                    include_notes: false,
                    origin_machine: None,
                },
                recovery_basis: recovering.then_some(self.retained.as_path()),
            },
            &held,
        )
    }
}

fn interrupted(
    fixture: &Fixture,
    tree: &task_sync::TaskTree,
    selected: &BTreeMap<String, KeyHistory>,
    stage: &str,
) {
    struct Reset;
    impl Drop for Reset {
        fn drop(&mut self) {
            FAILURE.with(|failure| *failure.borrow_mut() = None);
        }
    }
    let _reset = Reset;
    FAILURE.with(|failure| *failure.borrow_mut() = Some(stage.into()));
    assert!(fixture.publish(tree, selected, false).is_err(), "{stage}");
}

#[test]
fn memory_task_refresh_recovers_every_publication_stage() {
    for stage in [
        "fence_synced",
        "task_written",
        "task_removed",
        "project_published",
        "memory_revision_staged",
        "memory_revision_linked",
        "memory_head_staged",
        "memory_head_published",
        "memories_published",
    ] {
        let fixture = Fixture::new();
        let (tree, selected) = fixture.changed();
        interrupted(&fixture, &tree, &selected, stage);
        assert!(
            directory::read_resolved_data_folder_with_memories(&fixture.observe()).is_err(),
            "{stage}"
        );
        let before = inventory(&fixture.folder());
        let error = fixture.publish(&tree, &selected, false).unwrap_err();
        assert_eq!(
            directory::tests::refusal_code(&error),
            "memory_task_export_recovery_required"
        );
        assert_eq!(inventory(&fixture.folder()), before);
        fixture.publish(&tree, &selected, true).unwrap();
        assert_eq!(
            directory::read_resolved_data_folder_with_memories(&fixture.observe())
                .unwrap()
                .tree,
            tree
        );
        assert_eq!(
            memories::directory::read(&fixture.observe()).unwrap(),
            selected
        );
        let completed = inventory(&fixture.folder());
        FAILURE.with(|failure| *failure.borrow_mut() = Some("fence_synced".into()));
        fixture.publish(&tree, &selected, false).unwrap();
        FAILURE.with(|failure| *failure.borrow_mut() = None);
        assert_eq!(inventory(&fixture.folder()), completed);
    }
}

#[test]
fn memory_task_refresh_refuses_foreign_recovery_bytes_without_cleanup() {
    let fixture = Fixture::new();
    let (tree, selected) = fixture.changed();
    interrupted(&fixture, &tree, &selected, "task_written");
    fs::write(
        fixture
            .folder()
            .join(format!("tasks/{}.json", fixture.tree.tasks[0].id.0)),
        b"private foreign body",
    )
    .unwrap();
    let before = inventory(&fixture.folder());
    let error = fixture.publish(&tree, &selected, true).unwrap_err();
    assert_eq!(
        directory::tests::refusal_code(&error),
        "memory_task_export_recovery_mismatch"
    );
    assert!(!error.to_string().contains("private foreign body"));
    assert_eq!(inventory(&fixture.folder()), before);
}

#[test]
fn memory_task_refresh_recovers_torn_intended_task_with_full_fence() {
    let fixture = Fixture::new();
    let (tree, selected) = fixture.changed();
    interrupted(&fixture, &tree, &selected, "fence_synced");
    let (documents, _) = super::intended_documents(&MemoryTaskExport {
        tree: &tree,
        profile: &ExportProfile {
            include_notes: false,
            origin_machine: None,
        },
        basis: &fixture.selected,
        selected: &selected,
        recovery_basis: Some(&fixture.retained),
    })
    .unwrap();
    let path = format!("tasks/{}.json", fixture.tree.tasks[0].id.0);
    let intended = &documents[&path];
    fs::write(fixture.folder().join(path), &intended[..intended.len() / 2]).unwrap();
    fixture.publish(&tree, &selected, true).unwrap();
    assert_eq!(
        directory::read_resolved_data_folder_with_memories(&fixture.observe())
            .unwrap()
            .tree,
        tree
    );
    assert_eq!(
        memories::directory::read(&fixture.observe()).unwrap(),
        selected
    );
}

#[test]
fn memory_task_refresh_partial_marker_cannot_authorize_changed_tasks() {
    let fixture = Fixture::new();
    let (tree, selected) = fixture.changed();
    interrupted(&fixture, &tree, &selected, "task_written");
    let marker = fs::read_dir(fixture.folder())
        .unwrap()
        .map(Result::unwrap)
        .find(|entry| entry.file_name().to_str().unwrap().ends_with(".pending"))
        .unwrap()
        .path();
    let bytes = fs::read(&marker).unwrap();
    fs::write(marker, &bytes[..8]).unwrap();
    let before = inventory(&fixture.folder());
    let error = fixture.publish(&tree, &selected, true).unwrap_err();
    assert_eq!(
        directory::tests::refusal_code(&error),
        "memory_task_export_recovery_mismatch"
    );
    assert_eq!(inventory(&fixture.folder()), before);
}

#[test]
fn memory_task_refresh_retry_is_whole_output_noop_before_and_after_commit() {
    let fixture = Fixture::new();
    let (tree, selected) = fixture.changed();
    fixture.publish(&tree, &selected, false).unwrap();
    let before = inventory(&fixture.folder());
    for committed in [false, true] {
        if committed {
            git(&fixture.data, &["add", ".engram"]);
            git(
                &fixture.data,
                &[
                    "-c",
                    "commit.gpgSign=false",
                    "commit",
                    "-m",
                    "Publish refresh",
                ],
            );
        }
        FAILURE.with(|failure| *failure.borrow_mut() = Some("fence_synced".into()));
        fixture.publish(&tree, &selected, false).unwrap();
        FAILURE.with(|failure| *failure.borrow_mut() = None);
        assert_eq!(inventory(&fixture.folder()), before);
    }
}

#[test]
fn memory_task_refresh_dirty_components_refuse_without_mutation() {
    for kind in [
        "task", "project", "memory", "policy", "unknown", "evidence", "index",
    ] {
        let fixture = Fixture::new();
        let (tree, selected) = fixture.changed();
        let task = fixture
            .folder()
            .join(format!("tasks/{}.json", fixture.tree.tasks[0].id.0));
        match kind {
            "task" | "index" => {
                let mut document: serde_json::Value =
                    serde_json::from_slice(&fs::read(&task).unwrap()).unwrap();
                document["title"] = serde_json::json!("foreign task edit");
                fs::write(task, serde_json::to_vec(&document).unwrap()).unwrap();
                if kind == "index" {
                    git(&fixture.data, &["add", ".engram"]);
                }
            }
            "project" => {
                fs::write(fixture.folder().join("project.json"), b"{}").unwrap();
            }
            "memory" => {
                let head = fixture.folder().join("memories/rule/head.json");
                let mut bytes = fs::read(&head).unwrap();
                bytes.push(b' ');
                fs::write(head, bytes).unwrap();
                // A changed immutable revision is rejected before task writes.
                let revision = fixture.selected["rule"].revisions.keys().next().unwrap();
                let path = fixture
                    .folder()
                    .join(format!("memories/rule/{revision}.json"));
                let mut bytes = fs::read(&path).unwrap();
                bytes.push(b' ');
                fs::write(path, bytes).unwrap();
            }
            "policy" => {
                fs::write(fixture.folder().join(".gitattributes"), b"* text eol=lf\n").unwrap();
            }
            "unknown" => {
                fs::write(fixture.folder().join("unexpected"), b"foreign").unwrap();
            }
            "evidence" => {
                let path = task_sync::evidence::path::pack(&ObjectId::mint(), &ObjectId::mint());
                let path = fixture.data.join(path);
                fs::create_dir_all(path.parent().unwrap()).unwrap();
                fs::write(path, b"retained carrier").unwrap();
            }
            _ => unreachable!("fixed cases"),
        }
        let before = inventory(&fixture.folder());
        let index = git(&fixture.data, &["ls-files", "--stage", "-z"]);
        let head = git(&fixture.data, &["rev-parse", "HEAD"]);
        let error = fixture.publish(&tree, &selected, false).unwrap_err();
        let expected = match kind {
            "memory" => "memory_export_component_changed",
            "unknown" => "task_sync_destination_unknown_entry",
            "index" => "memory_task_export_index_changed",
            _ => "memory_task_export_component_changed",
        };
        assert_eq!(directory::tests::refusal_code(&error), expected, "{kind}");
        assert_eq!(inventory(&fixture.folder()), before, "{kind}");
        assert_eq!(git(&fixture.data, &["ls-files", "--stage", "-z"]), index);
        assert_eq!(git(&fixture.data, &["rev-parse", "HEAD"]), head);
    }
}

#[test]
fn memory_task_refresh_stale_recovery_selection_preserves_every_file() {
    let fixture = Fixture::new();
    let (tree, selected) = fixture.changed();
    interrupted(&fixture, &tree, &selected, "task_written");
    let mut changed = tree;
    changed.tasks[0].outcome = "new local change after interruption".into();
    let before = inventory(&fixture.folder());
    let error = fixture.publish(&changed, &selected, true).unwrap_err();
    assert_eq!(
        directory::tests::refusal_code(&error),
        "memory_task_export_recovery_mismatch"
    );
    assert!(
        error
            .to_string()
            .contains("git restore --source=HEAD --worktree -- .engram")
    );
    assert!(
        error
            .to_string()
            .contains("new task/memory files and project-*.pending")
    );
    assert_eq!(inventory(&fixture.folder()), before);
    // Carry out the documented route in the throwaway data repository only.
    git(
        &fixture.data,
        &["restore", "--source=HEAD", "--worktree", "--", ".engram"],
    );
    let retained = inventory(&fixture.retained);
    for path in inventory(&fixture.folder())
        .keys()
        .filter(|path| !retained.contains_key(*path))
    {
        fs::remove_file(fixture.folder().join(path)).unwrap();
    }
    fixture.publish(&changed, &selected, false).unwrap();
    assert_eq!(
        directory::read_resolved_data_folder_with_memories(&fixture.observe())
            .unwrap()
            .tree,
        changed
    );
}
