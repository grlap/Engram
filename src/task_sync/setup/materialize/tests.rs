use super::*;
use crate::task_sync::{
    ProjectDocument, TaskDocument, TaskTree,
    progress::{ExportCapture, ExportCut, ExportProfile},
    settings::{Action, Request},
};
use crate::{ChildRequirement, LocalWorkService, SessionId, WorkId, WorkItemKind, WorkLifecycle};
use std::{collections::BTreeMap, fs, process::Command};

mod admission;
mod aliases;
mod ownership;
mod recovery;

struct Fixture {
    owned: crate::test_support::TempHome,
    root: PathBuf,
    marker: PathBuf,
    store: PathBuf,
    admin: PathBuf,
    service: LocalWorkService,
    selection: MaterializeSelection,
    documents: BTreeMap<String, Vec<u8>>,
}

fn git(root: &Path, args: &[&str]) -> Vec<u8> {
    let output = Command::new("git")
        .current_dir(root)
        .env_remove("GIT_DIR")
        .env_remove("GIT_WORK_TREE")
        .env_remove("GIT_INDEX_FILE")
        .env_remove("GIT_CONFIG")
        .env_remove("GIT_CONFIG_COUNT")
        .env_remove("GIT_CONFIG_PARAMETERS")
        .env_remove("GIT_ATTR_SOURCE")
        .env("HOME", root.parent().unwrap())
        .env("XDG_CONFIG_HOME", root.parent().unwrap())
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_ATTR_NOSYSTEM", "1")
        .env(
            "GIT_CONFIG_GLOBAL",
            root.parent().unwrap().join("global.config"),
        )
        .args([
            "--no-optional-locks",
            "-c",
            "core.hooksPath=",
            "-c",
            "core.fsmonitor=",
        ])
        .args(args)
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    output.stdout
}

fn tree(count: usize) -> TaskTree {
    let mut tasks: Vec<_> = (0..count)
        .map(|index| TaskDocument {
            id: WorkId::new(),
            external_ref: None,
            title: format!("Shared task {index}"),
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
        })
        .collect();
    tasks.sort_by_key(|task| task.id.0);
    TaskTree {
        project: ProjectDocument {
            format: crate::task_sync::FORMAT.into(),
            project_id: ProjectId("materialize/fixture".into()),
            notes_included: false,
        },
        tasks,
    }
}

fn attribution() -> Attribution {
    Attribution {
        actor_id: "fixture-operator".into(),
        session_id: "fixture-materializer".into(),
        actor_context: None,
        source_skill: None,
    }
}

impl Fixture {
    fn new(existing_private: bool, count: usize) -> Self {
        Self::with_tree(existing_private, &tree(count))
    }

    fn with_tree(existing_private: bool, tree: &TaskTree) -> Self {
        Self::with_setup(existing_private, tree, true)
    }

    fn with_setup(existing_private: bool, tree: &TaskTree, setup_present: bool) -> Self {
        Self::with_setup_bytes(
            existing_private,
            tree,
            setup_present.then_some([&b"/.engram/\n/target/\n"[..], &b"*.md text eol=lf\n"[..]]),
        )
    }

    fn with_setup_bytes(
        existing_private: bool,
        tree: &TaskTree,
        setup: Option<[&[u8]; 2]>,
    ) -> Self {
        Self::with_setup_entries(
            existing_private,
            tree,
            setup.map_or([None, None], |bytes| bytes.map(Some)),
        )
    }

    fn with_setup_entries(
        existing_private: bool,
        tree: &TaskTree,
        setup: [Option<&[u8]>; 2],
    ) -> Self {
        let include_notes = tree.project.notes_included;
        let origin_machine = include_notes.then(|| "fixture-machine".to_owned());
        let owned = crate::test_support::temp_home().unwrap();
        fs::write(
            owned.path().join("global.config"),
            "[core]\n autocrlf = true\n eol = crlf\n",
        )
        .unwrap();
        let root = owned.path().join("repository");
        fs::create_dir(&root).unwrap();
        git(&root, &["init", "--initial-branch=ordinary"]);
        git(&root, &["config", "core.autocrlf", "false"]);
        git(
            &root,
            &[
                "config",
                "core.attributesFile",
                owned.path().join("attributes").to_str().unwrap(),
            ],
        );
        fs::write(owned.path().join("attributes"), b"").unwrap();
        fs::write(owned.path().join("excludes"), b"").unwrap();
        git(
            &root,
            &[
                "config",
                "core.excludesFile",
                owned.path().join("excludes").to_str().unwrap(),
            ],
        );
        // Product queries run in this process: repository-local rules override
        // ambient global/system attributes without changing process environment.
        fs::write(
            root.join(".git/info/attributes"),
            b"* -filter -diff -merge -working-tree-encoding -eol -ident\n.engram/project.json text eol=lf diff merge -filter -working-tree-encoding -ident\n.engram/tasks/*.json text eol=lf diff merge -filter -working-tree-encoding -ident\n",
        )
        .unwrap();
        let marker = root.join(".engram-project");
        fs::write(&marker, "materialize/fixture\n").unwrap();
        for (name, bytes) in [".gitignore", ".gitattributes"].into_iter().zip(setup) {
            if let Some(bytes) = bytes {
                fs::write(root.join(name), bytes).unwrap();
            }
        }
        fs::write(root.join("code.txt"), b"ordinary code\n").unwrap();
        git(&root, &["add", "."]);
        git(
            &root,
            &[
                "-c",
                "user.name=Fixture",
                "-c",
                "user.email=fixture@example.invalid",
                "commit",
                "-m",
                "fixture",
            ],
        );
        if existing_private {
            fs::create_dir(root.join(".engram")).unwrap();
            fs::write(root.join(".engram/private.db"), b"private preserved bytes").unwrap();
        }
        let project = tree.project.project_id.clone();
        let store = crate::project_database_path(&owned.path().join("home"), &project);
        let service = LocalWorkService::new(
            store.clone(),
            project.clone(),
            "fixture-operator".into(),
            SessionId("fixture-materializer".into()),
            None,
        );
        service
            .task_sync_settings(
                &Request {
                    action: Action::Enable,
                    user_instruction: Some("Enable task sync in this owned fixture".into()),
                },
                chrono::Utc::now(),
            )
            .unwrap();
        fs::write(&store, b"not SQLite; source changes independently").unwrap();
        let setup = service
            .prepare_task_sync_setup(
                &marker,
                &root,
                "ordinary",
                super::super::Assertions {
                    visibility: "private".into(),
                    readership: "fixture maintainers".into(),
                    profile: crate::task_sync::FORMAT.into(),
                    include_notes,
                    origin_machine: origin_machine.clone(),
                },
            )
            .unwrap();
        let progress_directory = owned.path().join("progress");
        let candidate_directory = owned.path().join("candidate");
        let prepared = progress::prepare(
            &candidate_directory,
            &progress_directory,
            &store,
            &project,
            &ExportProfile {
                include_notes,
                origin_machine,
            },
            false,
            |_| {
                Ok(ExportCapture {
                    tree: tree.clone(),
                    omissions: NoteOmissions {
                        counts: BTreeMap::from([("excluded-note".into(), 3)]),
                    },
                    activity_omissions: NoteOmissions {
                        counts: BTreeMap::from([("private-activity".into(), 5)]),
                    },
                    cut: ExportCut {
                        position: 0,
                        anchor: None,
                    },
                    full_reason: None,
                })
            },
        )
        .unwrap();
        let documents = crate::task_sync::directory::read_documents(&candidate_directory).unwrap();
        let selection = MaterializeSelection {
            repository: root.clone(),
            setup_operation: setup.operation,
            prepared_operation: prepared.operation,
            progress_directory,
            candidate_directory,
        };
        Self {
            owned,
            admin: root.join(".git"),
            root,
            marker,
            store,
            service,
            selection,
            documents,
        }
    }

    fn run(&self) -> MaterializeReceipt {
        self.service
            .materialize_task_sync_setup(&self.marker, &self.selection)
            .unwrap()
    }

    fn interrupt(&self, boundary: &str) -> anyhow::Error {
        let mut reached = false;
        let result = super::run(
            &self.marker,
            &ProjectId("materialize/fixture".into()),
            &self.selection,
            true,
            attribution(),
            &mut |point| {
                if point == boundary {
                    reached = true;
                    anyhow::bail!("owned test interruption at {point}");
                }
                Ok(())
            },
        );
        assert!(
            reached,
            "boundary never reached: {boundary}; result={result:?}"
        );
        result.unwrap_err()
    }

    fn disable(&self) {
        self.service
            .task_sync_settings(
                &Request {
                    action: Action::Disable,
                    user_instruction: None,
                },
                chrono::Utc::now(),
            )
            .unwrap();
    }

    fn private(&self) -> PathBuf {
        self.admin.join(format!(
            "engram-task-sync-materialize-{}-{}",
            self.selection.setup_operation, self.selection.prepared_operation
        ))
    }

    fn assert_output(&self) {
        assert_shared(self);
        assert_eq!(
            fs::read(&self.store).unwrap(),
            b"not SQLite; source changes independently"
        );
    }
}

#[test]
fn task_materialize_retains_noncanonical_task_bytes_and_omission_provenance() {
    let mut retained = tree(1);
    retained.project.notes_included = true;
    retained.tasks[0]
        .notes
        .push(crate::task_sync::notes::SharedNote {
            id: crate::ObjectId::mint(),
            kind: crate::task_sync::notes::SharedNoteKind::Generic,
            body: "Useful shared progress".into(),
            author: crate::task_sync::SharedActor {
                actor_id: "fixture-author".into(),
                actor_kind: "agent".into(),
            },
            recorded_at: chrono::Utc::now(),
            origin_machine: "fixture-origin".into(),
        });
    let activity_id = crate::ObjectId::mint();
    let notice = crate::task_sync::activity::ActivityNotice {
        activity_id: activity_id.clone(),
        task_id: retained.tasks[0].id,
        origin_machine: "fixture-origin".into(),
        actor: crate::task_sync::SharedActor {
            actor_id: "fixture-author".into(),
            actor_kind: "agent".into(),
        },
        state: crate::task_sync::activity::ActivityState::Started,
        state_revision: 1,
        event_id: activity_id.clone(),
    };
    retained.tasks[0].activity.insert(
        "fixture-origin".into(),
        BTreeMap::from([(activity_id, notice)]),
    );
    let mut fixture = Fixture::with_tree(false, &retained);
    let path = fixture
        .documents
        .keys()
        .find(|path| path.starts_with("tasks/"))
        .unwrap()
        .clone();
    let mut raw = b" \n".to_vec();
    raw.extend_from_slice(&fixture.documents[&path]);
    fixture.documents.insert(path.clone(), raw.clone());
    fs::write(fixture.selection.candidate_directory.join(&path), &raw).unwrap();
    let generation = fs::read_dir(&fixture.selection.progress_directory)
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .find(|path| path.is_dir())
        .unwrap();
    fs::write(generation.join("candidate").join(&path), raw).unwrap();
    let mut state: serde_json::Value =
        serde_json::from_slice(&fs::read(generation.join("state.json")).unwrap()).unwrap();
    state["documents_fingerprint"] = progress::documents_fingerprint(&fixture.documents).into();
    fs::write(
        generation.join("state.json"),
        serde_json::to_vec(&state).unwrap(),
    )
    .unwrap();
    let receipt = fixture.run();
    fixture.assert_output();
    assert_eq!(receipt.omissions.counts["excluded-note"], 3);
    assert_eq!(receipt.activity_omissions.counts["private-activity"], 5);
}

fn assert_shared(fixture: &Fixture) {
    let public = fixture.root.join(".engram");
    for (path, bytes) in &fixture.documents {
        assert_eq!(&fs::read(public.join(path)).unwrap(), bytes, "{path}");
    }
    let mut names: Vec<_> = fs::read_dir(public.join("tasks"))
        .unwrap()
        .map(|entry| entry.unwrap().file_name())
        .collect();
    names.sort();
    let expected: Vec<_> = fixture
        .documents
        .keys()
        .filter_map(|path| path.strip_prefix("tasks/"))
        .map(std::ffi::OsString::from)
        .collect();
    assert_eq!(names, expected);
}

fn snapshot(root: &Path) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    snapshot_with_lock_metadata(root, None)
}

fn snapshot_with_lock_metadata(
    root: &Path,
    locked: Option<&Path>,
) -> BTreeMap<PathBuf, Option<Vec<u8>>> {
    fn visit(
        root: &Path,
        path: &Path,
        locked: Option<&Path>,
        out: &mut BTreeMap<PathBuf, Option<Vec<u8>>>,
    ) {
        let meta = fs::symlink_metadata(path).unwrap();
        let linked = meta.file_type().is_symlink();
        #[cfg(windows)]
        let linked = {
            use std::os::windows::fs::MetadataExt;
            linked || meta.file_attributes() & 0x400 != 0
        };
        out.insert(
            path.strip_prefix(root).unwrap().into(),
            if meta.is_file() && !linked {
                if locked == Some(path) {
                    // Windows refuses ReadFile under the held byte-range lock.
                    // The caller checks its bytes separately after lock release.
                    let file = fs::File::open(path).unwrap();
                    let id = super::super::files::file_identity(&file).unwrap();
                    Some(serde_json::to_vec(&(id, meta.len())).unwrap())
                } else {
                    Some(fs::read(path).unwrap())
                }
            } else {
                None
            },
        );
        if meta.is_dir() && !linked {
            for entry in fs::read_dir(path).unwrap() {
                visit(root, &entry.unwrap().path(), locked, out);
            }
        }
    }
    let mut result = BTreeMap::new();
    visit(root, root, locked, &mut result);
    result
}

#[test]
fn task_materialize_publishes_raw_bytes_with_private_state_and_exact_repeat() {
    for existing in [false, true] {
        let fixture = Fixture::new(existing, 2);
        let index = fs::read(fixture.admin.join("index")).unwrap();
        let config = fs::read(fixture.admin.join("config")).unwrap();
        let head = git(&fixture.root, &["rev-parse", "HEAD"]);
        let first = fixture.run();
        assert!(!first.repeated);
        let completion: serde_json::Value =
            serde_json::from_slice(&fs::read(fixture.private().join("complete")).unwrap()).unwrap();
        assert_eq!(completion["operation"], first.operation.to_string());
        fixture.assert_output();
        assert_eq!(fs::read(fixture.admin.join("index")).unwrap(), index);
        assert_eq!(fs::read(fixture.admin.join("config")).unwrap(), config);
        assert_eq!(git(&fixture.root, &["rev-parse", "HEAD"]), head);
        if existing {
            assert_eq!(
                fs::read(fixture.root.join(".engram/private.db")).unwrap(),
                b"private preserved bytes"
            );
        }
        let before = snapshot(fixture.owned.path());
        let second = fixture.run();
        assert!(second.repeated);
        assert_eq!(second.operation, first.operation);
        assert_eq!(second.attribution, first.attribution);
        assert_eq!(snapshot(fixture.owned.path()), before);
    }
}
